# INSTALL.md — How QuadStor VTL was installed on akash

Detailed, auditable trail of every command run during Step 1 of the Remanence plan.
This is meant to let owner (or anyone else) understand exactly what was changed on the
host, and reproduce it on another machine, or roll it back.

**Host:** akash
**OS:** Ubuntu (kernel 6.8.0-106-generic, x86_64)
**Date:** 2026-05-16
**Driver:** Claude (Opus 4.7), instructed to drive end-to-end

---

## 0. Pre-install reconnaissance (no system changes)

Before installing anything, captured the initial state of the host:

```bash
hostname                                # → akash
uname -r                                # → 6.8.0-106-generic
ls /lib/modules/$(uname -r)/build       # → missing (kernel headers absent)
which sg_inq mtx lsscsi                 # → only sg_inq present (sg3-utils)
dpkg -l apache2 postgresql              # → neither installed
ls /var/www/                            # → does not exist (clean Apache slate)
ss -ltn | grep -E ':(80|443|9985)\b'    # → nothing listening
getent passwd www-data                  # → exists (id 33)
ls /dev/sg*                             # → none (no SCSI generic devices yet)
```

Inspected the .deb to know exactly what we're installing:

```bash
dpkg-deb --info /home/user/quadstor/quadstor-vtl-ext-3.0.79.32-debian12-x86_64.deb
dpkg-deb --contents /home/user/quadstor/quadstor-vtl-ext-*.deb | wc -l   # 2769 entries
dpkg-deb --control /home/user/quadstor/quadstor-vtl-ext-*.deb /tmp/quadstor-extract
cat /tmp/quadstor-extract/{preinst,postinst,prerm}
```

Key findings from the maintainer scripts that justify the order of operations below:

1. **preinst aborts if `/lib/modules/$(uname -r)/build/Makefile` is missing.**
   So `linux-headers-$(uname -r)` must be installed first — and it is **not** declared
   as a Debian dependency.
2. **postinst calls `a2enmod cgi`** — requires Apache 2 installed first (also not
   a declared dependency).
3. postinst overwrites `/var/www/html/index.html` with QuadStor's `vtindex.html`.
   On akash this is fine because `/var/www/` did not exist before this install.
4. The package declares `build-essential`, `postgresql`, `sg3-utils`,
   `uuid-runtime`, `psmisc`, `gzip`, `xz-utils`, `libpq-dev` as deps —
   these get pulled by `apt`.
5. The package builds its own kernel modules in postinst via `/quadstorvtl/bin/builditfusr`,
   writing a build log to `/quadstorvtl/tmp/build.log`. We tail this if anything fails.

## 1. Install manual prerequisites

