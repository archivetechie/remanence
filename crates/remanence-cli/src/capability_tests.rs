//! Capability rendering regressions use an in-memory gRPC connection so the
//! discovery-context query is exercised even where Unix sockets are forbidden.
use super::*;
use tonic::{Request, Response, Status};

pub(crate) fn endpoints() -> Vec<pb::ObservedEndpoint> {
    vec![pb::ObservedEndpoint {
        kind: "library".into(),
        serial: Some("DISCOVERED-SERVER".into()),
        path: Some("/dev/server-changer".into()),
        ownership: "library".into(),
        ..Default::default()
    }]
}
#[derive(Clone)]
pub(crate) struct DiscoveryDaemon;
#[tonic::async_trait]
impl pb::daemon_server::Daemon for DiscoveryDaemon {
    async fn health(&self, _: Request<()>) -> Result<Response<pb::HealthResponse>, Status> {
        Ok(Response::new(pb::HealthResponse {
            observed_endpoints: endpoints(),
            ..Default::default()
        }))
    }
    async fn version(&self, _: Request<()>) -> Result<Response<pb::VersionResponse>, Status> {
        Ok(Response::new(pb::VersionResponse {
            observed_endpoints: endpoints(),
            ..Default::default()
        }))
    }
    async fn get_operation(
        &self,
        _: Request<pb::GetOperationRequest>,
    ) -> Result<Response<pb::OperationStatus>, Status> {
        Err(Status::unimplemented("unused"))
    }
    async fn list_operations(
        &self,
        _: Request<pb::ListOperationsRequest>,
    ) -> Result<Response<pb::ListOperationsResponse>, Status> {
        Err(Status::unimplemented("unused"))
    }
    async fn cancel_operation(
        &self,
        _: Request<pb::CancelOperationRequest>,
    ) -> Result<Response<pb::CancelOperationResponse>, Status> {
        Err(Status::unimplemented("unused"))
    }
    type WatchOperationStream =
        tokio_stream::Iter<std::vec::IntoIter<Result<pb::OperationStatus, Status>>>;
    async fn watch_operation(
        &self,
        _: Request<pb::GetOperationRequest>,
    ) -> Result<Response<Self::WatchOperationStream>, Status> {
        Err(Status::unimplemented("unused"))
    }
}

#[test]
fn daemon_json_uses_server_discovery_and_keeps_debug_unchanged() {
    let _guard = OutputModeGuard::enter(CliMode::Rem);
    daemon_runtime().unwrap().block_on(async {
        let (client, server) = tokio::io::duplex(65536);
        let task = tokio::spawn(
            tonic::transport::Server::builder()
                .add_service(pb::daemon_server::DaemonServer::new(DiscoveryDaemon))
                .serve_with_incoming(tokio_stream::iter([Ok::<_, std::io::Error>(server)])),
        );
        let client = std::sync::Arc::new(std::sync::Mutex::new(Some(client)));
        let channel = tonic::transport::Endpoint::from_static("http://in-memory")
            .connect_with_connector(tower::service_fn(move |_| {
                let client = client.lock().unwrap().take().expect("one connection");
                async { Ok::<_, std::io::Error>(hyper_util::rt::TokioIo::new(client)) }
            }))
            .await
            .unwrap();
        observe_daemon(&channel, true).await.unwrap();
        let mut out = Vec::new();
        print_json_envelope("rem.catalog.test", "list", json!([]), &mut out).unwrap();
        let value: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(
            value["observed_endpoints"],
            json!(endpoint_json(&endpoints()))
        );
        assert_eq!(
            value["supported_endpoint_kinds"],
            json!(["library", "standalone"])
        );
        out.clear();
        print_json_error("test", "failure after discovery", &mut out).unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&out).unwrap()["observed_endpoints"],
            value["observed_endpoints"]
        );
        out.clear();
        print_operation_ref(pb::OperationRef::default(), true, &mut out).unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&out).unwrap()["observed_endpoints"],
            value["observed_endpoints"]
        );
        let _debug = OutputModeGuard::enter(CliMode::Debug);
        out.clear();
        print_json_envelope("legacy", "item", json!({"unchanged":true}), &mut out).unwrap();
        let legacy: Value = serde_json::from_slice(&out).unwrap();
        assert!(legacy.get("supported_endpoint_kinds").is_none());
        assert!(legacy.get("observed_endpoints").is_none());
        task.abort();
    });
}
