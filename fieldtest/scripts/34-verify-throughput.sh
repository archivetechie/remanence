#!/usr/bin/env bash
# Measure full tape verification through the daemon and retain phase and log evidence.
set -euo pipefail
SCRIPT_NAME="34-verify-throughput"
# shellcheck disable=SC1091
source "$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)/lib.sh"

usage() {
  cat <<'EOF'
Usage: 34-verify-throughput.sh [--tape-uuid UUID | --barcode LABEL | --finalize-bench]

Verify the first sealed tape in the selected library, or an exact sealed tape.
--finalize-bench closes the first ready fieldtest-a tape before verification;
FIELD_CONFIRM_FINALIZE=1 is also required. Closing removes appendability.
FIELD_VERIFY_TIMEOUT_SECONDS bounds verification and finalization (default 14400).
Run before and after a binary change on the same tape to compare phase timings.
EOF
}

fail() {
  fieldtest_evidence_record "$SCRIPT_NAME" "$1" FAIL "$2" "${3:-}"
  exit 1
}

# Catalog state sealed represents terminal finalization. Live UUID membership
# limits the catalog to the selected library; barcode matching alone is insufficient.
select_tape() {
  python3 - "$@" <<'PY'
import json, sys, uuid
from pathlib import Path
catalog, top, serial, requested, barcode, finalize = sys.argv[1:]
def uid(value):
    return str(uuid.UUID(value)) if value else None
libraries = json.loads(Path(top).read_text())["data"]["libraries"]
library = next(x for x in libraries if x["library"]["library_serial"] == serial)
resident = {uid(x.get("tape_uuid")) for x in library["slots"]}
resident.update(uid(x.get("loaded_tape_uuid")) for x in library["drives"])
for tape in json.loads(Path(catalog).read_text())["data"]["tapes"]:
    tape_id = uid(tape["tape_uuid"])
    if tape_id not in resident:
        continue
    if requested and tape_id != uid(requested):
        continue
    if barcode and tape.get("voltag") != barcode:
        continue
    if finalize == "1":
        eligible = tape["state"] == "ready" and tape.get("pool_id") == "fieldtest-a"
    else:
        eligible = tape["state"] == "sealed"
    if eligible:
        print(json.dumps(dict(tape, tape_uuid=tape_id), separators=(",", ":")))
        break
else:
    raise SystemExit("no eligible tape in the selected library")
PY
}

