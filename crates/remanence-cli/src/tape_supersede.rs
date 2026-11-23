//! Explicit daemon-backed tail recovery with a read-only physical preview.
use clap::Args;
use remanence_api::pb;
use std::io::{BufRead, IsTerminal, Write};
use std::process::ExitCode;

#[derive(Args, Debug)]
pub(crate) struct TapeSupersedeArgs {
    /// UUID of the tape whose uncommitted tail will be discarded.
    #[arg(long, alias = "tape")]
    tape_uuid: String,
    /// Repeat the tape UUID to acknowledge the recovery target.
    #[arg(long, alias = "acknowledge-tape")]
    ack_tape_uuid: String,
    /// Supply the exact confirmation phrase returned by the recovery preview.
    #[arg(long)]
    confirm_supersede: Option<String>,
    /// Daemon gRPC endpoint.
    #[arg(long, default_value = super::DEFAULT_DAEMON_ENDPOINT)]
    endpoint: String,
}

/// Read typed recovery confirmation through the reader held by CLI dispatch.
pub(crate) fn run(
    args: &TapeSupersedeArgs,
    input: &mut dyn BufRead,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> ExitCode {
    let connection = (|| -> Result<_, String> {
        let tape = uuid::Uuid::parse_str(&args.tape_uuid).map_err(|e| e.to_string())?;
        let ack = uuid::Uuid::parse_str(&args.ack_tape_uuid).map_err(|e| e.to_string())?;
        if tape != ack {
            return Err("--ack-tape-uuid must match --tape-uuid".into());
        }
        let runtime = super::daemon_runtime().map_err(|e| e.message)?;
        let channel = runtime.block_on(super::connect_daemon(&args.endpoint))?;
        Ok((
            tape,
            runtime,
            pb::catalog_client::CatalogClient::new(channel),
        ))
    })();
    match connection {
        Ok((tape, runtime, client)) => run_connected(args, tape, &runtime, client, input, out, err),
        Err(error) => {
            let _ = writeln!(err, "error: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Run the preview and confirmation exchange on an already connected catalog.
fn run_connected(
    args: &TapeSupersedeArgs,
    tape: uuid::Uuid,
    runtime: &tokio::runtime::Runtime,
    mut client: pb::catalog_client::CatalogClient<tonic::transport::Channel>,
    input: &mut dyn BufRead,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> ExitCode {
    let result = (|| -> Result<(), String> {
        let mut request = pb::SupersedeTailRequest {
            observed_tape_log_sequence: None,
            tape_uuid: tape.as_bytes().to_vec(),
            ack_tape_uuid: tape.as_bytes().to_vec(),
            confirm_supersede: None,
            expected_found: Vec::new(),
        };
        let preview = runtime
            .block_on(client.supersede_tail(request.clone()))
            .map_err(|e| e.to_string())?
            .into_inner();
        if let Some(inspection) = &preview.inspection {
            write!(
                out,
                "{}",
                crate::torn_tail_inspection::render(
                    &tape.to_string(),
                    &crate::torn_tail_inspection::inspection_json(inspection)
                )
            )
            .map_err(|e| e.to_string())?;
            return Err(
                crate::torn_tail_inspection::refusal(args.confirm_supersede.is_some()).into(),
            );
        }
        writeln!(err, "Tail to discard: {}", preview.found.join(", "))
            .map_err(|e| e.to_string())?;
        let confirmation = match &args.confirm_supersede {
            Some(value) => value.clone(),
            None => {
                if !std::io::stdin().is_terminal() {
                    return Err(format!(
                        "non-interactive supersede requires --confirm-supersede {:?}",
                        preview.required_confirmation
                    ));
                }
                write!(
                    err,
                    "Type {:?} to discard this tail: ",
                    preview.required_confirmation
                )
                .map_err(|e| e.to_string())?;
                err.flush().map_err(|e| e.to_string())?;
                let mut line = String::new();
                input.read_line(&mut line).map_err(|e| e.to_string())?;
                line.trim_end_matches(['\r', '\n']).to_owned()
            }
        };
        if confirmation != preview.required_confirmation {
            return Err("supersede confirmation did not match".into());
        }
        request.confirm_supersede = Some(confirmation);
        request.observed_tape_log_sequence = Some(preview.tape_log_sequence);
        request.expected_found = preview.found;
        let completed = runtime
            .block_on(client.supersede_tail(request))
            .map_err(|e| e.to_string())?
            .into_inner();
        if !completed.completed {
            return Err("daemon did not complete tail recovery".into());
        }
        writeln!(out, "Tape {tape} tail superseded; append is available.")
            .map_err(|e| e.to_string())
    })();
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            let _ = writeln!(err, "error: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn library_supersede_routes_to_daemon_and_requires_exact_uuid_acknowledgment() {
        let uuid = "12345678-1234-4234-8234-123456789abc";
        let words = [
            "rem",
            "tape",
            "supersede-tail",
            "--tape-uuid",
            uuid,
            "--ack-tape-uuid",
            uuid,
            "--confirm-supersede",
            "SUPERSEDE LABEL",
        ];
        assert!(!crate::standalone::selected_without_home_config(
            &words
                .iter()
                .map(std::ffi::OsString::from)
                .collect::<Vec<_>>()
        ));
        let parsed = crate::Cli::try_parse_from(words).unwrap();
        assert!(matches!(
            parsed.command,
            crate::RemCommand::Tape {
                command: crate::RemTapeCommand::SupersedeTail(_)
            }
        ));
        let mut out = Vec::new();
        let mut err = Vec::new();
        let result = run(
            &TapeSupersedeArgs {
                tape_uuid: uuid.into(),
                ack_tape_uuid: uuid::Uuid::nil().to_string(),
                confirm_supersede: None,
                endpoint: "invalid://must-not-connect".into(),
            },
            &mut std::io::empty(),
            &mut out,
            &mut err,
        );
        assert_eq!(result, ExitCode::FAILURE);
        assert!(String::from_utf8(err).unwrap().contains("must match"));
    }

    // In-memory Catalog service: inspect actual requests without socket access.
    struct TestCatalog {
        requests: std::sync::Arc<std::sync::Mutex<Vec<pb::SupersedeTailRequest>>>,
        preview: pb::SupersedeTailResponse,
    }

    #[tonic::async_trait]
    impl pb::catalog_server::Catalog for TestCatalog {
        async fn supersede_tail(
            &self,
            request: tonic::Request<pb::SupersedeTailRequest>,
        ) -> Result<tonic::Response<pb::SupersedeTailResponse>, tonic::Status> {
            let mut requests = self.requests.lock().unwrap();
            requests.push(request.into_inner());
            let mut reply = self.preview.clone();
            reply.completed = requests.len() == 2;
            Ok(tonic::Response::new(reply))
        }
        async fn get_object(
            &self,
            _request: Request<pb::GetObjectRequest>,
        ) -> Result<Response<pb::ObjectRecord>, Status> {
            Err(Status::unimplemented("unused"))
        }
        async fn list_files_in_object(
            &self,
            _request: Request<pb::ListFilesInObjectRequest>,
        ) -> Result<Response<pb::ListFilesInObjectResponse>, Status> {
            Err(Status::unimplemented("unused"))
        }
        async fn list_tapes(
            &self,
            _request: Request<pb::ListTapesRequest>,
        ) -> Result<Response<pb::ListTapesResponse>, Status> {
            Err(Status::unimplemented("unused"))
        }
        async fn get_tape(
            &self,
            _request: Request<pb::GetTapeRequest>,
        ) -> Result<Response<pb::Tape>, Status> {
            Err(Status::unimplemented("unused"))
        }
        async fn get_tape_inventory(
            &self,
            _request: Request<pb::TapeInventoryRequest>,
        ) -> Result<Response<Self::GetTapeInventoryStream>, Status> {
            Err(Status::unimplemented("unused"))
        }
        async fn start_tape_verification(
            &self,
            _: Request<pb::StartTapeVerificationRequest>,
        ) -> Result<Response<pb::OperationRef>, Status> {
            Err(Status::unimplemented("unused"))
        }
        async fn get_tape_verification(
            &self,
            _: Request<pb::GetOperationRequest>,
        ) -> Result<Response<pb::TapeIndexVerification>, Status> {
            Err(Status::unimplemented("unused"))
        }
        async fn verify_tape_index(
            &self,
            _request: Request<pb::VerifyTapeIndexRequest>,
        ) -> Result<Response<pb::TapeIndexVerification>, Status> {
            Err(Status::unimplemented("unused"))
        }
        async fn list_tape_files(
            &self,
            _request: Request<pb::ListTapeFilesRequest>,
        ) -> Result<Response<pb::ListTapeFilesResponse>, Status> {
            Err(Status::unimplemented("unused"))
        }
        async fn list_tape_pools(
            &self,
            _request: Request<pb::ListTapePoolsRequest>,
        ) -> Result<Response<pb::ListTapePoolsResponse>, Status> {
            Err(Status::unimplemented("unused"))
        }
        async fn get_tape_pool(
            &self,
            _request: Request<pb::GetTapePoolRequest>,
        ) -> Result<Response<pb::TapePool>, Status> {
            Err(Status::unimplemented("unused"))
        }
        async fn enumerate_objects(
            &self,
            _request: Request<pb::EnumerateObjectsRequest>,
        ) -> Result<Response<Self::EnumerateObjectsStream>, Status> {
            Err(Status::unimplemented("unused"))
        }
        async fn find_object_copies(
            &self,
            _request: Request<pb::FindObjectCopiesRequest>,
        ) -> Result<Response<pb::FindObjectCopiesResponse>, Status> {
            Err(Status::unimplemented("unused"))
        }
        async fn reconcile_tape(
            &self,
            _request: Request<pb::ReconcileTapeRequest>,
        ) -> Result<Response<pb::OperationRef>, Status> {
            Err(Status::unimplemented("unused"))
        }
        async fn closeout_tape(
            &self,
            _request: Request<pb::CloseoutTapeRequest>,
        ) -> Result<Response<pb::OperationRef>, Status> {
            Err(Status::unimplemented("unused"))
        }
        async fn finalize_tape(
            &self,
            _request: Request<pb::FinalizeTapeRequest>,
        ) -> Result<Response<pb::TapeFinalization>, Status> {
            Err(Status::unimplemented("unused"))
        }
        async fn get_tape_finalization(
            &self,
            _request: Request<pb::GetTapeFinalizationRequest>,
        ) -> Result<Response<pb::TapeFinalization>, Status> {
            Err(Status::unimplemented("unused"))
        }
        async fn get_file(
            &self,
            _request: Request<pb::GetFileRequest>,
        ) -> Result<Response<pb::FileRecord>, Status> {
            Err(Status::unimplemented("unused"))
        }
        async fn enumerate_units(
            &self,
            _request: Request<pb::EnumerateUnitsRequest>,
        ) -> Result<Response<Self::EnumerateUnitsStream>, Status> {
            Err(Status::unimplemented("unused"))
        }
        async fn get_catalog_unit(
            &self,
            _request: Request<pb::GetCatalogUnitRequest>,
        ) -> Result<Response<pb::CatalogUnit>, Status> {
            Err(Status::unimplemented("unused"))
        }
        async fn list_entries_in_unit(
            &self,
            _request: Request<pb::ListEntriesInUnitRequest>,
        ) -> Result<Response<pb::ListEntriesInUnitResponse>, Status> {
            Err(Status::unimplemented("unused"))
        }
        type GetTapeInventoryStream =
            tokio_stream::Iter<std::vec::IntoIter<Result<pb::TapeInventoryStreamItem, Status>>>;
        type EnumerateObjectsStream =
            tokio_stream::Iter<std::vec::IntoIter<Result<pb::ObjectRecord, Status>>>;
        type EnumerateUnitsStream =
            tokio_stream::Iter<std::vec::IntoIter<Result<pb::CatalogUnit, Status>>>;
    }

    use tonic::{Request, Response, Status};

    fn inspection_preview() -> pb::SupersedeTailResponse {
        pb::SupersedeTailResponse {
            tape_log_sequence: 41,
            inspection: Some(pb::TornTailInspection {
                site: "open-scan".into(),
                cursor_partition: 2,
                cursor_lba: 100,
                file_start_lba: 100,
                failing_lba: 102,
                blocks_read: 2,
                read_sense: "03/14/00".into(),
                accounted_bytes: Some(524288),
                host_bytes: None,
                head: "object header".into(),
                named_files: vec![],
            }),
            ..Default::default()
        }
    }

    fn catalog_exchange(
        preview: pb::SupersedeTailResponse,
        confirmation: Option<&str>,
    ) -> (ExitCode, String, String, Vec<pb::SupersedeTailRequest>) {
        let runtime = crate::daemon_runtime().unwrap();
        let requests = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let (channel, task) = runtime.block_on(async {
            let (client, server) = tokio::io::duplex(65536);
            let task = tokio::spawn(
                tonic::transport::Server::builder()
                    .add_service(pb::catalog_server::CatalogServer::new(TestCatalog {
                        requests: requests.clone(),
                        preview,
                    }))
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
            (channel, task)
        });
        let tape = uuid::Uuid::from_bytes([7; 16]);
        let args = TapeSupersedeArgs {
            tape_uuid: tape.to_string(),
            ack_tape_uuid: tape.to_string(),
            confirm_supersede: confirmation.map(str::to_owned),
            endpoint: "unused".into(),
        };
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = run_connected(
            &args,
            tape,
            &runtime,
            pb::catalog_client::CatalogClient::new(channel),
            &mut std::io::empty(),
            &mut out,
            &mut err,
        );
        task.abort();
        let requests = requests.lock().unwrap().clone();
        (
            code,
            String::from_utf8(out).unwrap(),
            String::from_utf8(err).unwrap(),
            requests,
        )
    }

    #[test]
    fn plr2a2b_d1_inspection_without_confirmation() {
        let (code, out, err, requests) = catalog_exchange(inspection_preview(), None);
        assert_eq!(code, ExitCode::FAILURE);
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].confirm_supersede, None);
        assert_eq!(requests[0].observed_tape_log_sequence, None);
        assert!(requests[0].expected_found.is_empty());
        assert_eq!(requests[0].tape_uuid, vec![7; 16]);
        assert_eq!(requests[0].ack_tape_uuid, vec![7; 16]);
        assert_eq!(out, "Tape 07070707-0707-0707-0707-070707070707: inspection only; this command wrote nothing to the tape and discarded nothing.\n\
Site: open-scan.\n\
Committed boundary: partition 2, block 100.\n\
Suffix read: started at block 100; READ failed at block 102; blocks read successfully from the start: 2; sense 03/14/00.\n\
Failed READ: bytes accounted 524288; bytes the host reported transferred unknown.\n\
Head observed: object header.\n\
Files the tape log names after the boundary (from the log, not read from the tape): none.\n\
Whether a filemark or end-of-data mark follows the boundary was not measured.\n");
        assert_eq!(
            err,
            "error: inspection only: this build cannot discard this tail; this command changed nothing on the tape\n"
        );
        assert!(!out.contains("Tail to discard") && !err.contains("Tail to discard"));
    }

    #[test]
    fn plr2a2b_d2_inspection_refuses_empty_confirmation() {
        let mut preview = inspection_preview();
        let inspection = preview.inspection.as_mut().unwrap();
        inspection.read_sense.clear();
        inspection.accounted_bytes = None;
        inspection.host_bytes = Some(0);
        inspection.named_files = vec!["first".into(), "second".into()];
        let (code, out, err, requests) = catalog_exchange(preview, Some(""));
        assert_eq!(code, ExitCode::FAILURE);
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].confirm_supersede, None);
        assert_eq!(err, "error: SupersedeInspectionOnly: --confirm-supersede cannot discard a torn tail in this build\n");
        assert_eq!(out, "Tape 07070707-0707-0707-0707-070707070707: inspection only; this command wrote nothing to the tape and discarded nothing.\n\
Site: open-scan.\n\
Committed boundary: partition 2, block 100.\n\
Suffix read: started at block 100; READ failed at block 102; blocks read successfully from the start: 2; sense none.\n\
Failed READ: bytes accounted unknown; bytes the host reported transferred 0.\n\
Head observed: object header.\n\
Files the tape log names after the boundary (from the log, not read from the tape): first, second.\n\
Whether a filemark or end-of-data mark follows the boundary was not measured.\n");
        assert!(!out.contains("Tail to discard") && !err.contains("Tail to discard"));
    }

    #[test]
    fn plr2a2b_d3_ordinary_tail_still_confirms() {
        let (code, out, err, requests) = catalog_exchange(
            pb::SupersedeTailResponse {
                found: vec!["object-id".into()],
                required_confirmation: "SUPERSEDE LABEL".into(),
                tape_log_sequence: 42,
                ..Default::default()
            },
            Some("SUPERSEDE LABEL"),
        );
        assert_eq!(code, ExitCode::SUCCESS);
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].confirm_supersede, None);
        assert_eq!(
            requests[1].confirm_supersede.as_deref(),
            Some("SUPERSEDE LABEL")
        );
        assert_eq!(requests[1].observed_tape_log_sequence, Some(42));
        assert_eq!(requests[1].expected_found, vec!["object-id"]);
        assert_eq!(
            out,
            "Tape 07070707-0707-0707-0707-070707070707 tail superseded; append is available.\n"
        );
        assert_eq!(err, "Tail to discard: object-id\n");
    }
}
