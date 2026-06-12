#!/usr/bin/env bash
set -euo pipefail

# Wake Codex as the Layer 3c implementer after Claude writes a new
# journal response or the user writes an explicit directive entry.
# Cron may call this every minute; this wrapper exits cheaply unless
# there is a new wake entry, and a lock prevents overlapping development
# runs.

REPO="${REMANENCE_REPO:-/home/user/remanence}"
CODEX_BIN="${CODEX_BIN:-/home/user/.npm-global/bin/codex}"
PYTHON_BIN="${PYTHON_BIN:-/usr/bin/python3}"
JQ_BIN="${JQ_BIN:-/usr/bin/jq}"
TIMEOUT_BIN="${TIMEOUT_BIN:-/usr/bin/timeout}"
LOCK_FILE="${REMANENCE_LAYER3C_LOCK:-/tmp/remanence-codex-layer3c-implementer.lock}"
STATE_DIR="${REMANENCE_LAYER3C_STATE_DIR:-/home/user/.codex/remanence-layer3c-implementer}"
STATE_FILE="${REMANENCE_LAYER3C_STATE_FILE:-$STATE_DIR/state.json}"
COMPLETE_FILE="${REMANENCE_LAYER3C_COMPLETE_FILE:-$STATE_DIR/layer3c-complete.json}"
LAST_MESSAGE_FILE="${REMANENCE_LAYER3C_LAST_MESSAGE:-$STATE_DIR/last-message.txt}"
START_DATE="${REMANENCE_LAYER3C_START_DATE:-2026-05-22}"
CODEX_TIMEOUT="${REMANENCE_LAYER3C_CODEX_TIMEOUT:-2h}"
JOURNAL_TZ="${REMANENCE_JOURNAL_TZ:-Asia/Kolkata}"

export TZ="$JOURNAL_TZ"

FORCE=0
DRY_RUN=0
for arg in "$@"; do
  case "$arg" in
    --force) FORCE=1 ;;
    --dry-run) DRY_RUN=1 ;;
    *)
      echo "unknown argument: $arg" >&2
      exit 64
      ;;
  esac
done

mkdir -p "$STATE_DIR"

exec 9>"$LOCK_FILE"
if ! flock -n 9; then
  echo "$(date -Iseconds) layer3c implementer already running"
  exit 0
fi

if [[ -f "$COMPLETE_FILE" && "$FORCE" -eq 0 ]]; then
  echo "$(date -Iseconds) layer3c implementer complete sentinel exists: $COMPLETE_FILE"
  exit 0
fi

cd "$REPO"

prompt_file="$(mktemp "$STATE_DIR/prompt.XXXXXX")"
scan_file="$(mktemp "$STATE_DIR/scan.XXXXXX")"
trap 'rm -f "$prompt_file" "$scan_file"' EXIT

set +e
"$PYTHON_BIN" - "$REPO" "$STATE_FILE" "$prompt_file" "$scan_file" "$START_DATE" "$FORCE" "$DRY_RUN" "$JOURNAL_TZ" "$COMPLETE_FILE" <<'PY'
import json
import pathlib
import sys
import textwrap
from datetime import datetime
from zoneinfo import ZoneInfo

repo = pathlib.Path(sys.argv[1])
state_file = pathlib.Path(sys.argv[2])
prompt_file = pathlib.Path(sys.argv[3])
scan_file = pathlib.Path(sys.argv[4])
start_date = sys.argv[5]
force = sys.argv[6] == "1"
dry_run = sys.argv[7] == "1"
journal_tz = sys.argv[8]
complete_file = pathlib.Path(sys.argv[9])
tz = ZoneInfo(journal_tz)
now = datetime.now(tz)
today = now.date().isoformat()

journal_dir = repo / "journal"
if not journal_dir.is_dir():
    raise SystemExit(f"missing journal directory: {journal_dir}")
current_journal_path = journal_dir / f"{today}.json"
current_journal_file = str(current_journal_path.relative_to(repo))
if not current_journal_path.exists():
    current_journal_path.write_text("[]\n", encoding="utf-8")

try:
    state = json.loads(state_file.read_text(encoding="utf-8"))
except (OSError, json.JSONDecodeError):
    state = {}

journal_paths = [
    path
    for path in sorted(journal_dir.glob("20??-??-??.json"))
    if start_date <= path.stem <= today
]

entries = []
for path in journal_paths:
    data = json.loads(path.read_text(encoding="utf-8"))
    if not isinstance(data, list):
        raise SystemExit(f"{path} is not a JSON array")
    for obj in data:
        if isinstance(obj, dict):
            item = dict(obj)
            item["_journal_file"] = str(path.relative_to(repo))
            entries.append(item)

def parse_datetime(value):
    if not value:
        return None
    try:
        parsed = datetime.fromisoformat(str(value))
    except ValueError:
        return None
    if parsed.tzinfo is None:
        return parsed.replace(tzinfo=tz)
    return parsed.astimezone(tz)

