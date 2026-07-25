#!/usr/bin/env bash
# Run the REM-OBJECT 1.0 coverage-guided fuzz campaign used for freeze evidence.
#
# The targets are run sequentially because the CBOR targets can exceed 1 GiB
# RSS during corpus growth. Each target uses its checked-in libFuzzer
# dictionary, then replays the saved corpus with -runs=0 to print compact
# final coverage and corpus statistics.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SECONDS_PER_TARGET="${1:-300}"
VERBOSITY="${FUZZ_VERBOSITY:-1}"

targets=(
  rem_object_envelope_header
  rem_object_envelope_metadata_cbor
  rem_object_manifest_cbor
  rem_object_plaintext_tar_loop
  rem_object_whole_object_open_verify
)

cd "$ROOT"

cargo +nightly fuzz check

for target in "${targets[@]}"; do
  dict="$ROOT/fuzz/fuzz_targets/$target.dict"
  if [[ ! -f "$dict" ]]; then
    echo "missing dictionary for $target: $dict" >&2
    exit 1
  fi

  cargo +nightly fuzz run "$target" -- \
    -max_total_time="$SECONDS_PER_TARGET" \
    -print_final_stats=1 \
    -verbosity="$VERBOSITY" \
    -dict="$dict"

  cargo +nightly fuzz run "$target" -- \
    -runs=0 \
    -print_final_stats=1 \
    -dict="$dict"
done
