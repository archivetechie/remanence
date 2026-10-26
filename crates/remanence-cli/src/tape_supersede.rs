//! Explicit daemon-backed tail recovery with a read-only physical preview.
use clap::Args;
use remanence_api::pb;
use std::io::{BufRead, IsTerminal, Write};
use std::process::ExitCode;

#[derive(Args, Debug)]
pub(crate) struct TapeSupersedeArgs {
    #[arg(long, alias = "tape")]
    tape_uuid: String,
    #[arg(long, alias = "acknowledge-tape")]
    ack_tape_uuid: String,
    #[arg(long)]
    confirm_supersede: Option<String>,
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
    let result = (|| -> Result<(), String> {
        let tape = uuid::Uuid::parse_str(&args.tape_uuid).map_err(|e| e.to_string())?;
        let ack = uuid::Uuid::parse_str(&args.ack_tape_uuid).map_err(|e| e.to_string())?;
        if tape != ack {
            return Err("--ack-tape-uuid must match --tape-uuid".into());
        }
        let runtime = super::daemon_runtime().map_err(|e| e.message)?;
        let channel = runtime.block_on(super::connect_daemon(&args.endpoint))?;
        let mut client = pb::catalog_client::CatalogClient::new(channel);
        let mut request = pb::SupersedeTailRequest {
            tape_uuid: tape.as_bytes().to_vec(),
            ack_tape_uuid: ack.as_bytes().to_vec(),
            confirm_supersede: None,
            expected_found: Vec::new(),
        };
        let preview = runtime
            .block_on(client.supersede_tail(request.clone()))
            .map_err(|e| e.to_string())?
            .into_inner();
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
        assert!(!crate::standalone::selected(
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
}
