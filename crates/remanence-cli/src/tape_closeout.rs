//! `rem tape closeout` resolves one exact tape and watches its idempotent
//! maintenance operation through completion using the ordinary Operation schema.
use clap::Args;
use remanence_api::pb;
use std::{io::Write, process::ExitCode};
use uuid::Uuid;

#[derive(Args, Debug)]
pub(crate) struct TapeCloseoutArgs {
    /// Tape UUID or unambiguous barcode.
    pub(crate) tape: String,
    #[arg(long)]
    pub(crate) expected_pool: Option<String>,
    #[arg(long)]
    pub(crate) idempotency_key: Option<Uuid>,
    #[arg(long, default_value = super::DEFAULT_DAEMON_ENDPOINT)]
    pub(crate) endpoint: String,
    #[arg(long)]
    pub(crate) json: bool,
}

pub(crate) fn run(args: &TapeCloseoutArgs, out: &mut dyn Write, err: &mut dyn Write) -> ExitCode {
    let result = (|| -> Result<(), super::DaemonClientError> {
        let key = args.idempotency_key.unwrap_or_else(Uuid::new_v4);
        if key.is_nil() {
            return Err(super::DaemonClientError::client(
                "--idempotency-key must not be the nil UUID",
            ));
        }
        // Publish and flush before even connecting: a lost response can be retried.
        if args.idempotency_key.is_none() {
            if args.json {
                writeln!(
                    err,
                    "{}",
                    serde_json::json!({"idempotency_key": key.to_string()})
                )
                .map_err(|e| super::DaemonClientError::client(e.to_string()))?;
            } else {
                writeln!(err, "closeout idempotency key: {key}")
                    .map_err(|e| super::DaemonClientError::client(e.to_string()))?;
            }
            err.flush()
                .map_err(|e| super::DaemonClientError::client(e.to_string()))?;
        }
        let runtime = super::daemon_runtime()?;
        runtime.block_on(async {
            let channel = super::connect_daemon(&args.endpoint)
                .await
                .map_err(super::DaemonClientError::from)?;
            super::observe_daemon(&channel, args.json).await?;
            let mut client = pb::catalog_client::CatalogClient::new(channel.clone());
            let uuid = if let Ok(uuid) = Uuid::parse_str(&args.tape) {
                uuid.as_bytes().to_vec()
            } else {
                let mut page_token = None;
                let mut found = None;
                loop {
                    let page = client
                        .list_tapes(pb::ListTapesRequest {
                            library_uuid: vec![],
                            page_token,
                            page_size: 0,
                            pool_id: String::new(),
                            kind: "data".into(),
                        })
                        .await
                        .map_err(super::status_error)?
                        .into_inner();
                    for tape in page.tapes {
                        if tape.voltag.as_deref() == Some(&args.tape)
                            && found.replace(tape.tape_uuid).is_some()
                        {
                            return Err(super::DaemonClientError::client(
                                "ambiguous tape barcode; use its UUID",
                            ));
                        }
                    }
                    page_token = page.next_page_token;
                    if page_token.is_none() {
                        break;
                    }
                }
                found.ok_or_else(|| super::DaemonClientError::client("tape barcode not found"))?
            };
            let operation = client
                .closeout_tape(pb::CloseoutTapeRequest {
                    tape_uuid: uuid,
                    expected_pool_id: args.expected_pool.clone(),
                    idempotency_key: Some(pb::IdempotencyKey {
                        value: key.as_bytes().to_vec(),
                    }),
                })
                .await
                .map_err(super::status_error)?
                .into_inner();
            let status = super::wait_for_operation(channel, operation.operation_id).await?;
            super::print_operation(status, args.json, out).map_err(super::DaemonClientError::from)
        })
    })();
    super::finish_daemon_client_result(result, args.json, err)
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn closeout_routes_all_flags_and_rejects_nil_key_before_connection() {
        let cli = crate::Cli::try_parse_from([
            "rem",
            "tape",
            "closeout",
            "CLOSE01L9",
            "--expected-pool",
            "archive",
            "--idempotency-key",
            "00000000-0000-0000-0000-000000000000",
            "--endpoint",
            "http://127.0.0.1:1",
            "--json",
        ])
        .unwrap();
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = crate::run(
            cli,
            || panic!("closeout must use the daemon"),
            &mut out,
            &mut err,
        );
        assert_ne!(code, ExitCode::SUCCESS);
        assert!(out.is_empty());
        assert!(String::from_utf8(err)
            .unwrap()
            .contains("--idempotency-key must not be the nil UUID"));
        assert!(crate::Cli::try_parse_from(["rem", "tape", "closeout"]).is_err());
        assert!(
            crate::Cli::try_parse_from(["rem", "tape", "closeout", "CLOSE01L9", "--force"])
                .is_err()
        );
    }
    /// A failed connection still leaves the generated retry key available first.
    #[test]
    fn b9_generated_key_precedes_connection_failure() {
        for json in [true, false] {
            let args = TapeCloseoutArgs {
                tape: Uuid::new_v4().to_string(),
                expected_pool: None,
                idempotency_key: None,
                endpoint: "not a URI".into(),
                json,
            };
            let (mut out, mut err) = (Vec::new(), Vec::new());
            assert_eq!(run(&args, &mut out, &mut err), ExitCode::from(1));
            assert!(out.is_empty());
            let text = String::from_utf8(err).unwrap();
            let lines = text.lines().collect::<Vec<_>>();
            assert_eq!(lines.len(), 2);
            if json {
                let key: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
                assert!(!Uuid::parse_str(key["idempotency_key"].as_str().unwrap())
                    .unwrap()
                    .is_nil());
                let error: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
                assert_eq!(error["code"], "daemon_client_error");
                assert!(error["details"].get("state").is_none());
            } else {
                assert!(lines[0].starts_with("closeout idempotency key: "));
            }
        }
    }

    /// All unsuccessful terminal states preserve identity in rem.error.v1 on stderr.
    #[test]
    fn b9_terminal_json_errors_keep_operation_identity() {
        for state in [
            pb::OperationState::Failed,
            pb::OperationState::Cancelled,
            pb::OperationState::CompletionUnknown,
        ] {
            let id = Uuid::new_v4();
            let result = crate::operation_result(pb::OperationStatus {
                operation_id: id.as_bytes().to_vec(),
                state: state as i32,
                error_summary: "injected failure".into(),
                ..Default::default()
            });
            let mut err = Vec::new();
            assert_eq!(
                crate::finish_daemon_client_result(result.map(|_| ()), true, &mut err),
                ExitCode::from(1)
            );
            let json: serde_json::Value = serde_json::from_slice(&err).unwrap();
            assert_eq!(json["schema"], "rem.error.v1");
            assert_eq!(json["code"], "operation_failed");
            assert_eq!(json["details"]["operation_id"], id.to_string());
            assert_eq!(
                json["details"]["state"],
                crate::operation_state_name(state as i32)
            );
        }
    }
    /// The user-facing reference and wire overview name the implemented retry contract.
    #[test]
    fn b9_closeout_reference_and_proto_describe_implemented_contract() {
        let reference = include_str!("../../../docs/reference-cli.md");
        let section = reference
            .split("### rem tape closeout")
            .nth(1)
            .expect("closeout command section")
            .split("### ")
            .next()
            .unwrap();
        for required in [
            "barcode",
            "disambiguate",
            "--expected-pool",
            "--idempotency-key",
            "before",
            "stderr",
            "details.operation_id",
            "details.state",
            "exit 1",
            "exit 2",
        ] {
            assert!(
                section.contains(required),
                "missing closeout contract: {required}"
            );
        }
        let proto = include_str!("../../../proto/layer5.proto");
        let overview = proto.split("syntax =").next().unwrap();
        assert!(overview.contains("Catalog.CloseoutTape and Catalog.FinalizeTape implement"));
    }
}
