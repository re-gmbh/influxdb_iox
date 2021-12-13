//! Contains implementation of IOx system tables:
//!
//! system.chunks
//! system.columns
//! system.chunk_columns
//! system.operations
//!
//! For example `SELECT * FROM system.chunks`

use super::{catalog::Catalog, query_log::QueryLog};
use crate::JobRegistry;
use arrow::{
    datatypes::{Field, Schema, SchemaRef},
    error::Result,
    record_batch::RecordBatch,
};
use datafusion::{
    catalog::schema::SchemaProvider,
    datasource::TableProvider,
    error::{DataFusionError, Result as DataFusionResult},
    physical_plan::{memory::MemoryExec, ExecutionPlan},
};
use futures::FutureExt;
use std::{any::Any, sync::Arc};

mod chunks;
mod columns;
mod operations;
mod persistence;
mod queries;

// The IOx system schema
pub const SYSTEM_SCHEMA: &str = "system";

const CHUNKS: &str = "chunks";
const COLUMNS: &str = "columns";
const CHUNK_COLUMNS: &str = "chunk_columns";
const OPERATIONS: &str = "operations";
const PERSISTENCE_WINDOWS: &str = "persistence_windows";
const QUERIES: &str = "queries";

pub struct SystemSchemaProvider {
    chunks: Arc<dyn TableProvider>,
    columns: Arc<dyn TableProvider>,
    chunk_columns: Arc<dyn TableProvider>,
    operations: Arc<dyn TableProvider>,
    persistence_windows: Arc<dyn TableProvider>,
    queries: Arc<dyn TableProvider>,
}

impl std::fmt::Debug for SystemSchemaProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SystemSchemaProvider")
            .field("fields", &"...")
            .finish()
    }
}

impl SystemSchemaProvider {
    pub fn new(
        db_name: impl Into<String>,
        catalog: Arc<Catalog>,
        jobs: Arc<JobRegistry>,
        query_log: Arc<QueryLog>,
    ) -> Self {
        let db_name = db_name.into();
        let chunks = Arc::new(SystemTableProvider {
            inner: chunks::ChunksTable::new(Arc::clone(&catalog)),
        });
        let columns = Arc::new(SystemTableProvider {
            inner: columns::ColumnsTable::new(Arc::clone(&catalog)),
        });
        let chunk_columns = Arc::new(SystemTableProvider {
            inner: columns::ChunkColumnsTable::new(Arc::clone(&catalog)),
        });
        let operations = Arc::new(SystemTableProvider {
            inner: operations::OperationsTable::new(db_name, jobs),
        });
        let persistence_windows = Arc::new(SystemTableProvider {
            inner: persistence::PersistenceWindowsTable::new(catalog),
        });
        let queries = Arc::new(SystemTableProvider {
            inner: queries::QueriesTable::new(query_log),
        });
        Self {
            chunks,
            columns,
            chunk_columns,
            operations,
            persistence_windows,
            queries,
        }
    }
}

const ALL_SYSTEM_TABLES: [&str; 6] = [
    CHUNKS,
    COLUMNS,
    CHUNK_COLUMNS,
    OPERATIONS,
    PERSISTENCE_WINDOWS,
    QUERIES,
];

impl SchemaProvider for SystemSchemaProvider {
    fn as_any(&self) -> &dyn Any {
        self as &dyn Any
    }

    fn table_names(&self) -> Vec<String> {
        ALL_SYSTEM_TABLES
            .iter()
            .map(|name| name.to_string())
            .collect()
    }

    fn table(&self, name: &str) -> Option<Arc<dyn TableProvider>> {
        match name {
            CHUNKS => Some(Arc::clone(&self.chunks)),
            COLUMNS => Some(Arc::clone(&self.columns)),
            CHUNK_COLUMNS => Some(Arc::clone(&self.chunk_columns)),
            OPERATIONS => Some(Arc::clone(&self.operations)),
            PERSISTENCE_WINDOWS => Some(Arc::clone(&self.persistence_windows)),
            QUERIES => Some(Arc::clone(&self.queries)),
            _ => None,
        }
    }

    fn table_exist(&self, name: &str) -> bool {
        ALL_SYSTEM_TABLES
            .iter()
            .any(|&system_table| system_table == name)
    }
}

/// The minimal thing that a system table needs to implement
trait IoxSystemTable: Send + Sync {
    /// Produce the schema from this system table
    fn schema(&self) -> SchemaRef;

    /// Get the contents of the system table as a single RecordBatch
    fn batch(&self) -> Result<RecordBatch>;
}

/// Adapter that makes any `IoxSystemTable` a DataFusion `TableProvider`
struct SystemTableProvider<T>
where
    T: IoxSystemTable,
{
    inner: T,
}

