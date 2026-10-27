# Installing Remanence

This page installs the Remanence binaries on a Linux host and prepares the
host for tape hardware. There are two ways to get the binaries. A release
archive needs no Rust toolchain: you download it, check it, and copy four
files into place. A source build needs Rust and a C compiler, and is the way
to get optional features or a revision that has not been released. The host
preparation that follows is the same for both.

## What a release contains

Releases after 0.2.0 carry these files on the
[releases page](https://github.com/archivetechie/remanence/releases):

| File | Contents |
|---|---|
| `remanence-<version>-x86_64-unknown-linux-musl.tar.gz` | `bin/rem`, `bin/rem-debug`, `bin/rem-daemon` and `bin/rem-recover`; the `README.md`, `LICENSING.md`, `LICENSE-APACHE-2.0` and `THIRD-PARTY-LICENSES.html` files; and the `docs/` directory as it stood at the release. |
| `rem-recover-x86_64-linux-musl.tar.gz` | `rem-recover` alone, with its licenses, for keeping beside escrowed keys without a full installation. |
| `SHA256SUMS` | The SHA-256 digest of every other file in the release. |
| `release-manifest.json` and the other `.json` files | Build evidence: the source commit, the CI runs, the compiler and the artifact digests. [Release verification](guide-release-verification.md) explains them. |

The binaries are statically linked against the musl C library. They load no
shared library from the host, so one archive runs on Debian, Ubuntu, RHEL and
other x86-64 Linux distributions regardless of the C library version each one
ships. Before a release is published, the release workflow unpacks the archive
on a clean machine and runs each binary in an empty root directory to confirm
this.

The release binaries are built without optional features. `rem watch`, which
needs `libudev`, is available only from a source build.

Release 0.2.0, the first release with binaries, publishes
`remanence-x86_64-linux.tar.gz` instead. Its binaries are linked against the
GNU C library of the build machine, sit at the top of the archive rather than
under `bin/`, and do not include standalone-drive mode.

## Install from a release archive

### Download and check

Set the version you want, then download the archive and the checksum list:

```sh
version=<version>
base="https://github.com/archivetechie/remanence/releases/download/v$version"
curl -fLO "$base/remanence-$version-x86_64-unknown-linux-musl.tar.gz"
curl -fLO "$base/SHA256SUMS"
sha256sum --ignore-missing -c SHA256SUMS
```

`SHA256SUMS` lists the whole release, so `--ignore-missing` limits the check
to the files you downloaded. The last command prints
`remanence-<version>-x86_64-unknown-linux-musl.tar.gz: OK`. Any other result
means the download is damaged or is not the published file; do not install it.

A matching checksum shows that the file is the one listed in `SHA256SUMS`.
To check that both were produced by this repository's release workflow, use
GitHub's attestation tooling:

```sh
gh attestation verify "remanence-$version-x86_64-unknown-linux-musl.tar.gz" \
    --repo archivetechie/remanence
```

### Unpack and copy into place

```sh
tar -xzf "remanence-$version-x86_64-unknown-linux-musl.tar.gz"
sudo install -m 0755 "remanence-$version-x86_64-unknown-linux-musl"/bin/* /usr/local/bin/
rem --version
```

`rem --version` prints `rem <version>`. `rem-debug --version` and
`rem-daemon --version` print the same version; `rem-recover` has no version
flag, and `rem-recover --help` confirms that it runs.

The binaries are installed with `sudo` so that root owns them. Two of them
receive a capability when you
[prepare the host](#prepare-the-host-for-tape-hardware), and a binary that
carries a capability should be writable only by root.

To use another prefix, install into its `bin` directory and add that
directory to your `PATH`:

```sh
prefix=/opt/remanence
sudo install -d "$prefix/bin"
sudo install -m 0755 "remanence-$version-x86_64-unknown-linux-musl"/bin/* "$prefix/bin/"
export PATH="$prefix/bin:$PATH"
```

Add the `export` line to your shell profile to keep it. Choose a prefix on a
filesystem that is not mounted `nosuid`: Linux ignores file capabilities
there, and the tape commands then fail with the permission errors described
in [troubleshooting](guide-troubleshooting.md). `findmnt -no OPTIONS -T
"$prefix"` shows the mount options.

The unpacked `docs/` directory is this documentation as of the release. Keep
it if the host has no network access.

## Install from source

You need Linux, a C compiler (`build-essential` on Debian/Ubuntu, `gcc` on
Fedora) and Rust 1.88 or newer (rustup). The protobuf definitions are compiled
in-process, so `protoc` is not needed.

```sh
git clone https://github.com/archivetechie/remanence
cd remanence
cargo build --release --locked
sudo install -m 0755 target/release/rem target/release/rem-debug \
    target/release/rem-daemon target/release/rem-recover /usr/local/bin/
```

These binaries are linked against the build host's C library and run on hosts
with the same or a newer version. To build the static binaries that a release
ships, for example to install on a host with an older C library, add the musl
target first:

```sh
rustup target add x86_64-unknown-linux-musl
sudo apt install musl-tools        # Debian/Ubuntu: provides musl-gcc
cargo build --release --locked --target x86_64-unknown-linux-musl \
    -p remanence-cli -p remanence-daemon -p rem-recover
```

The binaries are then in `target/x86_64-unknown-linux-musl/release/`.

`cargo install --locked --path crates/remanence-cli` installs `rem` and
`rem-debug` into `~/.cargo/bin` (repeat with `crates/remanence-daemon` and
`crates/rem-recover` for the others). That is convenient for the archive
commands, which need no hardware. For tape hardware, prefer the root-owned
copy above, because the capability described below belongs on a binary that
only root can replace, and because home directories are sometimes
mounted `nosuid`.

`--features remanence-cli/linux-udev` adds `rem watch` and needs the
`pkg-config` and `libudev-dev` system packages.

## Runtime requirement

`rem archive build` and `rem archive extract` need `bsdtar` (the
`libarchive-tools` package on Debian and Ubuntu, `bsdtar` on Fedora) for
objects that pack files into a `.remwrap.tar` wrapper. The
[quickstart](guide-quickstart.md#build) describes exactly when. Nothing else
is needed at run time.

<!-- code-anchor: crates/remanence-scsi/src/error.rs crates/remanence-cli/src/lib.rs crates/remanence-cli/src/standalone.rs crates/remanence-api/src/mount_backend.rs crates/remanence-api/src/read_core.rs @ 638210f7 -->
## Prepare the host for tape hardware

Commands that talk to a drive or a changer need four things from the host.
None of them is needed for the archive commands that work on local files.

### The tape group and CAP_SYS_RAWIO

Two separate gates must both be open. Your account needs the `tape` group,
which allows it to open `/dev/sgN`. The binary needs `CAP_SYS_RAWIO`, because
the kernel's SCSI command filter refuses most tape and changer commands from
callers without it, even when the device node is readable. `setcap` and
`getcap` come from the `libcap2-bin` package on Debian and Ubuntu and
`libcap` on Fedora and RHEL.

```sh
sudo usermod -aG tape "$USER"
sudo setcap cap_sys_rawio+ep /usr/local/bin/rem
sudo setcap cap_sys_rawio+ep /usr/local/bin/rem-debug
getcap /usr/local/bin/rem /usr/local/bin/rem-debug
```

Log out and back in after joining the group. Never run `rem` with `sudo`; use
your normal account with the group and the capability.

A file capability is stored on the file itself. Installing a new version
replaces the file and removes the capability, so repeat the `setcap` commands
after every upgrade.

The daemon needs the same two gates. Under systemd, grant the capability in
the unit rather than on the binary, and run the unit as an account in the
`tape` group:

```ini
[Service]
AmbientCapabilities=CAP_SYS_RAWIO
CapabilityBoundingSet=CAP_SYS_RAWIO
```

To run `rem-daemon` in the foreground instead, apply the same `setcap` to
`/usr/local/bin/rem-daemon`. `rem-recover` reads object files and never opens
a device, so it needs neither the group nor the capability.

### The lock directory

During direct drive operations, `rem` holds a lock file for each drive serial
in `/run/lock/rem`, so that two processes never drive the same drive at once.
The directory must exist and be writable by the `tape` group:

```sh
sudo install -d -o root -g tape -m 0770 /run/lock/rem
```

`/run` is emptied at every boot. To have the system recreate the directory,
put this line in `/etc/tmpfiles.d/rem.conf`:

```text
d /run/lock/rem 0770 root tape -
```

### The memory-lock limit

Reads lock their minimum buffers into RAM with `mlock`, so that tape data is
not swapped out while a stream is running. The minimum is
`staging_ring_buffers × read_batch_blocks × block_size`, which is 16 MiB with
the default settings and 256 KiB blocks. When the soft `RLIMIT_MEMLOCK` limit
is smaller, reads continue with unlocked buffers and print
`read reservoir uses unlocked buffers`. The data is still correct; only the
protection against swapping is lost.

`ulimit -l` shows the current soft limit in KiB, and `rem doctor` reports the
limit beside the minimum your configuration needs. To raise it for an
interactive account, add an entry to a file such as
`/etc/security/limits.d/rem.conf`, then log in again:

```text
<user> soft memlock unlimited
<user> hard memlock unlimited
```

A systemd service does not read `limits.d`. For the daemon, set
`LimitMEMLOCK=` in the unit to at least `daemon.io_memory_ceiling`, which is
24 GiB by default:

```ini
[Service]
LimitMEMLOCK=24G
```

The [configuration reference](reference-configuration.md#io-memory-ceiling-deployment-note)
explains the ceiling and the matching `MemoryMax` setting.

In plain terms: the group lets your account open the device, the capability
lets the program send the commands the kernel otherwise filters, the lock
directory keeps two programs off one drive, and the memory-lock limit lets
reads keep their buffers in RAM. The first three are required for tape work.
The fourth affects throughput and not correctness.

## First run

Follow the [quickstart](guide-quickstart.md) from
[First archive, no tape required](guide-quickstart.md#first-archive-no-tape-required);
its Build section is already done, and the archive walkthrough needs no
hardware. On a host with a tape drive, the quickstart runs `rem setup` and then
`rem doctor`, which checks the tape group, the capability, the lock directory,
the state directory and the memory-lock limit, and names the fix for each
failure. With one manually loaded drive, continue
with [Using a standalone LTO drive](guide-standalone-drive.md). With a
changer library, continue with
[Running the daemon](guide-quickstart.md#running-the-daemon-requires-hardware).
