//! Shared operator wording and inspection projection for both CLI routes.
use remanence_api::pb;
use serde_json::{json, Value};

const E_INSPECT: &str = "inspection only: this build reports a torn tail and discards nothing";
const E_CONFIRM: &str =
    "SupersedeInspectionOnly: --confirm-supersede cannot discard a torn tail in this build";
const STANDALONE_PUT: &str = "If the drive lost power during a write, `rem tape supersede-tail --acknowledge-tape <label|uuid>` inspects the tape's end and reports what it finds.";
const DAEMON_PUT: &str = "If the drive lost power during a write, `rem tape supersede-tail --tape-uuid <uuid> --ack-tape-uuid <uuid>` inspects the tape's end and reports what it finds.";

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
        "Tape {tape}: inspection only; nothing was written or discarded.\n\
         Site: {}.\n\
         Committed boundary: partition {}, block {}.\n\
         Torn file: starts at block {}; READ failed at block {} after {} blocks; sense {sense}.\n\
         Bytes accounted: {}; bytes delivered to the host: {}.\n\
         Head of the torn file: {}.\n\
         Files the log names after the boundary: {named}.\n\
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
        error.message = decorate_put(error.message, DAEMON_PUT);
    }
    error
}