impl<T> TableProvider for SystemTableProvider<T>
where
    T: IoxSystemTable + 'static,
{
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn schema(&self) -> SchemaRef {
        self.inner.schema()
    }

    fn scan<'life0, 'life1, 'life2, 'async_trait>(
        &'life0 self,
        projection: &'life1 Option<Vec<usize>>,
        _batch_size: usize,
        // It would be cool to push projection and limit down
        _filters: &'life2 [datafusion::logical_plan::Expr],
        _limit: Option<usize>,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = DataFusionResult<Arc<dyn ExecutionPlan>>>
                + Send
                + 'async_trait,
        >,
    >
    where
        'life0: 'async_trait,
        'life1: 'async_trait,
        'life2: 'async_trait,
        Self: 'async_trait,
    {
        async move { scan_batch(self.inner.batch()?, self.schema(), projection.as_ref()) }.boxed()
    }
}

/// Creates a DataFusion ExecutionPlan node that scans a single batch
/// of records.
fn scan_batch(
    batch: RecordBatch,
    schema: SchemaRef,
    projection: Option<&Vec<usize>>,
) -> DataFusionResult<Arc<dyn ExecutionPlan>> {
    // apply projection, if any
    let (schema, batch) = match projection {
        None => (schema, batch),
        Some(projection) => {
            let projected_columns: DataFusionResult<Vec<Field>> = projection
                .iter()
                .map(|i| {
                    if *i < schema.fields().len() {
                        Ok(schema.field(*i).clone())
                    } else {
                        Err(DataFusionError::Internal(format!(
                            "Projection index out of range in ChunksProvider: {}",
                            i
                        )))
                    }
                })
                .collect();

            let projected_schema = Arc::new(Schema::new(projected_columns?));

            let columns = projection
                .iter()
                .map(|i| Arc::clone(batch.column(*i)))
                .collect::<Vec<_>>();

            let projected_batch = RecordBatch::try_new(Arc::clone(&projected_schema), columns)?;
            (projected_schema, projected_batch)
        }
    };

    Ok(Arc::new(MemoryExec::try_new(&[vec![batch]], schema, None)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow::array::{ArrayRef, UInt64Array};
    use arrow_util::assert_batches_eq;

    fn seq_array(start: u64, end: u64) -> ArrayRef {
        Arc::new(UInt64Array::from_iter_values(start..end))
    }

    #[tokio::test]
    async fn test_scan_batch_no_projection() {
        let batch = RecordBatch::try_from_iter(vec![
            ("col1", seq_array(0, 3)),
            ("col2", seq_array(1, 4)),
            ("col3", seq_array(2, 5)),
            ("col4", seq_array(3, 6)),
        ])
        .unwrap();

        let projection = None;
        let scan = scan_batch(batch.clone(), batch.schema(), projection).unwrap();
        let collected = datafusion::physical_plan::collect(scan).await.unwrap();

        let expected = vec![
            "+------+------+------+------+",
            "| col1 | col2 | col3 | col4 |",
            "+------+------+------+------+",
            "| 0    | 1    | 2    | 3    |",
            "| 1    | 2    | 3    | 4    |",
            "| 2    | 3    | 4    | 5    |",
            "+------+------+------+------+",
        ];

        assert_batches_eq!(&expected, &collected);
    }

    #[tokio::test]
    async fn test_scan_batch_good_projection() {
        let batch = RecordBatch::try_from_iter(vec![
            ("col1", seq_array(0, 3)),
            ("col2", seq_array(1, 4)),
            ("col3", seq_array(2, 5)),
            ("col4", seq_array(3, 6)),
        ])
        .unwrap();

        let projection = Some(vec![3, 1]);
        let scan = scan_batch(batch.clone(), batch.schema(), projection.as_ref()).unwrap();
        let collected = datafusion::physical_plan::collect(scan).await.unwrap();

        let expected = vec![
            "+------+------+",
            "| col4 | col2 |",
            "+------+------+",
            "| 3    | 1    |",
            "| 4    | 2    |",
            "| 5    | 3    |",
            "+------+------+",
        ];

        assert_batches_eq!(&expected, &collected);
    }

    #[tokio::test]
    async fn test_scan_batch_bad_projection() {
        let batch = RecordBatch::try_from_iter(vec![
            ("col1", seq_array(0, 3)),
            ("col2", seq_array(1, 4)),
            ("col3", seq_array(2, 5)),
            ("col4", seq_array(3, 6)),
        ])
        .unwrap();

        // no column idex 5
        let projection = Some(vec![3, 1, 5]);
        let result = scan_batch(batch.clone(), batch.schema(), projection.as_ref());
        let err_string = result.unwrap_err().to_string();
        assert!(
            err_string
                .contains("Internal error: Projection index out of range in ChunksProvider: 5"),
            "Actual error: {}",
            err_string
        );
    }
}
