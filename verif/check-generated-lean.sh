#!/usr/bin/env bash
# Regenerate the selected Charon/Aeneas translations in an isolated temporary
# directory and compare them byte-for-byte with the maintained Lean artifacts.

set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
AENEAS_ROOT="${AENEAS_ROOT:-$HOME/toolchains/aeneas}"
CHARON_BIN="${CHARON_BIN:-$AENEAS_ROOT/charon/bin/charon}"
AENEAS_BIN="${AENEAS_BIN:-$AENEAS_ROOT/bin/aeneas}"
TEMP_PARENT="${TMPDIR:-/tmp}"

if [[ ! -x "$CHARON_BIN" ]]; then
    printf 'FAIL: Charon is unavailable or not executable: %s\n' "$CHARON_BIN" >&2
    printf 'Set CHARON_BIN or AENEAS_ROOT to the installed toolchain.\n' >&2
    exit 1
fi
if [[ ! -x "$AENEAS_BIN" ]]; then
    printf 'FAIL: Aeneas is unavailable or not executable: %s\n' "$AENEAS_BIN" >&2
    printf 'Set AENEAS_BIN or AENEAS_ROOT to the installed toolchain.\n' >&2
    exit 1
fi
if [[ ! -d "$TEMP_PARENT" ]]; then
    printf 'FAIL: temporary directory parent does not exist: %s\n' "$TEMP_PARENT" >&2
    exit 1
fi

TEMP_ROOT="$(mktemp -d "$TEMP_PARENT/remanence-generated-lean.XXXXXX")"
printf 'Fresh extraction workspace: %s\n' "$TEMP_ROOT"

compare_generated_file() {
    local label="$1"
    local checked_in="$2"
    local generated="$3"

    if [[ ! -f "$generated" ]]; then
        printf 'FAIL: %s was not generated at %s\n' "$label" "$generated" >&2
        return 1
    fi
    if cmp -s -- "$checked_in" "$generated"; then
        printf 'PASS: %s is byte-for-byte current\n' "$label"
        return 0
    fi

    printf 'FAIL: %s differs from a fresh Charon+Aeneas extraction\n' "$label" >&2
    printf 'Checked in: %s\nGenerated: %s\n' "$checked_in" "$generated" >&2
    diff -u --label "checked-in/$label" --label "generated/$label" \
        "$checked_in" "$generated" | sed -n '1,160p' >&2 || true
    return 1
}

generate_llbc() {
    local crate_dir="$1"
    local llbc_path="$2"

    (
        cd "$crate_dir"
        "$CHARON_BIN" cargo --preset=aeneas --dest-file="$llbc_path"
    )
}

parity_dir="$ROOT_DIR/verif/parity-capacity"
parity_llbc="$TEMP_ROOT/parity_capacity_verif.llbc"
parity_out="$TEMP_ROOT/parity-capacity"
generate_llbc "$parity_dir" "$parity_llbc"
mkdir -p "$parity_out"
"$AENEAS_BIN" -backend lean -no-progress-bar -dest "$parity_out" "$parity_llbc"
compare_generated_file \
    "ParityCapacity/Funs.lean" \
    "$parity_dir/lean/ParityCapacity/Funs.lean" \
    "$parity_out/ParityCapacityVerif.lean"

pool_dir="$ROOT_DIR/verif/pool-selection"
pool_llbc="$TEMP_ROOT/pool_selection_verif.llbc"
pool_out="$TEMP_ROOT/pool-selection"
generate_llbc "$pool_dir" "$pool_llbc"
"$AENEAS_BIN" -backend lean -no-progress-bar -split-files \
    -namespace PoolSelection -subdir PoolSelection -dest "$pool_out" "$pool_llbc"
compare_generated_file \
    "PoolSelection/Types.lean" \
    "$pool_dir/lean/PoolSelection/Types.lean" \
    "$pool_out/PoolSelection/Types.lean"
compare_generated_file \
    "PoolSelection/Funs.lean" \
    "$pool_dir/lean/PoolSelection/Funs.lean" \
    "$pool_out/PoolSelection/Funs.lean"

printf 'PASS: fresh generated Lean matches all maintained Funs/Types artifacts\n'
