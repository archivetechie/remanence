#!/usr/bin/env bash
# Remanence formal-proof inventory gate.
#
# This script replays every local verification crate's Rust drift guard, full
# Rust tests, Lean build, and maintained-proof placeholder scan. The Lean type
# checker is the proof trust anchor; this command is the repo-level gate that
# keeps the proof estate easy to audit after production or extraction changes.

set -uo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
VERIF_DIR="$ROOT_DIR/verif"

export PATH="$HOME/.elan/bin:$PATH"

failures=()

record_failure() {
    failures+=("$1")
}

require_command() {
    local command_name="$1"

    if ! command -v "$command_name" >/dev/null 2>&1; then
        record_failure "missing command: $command_name"
        return 1
    fi
}

run_in_dir() {
    local label="$1"
    local dir="$2"
    shift 2

    printf '\n== %s ==\n' "$label"
    if (cd "$dir" && "$@"); then
        printf 'PASS: %s\n' "$label"
    else
        printf 'FAIL: %s\n' "$label"
        record_failure "$label"
    fi
}

require_command cargo
require_command lake
require_command rg

if ((${#failures[@]} > 0)); then
    printf '\n== inventory summary ==\n'
    printf 'FAIL: required command check failed\n'
    for failure in "${failures[@]}"; do
        printf ' - %s\n' "$failure"
    done
    exit 1
fi

proof_crates=()
while IFS= read -r -d '' crate_dir; do
    proof_crates+=("$crate_dir")
done < <(
    find "$VERIF_DIR" -mindepth 2 -maxdepth 2 -type f -name Cargo.toml -printf '%h\0' |
        sort -z
)

if ((${#proof_crates[@]} == 0)); then
    printf 'FAIL: no verification crates found under %s\n' "$VERIF_DIR"
    exit 1
fi

for crate_dir in "${proof_crates[@]}"; do
    crate_name="$(basename "$crate_dir")"

    if ! rg -q 'fn drift_guard' "$crate_dir/src/lib.rs"; then
        printf '\nFAIL: %s has no drift_guard test in src/lib.rs\n' "$crate_name"
        record_failure "$crate_name: missing drift_guard test"
    fi

    run_in_dir "$crate_name: cargo test drift_guard" "$crate_dir" cargo test drift_guard
    run_in_dir "$crate_name: cargo test" "$crate_dir" cargo test

    if [[ -f "$crate_dir/lean/lakefile.toml" ]]; then
        run_in_dir "$crate_name: lake build" "$crate_dir/lean" lake build
    else
        printf '\nFAIL: %s has no lean/lakefile.toml\n' "$crate_name"
        record_failure "$crate_name: missing Lean lakefile"
    fi
done

placeholder_files=()
while IFS= read -r -d '' proof_file; do
    placeholder_files+=("$proof_file")
done < <(
    find "$VERIF_DIR" \
        \( \
            -path '*/target/*' -o \
            -path '*/lean/.lake/*' -o \
            -path '*/lean/.leanstral/*' -o \
            -path '*/lean-out/*' \
        \) -prune -o \
        -type f \( -name '*.rs' -o -name '*.lean' \) -print0 |
        sort -z
)

printf '\n== local placeholder scan ==\n'
if ((${#placeholder_files[@]} == 0)); then
    printf 'FAIL: no Rust or Lean proof files found under %s\n' "$VERIF_DIR"
    record_failure "local placeholder scan: no files"
elif rg -n '\b(sorry|admit|axiom)\b' "${placeholder_files[@]}"; then
    printf 'FAIL: local placeholder scan found proof placeholders\n'
    record_failure "local placeholder scan"
else
    printf 'PASS: local placeholder scan (%s files)\n' "${#placeholder_files[@]}"
fi

printf '\n== inventory summary ==\n'
if ((${#failures[@]} == 0)); then
    printf 'PASS: checked %s verification crates\n' "${#proof_crates[@]}"
else
    printf 'FAIL: %s inventory check(s) failed\n' "${#failures[@]}"
    for failure in "${failures[@]}"; do
        printf ' - %s\n' "$failure"
    done
    exit 1
fi
