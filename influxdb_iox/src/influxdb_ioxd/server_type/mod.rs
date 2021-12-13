use futures::future::BoxFuture;
use hyper::{Body, Request, Response};
use metric::Registry;
use snafu::Snafu;
use std::sync::Arc;
use trace::TraceCollector;

use crate::influxdb_ioxd::{http::error::HttpApiErrorSource, rpc::RpcBuilderInput};

pub mod common_state;
pub mod database;
pub mod router;

#[derive(Debug, Snafu)]
pub enum RpcError {
    #[snafu(display("gRPC transport error: {}{}", source, details))]
    TransportError {
        source: tonic::transport::Error,
        details: String,
    },
}

// Custom impl to include underlying source (not included in tonic
// transport error)
impl From<tonic::transport::Error> for RpcError {
    fn from(source: tonic::transport::Error) -> Self {
        use std::error::Error;
        let details = source
            .source()
            .map(|e| format!(" ({})", e))
            .unwrap_or_else(|| "".to_string());

        Self::TransportError { source, details }
    }
}

pub trait ServerType: std::fmt::Debug + Send + Sync + 'static {
    type RouteError: HttpApiErrorSource;

    /// Metric registry associated with the server.
    fn metric_registry(&self) -> Arc<Registry>;

    /// Trace collector associated with the server, if any.
    fn trace_collector(&self) -> Option<Arc<dyn TraceCollector>>;

    /// Route given HTTP request.
    ///
    /// Note that this is only called if none of the shared, common routes (e.g. `/health`) match.
    fn route_http_request(
        &self,
        req: Request<Body>,
    ) -> BoxFuture<'_, Result<Response<Body>, Self::RouteError>>;

    /// Construct and serve gRPC subsystem.
    fn server_grpc(
        self: Arc<Self>,
        builder_input: RpcBuilderInput,
    ) -> BoxFuture<'static, Result<(), RpcError>>;

    /// Join shutdown worker.
    ///
    /// This MUST NOT exit before `shutdown` is called, otherwise the server is deemed to be dead
    /// and the process will exit.
    fn join(self: Arc<Self>) -> BoxFuture<'static, ()>;

    /// Shutdown background worker.
    fn shutdown(&self);
}
