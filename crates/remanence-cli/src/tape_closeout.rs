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
}
