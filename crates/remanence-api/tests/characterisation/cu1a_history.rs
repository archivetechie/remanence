//! CU-1a-2: library history through the production services over in-memory HTTP/2.
use super::*;
use std::collections::{BTreeSet, HashMap};

fn request() -> pb::QueryAuditRequest {
    pb::QueryAuditRequest {
        since: Some(prost_types::Timestamp {
            seconds: 0,
            nanos: 0,
        }),
        until: Some(prost_types::Timestamp {
            seconds: 4_102_444_800,
            nanos: 0,
        }),
        filter: HashMap::new(),
    }
}

async fn entries(client: &Client, request: pb::QueryAuditRequest) -> Vec<pb::AuditEntry> {
    let mut stream = pb::audit_client::AuditClient::new(client.channel.clone())
        .query_audit(request)
        .await
        .unwrap()
        .into_inner();
    let mut entries = Vec::new();
    while let Some(entry) = stream.message().await.unwrap() {
        entries.push(entry);
    }
    entries
}

fn subject(entry: &pb::AuditEntry) -> serde_json::Value {
    let detail: serde_json::Value = serde_json::from_str(&entry.detail_json).unwrap();
    assert!(detail["subject"].is_object(), "{detail}");
    detail["subject"].clone()
}

fn stamp(entry: &pb::AuditEntry) -> (i64, i32) {
    let time = entry.timestamp.as_ref().unwrap();
    (time.seconds, time.nanos)
}

#[tokio::test]
async fn library_records_and_filters_roundtrip() {
    let fixture = Harness::new(true);
    let client = fixture.start().await;
    let writer = client.open().await;
    let bytes = b"library history includes write and read sessions";
    let object = client.append(&writer, "cu1a-history", bytes).await;
    client.close(&writer).await;
    client.assert_object(&object, bytes).await;

    let closeout = pb::catalog_client::CatalogClient::new(client.channel.clone())
        .closeout_tape(pb::CloseoutTapeRequest {
            tape_uuid: UUID.to_vec(),
            expected_pool_id: Some("archive".into()),
            idempotency_key: Some(pb::IdempotencyKey {
                value: uuid::Uuid::new_v4().as_bytes().to_vec(),
            }),
        })
        .await
        .unwrap()
        .into_inner();
    let mut watch = pb::daemon_client::DaemonClient::new(client.channel.clone())
        .watch_operation(pb::GetOperationRequest {
            operation_id: closeout.operation_id,
        })
        .await
        .unwrap()
        .into_inner();
    let last = tokio::time::timeout(Duration::from_secs(30), async {
        let mut last = None;
        while let Some(status) = watch.message().await.unwrap() {
            last = Some(status);
        }
        last.unwrap()
    })
    .await
    .unwrap();
    assert_eq!(last.state, pb::OperationState::Succeeded as i32, "{last:?}");
    client.state.shutdown_drive_pool().await.unwrap();

    let mut all = entries(&client, request()).await;
    assert!(all.len() >= 4, "E.len() >= 4: got {}", all.len());
    // The startup diagnostic has its own thread, which drive-pool shutdown does not join.
    let deadline = Instant::now() + Duration::from_secs(30);
    while !all
        .iter()
        .any(|entry| entry.detail_json.contains("ForeignDrivePollDisabled"))
    {
        assert!(
            Instant::now() < deadline,
            "startup diagnostic did not settle"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
        all = entries(&client, request()).await;
    }
    assert_eq!(
        all.iter().map(|entry| entry.sequence).collect::<Vec<_>>(),
        (1..=all.len() as u64).collect::<Vec<_>>()
    );
    for entry in &all {
        subject(entry);
    }
    let read_open = all
        .iter()
        .find(|entry| {
            if entry.event_kind != "SessionObserved.Opened" {
                return false;
            }
            let detail: serde_json::Value = serde_json::from_str(&entry.detail_json).unwrap();
            detail["body"]["kind"] == "Read"
        })
        .expect("read session opened");
    assert_eq!(read_open.session_id.len(), 16);
    assert!(all
        .iter()
        .any(|entry| entry.event_kind == "SessionObserved.Closed"
            && entry.session_id == read_open.session_id));

    for (event, family) in [
        ("SessionObserved", true),
        ("SessionObserved.Opened", false),
        ("OperationRecorded", true),
        ("OperationRecorded.Finished", false),
    ] {
        let expected: Vec<_> = all
            .iter()
            .filter(|entry| {
                if family {
                    entry.event_kind.split('.').next() == Some(event)
                } else {
                    entry.event_kind == event
                }
            })
            .cloned()
            .collect();
        assert!(!expected.is_empty(), "{event}");
        let mut query = request();
        query.filter.insert("event".into(), event.into());
        assert_eq!(entries(&client, query).await, expected, "{event}");
    }

    let tape = all
        .iter()
        .map(subject)
        .find(|subject| subject["kind"] == "tape")
        .expect("tape subject");
    let tape_id = uuid::Uuid::parse_str(tape["id"].as_str().unwrap()).unwrap();
    let expected: Vec<_> = all
        .iter()
        .filter(|entry| subject(entry) == tape)
        .cloned()
        .collect();
    assert!(!expected.is_empty());
    let mut query = request();
    query.filter.insert("subject_kind".into(), "tape".into());
    query.filter.insert(
        "subject_id".into(),
        tape_id.simple().to_string().to_ascii_uppercase(),
    );
    assert_eq!(entries(&client, query).await, expected);

    let times: BTreeSet<_> = all.iter().map(stamp).collect();
    assert!(times.len() >= 2, "two distinct observed timestamps");
    let mut times = times.into_iter();
    let since = times.next().unwrap();
    let until = times.next().unwrap();
    assert!(since < until);
    let expected: Vec<_> = all
        .iter()
        .filter(|entry| stamp(entry) >= since && stamp(entry) < until)
        .cloned()
        .collect();
    assert!(expected.iter().any(|entry| stamp(entry) == since));
    assert!(all.iter().any(|entry| stamp(entry) == until));
    let mut query = request();
    query.since = Some(prost_types::Timestamp {
        seconds: since.0,
        nanos: since.1,
    });
    query.until = Some(prost_types::Timestamp {
        seconds: until.0,
        nanos: until.1,
    });
    assert_eq!(entries(&client, query).await, expected);

    let mut query = request();
    query.filter.insert("unknown".into(), "value".into());
    let error = pb::audit_client::AuditClient::new(client.channel.clone())
        .query_audit(query)
        .await
        .unwrap_err();
    assert_eq!(error.code(), tonic::Code::InvalidArgument);
    client.shutdown().await;
}
