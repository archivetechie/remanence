#!/usr/bin/env bash
# Run the REM-PARITY 1.0 coverage-guided fuzz campaign used for freeze
# evidence (specification §18.3). Mirrors run_rem_object_fuzz_campaign.sh,
# and additionally records each target's end state into a dated report under
# fuzz/reports/ so "corpus plateau" is a recorded observation, not a claim.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SECONDS_PER_TARGET="${1:-3600}"
VERBOSITY="${FUZZ_VERBOSITY:-1}"
RSS_LIMIT_MB="${FUZZ_RSS_LIMIT_MB:-4096}"
CASE_TIMEOUT_S="${FUZZ_CASE_TIMEOUT_S:-30}"

targets=(
  rem_parity_bootstrap_parse
  rem_parity_bootstrap_structured
  rem_parity_sidecar_parse
  rem_parity_map_parse
  rem_parity_scan_walk
)

cd "$ROOT"

REPORT_DIR="$ROOT/fuzz/reports"
mkdir -p "$REPORT_DIR"
STAMP="$(date -u +%Y%m%dT%H%M%SZ)"
REPORT="$REPORT_DIR/rem-parity-campaign-$STAMP.txt"

"$ROOT/tools/seed_rem_parity_fuzz_corpora.sh"

cargo +nightly fuzz check

{
  echo "rem-parity fuzz campaign — $STAMP"
  echo "seconds/target=$SECONDS_PER_TARGET rss_limit_mb=$RSS_LIMIT_MB case_timeout_s=$CASE_TIMEOUT_S"
} | tee "$REPORT"

for target in "${targets[@]}"; do
  dict="$ROOT/fuzz/fuzz_targets/$target.dict"
  [[ -f "$dict" ]] || { echo "missing dictionary: $dict" >&2; exit 1; }

  echo "=== $target: campaign ===" | tee -a "$REPORT"
  cargo +nightly fuzz run "$target" -- \
    -max_total_time="$SECONDS_PER_TARGET" \
    -rss_limit_mb="$RSS_LIMIT_MB" \
    -timeout="$CASE_TIMEOUT_S" \
    -print_final_stats=1 \
    -verbosity="$VERBOSITY" \
    -dict="$dict" 2>&1 | tail -25 | tee -a "$REPORT"

  echo "=== $target: corpus replay (final coverage) ===" | tee -a "$REPORT"
  cargo +nightly fuzz run "$target" -- \
    -runs=0 \
    -print_final_stats=1 \
    -dict="$dict" 2>&1 | tail -15 | tee -a "$REPORT"

  echo "corpus_files=$(ls "$ROOT/fuzz/corpus/$target" 2>/dev/null | wc -l)" | tee -a "$REPORT"
done

echo "report: $REPORT"
