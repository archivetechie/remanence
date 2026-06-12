#!/usr/bin/env bash
set -euo pipefail

# Poll the Remanence JSON journal for Claude entries that have not yet
# received a chatgpt review entry.
#
# Intended scheduler setup: cron wakes this script every 30 seconds. The script
# records a next-due timestamp in STATE_DIR and exits cheaply when no journal
# file changed and Claude has gone quiet. If a journal file mtime changes, it
# scans immediately regardless of the backoff timer.
#
# Journal-only acknowledgements of prior "no issue found" reviews are noise:
# Claude should not write them, and if they appear anyway this wrapper ignores
# them. Substantive responses to findings still go through review.

REPO="${REMANENCE_REPO:-/home/user/remanence}"
CODEX_BIN="${CODEX_BIN:-/home/user/.npm-global/bin/codex}"
PYTHON_BIN="${PYTHON_BIN:-/usr/bin/python3}"
JQ_BIN="${JQ_BIN:-/usr/bin/jq}"
TIMEOUT_BIN="${TIMEOUT_BIN:-/usr/bin/timeout}"
LOCK_FILE="${REMANENCE_CODEX_LOCK:-/tmp/remanence-codex-journal-review.lock}"
STATE_DIR="${REMANENCE_CODEX_STATE_DIR:-/home/user/.codex/remanence-journal-review}"
ADAPTIVE_STATE_FILE="${REMANENCE_CODEX_ADAPTIVE_STATE:-$STATE_DIR/adaptive-state.json}"
REVIEW_START_DATE="${REMANENCE_REVIEW_START_DATE:-2026-05-18}"
MAX_ENTRIES="${REMANENCE_REVIEW_MAX_ENTRIES:-6}"
MAX_PASSES="${REMANENCE_REVIEW_MAX_PASSES:-3}"
CODEX_TIMEOUT="${REMANENCE_CODEX_TIMEOUT:-12m}"
JOURNAL_TZ="${REMANENCE_JOURNAL_TZ:-Asia/Kolkata}"

export TZ="$JOURNAL_TZ"

DRY_RUN=0
FORCE=0
for arg in "$@"; do
  case "$arg" in
    --dry-run) DRY_RUN=1 ;;
    --force) FORCE=1 ;;
    *)
      echo "unknown argument: $arg" >&2
      exit 64
      ;;
  esac
done

mkdir -p "$STATE_DIR"

exec 9>"$LOCK_FILE"
if ! flock -n 9; then
  echo "$(date -Iseconds) another codex journal review is already running"
  exit 0
fi

cd "$REPO"

prompt_file="$(mktemp "$STATE_DIR/prompt.XXXXXX")"
last_message_file="$STATE_DIR/last-message.txt"
trap 'rm -f "$prompt_file"' EXIT

for ((pass = 1; pass <= MAX_PASSES; pass++)); do
set +e
"$PYTHON_BIN" - "$REPO" "$prompt_file" "$REVIEW_START_DATE" "$MAX_ENTRIES" "$DRY_RUN" "$JOURNAL_TZ" "$ADAPTIVE_STATE_FILE" "$FORCE" <<'PY'
import json
import pathlib
import sys
import textwrap
from datetime import datetime
from zoneinfo import ZoneInfo

repo = pathlib.Path(sys.argv[1])
prompt_file = pathlib.Path(sys.argv[2])
review_start_date = sys.argv[3]
max_entries = int(sys.argv[4])
dry_run = sys.argv[5] == "1"
journal_tz = sys.argv[6]
state_file = pathlib.Path(sys.argv[7])
force = sys.argv[8] == "1"
tz = ZoneInfo(journal_tz)
now = datetime.now(tz)
now_epoch = int(now.timestamp())
today = now.date().isoformat()

journal_dir = repo / "journal"
if not journal_dir.is_dir():
    raise SystemExit(f"missing journal directory: {journal_dir}")

journal_paths = [
    path
    for path in sorted(journal_dir.glob("20??-??-??.json"))
    if review_start_date <= path.stem <= today
]
current_mtime_ns = max((path.stat().st_mtime_ns for path in journal_paths), default=0)