# Keep decimal-string counters exact; report decimal MB/s and compare only UUID matches.
write_split() {
  python3 - "$@" <<'PY'
import csv, json, sys, uuid
from pathlib import Path
source, split, delta, evidence, tape_uuid = sys.argv[1:]
source, split, delta, evidence = map(Path, (source, split, delta, evidence))
payload = json.loads(source.read_text())
# capture_json retains stdout inside its failure envelope on a nonzero exit.
if "exit_code" in payload:
    payload = json.loads(payload["stdout"])
data = payload["data"]
if uuid.UUID(data["tape_uuid"]) != uuid.UUID(tape_uuid):
    raise ValueError("verification tape UUID does not match selection")
io = data["io"]
counts_keys = ("locate", "space", "read_position", "single_read", "batch_read", "other")
header = ["phase", "elapsed_s", "command_s", "between_s", "commands", "max_command_s", "bytes", "MB_per_s",
          "lt5ms", "5to20ms", "20to100ms", "100to1000ms", "1to10s", "ge10s", *counts_keys, "tape_uuid"]
def row(phase, counts):
    values = {k: int(counts[k]) for k in (*counts_keys, "elapsed_ms", "command_ms", "max_command_ms", "bytes")}
    buckets = [int(x) for x in counts["command_latency_buckets"]]
    if len(buckets) != 6 or min(*values.values(), *buckets) < 0:
        raise ValueError("invalid timing counters or latency buckets")
    elapsed, command = values["elapsed_ms"], values["command_ms"]
    if command > elapsed:
        raise ValueError("command time exceeds elapsed time")
    commands = sum(values[k] for k in counts_keys)
    if sum(buckets) != commands:
        raise ValueError("latency buckets do not sum to commands")
    return [phase, f"{elapsed / 1000:.3f}", f"{command / 1000:.3f}", f"{(elapsed-command) / 1000:.3f}",
            commands, f"{values['max_command_ms'] / 1000:.3f}", values["bytes"],
            f"{values['bytes'] / elapsed / 1000:.3f}" if elapsed else "n/a",
            *buckets, *(values[k] for k in counts_keys), tape_uuid]
rows = []
seen = {"total"}
for phase in io["phases"]:
    name = phase["phase"]
    if not name or any(c in name for c in "\t\n\r") or name in seen:
        raise ValueError("invalid or duplicate phase")
    seen.add(name)
    r = row(name, phase["counts"])
    if r[4]:
        rows.append(r)
rows.append(row("total", io["total"]))
if not rows[-1][4]:
    raise ValueError("verification issues no commands")
# UUID is stored on every row so evidence stays comparable when files move.
candidates = []
for path in evidence.rglob("verify-split-*.tsv"):
    if path == split:
        continue
    with path.open() as handle:
        prior = list(csv.DictReader(handle, delimiter="\t"))
    if prior and all(r.get("tape_uuid") == tape_uuid for r in prior):
        candidates.append((path.stat().st_mtime_ns, str(path), prior))
with split.open("w") as handle:
    writer = csv.writer(handle, delimiter="\t", lineterminator="\n")
    writer.writerow(header)
    writer.writerows(rows)
with delta.open("w") as handle:
    if candidates:
        _, path, prior = max(candidates)
        handle.write(f"reference={Path(path).name} tape_uuid={tape_uuid}\n")
        old = {r["phase"]: r for r in prior}
        for r in rows:
            name = r[0]
            if name not in old:
                handle.write(f"{name}: no reference phase\n")
                continue
            parts = []
            for index, metric in ((1, "elapsed_s"), (2, "command_s"), (3, "between_s")):
                before, after = float(old[name][metric]), float(r[index])
                seconds = after - before
                percent = f"{100 * seconds / before:+.2f}%" if before else "n/a (zero reference)"
                parts.append(f"{metric}={seconds:+.3f} ({percent})")
            handle.write(f"{name}: " + "; ".join(parts) + "\n")
print(data["verification_state"])
PY
}