def sort_key(obj):
    parsed = parse_datetime(obj.get("datetime"))
    return (
        parsed.timestamp() if parsed else 0,
        str(obj.get("uuid") or ""),
    )

def is_wake_entry(obj):
    author = obj.get("author")
    if author == "claude":
        return True
    if author == "user":
        subject = str(obj.get("subject") or "").lower()
        return subject.startswith(("directive", "request", "priority"))
    return False

wake_entries = [
    obj for obj in entries
    if is_wake_entry(obj)
    and obj.get("uuid")
    and parse_datetime(obj.get("datetime")) is not None
    and parse_datetime(obj.get("datetime")).date().isoformat() <= today
]
wake_entries.sort(key=sort_key)

latest_wake = wake_entries[-1] if wake_entries else None
last_seen_uuid = state.get("last_seen_wake_uuid") or state.get("last_seen_claude_uuid")
new_wake_entries = []
if latest_wake:
    seen = last_seen_uuid is None
    for obj in wake_entries:
        if obj.get("uuid") == last_seen_uuid:
            seen = True
            continue
        if seen:
            new_wake_entries.append(obj)

should_run = force or bool(new_wake_entries)
scan = {
    "now": now.isoformat(timespec="seconds"),
    "today": today,
    "current_journal_file": current_journal_file,
    "latest_wake_uuid": latest_wake.get("uuid") if latest_wake else None,
    "latest_wake_datetime": latest_wake.get("datetime") if latest_wake else None,
    "latest_wake_author": latest_wake.get("author") if latest_wake else None,
    "last_seen_wake_uuid": last_seen_uuid,
    "new_wake_count": len(new_wake_entries),
    "should_run": should_run,
}
scan_file.write_text(json.dumps(scan, indent=2, sort_keys=True) + "\n", encoding="utf-8")

if not should_run:
    raise SystemExit(0)

def compact(obj):
    summary = str(obj.get("summary") or "")
    if len(summary) > 2200:
        summary = summary[:2200] + "\n...[truncated by scheduler prompt]..."
    return {
        "uuid": obj.get("uuid"),
        "datetime": obj.get("datetime"),
        "author": obj.get("author"),
        "subject": obj.get("subject"),
        "idref": obj.get("idref"),
        "files": obj.get("files"),
        "commits": obj.get("commits"),
        "journal_file": obj.get("_journal_file"),
        "summary_excerpt": summary,
    }

new_context = [compact(obj) for obj in new_wake_entries[-8:]]
latest_context = compact(latest_wake) if latest_wake else None

prompt = f"""
You are Codex acting as the IMPLEMENTER for Remanence Layer 3c, not the reviewer.

Latest design context:
- Implement `docs/layer3c-design.md` v0.4.4.
- Treat `docs/remanence-3c-implementation-addendum-v0.2.md` as a
  normative implementation addendum for Layer 3c v0.4.4 and rem-tar
  v0.9.3. Where it tightens or supersedes current implementation details,
  the addendum is the active implementation contract.
- Use `docs/remanence-testing-plan.md` as the cross-layer test gate.
- Also account for `docs/rem-tar-v1-design.md` v0.9.3 and
  `docs/3b-catalog-schema-followup.md` where Layer 3c touches body format,
  filemark map, catalog commit, or crash/restart behavior.
- Important v0.4.4 deltas: `CapacityReserveCause::TapeCapacity` versus
  `ParitySpoolCapacity`; `ResumeAppendResult`; resume-generated sidecars
  commit like ordinary sidecars; append resumes after the last catalog-
  committed tape file; `try_read_bootstrap_at` takes explicit block size;
  catalog-less scan classifies bootstrap/sidecar only after magic plus CRC/
  header validation; all full completion claims must satisfy the testing plan.
- Important addendum v0.2 deltas: replicated sidecar header/index metadata
  with tail copy plus footer locator; sidecar epoch directory and `parity_map`
  tape files; scan reconstruction directory overlay; no volatile sidecar
  deferral in v1; object commit bundles; committed restart state must satisfy
  `T - W < data_ordinals_per_epoch`; bulk/epoch recovery with memory caps;
  hard drive-compression-disabled verification; owned GF(2^8) codec policy;
  one-object-one-tape capacity enforcement; live hardware proof gates.

Journal protocol:
- The current IST journal file for this run is `{current_journal_file}`.
- Before making changes, read `{current_journal_file}` and check whether Claude
  has left review/feedback entries for Codex implementation work. Also use the
  scheduler wake entries below, which may include late entries from prior
  journal files.
- Incorporate substantive Claude feedback before choosing the next step.
- Treat explicit `author: "user"` directive entries as priority guidance,
  after resolving any open [High] or [Critical] Claude review findings.
- You MUST first resolve any open [High] or [Critical] Claude review findings
  before picking a new bounded Layer 3c implementation step.
- After the implementation run and tests, check `{current_journal_file}` again.
- Append an `author: "chatgpt"` JSON object to `{current_journal_file}` for
  your run. Use a UUID, IST datetime, a clear `subject`, `summary`,
  `files_modified`, and `verification`. If responding to a Claude entry, set
  `idref` to that Claude UUID.
- Do not write noisy acknowledgement-only entries.

Development task:
- Inspect the current repository state; other agents/users may have dirty
  changes. Do not revert work you did not make.
- Pick one bounded next implementation step that moves Layer 3c toward the
  v0.4.4 design, the addendum v0.2 contract, and the testing-plan gates. If
  no open [High] or [Critical] Claude finding exists, prefer fixing other
  substantive Claude feedback when present; otherwise continue from the next
  missing/weak addendum implementation or test gate.
- Do not continue hardening old multi-epoch committed `W < T` resume behavior
  as a production v1 path. Under addendum v0.2, committed v1 state must have
  `T - W < data_ordinals_per_epoch`; larger rebuilds are legacy/forensic or
  catalog-corruption handling unless explicitly scoped as such.
- Do not resurrect the removed legacy `ParitySource` API. Interpret the
  addendum's bulk/region recovery API through the current `ObjectParitySource`
  and recovery-module architecture unless a deliberate API update is required.
- Preferred addendum implementation order, when not blocked by reviews:
  (1) sidecar wire format replication: primary header/index, tail copy,
  footer locator, and codec tests; (2) sidecar epoch directory and `parity_map`
  codec/bootstrap reference; (3) scan reconstruction overlay; (4) object
  commit bundles, no volatile deferral, and bounded restart invariant;
  (5) bulk/epoch recovery planner and memory cap; (6) compression verification,
  object-too-large preflight, GF codec conformance, and hardware gates.
- Make the code/docs/tests change, run the relevant tests, and keep going until
  that bounded step is genuinely complete.
- Validate any SCSI sense-code or tape command behavior against
  `docs/LTO SCSI Reference GA32-0928-08 (EXTERNAL).pdf`.
- Do not mark Layer 3c complete until the whole v0.4.4 surface and end-to-end
  test plan gates are complete. When, and only when, that is true, write a
  completion JSON object to `{complete_file}` explaining the evidence.

New scheduler wake entries since the last scheduler run
(Claude reviews/feedback and explicit user directives):
{json.dumps(new_context, indent=2)}

Latest scheduler wake entry:
{json.dumps(latest_context, indent=2)}
"""