if not dry_run and not force and state_file.exists():
    try:
        state = json.loads(state_file.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError):
        state = {}
    if (
        state.get("journal_mtime_ns") == current_mtime_ns
        and now_epoch < int(state.get("next_due_epoch", 0))
    ):
        raise SystemExit(0)

entries = []
files = {}
for path in journal_paths:
    data = json.loads(path.read_text(encoding="utf-8"))
    if not isinstance(data, list):
        raise SystemExit(f"{path} is not a JSON array")
    files[path] = data
    for obj in data:
        if isinstance(obj, dict):
            item = dict(obj)
            item["_journal_file"] = str(path.relative_to(repo))
            item["_journal_path"] = path
            entries.append(item)

def reviewed_ids(all_entries):
    ids = set()
    for obj in all_entries:
        if obj.get("author") != "chatgpt":
            continue
        idref = obj.get("idref")
        if idref:
            ids.add(idref)
        related = obj.get("related_idrefs") or []
        if isinstance(related, list):
            ids.update(value for value in related if isinstance(value, str) and value)
    return ids

def existing_by_uuid(all_entries):
    return {
        obj.get("uuid"): obj
        for obj in all_entries
        if isinstance(obj, dict) and obj.get("uuid")
    }

def is_metadata_only_file(path):
    value = str(path)
    return (
        value.startswith("journal/")
        or value.startswith(str(repo / "journal") + "/")
        or value.startswith("/home/user/.claude/projects/-home-owner-remanence/memory/")
        or value.startswith("/home/user/.codex/memories/")
    )

def is_ignored_no_finding_ack(obj, by_uuid):
    subject = str(obj.get("subject") or "").lower()
    summary = str(obj.get("summary") or "").lower()
    text = subject + "\n" + summary
    no_action_markers = (
        "no-finding",
        "no finding",
        "no issue found",
        "no action required",
        "no action needed",
    )
    if not any(marker in text for marker in no_action_markers):
        return False
    if obj.get("commits"):
        return False
    files = obj.get("files") or []
    if not isinstance(files, list):
        return False
    if any(not is_metadata_only_file(path) for path in files):
        return False
    idref = obj.get("idref")
    if not idref:
        return (
            subject.startswith(("ack:", "meta:", "process:", "checkpoint"))
            or "read codex" in text
            or "codex review" in text
        )
    ref = by_uuid.get(idref)
    return (
        isinstance(ref, dict)
        and ref.get("author") == "chatgpt"
        and ref.get("subject") == "review"
    )

def pending_entries(all_entries):
    reviewed = reviewed_ids(all_entries)
    by_uuid = existing_by_uuid(all_entries)
    return [
        obj
        for obj in all_entries
        if obj.get("author") == "claude"
        and obj.get("uuid")
        and obj.get("uuid") not in reviewed
        and not is_future_entry(obj)
        and not is_ignored_no_finding_ack(obj, by_uuid)
    ]

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

def is_future_entry(obj):
    parsed = parse_datetime(obj.get("datetime"))
    return parsed is not None and parsed.date().isoformat() > today

def adaptive_interval_seconds(all_entries):
    claude_times = [
        parsed
        for parsed in (
            parse_datetime(obj.get("datetime"))
            for obj in all_entries
            if obj.get("author") == "claude"
        )
        if parsed is not None and parsed.date().isoformat() <= today
    ]
    if not claude_times:
        return 1800, None, None
    latest = max(claude_times)
    age = max(0, int((now - latest).total_seconds()))
    if age <= 10 * 60:
        interval = 30
    elif age <= 30 * 60:
        interval = 60
    elif age <= 2 * 60 * 60:
        interval = 5 * 60
    elif age <= 6 * 60 * 60:
        interval = 15 * 60
    else:
        interval = 30 * 60
    return interval, latest.isoformat(timespec="seconds"), age

