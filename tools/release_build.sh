#!/usr/bin/env bash
# This script builds Remanence's static release binaries with remapped source paths.
# It checks static linkage and prints tool versions and digests for build comparisons.
set -euo pipefail

fail() {
  printf 'release_build.sh: %s\n' "$*" >&2
  exit 1
}

build_dir=$(pwd -P)
repo_root=$(git rev-parse --show-toplevel) || fail 'run from the repository root (git root is unavailable)'
[[ "$repo_root" == "$build_dir" ]] || fail 'run from the repository root, not a subdirectory'
[[ -f Cargo.lock ]] || fail 'the repository root must contain Cargo.lock'

for variable in RUSTFLAGS CARGO_ENCODED_RUSTFLAGS REMANENCE_SOFTWARE_BUILD; do
  value=${!variable-}
  [[ -z "${value//[[:space:]]/}" ]] || fail "$variable must be blank for a release build"
done

target=${TARGET:-x86_64-unknown-linux-musl}
cargo_home=${CARGO_HOME:-$HOME/.cargo}
mkdir -p "$cargo_home"
CARGO_HOME=$(cd "$cargo_home" && pwd -P)
export CARGO_HOME

sysroot=$(rustc --print sysroot)
[[ -n "$sysroot" ]] || fail 'rustc --print sysroot is empty'
rustc_version=$(rustc -vV)
commit_hash=$(sed -n 's/^commit-hash: //p' <<< "$rustc_version")
[[ -n "$commit_hash" ]] || fail 'rustc -vV has no commit-hash'
export CARGO_ENCODED_RUSTFLAGS="--remap-path-prefix=$CARGO_HOME=/cargo"$'\x1f'"--remap-path-prefix=$build_dir=/build"$'\x1f'"--remap-path-prefix=$sysroot/lib/rustlib/src/rust=/rustc/$commit_hash"

description=$(git describe --always --tags) || description='unavailable (git describe failed)'
printf 'Build: %s\n' "$description"
sed -n '/^release: /p' <<< "$rustc_version"
cc_version=$(cc --version)
printf '%s\n' "${cc_version%%$'\n'*}"
ld_version=$(ld --version)
printf '%s\n' "${ld_version%%$'\n'*}"

cargo build --release --locked --target "$target" --target-dir target \
  -p remanence-cli -p remanence-daemon -p rem-recover \
  --bin rem --bin rem-debug --bin rem-daemon --bin rem-recover
for bin in rem rem-debug rem-daemon rem-recover; do
  file "target/$target/release/$bin" | grep -E 'statically linked|static-pie linked'
done
for bin in rem rem-debug rem-daemon rem-recover; do
  sha256sum "target/$target/release/$bin"
done
