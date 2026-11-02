#!/usr/bin/env bash
# Build a committed owner kit, or a clearly marked local dry package for review.
# Usage: package-owner-run.sh [--dry-run] OUT
set -euo pipefail
dry=0
if [[ ${1:-} == --dry-run ]]; then dry=1; shift; fi
[[ $# == 1 ]] || { echo 'usage: package-owner-run.sh [--dry-run] OUT' >&2; exit 2; }
out=$1
repo="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)"
cd "$repo"
if (( ! dry )) && [[ -n $(git status --porcelain) ]]; then
  echo 'refused: owner package must match a clean commit; use --dry-run for uncommitted review' >&2
  exit 1
fi
commit=$(git rev-parse HEAD)
name="remfield-run-${commit:0:8}"
(( ! dry )) || name+="-dry-run"
mkdir -p "$out"
out=$(cd "$out" && pwd -P)
stage=$(mktemp -d "$out/.stage.XXXXXX")
trap 'rm -rf -- "$stage"' EXIT
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$repo/target-step3}"
if (( dry )); then
  bin="$CARGO_TARGET_DIR/release"
else
  target=x86_64-unknown-linux-musl
  cargo build --locked --release --target "$target" -p remanence-cli -p remanence-daemon --features remanence-daemon/field-kit --bin rem --bin rem-debug --bin rem-daemon
  cargo build --locked --release --target "$target" --manifest-path fieldtest/tools/remfield-io/Cargo.toml
  bin="$CARGO_TARGET_DIR/$target/release"
fi
pkg="$stage/$name"
mkdir -p "$pkg/bin"
for binary in rem rem-debug rem-daemon remfield-io; do cp "$bin/$binary" "$pkg/bin/"; done
if (( dry )); then
  cp -a fieldtest/scripts fieldtest/plan fieldtest/run.sh fieldtest/OWNER.md fieldtest/RUNBOOK.md "$pkg/"
  printf '%s\n' 'Review artifact only. Not approved for hardware.' > "$pkg/DRY-RUN-NOT-FOR-HARDWARE"
  git diff --binary > "$pkg/source.diff"
else
  git archive HEAD fieldtest/scripts fieldtest/plan fieldtest/run.sh fieldtest/OWNER.md fieldtest/RUNBOOK.md | tar -x -C "$stage"
  mv "$stage/fieldtest/"* "$pkg/"
  rmdir "$stage/fieldtest"
fi
printf '%s\n' "$commit" > "$pkg/SOURCE-COMMIT"
printf '%s\n' "$commit" > "$pkg/KIT-COMMIT"
[[ -f "$pkg/plan/session-epoch-a.env" && -x "$pkg/scripts/41-session-closeout.sh" ]]
(cd "$pkg" && find . -type f ! -name SHA256SUMS -printf '%P\n' | LC_ALL=C sort | xargs -d '\n' sha256sum > SHA256SUMS)
tar -C "$stage" --owner=0 --group=0 -czf "$out/$name.tar.gz" "$name"
(cd "$out" && sha256sum "$name.tar.gz" > "$name.tar.gz.sha256")
cat "$out/$name.tar.gz.sha256"