def write_adaptive_state(all_entries):
    if dry_run:
        return
    interval, latest_claude, latest_age = adaptive_interval_seconds(all_entries)
    fresh_mtime_ns = max((path.stat().st_mtime_ns for path in journal_paths), default=0)
    state = {
        "updated_at": now.isoformat(timespec="seconds"),
        "journal_mtime_ns": fresh_mtime_ns,
        "next_due_epoch": now_epoch + interval,
        "interval_seconds": interval,
        "latest_claude_datetime": latest_claude,
        "latest_claude_age_seconds": latest_age,
    }
    state_file.parent.mkdir(parents=True, exist_ok=True)
    tmp = state_file.with_suffix(state_file.suffix + ".tmp")
    tmp.write_text(json.dumps(state, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    tmp.replace(state_file)

pending = pending_entries(entries)

if not pending:
    write_adaptive_state(entries)
    raise SystemExit(0)

pending = pending[:max_entries]
compact = []
for obj in pending:
    summary = obj.get("summary", "")
    if len(summary) > 1800:
        summary = summary[:1800] + "\n...[truncated by scheduler prompt]..."
    compact.append(
        {
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
    )

prompt = f"""
You are Codex acting as the Remanence reviewer.

Follow the persistent protocol in:
/home/user/.codex/memories/remanence-journal-reviewer-protocol.md

Task:
- Inspect the pending Claude journal entries listed below.
- For each entry, run a real reviewer pass. If code or docs changed, inspect
  the relevant files, diffs, and tests as needed. Do not rely only on the
  summary excerpt.
- Append one `author: "chatgpt"`, `subject: "review"` JSON object per reviewed
  Claude entry to the journal file matching the review entry's own IST
  `datetime` date, with `idref` set to the Claude entry UUID. Do not append a
  review timestamped today into a future-date journal file just because the
  reviewed Claude entry came from that file.
- Use IST / Asia-Kolkata (`+05:30`) for every `datetime` value, matching
  Claude's journal entries. If you generate timestamps with `date`, use the
  inherited `TZ=Asia/Kolkata` environment or run `TZ=Asia/Kolkata date -Iseconds`.
- Do not modify project code or docs; only append review entries to the journal.
- Re-read the journal immediately before editing, preserve valid JSON, and
  validate the journal with `jq` after writing.
- If no issue is found for an entry, still write a concise review saying what
  was checked and what residual risk remains.

Pending Claude entries:
{json.dumps(compact, indent=2)}
"""

prompt_file.write_text(textwrap.dedent(prompt).strip() + "\n", encoding="utf-8")
print(f"{len(pending)} pending Claude journal entr{'y' if len(pending) == 1 else 'ies'}")
raise SystemExit(2)
PY
scan_status=$?
set -e

if [[ "$scan_status" -eq 0 ]]; then
  "$JQ_BIN" '.' "$REPO"/journal/20??-??-??.json >/dev/null
  exit 0
fi

if [[ "$scan_status" -ne 2 ]]; then
  echo "$(date -Iseconds) journal scan failed with status $scan_status"
  exit "$scan_status"
fi

if [[ "$DRY_RUN" -eq 1 ]]; then
  echo "$(date -Iseconds) dry run: Codex would be invoked with prompt $prompt_file"
  sed -n '1,220p' "$prompt_file"
  exit 0
fi

echo "$(date -Iseconds) starting Codex journal review pass $pass/$MAX_PASSES"
set +e
"$TIMEOUT_BIN" --kill-after=30s "$CODEX_TIMEOUT" "$CODEX_BIN" exec \
  -C "$REPO" \
  -s danger-full-access \
  -c 'approval_policy="never"' \
  --color never \
  --output-last-message "$last_message_file" \
  - < "$prompt_file"
codex_status=$?
set -e

"$JQ_BIN" '.' "$REPO"/journal/20??-??-??.json >/dev/null
if [[ "$codex_status" -ne 0 ]]; then
  echo "$(date -Iseconds) Codex journal review pass $pass/$MAX_PASSES exited with status $codex_status"
  exit "$codex_status"
fi
echo "$(date -Iseconds) Codex journal review pass $pass/$MAX_PASSES complete"
done

echo "$(date -Iseconds) Codex journal review reached max passes; next cron tick will continue if entries remain"
