<!-- code-anchor: Cargo.toml crates/remanence-cli/Cargo.toml .github/workflows/release.yml @ 625f7044 -->
# Installing Remanence

This page is for an operator who wants the Remanence binaries on a Linux host
and the host ready for tape hardware. Install the 0.2.0 release archive or
build from source, then copy four programs into place. See
[Releases and versions](status.md#releases-and-versions) for the release history.
The host preparation that follows is the same for both routes.

## Before you start

The release archive needs a Linux x86-64 host, `tar` and `sha256sum`.
For the source route, use a Linux host with:

- `git` and a C compiler (`build-essential` on Debian and Ubuntu, `gcc` on
  Fedora), which the bundled SQLite library and the TLS library need;
- Rust 1.89 or newer, installed with [rustup](https://rustup.rs).

For a source build, the protobuf definitions of the daemon's API are compiled
in-process, so `protoc` is not needed. The build needs no other system packages
unless you add the optional feature described under
[Optional features](#optional-features).

## Install the release

Download `remanence-0.2.0-x86_64-unknown-linux-musl.tar.gz` and `SHA256SUMS`
from the [v0.2.0 GitHub release](https://github.com/archivetechie/remanence/releases/tag/v0.2.0).
Check the download as [Release verification](guide-release-verification.md#check-a-binary-release)
describes, downloading the other checksum-listed files for the complete check.
After verification, extract the archive in an empty directory and enter it:

```sh
mkdir remanence-0.2.0
cd remanence-0.2.0
tar -xzf ../remanence-0.2.0-x86_64-unknown-linux-musl.tar.gz
```

The four programs are in `bin/`. Continue at [Install the binaries](#install-the-binaries).
They are statically linked; this route needs no Rust toolchain or C compiler.

## Build from source

For the source route, clone the repository, check out the release tag and build:

```sh
git clone https://github.com/archivetechie/remanence
cd remanence
git checkout v0.2.0
cargo build --release --locked
```

`--locked` makes Cargo use the exact dependency versions recorded in
`Cargo.lock`, which are the versions CI tests. The first build downloads and
compiles every dependency, so it takes a while. Its last line begins
``Finished `release` profile [optimized]``.

The build produces four programs in `target/release/`:

| Program | What it is |
|---|---|
| `rem` | The operator command-line tool: archive objects on local files, a standalone drive, or a library through the daemon. |
| `rem-debug` | The break-glass tool, with wider direct access to drives and changers. |
| `rem-daemon` | The service that runs a library and serves the gRPC API. |
| `rem-recover` | A recovery tool that decrypts and extracts one encrypted object with no daemon, catalog or configuration file. |

To check the build:

```sh
target/release/rem --version
```

It prints the source version:

```text
rem 0.2.0
```

The workspace also defines `rem-dev`, a development tool for review vectors and
scratch-tape drills. Operators do not need it, and a plain build skips it
because it needs the `fixtures` feature. To build it, name the feature:

```sh
cargo build -p remanence-cli --features fixtures --bin rem-dev
```

[Development tools](development-tools.md) describes what it does.

## Install the binaries

Choose the directory for your route, from the extracted archive directory or
the source checkout respectively, then copy the four programs into `/usr/local/bin`:

```sh
binary_dir=bin                # extracted release archive
# binary_dir=target/release  # source build with cargo build --release
# binary_dir=target/x86_64-unknown-linux-musl/release  # static source build below
sudo install -m 0755 "$binary_dir/rem" "$binary_dir/rem-debug" \
    "$binary_dir/rem-daemon" "$binary_dir/rem-recover" /usr/local/bin/
rem --version
```

`rem --version` prints `rem 0.2.0`. `rem-debug --version` and
`rem-daemon --version` print the same version. `rem-recover` has no version
flag; `rem-recover --help` confirms that it runs.

The binaries are installed with `sudo` so that root owns them. Two of them
receive a capability when you
[prepare the host](#prepare-the-host-for-tape-hardware), and a binary that
carries a capability should be writable only by root.

To use another prefix, install into its `bin` directory and add that directory
to your `PATH`:

```sh
prefix=/opt/remanence
sudo install -d "$prefix/bin"
sudo install -m 0755 "$binary_dir/rem" "$binary_dir/rem-debug" \
    "$binary_dir/rem-daemon" "$binary_dir/rem-recover" "$prefix/bin/"
export PATH="$prefix/bin:$PATH"
```

Add the `export` line to your shell profile to keep it. Choose a prefix on a
filesystem that is not mounted `nosuid`. Linux ignores file capabilities there,
and the tape commands then fail with the permission errors described in
[troubleshooting](guide-troubleshooting.md). `findmnt -no OPTIONS -T "$prefix"`
shows the mount options.

### Static binaries for a host with an older C library

The binaries from `cargo build --release` are linked against the build host's
GNU C library, and they run on hosts with the same or a newer version. For a
host with an older C library, build statically linked binaries for the musl
target instead. They load no shared library from the host, so the host's C
library version does not matter. Run `tools/release_build.sh` from the repository
root to use the release workflow's own build. It remaps the Cargo, checkout and
Rust standard-library source paths and prints the four binaries' SHA-256 digests.

```sh
rustup target add x86_64-unknown-linux-musl
sudo apt install musl-tools file   # Debian and Ubuntu: provides musl-gcc and file
tools/release_build.sh
```

The binaries are then in `target/x86_64-unknown-linux-musl/release/`.

### Installing with cargo install

`cargo install --locked --path crates/remanence-cli --bin rem --bin rem-debug`
installs `rem` and `rem-debug` into `~/.cargo/bin`. Repeat it with
`crates/remanence-daemon` and `crates/rem-recover` for the other two. That is
convenient for the archive commands, which need no hardware. For tape
hardware, prefer the root-owned copy above. The capability described below
belongs on a binary that only root can replace, and home directories are
sometimes mounted `nosuid`.

### Optional features

One optional Cargo feature adds `rem watch`, which reports hot-plug events for
tape drives and changers. It needs the `pkg-config` and `libudev-dev` system
packages:

```sh
cargo build --release --locked --features remanence-cli/linux-udev
```

Without the feature, `rem watch` exits with
`` error: `rem watch` requires the `linux-udev` build feature. ``

## The runtime requirement: bsdtar

`rem archive build` and `rem archive extract` need `bsdtar` (the
`libarchive-tools` package on Debian and Ubuntu, `bsdtar` on Fedora) for
objects that pack files into a `.remwrap.tar` wrapper. `rem archive extract`
checks for it before it writes anything, and range and blob-member extracts
never need it. The [CLI reference](reference-cli.md) describes exactly when it
is used. Nothing else is needed at run time, and the test suite needs `bsdtar`
as well.

<!-- code-anchor: crates/remanence-scsi/src/error.rs crates/remanence-cli/src/lib.rs crates/remanence-cli/src/standalone.rs crates/remanence-api/src/read_core.rs @ 625f7044 -->
## Prepare the host for tape hardware

Commands that talk to a drive or a changer need four things from the host.
None of them is needed for the archive commands that work on local files.

### The tape group and CAP_SYS_RAWIO

Two separate gates must both be open. Your account needs the `tape` group,
which lets it open `/dev/sgN`. The binary needs `CAP_SYS_RAWIO`, because the
kernel's SCSI command filter refuses most tape and changer commands from
callers without it, even when the device node is readable. `setcap` and
`getcap` come from the `libcap2-bin` package on Debian and Ubuntu and from
`libcap` on Fedora and RHEL.

```sh
sudo usermod -aG tape "$USER"
sudo setcap cap_sys_rawio+ep /usr/local/bin/rem
sudo setcap cap_sys_rawio+ep /usr/local/bin/rem-debug
getcap /usr/local/bin/rem /usr/local/bin/rem-debug
```

`getcap` prints one line for each binary that carries the capability:

```text
/usr/local/bin/rem cap_sys_rawio=ep
/usr/local/bin/rem-debug cap_sys_rawio=ep
```

Log out and back in after joining the group. Never run `rem` with `sudo`; use
your normal account with the group and the capability.

A file capability is stored on the file itself. Installing a new build replaces
the file and removes the capability, so repeat the `setcap` commands after
every rebuild or upgrade.

The daemon needs the same two gates. Under systemd, grant the capability in the
unit rather than on the binary, and run the unit as an account in the `tape`
group:

```ini
[Service]
AmbientCapabilities=CAP_SYS_RAWIO
CapabilityBoundingSet=CAP_SYS_RAWIO
```

To run `rem-daemon` in the foreground instead, apply the same `setcap` to
`/usr/local/bin/rem-daemon`. `rem-recover` reads object files and never opens a
device, so it needs neither the group nor the capability.

### The lock directory

During direct drive operations, `rem` holds a lock file for each drive serial
in `/run/lock/rem`, so that two processes never drive the same drive at once.
The directory must exist and be writable by the `tape` group:

```sh
sudo install -d -o root -g tape -m 0770 /run/lock/rem
```

`/run` is emptied at every boot. To have the system recreate the directory, put
this line in `/etc/tmpfiles.d/rem.conf`:

```text
d /run/lock/rem 0770 root tape -
```

### The memory-lock limit

Reads lock their minimum buffers into RAM with `mlock`, so that tape data is
not swapped out while a stream is running. The minimum is
`staging_ring_buffers × read_batch_blocks × block_size`, which is 16 MiB with
the default settings and 256 KiB blocks. When the soft `RLIMIT_MEMLOCK` limit
is smaller, reads continue with unlocked buffers and print a warning that
begins `read reservoir uses unlocked buffers`. The data is still correct; only
the protection against swapping is lost.

`ulimit -l` shows the current soft limit in KiB, and `rem doctor` reports the
limit beside the minimum your configuration needs. To raise it for an
interactive account, add an entry to a file such as
`/etc/security/limits.d/rem.conf`, then log in again:

```text
<user> soft memlock unlimited
<user> hard memlock unlimited
```

A systemd service does not read `limits.d`. For the daemon, set `LimitMEMLOCK=`
in the unit to at least `daemon.io_memory_ceiling`, which is 24 GiB by default:

```ini
[Service]
LimitMEMLOCK=24G
```

The [configuration reference](reference-configuration.md#io-memory-ceiling-deployment-note)
explains the ceiling and the matching `MemoryMax` setting.

The first three are required for tape work. The memory-lock limit affects
whether read buffers can be swapped out, not whether the data is correct.

## Check the host

On a host with a tape drive, `rem doctor` checks the tape group, the
capability on the `rem` binary it runs from, the lock directory, the state
directory, whether the state directory is on a filesystem it trusts for
durable writes, and the memory-lock limit. Its `journal volume` check requires
a local ext3, ext4, XFS, btrfs, ZFS, f2fs or bcachefs filesystem with write
barriers on and, on ZFS, flush switches set to `0`. ZFS also gets an advisory
`journal sync` row warning about `sync=disabled`. It then looks for drives.
Each failed check names its fix. The
[standalone-drive guide](guide-standalone-drive.md#1-set-up-the-drive) shows
it in use.

Remanence makes its records durable with fsync, as a database does; it cannot test
whether a disk keeps what it confirms. **Torn-tail READ refusal.** A power cut during
a tape write can leave a torn tail. If `rem put` prints a sentence beginning `If the
drive lost power during a write,`, run the inspection command it names. Its diagnostic
begins `append suffix at LBA` or `tape identity invalidated`. An initial
`supersede-tail` inspection changes nothing on tape and exits 1; discarding this tail
is not available in this release, including with `--confirm-supersede`. Earlier checks
can refuse before inspection. A newer state directory does not repair this READ
failure; another write that meets it refuses again. The tail refusal itself leaves
earlier files available for recovery. Try `rem tape restore --scan --out rescued` on a
standalone drive, with `--private-key` for encrypted files. Discovery precedes
extraction and can stop on another error, such as `03/11/00` while measuring
filemarks. Where read admission permits, try `rem get` for known committed files. An
incomplete candidate makes extraction exit 1; exit 0 covers only the candidates
discovered. Check the report and rescued files before [`rem tape
retire`](reference-cli.md#rem-tape-retire), whose conditions still apply. To write
elsewhere, use another eligible tape; the
[standalone](reference-cli.md#rem-put-standalone) and
[library](reference-cli.md#rem-put-library) reference sections give the drive, guard
and endpoint options. **Lost journal records.** If a power cut loses the last journal
records, nothing on tape is lost or overwritten. The next write to that tape can stop
with `TailBeyondKnownState`. For journal-record loss, start from a newer state
directory that accounts for the data, or try recovery as described above. After
`TailBeyondKnownState`, discard is refused in this release if the tape log has a
latest commit and no open write session. If this state records no prior commit for the
tape, or still shows an open write session, you can preview the suffix and confirm
discard if the recovery checks pass. Keep a tape whose suffix cannot be discarded
closed for writing, or follow the retirement procedure; account for the whole suffix
before confirming any discard. A UPS, or a disk with power-loss protection, removes
this power-cut exposure.

Never edit `queue/write_cache` or mount with `nobarrier`, `barrier=0`, f2fs
`fsync_mode=nobarrier` or `checkpoint=disable` (including `checkpoint=disable:…`),
or bcachefs `journal_flush_disabled`. On ZFS keep `zfs_nocacheflush` and
`zil_nocacheflush` at `0` and the dataset's `sync=standard`.

## First run

Follow the [quickstart](guide-quickstart.md) from
[First archive, no tape required](guide-quickstart.md#first-archive-no-tape-required);
its build step is already done, and the archive walkthrough needs no hardware.
With one manually loaded drive, continue with
[Using a standalone LTO drive](guide-standalone-drive.md). With a changer
library, continue with
[Running the daemon](guide-quickstart.md#running-the-daemon-requires-hardware).

## When a binary release is published

0.2.0 is published on [GitHub Releases](https://github.com/archivetechie/remanence/releases/tag/v0.2.0).
The release workflow builds and publishes binaries when a release is tagged.
The release carries these files:

| File | Contents |
|---|---|
| `remanence-0.2.0-x86_64-unknown-linux-musl.tar.gz` | `bin/rem`, `bin/rem-debug`, `bin/rem-daemon` and `bin/rem-recover`; the `README.md`, `LICENSING.md`, `LICENSE-APACHE-2.0` and `THIRD-PARTY-LICENSES.html` files; and the `docs/` directory as it stood at the release. |
| `rem-recover-x86_64-linux-musl.tar.gz` | `rem-recover` alone, with its licenses, for keeping beside escrowed keys without a full installation. |
| `SHA256SUMS` | The SHA-256 digest of every other file in the release. |
| `release-manifest.json` and the other `.json` files | Build evidence: the source commit, the CI runs, the compiler and the artifact digests. |

The binaries are statically linked against the musl C library and built with
the default features, so `rem watch` is not in them. Before a release is
published, the workflow unpacks the archive on a clean machine and runs each
binary in an otherwise empty root directory. It also builds the four binaries
a second time in another directory, with a separate Cargo home, and stops the
release if any binary differs. After publishing, its `check-published` job
downloads the release and checks the checksums, attestations, tag signature and
source commit. [Release
verification](guide-release-verification.md#check-a-binary-release) explains how
to check the downloaded files yourself.

### Rebuilding a release to compare

Use Ubuntu 24.04 on x86-64; a fresh container such as `ubuntu:24.04` is the
simplest starting point. Use no Cargo configuration or environment of your own
that sets the compiler, linker, profile, target triple or target directory. Install rustup,
then install `musl-tools`, `file`, `git` and the C build tools from Ubuntu's
packages. Run the package commands as root in a container, omitting `sudo`.

Use the Rust toolchain named in the release's `build-remanence-linux.json`
`rustc` field, with the musl target. This workflow uses 1.95.0:

```sh
sudo apt-get update
sudo apt-get install -y build-essential musl-tools file git
rustup toolchain install 1.95.0 --target x86_64-unknown-linux-musl
git clone https://github.com/archivetechie/remanence
cd remanence
git checkout v<VERSION>
unset RUSTFLAGS CARGO_ENCODED_RUSTFLAGS REMANENCE_SOFTWARE_BUILD TARGET
RUSTUP_TOOLCHAIN=1.95.0 tools/release_build.sh
```

Replace `<VERSION>` with the release's version and `1.95.0` with its recorded
toolchain version if different. Build from a git clone with its tags, because
the build string includes `git describe --always --tags`; a clone without tags
gives a different string. The script refuses to run outside a git checkout's
root or with a non-blank `RUSTFLAGS`, `CARGO_ENCODED_RUSTFLAGS` or
`REMANENCE_SOFTWARE_BUILD`. The recipe unsets `TARGET` because a non-empty
value selects another target triple. The output directory is always `target/`.

Unpack the operator archive and run `sha256sum` on its `bin/*`. Compare those
four digests with the script's printed digests for `rem`, `rem-debug`,
`rem-daemon` and `rem-recover`.

A match depends on the same Rust toolchain and the same C compiler, linker
and musl. The latter come from Ubuntu 24.04's packages and can change between
package updates; bundled SQLite records the GCC version in the binary. The
manifest's build `runner` field names the runner image and its version, not
package versions. The release build's log prints the C compiler and linker
versions reported by the script, so compare those when the digests differ.
