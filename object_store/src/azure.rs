//! This module contains the IOx implementation for using Azure Blob storage as
//! the object store.
use crate::{
    path::{cloud::CloudPath, DELIMITER},
    GetResult, ListResult, ObjectMeta, ObjectStoreApi, ObjectStorePath,
};
use azure_core::prelude::*;
use azure_storage::{
    blob::prelude::{AsBlobClient, AsContainerClient, ContainerClient},
    core::clients::{AsStorageClient, StorageAccountClient},
    DeleteSnapshotsMethod,
};
use bytes::Bytes;
use futures::{
    future::BoxFuture,
    stream::{self, BoxStream},
    FutureExt, StreamExt,
};
use snafu::{ResultExt, Snafu};
use std::{convert::TryInto, sync::Arc};

/// A specialized `Result` for Azure object store-related errors
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// A specialized `Error` for Azure object store-related errors
#[derive(Debug, Snafu)]
#[allow(missing_docs)]
pub enum Error {
    #[snafu(display("Unable to DELETE data. Location: {}, Error: {}", location, source,))]
    Delete {
        source: Box<dyn std::error::Error + Send + Sync>,
        location: String,
    },

    #[snafu(display("Unable to GET data. Location: {}, Error: {}", location, source,))]
    Get {
        source: Box<dyn std::error::Error + Send + Sync>,
        location: String,
    },

    #[snafu(display("Unable to PUT data. Location: {}, Error: {}", location, source,))]
    Put {
        source: Box<dyn std::error::Error + Send + Sync>,
        location: String,
    },

    #[snafu(display("Unable to list data. Error: {}", source))]
    List {
        source: Box<dyn std::error::Error + Send + Sync>,
    },
}

/// Configuration for connecting to [Microsoft Azure Blob Storage](https://azure.microsoft.com/en-us/services/storage/blobs/).
#[derive(Debug)]
pub struct MicrosoftAzure {
    container_client: Arc<ContainerClient>,
    #[allow(dead_code)]
    container_name: String,
}

impl ObjectStoreApi for MicrosoftAzure {
    type Path = CloudPath;
    type Error = Error;

    fn new_path(&self) -> Self::Path {
        CloudPath::default()
    }

    fn path_from_raw(&self, raw: &str) -> Self::Path {
        CloudPath::raw(raw)
    }

    fn put<'a>(
        &'a self,
        location: &'a Self::Path,
        bytes: Bytes,
    ) -> BoxFuture<'a, Result<(), Self::Error>> {
        async move {
            let location = location.to_raw();

            let bytes = bytes::BytesMut::from(&*bytes);

            self.container_client
                .as_blob_client(&location)
                .put_block_blob(bytes)
                .execute()
                .await
                .context(Put {
                    location: location.to_owned(),
                })?;

            Ok(())
        }
        .boxed()
    }

    fn get<'a>(
        &'a self,
        location: &'a Self::Path,
    ) -> BoxFuture<'a, Result<GetResult<Self::Error>, Self::Error>> {
        async move {
            let container_client = Arc::clone(&self.container_client);
            let location = location.to_raw();
            let s = async move {
                container_client
                    .as_blob_client(&location)
                    .get()
                    .execute()
                    .await
                    .map(|blob| blob.data)
                    .context(Get {
                        location: location.to_owned(),
                    })
            }
            .into_stream()
            .boxed();

            Ok(GetResult::Stream(s))
        }
        .boxed()
    }

    fn delete<'a>(&'a self, location: &'a Self::Path) -> BoxFuture<'a, Result<(), Self::Error>> {
        async move {
            let location = location.to_raw();
            self.container_client
                .as_blob_client(&location)
                .delete()
                .delete_snapshots_method(DeleteSnapshotsMethod::Include)
                .execute()
                .await
                .context(Delete {
                    location: location.to_owned(),
                })?;

            Ok(())
        }
        .boxed()
    }

    #[allow(clippy::type_complexity)]
    fn list<'a>(
        &'a self,
        prefix: Option<&'a Self::Path>,
    ) -> BoxFuture<'a, Result<BoxStream<'a, Result<Vec<Self::Path>>>>> {
        async move {
            #[derive(Clone)]
            enum ListState {
                Start,
                HasMore(String),
                Done,
            }

            Ok(stream::unfold(ListState::Start, move |state| async move {
                let mut request = self.container_client.list_blobs();

                let prefix = prefix.map(|p| p.to_raw());
                if let Some(ref p) = prefix {
                    request = request.prefix(p as &str);
                }

                match state {
                    ListState::HasMore(ref marker) => {
                        request = request.next_marker(marker as &str);
                    }
                    ListState::Done => {
                        return None;
                    }
                    ListState::Start => {}
                }

                let resp = match request.execute().await.context(List) {
                    Ok(resp) => resp,
                    Err(err) => return Some((Err(err), state)),
                };

                let next_state = if let Some(marker) = resp.next_marker {
                    ListState::HasMore(marker.as_str().to_string())
                } else {
                    ListState::Done
                };

                let names = resp
                    .blobs
                    .blobs
                    .into_iter()
                    .map(|blob| CloudPath::raw(blob.name))
                    .collect();

                Some((Ok(names), next_state))
            })
            .boxed())
        }
        .boxed()
    }

    fn list_with_delimiter<'a>(
        &'a self,
        prefix: &'a Self::Path,
    ) -> BoxFuture<'a, Result<ListResult<Self::Path>, Self::Error>> {
        async move {
            let mut request = self.container_client.list_blobs();

            let prefix = prefix.to_raw();

            request = request.delimiter(Delimiter::new(DELIMITER));
            request = request.prefix(&*prefix);

            let resp = request.execute().await.context(List)?;

            let next_token = resp.next_marker.as_ref().map(|m| m.as_str().to_string());

            let common_prefixes = resp
                .blobs
                .blob_prefix
                .map(|prefixes| {
                    prefixes
                        .iter()
                        .map(|prefix| CloudPath::raw(&prefix.name))
                        .collect()
                })
                .unwrap_or_else(Vec::new);

            let objects = resp
                .blobs
                .blobs
                .into_iter()
                .map(|blob| {
                    let location = CloudPath::raw(blob.name);
                    let last_modified = blob.properties.last_modified;
                    let size = blob
                        .properties
                        .content_length
                        .try_into()
                        .expect("unsupported size on this platform");

                    ObjectMeta {
                        location,
                        last_modified,
                        size,
                    }
                })
                .collect();

            Ok(ListResult {
                next_token,
                common_prefixes,
                objects,
            })
        }
        .boxed()
    }
}

