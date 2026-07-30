#!/usr/bin/env bash
# Extended REM-PARITY 1.0 fuzz campaign (specification §18.3).
#
# The freeze campaign (run_rem_parity_fuzz_campaign.sh) runs each target
# sequentially in a single process. That satisfies "no panics, hangs, or
# unbounded allocations" but is weak evidence for the criterion's other half,
# "reaches a corpus plateau": a one-hour single-process run left the bootstrap
# corpus still growing — 93 new entries in a 45-second follow-up probe.
#
# This runner exists to answer the plateau question honestly. It runs all four
# targets concurrently, each with several independent processes sharing that
# target's corpus directory (libFuzzer RELOADs peers' finds), and records the
# corpus size before and after so growth is a measured number rather than an
# assertion.
#
# Deliberately NOT using libFuzzer's -jobs/-workers: those write fuzz-N.log
# into the working directory, and four concurrent targets would clobber each
# other's logs. Independent processes over a shared corpus dir get the same
# parallelism without the collision.
#
# Usage: run_rem_parity_fuzz_overnight.sh [seconds_per_target] [procs_per_target]

set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SECONDS_PER_TARGET="${1:-23400}"
PROCS_PER_TARGET="${2:-3}"
RSS_LIMIT_MB="${FUZZ_RSS_LIMIT_MB:-4096}"
CASE_TIMEOUT_S="${FUZZ_CASE_TIMEOUT_S:-30}"

# FUZZ_TARGETS narrows the run to a subset (space-separated). Used to give a
# single unsaturated target a focused campaign without re-running the three
# that already went flat.
if [[ -n "${FUZZ_TARGETS:-}" ]]; then
    read -r -a targets <<<"$FUZZ_TARGETS"
else
    targets=(
      rem_parity_bootstrap_parse
      rem_parity_bootstrap_structured
      rem_parity_sidecar_parse
      rem_parity_map_parse
      rem_parity_scan_walk
    )
fi

cd "$ROOT"

STAMP="$(date -u +%Y%m%dT%H%M%SZ)"
REPORT_DIR="$ROOT/fuzz/reports"
WORK_DIR="$REPORT_DIR/overnight-$STAMP"
mkdir -p "$WORK_DIR"
REPORT="$REPORT_DIR/rem-parity-campaign-overnight-$STAMP.txt"

corpus_count() { ls "$ROOT/fuzz/corpus/$1" 2>/dev/null | wc -l; }

{
  echo "rem-parity EXTENDED fuzz campaign — $STAMP"
  echo "seconds/target=$SECONDS_PER_TARGET procs/target=$PROCS_PER_TARGET"
  echo "rss_limit_mb=$RSS_LIMIT_MB case_timeout_s=$CASE_TIMEOUT_S"
  echo "targets run CONCURRENTLY; each target's processes share its corpus dir"
  echo
  echo "=== corpus BEFORE ==="
  for t in "${targets[@]}"; do echo "$t=$(corpus_count "$t")"; done
  echo
} | tee "$REPORT"

"$ROOT/tools/seed_rem_parity_fuzz_corpora.sh" >/dev/null 2>&1 || true

# Prebuild once so the concurrent runs do not contend on the cargo build lock.
cargo +nightly fuzz build >/dev/null 2>&1 || {
  echo "FATAL: cargo fuzz build failed" | tee -a "$REPORT"; exit 1; }

pids=()
for target in "${targets[@]}"; do
  dict="$ROOT/fuzz/fuzz_targets/$target.dict"
  [[ -f "$dict" ]] || { echo "missing dictionary: $dict" >&2; exit 1; }
  for i in $(seq 1 "$PROCS_PER_TARGET"); do
    cargo +nightly fuzz run "$target" -- \
      -max_total_time="$SECONDS_PER_TARGET" \
      -rss_limit_mb="$RSS_LIMIT_MB" \
      -timeout="$CASE_TIMEOUT_S" \
      -print_final_stats=1 \
      -dict="$dict" \
      > "$WORK_DIR/$target-p$i.log" 2>&1 &
    pids+=($!)
  done
done

echo "launched ${#pids[@]} fuzz processes at $(date -u +%H:%M:%SZ)" | tee -a "$REPORT"

failed=0
for pid in "${pids[@]}"; do
  wait "$pid" || failed=$((failed + 1))
done

{
  echo
  echo "processes exiting nonzero: $failed"
  echo "(a crash, hang, or OOM makes libFuzzer exit nonzero and leaves a"
  echo " reproducer under fuzz/artifacts/ — check there if this is not 0)"
  echo
  echo "=== corpus AFTER ==="
  for t in "${targets[@]}"; do echo "$t=$(corpus_count "$t")"; done
  echo
  echo "=== per-target final stats ==="
} | tee -a "$REPORT"

for target in "${targets[@]}"; do
  echo "--- $target ---" | tee -a "$REPORT"
  grep -hE "^stat::|^Done .* runs in" "$WORK_DIR/$target"-p*.log 2>/dev/null \
    | tee -a "$REPORT"
  echo "corpus_files=$(corpus_count "$target")" | tee -a "$REPORT"
done

{
  echo
  echo "=== artifacts (crashes/hangs/OOM reproducers) ==="
  if compgen -G "$ROOT/fuzz/artifacts/*/*" >/dev/null; then
    ls -la "$ROOT"/fuzz/artifacts/*/*
  else
    echo "none"
  fi
} | tee -a "$REPORT"

# Replay each final corpus for the coverage figure. This is a determinism
# check, not plateau evidence — the corpus BEFORE/AFTER delta above is what
# speaks to plateau.
echo | tee -a "$REPORT"
echo "=== corpus replay (final coverage; determinism check only) ===" | tee -a "$REPORT"
for target in "${targets[@]}"; do
  echo "--- $target ---" | tee -a "$REPORT"
  cargo +nightly fuzz run "$target" -- -runs=0 -print_final_stats=1 \
    -dict="$ROOT/fuzz/fuzz_targets/$target.dict" 2>&1 | tail -8 | tee -a "$REPORT"
done

rm -f "$ROOT"/fuzz-*.log
echo "report: $REPORT"
