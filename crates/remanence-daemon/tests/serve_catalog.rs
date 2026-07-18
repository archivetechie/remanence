//! End-to-end: serve() over a temp Unix socket, then a real gRPC client
//! round-trips Daemon.health, Catalog.list_tape_pools, and
//! LibraryService.list_libraries through it.

use remanence_api::{pb, ApiState};
use remanence_state::{
    AuditActor, AuditEvent, AuditEventRecord, AuditSubject, CatalogIndex, FileAuditLog,
    SourceLayer, TapePoolProjectionInput,
};
use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use tonic::transport::{Certificate, ClientTlsConfig, Endpoint, Identity, ServerTlsConfig};

#[tokio::test]
async fn serve_catalog_roundtrips_health_and_pools_over_unix_socket() {
    let dir = tempfile::Builder::new()
        .prefix("rem-daemon-it")
        .tempdir()
        .expect("tempdir");
    let socket_path = dir.path().join("rem.sock");

    let mut index = CatalogIndex::open(dir.path().join("state.sqlite")).expect("open index");
    index
        .upsert_tape_pool_projection(TapePoolProjectionInput {
            pool_id: "scenario-a".to_string(),
            display_name: None,
            copy_class: None,
            content_class: None,
            created_at_utc: None,
        })
        .expect("seed pool");
    let session_id = uuid::Uuid::new_v4();
    FileAuditLog::open(dir.path().join("audit"), false)
        .expect("open audit")
        .append_and_return_record(AuditEventRecord {
            actor: AuditActor::System,
            source_layer: SourceLayer::Layer5,
            operation_id: None,
            session_id: Some(session_id),
            idempotency_key: None,
            event: AuditEvent::OperationFailed,
            subject: AuditSubject {
                kind: "session".to_string(),
                id: Some(session_id.to_string()),
            },
            detail: BTreeMap::new(),
        })
        .expect("seed audit");
    let state = ApiState::new(index);
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let serve_socket = socket_path.clone();
    let server = tokio::spawn(async move {
        remanence_daemon::serve(state, &serve_socket, None, async {
            let _ = shutdown_rx.await;
        })
        .await
        .expect("serve");
    });

    for _ in 0..100 {
        if socket_path.exists() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(socket_path.exists(), "daemon did not create the socket");

    let channel = remanence_api::connect_unix(socket_path.clone())
        .await
        .expect("connect unix");

    let mut daemon = pb::daemon_client::DaemonClient::new(channel.clone());
    daemon.health(()).await.expect("health");

    let mut catalog = pb::catalog_client::CatalogClient::new(channel.clone());
    let pools = catalog
        .list_tape_pools(pb::ListTapePoolsRequest::default())
        .await
        .expect("list_tape_pools")
        .into_inner()
        .pools;
    assert_eq!(pools.len(), 1);
    assert_eq!(pools[0].pool_id, "scenario-a");

    let mut libraries = pb::library_service_client::LibraryServiceClient::new(channel);
    let libraries = libraries
        .list_libraries(())
        .await
        .expect("list_libraries")
        .into_inner()
        .libraries;
    assert!(
        libraries.is_empty(),
        "read-only state has no inventory snapshot"
    );

    let channel = remanence_api::connect_unix(socket_path.clone())
        .await
        .expect("reconnect unix audit");
    let mut audit = pb::audit_client::AuditClient::new(channel);
    let mut entries = audit
        .query_audit(pb::QueryAuditRequest {
            since: Some(prost_types::Timestamp {
                seconds: 0,
                nanos: 0,
            }),
            until: Some(prost_types::Timestamp {
                seconds: 4_102_444_800,
                nanos: 0,
            }),
            filter: [("session_id".to_string(), session_id.to_string())]
                .into_iter()
                .collect(),
        })
        .await
        .expect("query audit over unix transport")
        .into_inner();
    let entry = entries
        .message()
        .await
        .expect("read audit entry")
        .expect("filtered audit entry");
    assert_eq!(entry.session_id, session_id.as_bytes());
    assert_eq!(entry.event_kind, "OperationFailed");
    assert!(
        entries
            .message()
            .await
            .expect("finish audit stream")
            .is_none(),
        "audit filter must return exactly one entry"
    );

    let _ = shutdown_tx.send(());
    server.await.expect("server task");
    assert!(
        !socket_path.exists(),
        "socket should be unlinked on shutdown"
    );
}

#[tokio::test]
async fn serve_audit_query_roundtrips_over_mtls_transport() {
    let dir = tempfile::Builder::new()
        .prefix("rem-daemon-audit-mtls")
        .tempdir()
        .expect("tempdir");
    let socket_path = dir.path().join("rem.sock");
    let index = CatalogIndex::open(dir.path().join("state.sqlite")).expect("open index");
    let session_id = uuid::Uuid::new_v4();
    FileAuditLog::open(dir.path().join("audit"), false)
        .expect("open audit")
        .append_and_return_record(AuditEventRecord {
            actor: AuditActor::System,
            source_layer: SourceLayer::Layer5,
            operation_id: None,
            session_id: Some(session_id),
            idempotency_key: None,
            event: AuditEvent::OperationFailed,
            subject: AuditSubject {
                kind: "session".to_string(),
                id: Some(session_id.to_string()),
            },
            detail: BTreeMap::new(),
        })
        .expect("seed audit");
    let reserved_tcp = std::net::TcpListener::bind("127.0.0.1:0")
        .expect("loopback TCP must be available for the mTLS transport test");
    let tcp_addr = reserved_tcp.local_addr().expect("reserved TCP address");
    drop(reserved_tcp);
    let ca = include_bytes!("fixtures/tls/ca.pem");
    let tls_listener = remanence_daemon::TlsListener {
        addr: tcp_addr,
        tls: ServerTlsConfig::new()
            .identity(Identity::from_pem(
                include_bytes!("fixtures/tls/server.pem"),
                include_bytes!("fixtures/tls/server.key"),
            ))
            .client_ca_root(Certificate::from_pem(ca)),
    };
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let serve_socket = socket_path.clone();
    let server = tokio::spawn(async move {
        remanence_daemon::serve(
            ApiState::new(index),
            &serve_socket,
            Some(tls_listener),
            async {
                let _ = shutdown_rx.await;
            },
        )
        .await
        .expect("serve");
    });
    for _ in 0..100 {
        if socket_path.exists() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(socket_path.exists(), "daemon did not create the socket");

    let tls = ClientTlsConfig::new()
        .domain_name("localhost")
        .ca_certificate(Certificate::from_pem(ca))
        .identity(Identity::from_pem(
            include_bytes!("fixtures/tls/client.pem"),
            include_bytes!("fixtures/tls/client.key"),
        ));
    let channel = Endpoint::from_shared(format!("https://localhost:{}", tcp_addr.port()))
        .expect("TCP endpoint")
        .tls_config(tls)
        .expect("client TLS config")
        .connect()
        .await
        .expect("connect mTLS transport");
    let mut audit = pb::audit_client::AuditClient::new(channel);
    let mut entries = audit
        .query_audit(pb::QueryAuditRequest {
            since: Some(prost_types::Timestamp {
                seconds: 0,
                nanos: 0,
            }),
            until: Some(prost_types::Timestamp {
                seconds: 4_102_444_800,
                nanos: 0,
            }),
            filter: [("event_kind".to_string(), "OperationFailed".to_string())]
                .into_iter()
                .collect(),
        })
        .await
        .expect("query audit over mTLS transport")
        .into_inner();
    let entry = entries
        .message()
        .await
        .expect("read mTLS audit entry")
        .expect("filtered mTLS audit entry");
    assert_eq!(entry.session_id, session_id.as_bytes());
    assert_eq!(entry.event_kind, "OperationFailed");
    assert!(
        entries
            .message()
            .await
            .expect("finish mTLS audit stream")
            .is_none(),
        "mTLS audit filter must return exactly one entry"
    );

    let _ = shutdown_tx.send(());
    server.await.expect("server task");
}

#[tokio::test]
async fn serve_exits_when_shutdown_is_already_ready() {
    let dir = tempfile::Builder::new()
        .prefix("rem-daemon-immediate-stop")
        .tempdir()
        .expect("tempdir");
    let socket_path = dir.path().join("rem.sock");
    let index = CatalogIndex::open(dir.path().join("state.sqlite")).expect("open index");
    let state = ApiState::new(index);

    remanence_daemon::serve(state, &socket_path, None, async {})
        .await
        .expect("serve exits");

    assert!(
        !socket_path.exists(),
        "socket should be unlinked after immediate shutdown"
    );
}

#[tokio::test]
async fn serve_hardens_created_socket_directory_and_socket_mode() {
    let dir = tempfile::Builder::new()
        .prefix("rem-daemon-socket-mode")
        .tempdir()
        .expect("tempdir");
    let socket_dir = dir.path().join("runtime");
    let socket_path = socket_dir.join("rem.sock");
    let index = CatalogIndex::open(dir.path().join("state.sqlite")).expect("open index");
    let state = ApiState::new(index);

    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let serve_socket = socket_path.clone();
    let server = tokio::spawn(async move {
        remanence_daemon::serve(state, &serve_socket, None, async {
            let _ = shutdown_rx.await;
        })
        .await
        .expect("serve");
    });

    for _ in 0..100 {
        if socket_path.exists() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(socket_path.exists(), "daemon did not create the socket");

    let dir_mode = std::fs::metadata(&socket_dir)
        .expect("socket dir metadata")
        .permissions()
        .mode()
        & 0o777;
    let socket_mode = std::fs::metadata(&socket_path)
        .expect("socket metadata")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(dir_mode, 0o700);
    assert_eq!(socket_mode, 0o660);

    let _ = shutdown_tx.send(());
    server.await.expect("server task");
}

#[tokio::test]
async fn serve_refuses_to_unlink_live_unix_socket() {
    let dir = tempfile::Builder::new()
        .prefix("rem-daemon-live-socket")
        .tempdir()
        .expect("tempdir");
    let socket_path = dir.path().join("rem.sock");
    let _owner = std::os::unix::net::UnixListener::bind(&socket_path).expect("occupy socket");
    let index = CatalogIndex::open(dir.path().join("state.sqlite")).expect("open index");
    let state = ApiState::new(index);

    let err = remanence_daemon::serve(state, &socket_path, None, async {})
        .await
        .expect_err("live socket must not be stolen");

    assert!(
        err.to_string().contains("already has a listening owner"),
        "{err}"
    );
    assert!(socket_path.exists(), "live socket should remain in place");
}

#[tokio::test]
async fn read_only_state_refuses_write_session() {
    use remanence_api::pb::write_session_service_server::WriteSessionService as _;
    use tonic::Request;

    let dir = tempfile::Builder::new()
        .prefix("rem-s2-ro")
        .tempdir()
        .expect("tempdir");
    let index = CatalogIndex::open(dir.path().join("state.sqlite")).expect("open index");
    let state = ApiState::new(index);

    let err = state
        .write_session_service()
        .get_write_session(Request::new(pb::GetWriteSessionRequest {
            session_id: vec![0u8; 16],
        }))
        .await
        .expect_err("read-only must refuse writes");
    assert_eq!(err.code(), tonic::Code::Unavailable);
}