The official QuadStor doc (https://www.quadstor.com/vtlsupport/145-installation-on-rhel-centos-sles-debian.html)
lists this exact set for Debian — using the same list, plus `mtx` and `lsscsi` for plan verification:

```bash
sudo apt-get update
sudo apt-get install -y \
    uuid-runtime build-essential sg3-utils apache2 \
    gzip xz-utils postgresql libpq-dev psmisc \
    linux-headers-$(uname -r) \
    mtx lsscsi
```

- The QuadStor-doc set covers all .deb dependencies + Apache + kernel headers.
- `mtx` and `lsscsi` are extra, for plan Step 1 verification (`mtx -f /dev/sgN status`, `lsscsi -g`).
- We skip `firmware-qlogic` — only needed for Fibre Channel HBA access, not for the
  virtual library we're configuring (which exposes virtual SCSI generic nodes, not FC).

## 2. Install the .deb

```bash
sudo dpkg -i /home/user/quadstor/quadstor-vtl-ext-3.0.79.32-debian12-x86_64.deb
# If apt-declared deps (postgresql, libpq-dev, etc.) aren't satisfied:
sudo apt-get -f install -y
```

Watch points:
- preinst: prints `Adding group vtprocgrp` and adds `www-data` to it.
- postinst: prints "Performing post install. Please wait...",
  initializes `/quadstorvtl/pgsqlsys/data` via `pgpost.sh`,
  enables the systemd unit (`systemctl enable quadstorvtl`),
  runs `a2enmod cgi`, then runs `/quadstorvtl/bin/builditfusr`
  to build kernel modules. The build log lives at `/quadstorvtl/tmp/build.log`.
- Ends with `Installation complete. A system reboot is required to start the VTL service`.

## 3. Start (or reboot to start) the service

```bash
# Try without reboot first — the modules should be buildable without one
# since DKMS / module build happens in postinst, not at boot
sudo systemctl daemon-reload
sudo systemctl start quadstorvtl
sudo systemctl status quadstorvtl
lsmod | grep -E 'vtlitf|coredev|ldev|netvtl'  # expect QuadStor modules loaded
```

If the modules can't be loaded into the running kernel for any reason, reboot:

```bash
sudo reboot
```

## 4. Verify

```bash
sudo systemctl status quadstorvtl       # should be active
ss -ltn | grep -E ':(80|9985)\b'        # apache on 80, qs pg on 9985
curl -s http://127.0.0.1/quadstorvtl/   # web UI reachable
lsmod | grep -E 'vtlitf|coredev|ldev'
```

Once a virtual library is configured (Step 5), `/dev/sg*` nodes appear and:

```bash
lsscsi -g
sg_inq -v /dev/sgN
sudo mtx -f /dev/sg_changer status
```

## 5. Rollback

If we need to undo the install:

```bash
sudo systemctl stop quadstorvtl
sudo dpkg -r quadstor-vtl-ext
# prerm runs qlauninst which uninstalls modules and removes systemd unit
sudo apt-get autoremove   # removes unused deps like libpq-dev
sudo groupdel vtprocgrp   # cleanup
```

This does NOT remove `/quadstorvtl/pgsqlsys/data` (the bundled postgres). To purge it:

```bash
sudo rm -rf /quadstorvtl /var/www/html/quadstorvtl /var/www/html/vtindex.html
```

---

## Execution log — what actually ran

### 2026-05-16 (post-reboot state, kernel 6.8.0-111-generic)

```bash
sudo apt-get update
sudo apt-get install -y uuid-runtime build-essential sg3-utils apache2 \
    gzip xz-utils postgresql libpq-dev psmisc \
    linux-headers-$(uname -r) mtx lsscsi
```

Result: success. apache2 + postgresql now running on akash. The apt run also
pulled in `linux-image-6.8.0-111-generic` as part of the kernel upgrade chain
that was already pending. owner rebooted; akash now runs 6.8.0-111-generic and
headers for that kernel are installed.

```bash
sudo dpkg -i /home/user/quadstor/quadstor-vtl-ext-3.0.79.32-debian12-x86_64.deb
```

Result: **postinst FAILED at kernel module build.** Full log at
`/quadstorvtl/tmp/build.log`. Two distinct issues:

1. **Real kernel API breakage** (the blocker):
   - `core_itf.c:1866` calls `blkdev_put()` — removed in kernel 6.5.
   - `core_itf.c:2458` calls `blkdev_get_by_path()` — removed in kernel 6.5.
     Replacements are `bdev_release()` and `bdev_open_by_path()` (which now
     returns `struct bdev_handle *` instead of `struct block_device *`).
   The .deb is labeled `debian12` — Debian 12 shipped kernel 6.1, before the
   removal. QuadStor's 3.0.79.32 source predates kernel 6.5.
2. **FORTIFY_SOURCE false positives** (secondary, would have been the next
   wall after fixing #1): `memset(raw, 0, sizeof(*raw) + sizeof(*req))` where
   `raw` was just allocated with extra trailing bytes — the FORTIFY checker
   in 6.x can't see through `node_msg_alloc`. Fix would be either an explicit
   `void *` cast or `EXTRA_CFLAGS += -Wno-error=attribute-warning` in
   `/quadstorvtl/src/export/Makefile`.

Package is left **installed but non-functional** — the dpkg state is "installed",
the systemd unit `quadstorvtl.service` is enabled, but no .ko's exist under
`/quadstorvtl/lib/modules/$(uname -r)/`. The service will refuse to start until
modules are built. This is recoverable; cleanup procedure is in section 5.

### Decision needed

Three viable paths from here. No further system changes until owner picks one.

**Option A — Patch QuadStor source for kernel 6.8 APIs.**
- Scope: replace `blkdev_get_by_path()` → `bdev_open_by_path()` (returns `struct bdev_handle *`),
  replace `blkdev_put()` → `bdev_release()`, update the `iodev_t` typedef to hold a
  `bdev_handle *` and adjust accessors that touch the underlying `block_device`. Then
  drop `-Wno-error=attribute-warning` in `/quadstorvtl/src/export/Makefile` to defang the
  FORTIFY false positives.
- Likely 20-100 lines of changes across 2-4 files. Risk: more 6.x API gaps could surface
  once these are fixed (we only see the first compile error).
- Pro: keeps akash on its current kernel; teaches kernel-module porting.
- Con: real time sink, possibly multi-evening if more breakage surfaces.

**Option B — Drop akash to an older kernel that has the old API.**
- `blkdev_put` / `blkdev_get_by_path` were removed in Linux 6.5. Last kernel with them is 6.4.
- Ubuntu 24.04 (akash's distro) doesn't ship anything older than 6.8 in its archives. Path
  is install `linux-image-5.15.0-XXX-generic` from Ubuntu 22.04 jammy (definitely has the
  old API), pin GRUB to it, reboot.
- Pro: fastest unblock; QuadStor postinst rebuilds against 5.15 cleanly.
- Con: akash spends its life on an older kernel; future security updates regress; risky
  on a host that does other sysadmin work.

**Option C — Skip QuadStor; build Layer 1 against captured-fixture tests only.**
- Lean on plan Step 2 (real MSL3040 fixtures) for the test corpus. Develop SCSI parsers
  TDD-style without a live target. Validate end-to-end when an access window opens.
- Pro: no kernel-downgrade or porting detour; matches the plan's own "highest leverage:
  capture real fixtures" framing.
- Con: defers integration testing; can't write live commands until fixtures + real hw.

A possible **Option D** (try `mhvtl`, a userspace alternative VTL) exists but I haven't
verified mhvtl compiles on 6.8 either — it has its own kernel module. Could be the
same story; would need a 30-min probe.

Recommendation: **C if the next MSL3040 access window is imminent; A if it isn't.** B
trades a sysadmin headache for unblocking the dev environment — likely worth it only if
you'd otherwise be blocked for weeks.

### Cleanup if abandoning QuadStor entirely

```bash
sudo systemctl disable --now quadstorvtl 2>/dev/null
sudo dpkg -r quadstor-vtl-ext   # runs prerm (qlauninst); see section 5
sudo apt-get autoremove
sudo groupdel vtprocgrp
sudo rm -rf /quadstorvtl /var/www/html/quadstorvtl /var/www/html/vtindex.html /var/www/html/index.html
```

---

## Resolution — Option A applied: patched QuadStor source for kernel 6.8

Five surgical patches under `/quadstorvtl/src/export/`, mirroring QuadStor's own
existing kernel-version conditional pattern. Each patch adds a new `6.5 ≤ kernel < 6.10`
branch alongside QuadStor's existing `< 6.5` and `≥ 6.10` branches, because Ubuntu 6.8
has the post-6.5 API set (`bdev_open_by_path` + `bdev_release` + `struct bdev_handle`)
but not the 6.10 set (`bdev_file_open_by_path` + `struct file` iodev). Unified diffs
are saved at `/home/user/remanence/patches/quadstor-3.0.79.32/diffs/`.

Patch summary:

1. **`linuxdefs.h`** — added `typedef struct bdev_handle iodev_t;` for the 6.5-6.9 range.
2. **`core_itf.c`** at `core_itf_iodev_close()` — added `bdev_release(b_dev)` branch.
3. **`core_itf.c`** at `core_itf_bdev_open()` — added `bdev_open_by_path(spec->name,
   BLK_OPEN_READ | BLK_OPEN_WRITE, THIS_MODULE, NULL)` branch.
4. **`core_itf.c`** at the block-device accessors — added `bdev_logical_block_size(b_dev->bdev)`
   and `b_dev->bdev->bd_inode->i_size` branch.
5. **`core_itf.c`** at `blkdev_issue_flush()`, `bio_alloc()`, `bio_set_dev()`,
   `bdev_max_discard_sectors()`, `blkdev_issue_discard()` — added matching branches
   that use `iodev->bdev` for the underlying `struct block_device *`.
6. **`core_sock.c`** at `validate_block_device()` — added `b_dev->bdev->bd_dev` branch.
7. **`Makefile`** — added `ccflags-y += -Wno-attribute-warning -Wno-error=attribute-warning`
   to defang the `FORTIFY_SOURCE` false positives around `memset(raw, 0,
   sizeof(*raw) + sizeof(*req))`. Verified the code is correct (`node_msg_alloc()` allocates
   `sizeof(*raw) + msg_len`), the warning is a false positive that FORTIFY can't see through
   the helper allocator. EXTRA_CFLAGS alone wasn't enough; ccflags-y is appended
   after KBUILD's own flags in 6.x and so wins on the cc command line.

### Skipped: Fibre Channel target driver

The QuadStor build also tries to compile the `qla2xxx` and `fcint` kernel modules for
FC SAN target functionality. Those fail on 6.8 too (`trace_array_get_by_name`'s
signature changed to take a `systems` argument). We deliberately did NOT patch them
because Remanence's use case is virtual library + iSCSI / SCSI generic — FC target
mode is out of scope. The init script treats `fcint.ko` as optional (the `check_error
"..." 0` form is warning-only). Service starts cleanly without it.

If FC is ever needed, the fix is mechanical: change
`qla_trc_array = trace_array_get_by_name("qla2xxx");`
to
`qla_trc_array = trace_array_get_by_name("qla2xxx", NULL);`
in `qla2xxx.66/qla_os.c:2897`, plus likely more breakage as we go deeper.

### Build/install verification (2026-05-16, post-patches)

```bash
sudo /quadstorvtl/bin/builditfusr      # builds vtlitf.ko, vtldev.ko, iscsit.ko successfully
sudo systemctl start quadstorvtl
sudo systemctl status quadstorvtl       # active (running)
lsmod | grep -E 'vtlitf|vtldev|iscsit'  # all three loaded
ss -ltn | grep -E ':(80|3260)\b'        # Apache + iSCSI target listening
```

Service log shows only one expected warning:
```
insmod: ERROR: could not load module fcint.ko: No such file or directory
```
This is expected (FC build skipped); does not affect VTL functionality.

---

## Configuration (CLI, captured in `scripts/quadstor/`)

Per the "CLI over GUI, scripts for easy reset" preference, every state-changing
step lives in `/home/user/remanence/scripts/quadstor/`:

- `common.sh`     — shared config (names, sizes, paths). Override any var via env.
- `setup.sh`      — idempotent: imports device defs, creates the VTL on top of
                    a pre-configured backing disk, adds 10 vcartridges.
- `status.sh`     — read-only snapshot of all QuadStor state on the host.
- `reset.sh`      — delete vcartridges + VTL, then re-run setup (keeps backing).
- `teardown.sh`   — full reverse; flags `--keep-backing`, `--remove-defs`.

**What's done by setup.sh today:**
- `devicedef -a --changer HP_MSL_Series` (HP MSL G3 Series)
- `devicedef -a --drive HP_LTO9` (HPE Ultrium 9-SCSI, mediatype 27 = LTO-9 18 TB)
- Verifies the `Default` storage pool exists (auto-created by QuadStor)

**What's gated on operator input:**
- Backing disk. QuadStor's daemon refuses every synthetic block device we
  tried: loop devices, `dm-linear` over a loop, `scsi_debug` (including with
  vendor strings spoofed to "HP"/"LOGICAL VOLUME"). It enumerates from real
  HBAs only, and accepts disks via their `/dev/sgN` path.
- VTL creation depends on at least one disk being in the pool, so the VTL
  and the vcartridges wait too.

When you have a real SCSI disk available on akash:

```bash
lsscsi -g                                                  # pick /dev/sgN
sudo /quadstorvtl/bin/bdconfig -a -d /dev/sgN -g Default   # ~30s init
sudo /home/user/remanence/scripts/quadstor/setup.sh       # finishes Step 1
```

The setup script's "no disk yet" path was tested — it exits 0 with a one-line
operator instruction; nothing is partially configured if you hit it.

### Two CLI gotchas worth documenting

1. **`vtconfig`'s `-T` short flag is broken.** The usage text says
   `-T <drivedef>`, but the short form actually maps to a numeric
   drive-vendor-type ID and the daemon rejects it with `Invalid message
   msg_data` if you pass a def name. Use the long form `--drivedef=<name>`.
   The scripts use long-form throughout.
2. **`vcconfig`'s `-p` prefix must be 6 characters when count > 1.** The
   binary auto-appends a 2-char media-type suffix (`L9` for LTO-9) to make
   the final 8-char label (e.g. `RMN001L9`, `RMN002L9`, …). Default in
   `common.sh` is `RMN001`.

### Backing-storage path: sparse file → loop → LVM → QuadStor

QuadStor's docs (page 149, "Configuring Disk Storage") quietly mention
**"LVM volumes can also be configured"** — that's the unlock. The daemon
filters out raw loop devices, dm-linear, and scsi_debug, but it accepts an
LVM logical volume even when its underlying PV is a loop device. So:

```bash
truncate -s 100G /var/lib/quadstor-backing/main.img
LOOP=$(losetup -f --show /var/lib/quadstor-backing/main.img)
pvcreate $LOOP
vgcreate qsvg $LOOP
lvcreate -l 100%FREE -n qslv qsvg
sudo /quadstorvtl/bin/bdconfig -a -d /dev/mapper/qsvg-qslv -g Default
```

QuadStor enumerates the LV (Vendor=`LVM`, Model=`LV`) and accepts it for
addition. Disk init runs at ~150 MB/s on akash; for a 100 GB LV that's
~11 minutes the first time, instant on subsequent setup.sh runs.

The `setup.sh` script automates this and survives reboots via a systemd
unit that re-attaches the loop and activates the VG before
`quadstorvtl.service` starts.

### Final Step 1 state (2026-05-16, end of session) — COMPLETE

| Item                                          | Status |
|-----------------------------------------------|--------|
| QuadStor .deb installed                       | ✅ done |
| Kernel modules built (`vtlitf`, `vtldev`, `iscsit`) | ✅ done |
| Service active                                | ✅ done |
| Web UI reachable (http://akash/)              | ✅ done |
| Source patches captured as unified diffs      | ✅ done |
| `HP_MSL_Series` changer def imported          | ✅ done |
| `HP_LTO9` drive def imported                  | ✅ done |
| `Default` storage pool exists                 | ✅ done |
| Backing storage (LVM LV on sparse loop)       | ✅ done (100G, Active) |
| VTL `mainlib` created                         | ✅ done (4 LTO-9 drives, 40 slots, 4 IE) |
| 10 LTO-9 vcartridges                          | ✅ done (RMN001L9 – RMN010L9) |
| `/dev/sch0`, `/dev/sg0–sg4`, `/dev/st0–st3`   | ✅ live |
| `sg_inq` on changer and drives                | ✅ returns proper SCSI INQUIRY |
| `mtx -f /dev/sch0 status`                     | ✅ "4 Drives, 44 Slots (4 IE)" with 10 cartridges loaded |
| `setup.sh` idempotent                         | ✅ second run completes in <1s |
| `reset.sh`, `teardown.sh`, `status.sh`        | ✅ present in `scripts/quadstor/` |

---

## Host privileges for running `rem` as a non-root user (2026-05-17)

By default, only `root` can issue the SCSI commands Remanence's
discovery uses (READ ELEMENT STATUS, etc.). Running `rem` as an
unprivileged user requires *two* host configuration steps. Both
should be applied to any host that will run Remanence — dev box or
production:

### 1. Add the operator user to the `tape` group

`/dev/sg*` is mode `crw-rw----` owned by `root:tape`. Group
membership is what gates the `open("/dev/sgN")` call.

```bash
sudo usermod -aG tape owner    # done on akash 2026-05-17
```

The user must log out and back in (or `newgrp tape`) for the new
group to take effect in their shell. Verify with `id` — the output
must include `26(tape)` (the gid is `26` on Ubuntu 24.04; check
`getent group tape` if a different distro).

### 2. Grant `CAP_SYS_RAWIO` to the `rem` binary

Group membership lets you `open()` /dev/sgN, but the Linux SCSI
command filter still intercepts SG_IO ioctls for "potentially
dangerous" opcodes and returns `EPERM` to non-CAP_SYS_RAWIO callers.
INQUIRY (0x12) is whitelisted, so `sg_inq /dev/sg4` works after
step 1; but READ ELEMENT STATUS (0xb8), MOVE MEDIUM (0xa5), and
most other SMC commands are *not* whitelisted. Discovery hits this
on the very first RES call and surfaces as
`DiscoveryError::NoLibraries`.

For a development binary (regenerated by `cargo build`), apply the
capability after each build. The repo has a helper target for this:

```bash
cd /home/user/remanence
make rem-dev
```

Note: file capabilities live in xattrs, *not* the ELF, so every
`cargo build` can reset them. Use `make rem-dev` after rebuilding,
and run the generated binary directly instead of `cargo run`:

```bash
/home/user/remanence/target/debug/rem libraries
```

For production, the daemon's systemd unit should grant the capability
via:

```ini
[Service]
AmbientCapabilities=CAP_SYS_RAWIO
CapabilityBoundingSet=CAP_SYS_RAWIO
```

so the long-running service gets the capability without the
on-disk binary itself being capability-bearing.

### Verification

After both steps:

```bash
$ id | tr , '\n' | grep tape
26(tape)
$ cd /home/user/remanence
$ make rem-dev
cargo build -p remanence-cli
sudo setcap cap_sys_rawio+ep target/debug/rem
getcap target/debug/rem
target/debug/rem cap_sys_rawio=ep
$ getcap /home/user/remanence/target/debug/rem
/home/user/remanence/target/debug/rem cap_sys_rawio=ep
$ /home/user/remanence/target/debug/rem libraries
7CBAD9CF74  HP MSL G3 Series  /dev/sg4  (4 drives, 40 slots [10 loaded], 4 IE)
```

If the third command returns `error: no tape libraries reachable on
this host`, run `strace -e ioctl rem libraries 2>&1 | grep SG_IO`
to confirm — `SG_IO ... = -1 EPERM` means step 2 hasn't been
applied (or got reset by a rebuild).

## Layer 2c hot-plug watcher build (2026-05-18)

The `rem watch` subcommand and the live udev integration test are
gated behind the `linux-udev` Cargo feature (cross-platform build is
otherwise free of native deps). On Debian/Ubuntu hosts the two
required system packages are `pkg-config` and `libudev-dev`:

```bash
sudo apt update
sudo apt install -y pkg-config libudev-dev
```

Versions actually installed on akash on 2026-05-18:

| Package | Version |
|--|--|
| `pkg-config` | `1.8.1-2build1` |
| `libudev-dev` | `255.4-1ubuntu8.15` |

Both are standard noble repository packages; no third-party PPAs.

### Building with the feature

```bash
# Workspace-wide build with the linux-udev backend enabled. The
# feature lives on the CLI crate and pulls remanence-library's
# linux-udev backend + tokio.
cargo build --workspace --features remanence-cli/linux-udev

# Release binary (the one to use against production / dev hardware):
cargo build --release --features remanence-cli/linux-udev
```

The default build (no `--features`) continues to work without
`pkg-config` or `libudev-dev`; only the watcher backend needs them.

### Verifying the backend live

Two checks proven on akash on 2026-05-18:

1. **Integration test** — `#[ignore]`-gated, requires root because
   it writes to `/sys/.../uevent` to synthesise a netlink event:

   ```bash
   cargo test --features remanence-cli/linux-udev --no-run --test watch_live_udev
   sudo target/debug/deps/watch_live_udev-<hash> \
        --ignored --test-threads=1 --nocapture
   ```

   Expected: `1 passed; 0 failed`. The test picks any `/sys/class/scsi_generic/sg*`,
   pokes its `uevent`, waits up to 5s for a coalesced burst, and
   asserts the burst mentions the sg's name in `touched_paths`.

2. **`rem watch` CLI** — runs in the foreground, one line per
   coalesced burst. Trigger synthetic events with `echo change > …`:

   ```bash
   target/release/rem watch --coalesce-window 200ms &
   sleep 1
   sudo sh -c 'for s in sg0 sg1 sg2 sg3 sg4; do
                 echo change > /sys/class/scsi_generic/$s/uevent;
               done'
   sleep 2
   kill -INT %1
   ```

   First successful run on akash:

   ```text
   [    1.221s] burst #1: events=5 span=6.949125ms \
                subsystems=[ScsiGeneric] kinds=[Changed] \
                paths=10 unknown_scope=false
                /dev/sg0 … /dev/sg4
                /sys/devices/.../scsi_generic/sg0 … sg4
   ```

   Five raw events collapsed into one burst on a 200ms sliding
   coalesce window. `paths=10` is the union of 5 `/dev/sgN`
   device-node paths + 5 sysfs paths.

### Verification snapshot — what actually ran (2026-05-18)

```text
$ sudo apt install -y pkg-config libudev-dev
$ cargo build --workspace --features remanence-cli/linux-udev   # green
$ cargo clippy --workspace --all-targets --features remanence-cli/linux-udev -- -D warnings   # green
$ cargo test --features remanence-cli/linux-udev --no-run --test watch_live_udev
$ sudo target/debug/deps/watch_live_udev-<hash> --ignored --test-threads=1 --nocapture
test live_udev_delivers_change_event ... using sg device: /sys/class/scsi_generic/sg4
got burst: 1 events, subsystems={ScsiGeneric}, kinds={Changed}, paths=2
ok
test result: ok. 1 passed; 0 failed
$ target/release/rem watch --coalesce-window 200ms   # produced the burst shown above
```