/// Configure a connection to container with given name on Microsoft Azure
/// Blob store.
///
/// The credentials `account` and `access_key` must provide access to the
/// store.
pub fn new_azure(
    account: impl Into<String>,
    access_key: impl Into<String>,
    container_name: impl Into<String>,
) -> Result<MicrosoftAzure> {
    let account = account.into();
    let access_key = access_key.into();
    let http_client: Arc<dyn HttpClient> = Arc::new(reqwest::Client::new());

    let storage_account_client =
        StorageAccountClient::new_access_key(Arc::clone(&http_client), &account, &access_key);

    let storage_client = storage_account_client.as_storage_client();

    let container_name = container_name.into();

    let container_client = storage_client.as_container_client(&container_name);

    Ok(MicrosoftAzure {
        container_client,
        container_name,
    })
}

#[cfg(test)]
mod tests {
    use crate::tests::{list_with_delimiter, put_get_delete_list};
    use crate::ObjectStore;
    use std::env;

    #[derive(Debug)]
    struct AzureConfig {
        storage_account: String,
        access_key: String,
        bucket: String,
    }

    // Helper macro to skip tests if TEST_INTEGRATION and the Azure environment
    // variables are not set.
    macro_rules! maybe_skip_integration {
        () => {{
            dotenv::dotenv().ok();

            let required_vars = [
                "AZURE_STORAGE_ACCOUNT",
                "INFLUXDB_IOX_BUCKET",
                "AZURE_STORAGE_ACCESS_KEY",
            ];
            let unset_vars: Vec<_> = required_vars
                .iter()
                .filter_map(|&name| match env::var(name) {
                    Ok(_) => None,
                    Err(_) => Some(name),
                })
                .collect();
            let unset_var_names = unset_vars.join(", ");

            let force = std::env::var("TEST_INTEGRATION");

            if force.is_ok() && !unset_var_names.is_empty() {
                panic!(
                    "TEST_INTEGRATION is set, \
                        but variable(s) {} need to be set",
                    unset_var_names
                )
            } else if force.is_err() {
                eprintln!(
                    "skipping Azure integration test - set {}TEST_INTEGRATION to run",
                    if unset_var_names.is_empty() {
                        String::new()
                    } else {
                        format!("{} and ", unset_var_names)
                    }
                );
                return;
            } else {
                AzureConfig {
                    storage_account: env::var("AZURE_STORAGE_ACCOUNT")
                        .expect("already checked AZURE_STORAGE_ACCOUNT"),
                    access_key: env::var("AZURE_STORAGE_ACCESS_KEY")
                        .expect("already checked AZURE_STORAGE_ACCESS_KEY"),
                    bucket: env::var("INFLUXDB_IOX_BUCKET")
                        .expect("already checked INFLUXDB_IOX_BUCKET"),
                }
            }
        }};
    }

    #[tokio::test]
    async fn azure_blob_test() {
        let config = maybe_skip_integration!();
        let integration = ObjectStore::new_microsoft_azure(
            config.storage_account,
            config.access_key,
            config.bucket,
        )
        .unwrap();

        put_get_delete_list(&integration).await.unwrap();
        list_with_delimiter(&integration).await.unwrap();
    }
}
