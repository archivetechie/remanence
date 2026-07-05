#!/usr/bin/env bash
set -euo pipefail

SCRIPT_NAME="11-happy-path"
# shellcheck disable=SC1091
source "$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)/lib.sh"

usage() {
  cat <<'EOF'
Usage: 11-happy-path.sh [--help]

Builds a fixture archive, writes it to tape, restores it, checks SHA-256, and
verifies plaintext/encrypted and ranged-read coverage.
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

generate_inputs() {
  local root="$1" size_gb="$2"
  python3 - "$root" "$size_gb" <<'PY'
import os
import sys
from pathlib import Path

root = Path(sys.argv[1])
size_gb = float(sys.argv[2])
bytes_total = int(size_gb * 1024 * 1024 * 1024)
chunk = 8 * 1024 * 1024
root.mkdir(parents=True, exist_ok=True)

random_path = root / "random.bin"
zero_path = root / "zeros.bin"
for path, mode in [(random_path, "random"), (zero_path, "zero")]:
    remaining = bytes_total
    with path.open("wb") as handle:
        while remaining > 0:
            n = min(chunk, remaining)
            if mode == "random":
                handle.write(os.urandom(n))
            else:
                handle.write(b"\0" * n)
            remaining -= n
PY
}

build_object() {
  local object="$1" manifest="$2" inputs_dir="$3" key_file="${4:-}"
  if [[ -n "$key_file" ]]; then
    "$(fieldtest_rem_bin)" --allow "$(fieldtest_selected_library_serial)" archive build \
      --inputs "$inputs_dir/random.bin" --inputs "$inputs_dir/zeros.bin" \
      --out "$object" --encrypt --key-file "$key_file" \
      --key-id 00112233445566778899aabbccddeeff
  else
    "$(fieldtest_rem_bin)" archive build \
      --inputs "$inputs_dir/random.bin" --inputs "$inputs_dir/zeros.bin" \
      --out "$object"
  fi
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
    echo "error: no selected library; run 01-allowlist.sh or 03-bringup.sh first" >&2
    exit 1
  fi
  fieldtest_require_allowlisted "$(fieldtest_allowlist_barcodes | head -n 1)"
  fieldtest_require_pool_appendable_tapes fieldtest-a 1 "plaintext happy-path write"
  fieldtest_require_pool_appendable_tapes fieldtest-b 1 "encrypted happy-path write"

  local stamp workdir object_plain manifest_plain restored_plain locator_plain read_plain range_dir
  local size_gb="${FIELD_HAPPY_GB:-2}"
  [[ "${REMFIELD_ENV:-unknown}" == vtl ]] && size_gb="${FIELD_HAPPY_GB_VTL:-0.25}"
  stamp="$(fieldtest_timestamp_id)"
  workdir="$(mktemp -d "$(fieldtest_spool_dir)/happy-${stamp}.XXXXXX")"
  object_plain="$workdir/plain.rao"
  manifest_plain="$workdir/plain-manifest.json"
  restored_plain="$workdir/restored.rao"
  range_dir="$workdir/range"
  generate_inputs "$workdir/inputs" "$size_gb"
  build_object "$object_plain" "$manifest_plain" "$workdir/inputs"

  local object_sha restore_sha
  object_sha="$(sha256_file "$object_plain")"

  locator_plain="$(fieldtest_artifact_path "$SCRIPT_NAME" write-locator-plain "$stamp")"
  if ! fieldtest_capture_json "$locator_plain" "$(fieldtest_io_bin)" --endpoint "$(fieldtest_rem_endpoint)" write --library "$serial" --file "$object_plain" --pool fieldtest-a; then
    fieldtest_evidence_record "$SCRIPT_NAME" write-plain FAIL "daemon write failed for plaintext object" "$locator_plain"
    exit 1
  fi
  fieldtest_evidence_record "$SCRIPT_NAME" write-plain PASS "plaintext object written to fieldtest-a" "$locator_plain"

  read_plain="$(fieldtest_artifact_path "$SCRIPT_NAME" read-plain "$stamp")"
  if ! fieldtest_capture_json "$read_plain" "$(fieldtest_io_bin)" --endpoint "$(fieldtest_rem_endpoint)" read --object "$(cat "$locator_plain")" --out "$restored_plain"; then
    fieldtest_evidence_record "$SCRIPT_NAME" read-plain FAIL "daemon read failed for plaintext object" "$read_plain"
    exit 1
  fi
  restore_sha="$(sha256_file "$restored_plain")"
  if [[ "$restore_sha" != "$object_sha" ]]; then
    fieldtest_evidence_record "$SCRIPT_NAME" fidelity FAIL "restored plaintext object SHA mismatch" "$read_plain"
    exit 1
  fi
  fieldtest_evidence_record "$SCRIPT_NAME" fidelity PASS "plaintext object round-tripped with matching SHA-256" "$read_plain"

  range_dir="$workdir/range-plain"
  mkdir -p "$range_dir"
  if ! fieldtest_capture_json "$workdir/range.json" "$(fieldtest_rem_bin)" archive extract --object "$object_plain" --dest "$range_dir" --path random.bin --range 1048576:1048576 --overwrite; then
    fieldtest_evidence_record "$SCRIPT_NAME" range FAIL "plain archive range extraction failed" "$workdir/range.json"
    exit 1
  fi
  fieldtest_evidence_record "$SCRIPT_NAME" range PASS "1 MiB range extracted from plain object" "$workdir/range.json"

  local key_file encrypted_object encrypted_manifest encrypted_locator restored_encrypted encrypted_sha
  key_file="$workdir/key.bin"
  python3 - "$key_file" <<'PY'
from pathlib import Path
import os, sys
Path(sys.argv[1]).write_bytes(os.urandom(32))
PY
  encrypted_object="$workdir/encrypted.rao"
  encrypted_manifest="$workdir/encrypted-manifest.json"
  build_object "$encrypted_object" "$encrypted_manifest" "$workdir/inputs" "$key_file"
  encrypted_sha="$(sha256_file "$encrypted_object")"
  encrypted_locator="$(fieldtest_artifact_path "$SCRIPT_NAME" write-locator-encrypted "$stamp")"
  if ! fieldtest_capture_json "$encrypted_locator" "$(fieldtest_io_bin)" --endpoint "$(fieldtest_rem_endpoint)" write --library "$serial" --file "$encrypted_object" --pool fieldtest-b; then
    fieldtest_evidence_record "$SCRIPT_NAME" write-encrypted FAIL "daemon write failed for encrypted object" "$encrypted_locator"
    exit 1
  fi
  restored_encrypted="$workdir/restored-encrypted.rao"
  if ! fieldtest_capture_json "$workdir/read-encrypted.json" "$(fieldtest_io_bin)" --endpoint "$(fieldtest_rem_endpoint)" read --object "$(cat "$encrypted_locator")" --out "$restored_encrypted"; then
    fieldtest_evidence_record "$SCRIPT_NAME" read-encrypted FAIL "daemon read failed for encrypted object" "$workdir/read-encrypted.json"
    exit 1
  fi
  if [[ "$(sha256_file "$restored_encrypted")" != "$encrypted_sha" ]]; then
    fieldtest_evidence_record "$SCRIPT_NAME" encrypted-fidelity FAIL "restored encrypted object SHA mismatch" "$workdir/read-encrypted.json"
    exit 1
  fi
  fieldtest_evidence_record "$SCRIPT_NAME" encrypted-fidelity PASS "encrypted object round-tripped with matching SHA-256" "$workdir/read-encrypted.json"

  local verify_json
  verify_json="$workdir/verify.json"
  if ! fieldtest_capture_json "$verify_json" "$(fieldtest_io_bin)" --endpoint "$(fieldtest_rem_endpoint)" read --object "$(cat "$locator_plain")" --out "$workdir/verify-restored.rao"; then
    fieldtest_evidence_record "$SCRIPT_NAME" verify FAIL "daemon verify read failed for plaintext object" "$verify_json"
    exit 1
  fi
  if [[ "$(sha256_file "$workdir/verify-restored.rao")" != "$object_sha" ]]; then
    fieldtest_evidence_record "$SCRIPT_NAME" verify FAIL "daemon verify read SHA mismatch" "$verify_json"
    exit 1
  fi
  fieldtest_evidence_record "$SCRIPT_NAME" verify PASS "daemon verify read succeeded for plaintext object" "$verify_json"

  rm -rf -- "$workdir"
}

fieldtest_run_with_lock main "$@"