prompt_file.write_text(textwrap.dedent(prompt).strip() + "\n", encoding="utf-8")
raise SystemExit(2)
PY
scan_status=$?
set -e

if [[ "$scan_status" -eq 0 ]]; then
  "$JQ_BIN" empty "$REPO"/journal/20??-??-??.json
  exit 0
fi

if [[ "$scan_status" -ne 2 ]]; then
  echo "$(date -Iseconds) layer3c implementer scan failed with status $scan_status"
  exit "$scan_status"
fi

if [[ "$DRY_RUN" -eq 1 ]]; then
  echo "$(date -Iseconds) dry run: would start Codex with prompt $prompt_file"
  sed -n '1,260p' "$prompt_file"
  exit 0
fi

echo "$(date -Iseconds) starting Codex Layer 3c implementer"
set +e
"$TIMEOUT_BIN" --kill-after=60s "$CODEX_TIMEOUT" "$CODEX_BIN" exec \
  -C "$REPO" \
  -s danger-full-access \
  -c 'approval_policy="never"' \
  --color never \
  --output-last-message "$LAST_MESSAGE_FILE" \
  - < "$prompt_file"
codex_status=$?
set -e

"$JQ_BIN" empty "$REPO"/journal/20??-??-??.json

if [[ "$codex_status" -ne 0 ]]; then
  echo "$(date -Iseconds) Codex Layer 3c implementer exited with status $codex_status"
  exit "$codex_status"
fi

"$PYTHON_BIN" - "$STATE_FILE" "$scan_file" "$JOURNAL_TZ" <<'PY'
import json
import pathlib
import sys
from datetime import datetime
from zoneinfo import ZoneInfo

state_file = pathlib.Path(sys.argv[1])
scan_file = pathlib.Path(sys.argv[2])
tz = ZoneInfo(sys.argv[3])
scan = json.loads(scan_file.read_text(encoding="utf-8"))
state = {
    "updated_at": datetime.now(tz).isoformat(timespec="seconds"),
    "last_seen_wake_uuid": scan.get("latest_wake_uuid"),
    "last_seen_wake_datetime": scan.get("latest_wake_datetime"),
    "last_seen_wake_author": scan.get("latest_wake_author"),
    # Keep these legacy keys for compatibility with older prompts/log readers.
    "last_seen_claude_uuid": scan.get("latest_wake_uuid"),
    "last_seen_claude_datetime": scan.get("latest_wake_datetime"),
    "last_run_reason": "new-journal-wake-entry",
}
state_file.parent.mkdir(parents=True, exist_ok=True)
tmp = state_file.with_suffix(state_file.suffix + ".tmp")
tmp.write_text(json.dumps(state, indent=2, sort_keys=True) + "\n", encoding="utf-8")
tmp.replace(state_file)
PY

echo "$(date -Iseconds) Codex Layer 3c implementer complete"
