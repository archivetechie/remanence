# Plan C — build remanence from source, offline, on the RHEL 9 box

Only needed if BOTH `bin/` (RHEL9-built) and `bin/musl/` (static)
binaries fail to run (preflight tells you). Takes ~20 min, no internet.

Prerequisites (RHEL 9): `sudo dnf install gcc gcc-c++ make perl`
(usually already present on a build-capable host).

```bash
cd ~/remfield/toolchain

# 1. Install the standalone Rust toolchain (no rustup, no network)
tar xf rust-dist.tar.xz
(cd rust-1.83.0-x86_64-unknown-linux-gnu && sudo ./install.sh --prefix=$HOME/remfield/rust --disable-ldconfig)
export PATH=$HOME/remfield/rust/bin:$PATH

# 2. Unpack the source + vendored crates
tar xf remanence-src.tar.gz && cd remanence

# 3. Point cargo at the vendored sources (offline)
mkdir -p .cargo
cp ../vendor-config.toml .cargo/config.toml   # [source.crates-io] replace-with vendored
export PROTOC=$HOME/remfield/toolchain/protoc/bin/protoc

# 4. Build
cargo build --release --offline

# 5. Install into the kit
cp target/release/{rem,rem-daemon,rem-debug} ~/remfield/bin/
```

Then continue from `00-preflight.sh` as normal.
