//! Shared operator wording and inspection projection for both CLI routes.
use remanence_api::pb;
use serde_json::{json, Value};

const E_INSPECT: &str = "inspection only: this build cannot discard this tail; this command changed nothing on the tape";
const E_CONFIRM: &str =
    "SupersedeInspectionOnly: --confirm-supersede cannot discard a torn tail in this build";
const STANDALONE_PUT: &str = "If the drive lost power during a write, `rem tape supersede-tail --acknowledge-tape <label|uuid>`, run with the same drive and state options as this command (such as `--config`, `--device` and `--allow-device`), inspects the tape's suffix at the committed boundary and reports what it finds.";

pub(crate) fn refusal(confirmation_supplied: bool) -> &'static str {
    if confirmation_supplied {
        E_CONFIRM
    } else {
        E_INSPECT
    }
}

/// Keep every wire field, including absent byte measurements as JSON null.
pub(crate) fn inspection_json(inspection: &pb::TornTailInspection) -> Value {
    json!({
        "site": inspection.site,
        "cursor_partition": inspection.cursor_partition,
        "cursor_lba": inspection.cursor_lba,
        "file_start_lba": inspection.file_start_lba,
        "failing_lba": inspection.failing_lba,
        "blocks_read": inspection.blocks_read,
        "read_sense": inspection.read_sense,
        "accounted_bytes": inspection.accounted_bytes,
        "host_bytes": inspection.host_bytes,
        "head": inspection.head,
        "named_files": inspection.named_files,
    })
}

/// Render the projection made above, also carried by the standalone result.
pub(crate) fn render(tape: &str, inspection: &Value) -> String {
    let text = |key: &str| inspection[key].as_str().unwrap_or_default();
    let count = |key: &str| {
        inspection[key]
            .as_u64()
            .map(|n| n.to_string())
            .unwrap_or_else(|| "unknown".into())
    };
    let sense = text("read_sense");
    let sense = if sense.is_empty() { "none" } else { sense };
    let named = inspection["named_files"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect::<Vec<_>>()
        .join(", ");
    let named = if named.is_empty() { "none" } else { &named };
    format!(
        "Tape {tape}: inspection only; this command wrote nothing to the tape and discarded nothing.\n\
         Site: {}.\n\
         Committed boundary: partition {}, block {}.\n\
         Suffix read: started at block {}; READ failed at block {}; blocks read successfully from the start: {}; sense {sense}.\n\
         Failed READ: bytes accounted {}; bytes the host reported transferred {}.\n\
         Head observed: {}.\n\
         Files the tape log names after the boundary (from the log, not read from the tape): {named}.\n\
         Whether a filemark or end-of-data mark follows the boundary was not measured.\n",
        text("site"),
        inspection["cursor_partition"],
        inspection["cursor_lba"],
        inspection["file_start_lba"],
        inspection["failing_lba"],
        inspection["blocks_read"],
        count("accounted_bytes"),
        count("host_bytes"),
        text("head"),
    )
}

fn decorate_put(message: String, sentence: &str) -> String {
    format!("{message}. {sentence}")
}

pub(crate) fn standalone_put_message(message: String) -> String {
    decorate_put(message, STANDALONE_PUT)
}

/// Preserve the daemon error code while decorating only labelled open refusals.
pub(crate) fn daemon_put_error(status: tonic::Status) -> crate::DaemonClientError {
    let marked = remanence_api::status_marks_torn_tail(&status);
    let mut error = crate::status_error(status);
    if marked {
        let sentence = format!(
            "If the drive lost power during a write, `rem tape supersede-tail --tape-uuid <uuid> --ack-tape-uuid <uuid> --endpoint <endpoint>`, with the endpoint this command used (default `{}`), inspects the tape's suffix at the committed boundary and reports what it finds.",
            crate::DEFAULT_DAEMON_ENDPOINT,
        );
        error.message = decorate_put(error.message, &sentence);
    }
    error
}
