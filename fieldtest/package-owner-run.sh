#!/usr/bin/env bash
# Build the owner-run field package from the committed tree:
#
#   fieldtest/package-owner-run.sh <out-dir>
#
# Writes <out-dir>/remfield-run-<commit>.tar.gz and its .sha256. The package holds
# static musl binaries, the kit scripts, the plans, run.sh, OWNER.md, RUNBOOK.md
# and KIT-COMMIT under one top-level directory remfield-run-<commit>/.
set -euo pipefail

out="${1:?usage: package-owner-run.sh <out-dir>}"
repo="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)"
cd "$repo"
if [[ -n "$(git status --porcelain)" ]]; then
  echo "refused: the working tree has uncommitted changes; the package must match a commit" >&2
  exit 1
fi
commit="$(git rev-parse --short=8 HEAD)"
name="remfield-run-$commit"
mkdir -p "$out"
out="$(cd -- "$out" && pwd -P)"
stage="$(mktemp -d "$out/.stage.XXXXXX")"
trap 'rm -rf -- "$stage"' EXIT

target=x86_64-unknown-linux-musl
cargo build --release --target "$target" -p remanence-cli -p remanence-daemon --bin rem --bin rem-debug --bin rem-daemon
cargo build --release --target "$target" --manifest-path fieldtest/tools/remfield-io/Cargo.toml

pkg="$stage/$name"
mkdir -p "$pkg/bin"
cp "target/$target/release/rem" "target/$target/release/rem-debug" "target/$target/release/rem-daemon" "$pkg/bin/"
cp "fieldtest/tools/remfield-io/target/$target/release/remfield-io" "$pkg/bin/"
git archive HEAD fieldtest/scripts fieldtest/plan fieldtest/run.sh fieldtest/OWNER.md fieldtest/RUNBOOK.md |
  tar -x -C "$stage"
mv "$stage/fieldtest/"* "$pkg/"
rmdir "$stage/fieldtest"
printf '%s\n' "$commit" >"$pkg/KIT-COMMIT"
(cd "$pkg" && find . -type f ! -name SHA256SUMS -printf '%P\n' | LC_ALL=C sort | xargs -d '\n' sha256sum >SHA256SUMS)

tar -C "$stage" --owner=0 --group=0 -czf "$out/$name.tar.gz" "$name"
(cd "$out" && sha256sum "$name.tar.gz" >"$name.tar.gz.sha256")
cat "$out/$name.tar.gz.sha256"
