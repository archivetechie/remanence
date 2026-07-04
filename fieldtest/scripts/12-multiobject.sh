#!/usr/bin/env bash
set -euo pipefail

SCRIPT_NAME="12-multiobject"
# shellcheck disable=SC1091
source "$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)/lib.sh"

usage() {
  cat <<'EOF'
Usage: 12-multiobject.sh [--help]

Builds a multi-object archive, restores a random sample, and performs ranged
extract checks on several members.
EOF
}

sha256_file() {
  python3 - "$1" <<'PY'
import hashlib
import sys
from pathlib import Path

digest = hashlib.sha256()
with Path(sys.argv[1]).open("rb") as handle:
    while True:
        chunk = handle.read(1024 * 1024)
        if not chunk:
            break
        digest.update(chunk)
print(digest.hexdigest())
PY
}

main() {
  if [[ "${1:-}" == --help || "${1:-}" == -h ]]; then
    usage
    exit 0
  fi

  fieldtest_init_layout
  fieldtest_detect_env || true
  local serial
  serial="$(fieldtest_selected_library_serial)"
  if [[ -z "$serial" ]]; then
    echo "error: no selected library; run bringup first" >&2
    exit 1
  fi

  local stamp workdir count min_mb max_mb
  stamp="$(fieldtest_timestamp_id)"
  workdir="$(mktemp -d "$(fieldtest_spool_dir)/multi-${stamp}.XXXXXX")"
  count="${FIELD_MULTI_COUNT:-200}"
  min_mb="${FIELD_MULTI_MIN_MB:-5}"
  max_mb="${FIELD_MULTI_MAX_MB:-50}"
  [[ "${REMFIELD_ENV:-unknown}" == vtl ]] && max_mb="${FIELD_MULTI_MAX_MB_VTL:-10}"

  python3 - "$workdir/inputs" "$count" "$min_mb" "$max_mb" <<'PY'
import os
import random
import sys
from pathlib import Path

root = Path(sys.argv[1])
count = int(sys.argv[2])
min_mb = int(sys.argv[3])
max_mb = int(sys.argv[4])
rng = random.Random(0xC0DE)
root.mkdir(parents=True, exist_ok=True)
chunk = 8 * 1024 * 1024
for idx in range(count):
    size_mb = rng.randint(min_mb, max_mb)
    path = root / f"item-{idx:03d}.bin"
    remaining = size_mb * 1024 * 1024
    with path.open("wb") as handle:
        while remaining > 0:
            n = min(chunk, remaining)
            if idx % 2 == 0:
                handle.write(os.urandom(n))
            else:
                handle.write((b"\0" * n))
            remaining -= n
PY

  local object="$workdir/multi.rao" manifest="$workdir/multi-manifest.json"
  mapfile -t inputs < <(find "$workdir/inputs" -maxdepth 1 -type f | sort)
  if ! "$(fieldtest_rem_bin)" archive build --inputs "${inputs[@]}" --out "$object"; then
    fieldtest_evidence_record "$SCRIPT_NAME" build FAIL "multiobject archive build failed" "$manifest"
    exit 1
  fi

  local object_sha
  object_sha="$(sha256_file "$object")"
  local locator="$workdir/locator.json"
  if ! fieldtest_capture_json "$locator" "$(fieldtest_io_bin)" --endpoint "$(fieldtest_rem_endpoint)" write --library "$serial" --file "$object" --pool fieldtest-a; then
    fieldtest_evidence_record "$SCRIPT_NAME" write FAIL "multiobject daemon write failed" "$locator"
    exit 1
  fi

  local restored="$workdir/restored.rao"
  if ! fieldtest_capture_json "$workdir/read.json" "$(fieldtest_io_bin)" --endpoint "$(fieldtest_rem_endpoint)" read --object "$(cat "$locator")" --out "$restored"; then
    fieldtest_evidence_record "$SCRIPT_NAME" read FAIL "multiobject daemon read failed" "$workdir/read.json"
    exit 1
  fi
  if [[ "$(sha256_file "$restored")" != "$object_sha" ]]; then
    fieldtest_evidence_record "$SCRIPT_NAME" fidelity FAIL "restored multiobject archive SHA mismatch" "$workdir/read.json"
    exit 1
  fi

  python3 - "$manifest" "$workdir/sample.txt" "$workdir/range.txt" <<'PY'
import json
import random
import sys
from pathlib import Path

manifest = json.loads(Path(sys.argv[1]).read_text())
entries = manifest.get("members", []) or manifest.get("files", [])
if len(entries) < 5:
    raise SystemExit("manifest has too few members")
rng = random.Random(0xBEEF)
samples = rng.sample(entries, 5)
Path(sys.argv[2]).write_text("\n".join(e["path"] for e in samples[:3]) + "\n")
Path(sys.argv[3]).write_text("\n".join(e["path"] for e in samples[3:]) + "\n")
PY

  local restore_dir="$workdir/restore"
  mkdir -p "$restore_dir"
  local sample
  while IFS= read -r sample; do
    [[ -n "$sample" ]] || continue
    if ! fieldtest_capture_json "$workdir/restore-$sample.json" "$(fieldtest_rem_bin)" archive extract --object "$object" --dest "$restore_dir" --path "$sample" --overwrite; then
      fieldtest_evidence_record "$SCRIPT_NAME" restore FAIL "failed to restore $sample from multiobject archive" "$workdir/restore-$sample.json"
      exit 1
    fi
    if [[ "$(sha256_file "$restore_dir/$sample")" != "$(sha256_file "$workdir/inputs/$sample")" ]]; then
      fieldtest_evidence_record "$SCRIPT_NAME" restore FAIL "restored bytes mismatch for $sample" "$workdir/restore-$sample.json"
      exit 1
    fi
  done <"$workdir/sample.txt"

  local range_sample i
  i=0
  while IFS= read -r sample; do
    [[ -n "$sample" ]] || continue
    if ! fieldtest_capture_json "$workdir/range-$i.json" "$(fieldtest_rem_bin)" archive extract --object "$object" --dest "$workdir/range-$i" --path "$sample" --range 1048576:1048576 --overwrite; then
      fieldtest_evidence_record "$SCRIPT_NAME" range FAIL "range extract failed for $sample" "$workdir/range-$i.json"
      exit 1
    fi
    fieldtest_evidence_record "$SCRIPT_NAME" range PASS "range extract succeeded for $sample" "$workdir/range-$i.json"
    i=$((i + 1))
  done <"$workdir/range.txt"

  fieldtest_evidence_record "$SCRIPT_NAME" manifest PASS "multiobject archive with $count members built and sampled" "$manifest"
  rm -rf -- "$workdir"
}

fieldtest_run_with_lock main "$@"