main() {
  set -e
  local requested="" barcode="" finalize=0
  while (($#)); do
    case "$1" in
      --help|-h) usage; return 0 ;;
      --tape-uuid|--barcode)
        [[ $# -ge 2 && -n "$2" && "$2" != --* ]] || { usage >&2; return 2; }
        if [[ "$1" == --tape-uuid ]]; then requested="$2"; else barcode="$2"; fi
        shift 2 ;;
      --finalize-bench) finalize=1; shift ;;
      *) usage >&2; return 2 ;;
    esac
  done
  if (( ${#requested} > 0 && ${#barcode} > 0 )) || { (( finalize )) && [[ -n "$requested$barcode" ]]; }; then
    echo 'select one of --tape-uuid, --barcode, --finalize-bench' >&2; return 2
  fi
  if (( finalize )) && [[ "${FIELD_CONFIRM_FINALIZE:-}" != 1 ]]; then
    fail finalize 'set FIELD_CONFIRM_FINALIZE=1 to close an appendable fieldtest-a tape'
  fi
  local limit="${FIELD_VERIFY_TIMEOUT_SECONDS:-14400}"
  [[ "$limit" =~ ^[1-9][0-9]*$ ]] || fail timeout 'FIELD_VERIFY_TIMEOUT_SECONDS must be a positive integer'
  command -v timeout >/dev/null || fail timeout 'timeout is required'
  fieldtest_init_layout
  local serial rem endpoint stamp dir top catalog selected tape pool out split delta diag
  serial="$(fieldtest_selected_library_serial)" || fail library 'select a library with bringup first'
  [[ -n "$serial" ]] || fail library 'selected library is empty'
  rem="$(fieldtest_rem_bin)" || fail binary 'rem is unavailable'
  endpoint="$(fieldtest_rem_endpoint)"
  stamp="$(date -u +%Y%m%dT%H%M%S.%NZ)"
  dir="$(fieldtest_script_evidence_dir "$SCRIPT_NAME")"
  mkdir -p "$dir"
  top="$dir/$stamp-bays.json"
  catalog="$dir/$stamp-catalog.json"
  selected="$dir/$stamp-selected.json"
  out="$dir/verify-$stamp.json"
  split="$dir/verify-split-$stamp.tsv"
  delta="$dir/verify-delta-$stamp.txt"
  diag="$dir/verify-diag-$stamp.jsonl"
  wait_for_idle_bays "$serial" "$top" || fail bays 'idle-bay wait fails or expires' "$top"
  fieldtest_capture_json "$catalog" "$rem" catalog tapes --json --endpoint "$endpoint" || fail catalog 'catalog tapes fails' "$catalog"
  select_tape "$catalog" "$top" "$serial" "$requested" "$barcode" "$finalize" >"$selected" || fail selection 'no eligible tape in the selected library' "$catalog"
  tape="$(fieldtest_json_get tape_uuid <"$selected")"
  pool="$(fieldtest_json_get pool_id <"$selected")"
  fieldtest_evidence_record "$SCRIPT_NAME" selection INFO "tape_uuid=$tape pool=$pool finalize_bench=$finalize" "$selected"
  if (( finalize )); then
    local help key finalout
    help="$dir/$stamp-tape-help.json"
    fieldtest_capture_text "$help" "$rem" tape --help || fail finalize 'cannot inspect tape commands' "$help"
    if ! fieldtest_json_get stdout <"$help" | grep -Eq '^[[:space:]]+finalize[[:space:]]'; then
      fail finalize 'resolved rem does not offer tape finalize' "$help"
    fi
    key="$(python3 -c 'import uuid; print(uuid.uuid4())')"
    finalout="$dir/$stamp-finalize.json"
    fieldtest_evidence_record "$SCRIPT_NAME" finalize-intent INFO "tape_uuid=$tape pool=fieldtest-a idempotency_key=$key; finalization removes appendability" "$selected"
    fieldtest_capture_json "$finalout" timeout --kill-after=30s "${limit}s" "$rem" tape finalize \
      --tape-uuid "$tape" --expected-pool fieldtest-a --reason 'fieldtest verification throughput' \
      --idempotency-key "$key" --ack-tape-uuid "$tape" --wait --json --endpoint "$endpoint" \
      || fail finalize "finalization fails for tape_uuid=$tape; inspect status before retrying" "$finalout"
    [[ "$(fieldtest_json_get data.outcome <"$finalout")" == finalized ]] || fail finalize "tape_uuid=$tape does not reach finalized" "$finalout"
    fieldtest_evidence_record "$SCRIPT_NAME" finalize PASS "finalized tape_uuid=$tape pool=fieldtest-a" "$finalout"
    wait_for_idle_bays "$serial" "$top" || fail bays 'idle-bay wait fails after finalization' "$top"
  fi
  local verify_help="$dir/$stamp-verify-help.json" ready_args=()
  fieldtest_capture_text "$verify_help" "$rem" tape verify-index --help || fail readiness 'cannot inspect verify-index options' "$verify_help"
  if fieldtest_json_get stdout <"$verify_help" | grep -Eq -- '(^|[[:space:]])--ready-wait([=[:space:]]|$)'; then
    ready_args=(--ready-wait 15m)
    fieldtest_evidence_record "$SCRIPT_NAME" readiness INFO 'verify-index uses --ready-wait 15m' "$verify_help"
  else
    local objects="$dir/$stamp-objects.json" locator spool io length
    io="$(fieldtest_io_bin)" || fail readiness 'remfield-io is unavailable'
    fieldtest_capture_json "$objects" "$io" --endpoint "$endpoint" list --pool "$pool" || fail readiness 'cannot list objects for readiness read' "$objects"
    locator="$(python3 - "$objects" "$tape" <<'PY'
import json, sys, uuid
from pathlib import Path
for obj in json.loads(Path(sys.argv[1]).read_text())["objects"]:
    if uuid.UUID(obj["tape_uuid"]) == uuid.UUID(sys.argv[2]) and int(obj.get("logical_size_bytes") or 0) > 0:
        locator = {k: obj[k] for k in ("object_id", "tape_uuid", "pool_id")}
        locator["read_length"] = min(1048576, int(obj["logical_size_bytes"]))
        print(json.dumps(locator))
        break
else:
    raise SystemExit("no nonempty object on selected tape")
PY
)" || fail readiness 'no object on selected tape for readiness read' "$objects"
    length="$(printf '%s\n' "$locator" | fieldtest_json_get read_length)"
    spool="$(mktemp "$(fieldtest_spool_dir)/verify-ready.XXXXXX")"
    if ! fieldtest_capture_io_json "$dir/$stamp-ready-read.json" "$io" --endpoint "$endpoint" read --object "$locator" --length "$length" --out "$spool"; then
      rm -f -- "$spool"
      fail readiness 'readiness read fails' "$dir/$stamp-ready-read.json"
    fi
    rm -f -- "$spool"
    fieldtest_evidence_record "$SCRIPT_NAME" readiness PASS "small read completes on tape_uuid=$tape" "$dir/$stamp-ready-read.json"
    wait_for_idle_bays "$serial" "$top" || fail bays 'idle-bay wait fails after readiness read' "$top"
  fi
  local log offset identity rc=0 state diag_rc=0
  log="$(fieldtest_log_dir)/rem-daemon.log"
  [[ -f "$log" ]] || fail diagnostics 'daemon pipe-pane log is absent'
  offset="$(stat -c %s "$log")"
  identity="$(stat -c %d:%i "$log")"
  fieldtest_capture_json "$out" timeout --kill-after=30s "${limit}s" "$rem" tape verify-index \
    --tape-uuid "$tape" --json --endpoint "$endpoint" "${ready_args[@]}" || rc=$?
  # Allow the tmux pipe to drain before taking its ending byte boundary.
  sleep 1
  python3 - "$log" "$offset" "$identity" "$diag" <<'PY' || diag_rc=$?
import json, sys
from pathlib import Path
log, offset, identity, dest = sys.argv[1:]
with Path(log).open("rb") as handle, Path(dest).open("w") as output:
    import os
    stat = os.fstat(handle.fileno())
    if f"{stat.st_dev}:{stat.st_ino}" != identity or stat.st_size < int(offset):
        raise SystemExit("daemon log rotates or truncates during verification")
    handle.seek(int(offset))
    # Stream a bounded window so hours of daemon traffic do not fill memory.
    while handle.tell() < stat.st_size:
        raw = handle.readline(stat.st_size - handle.tell())
        if not raw:
            raise SystemExit("daemon log truncates during capture")
        line = raw.decode(errors="replace").rstrip("\r\n")
        if "remanence_verify_diag" in line:
            # Preserve native JSON; encode plain tmux output without losing the line.
            try:
                event = json.loads(line)
            except ValueError:
                event = {"line": line}
            output.write(json.dumps(event) + "\n")
PY
  if (( diag_rc )) || [[ ! -s "$diag" ]]; then
    fieldtest_evidence_record "$SCRIPT_NAME" diagnostics FAIL 'no complete daemon diagnostic capture for this run' "$diag"
    diag_rc=1
  else
    fieldtest_evidence_record "$SCRIPT_NAME" diagnostics INFO 'daemon verification events in the command byte window' "$diag"
  fi
  state="$(write_split "$out" "$split" "$delta" "$(fieldtest_evidence_dir)" "$tape")" || fail split "cannot parse verification timing; command_rc=$rc" "$out"
  cat "$split"
  local row phase
  # The detail path is metadata; evidence_record writes only records.jsonl.
  # shellcheck disable=SC2094
  while IFS= read -r row; do
    [[ "$row" == phase$'\t'* ]] && continue
    phase="${row%%$'\t'*}"
    fieldtest_evidence_record "$SCRIPT_NAME" "phase-$phase" INFO "$row" "$split"
  done <"$split"
  # The detail path is metadata; evidence_record writes only records.jsonl.
  # shellcheck disable=SC2094
  while IFS= read -r row; do
    fieldtest_evidence_record "$SCRIPT_NAME" delta INFO "$row" "$delta"
  done <"$delta"
  if [[ "$state" == verified_complete ]] && (( rc == 0 )); then
    fieldtest_evidence_record "$SCRIPT_NAME" verification PASS "tape_uuid=$tape verification_state=$state" "$out"
  else
    fail verification "tape_uuid=$tape verification_state=$state command_rc=$rc" "$out"
  fi
  return "$diag_rc"
}

fieldtest_run_with_lock main "$@"
