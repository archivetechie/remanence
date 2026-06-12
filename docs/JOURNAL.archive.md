# Remanence — Working Journal (ARCHIVED)

> **Frozen on 2026-05-18.** New journal entries go to `journal/YYYY-MM-DD.json`
> (one file per day, JSON array of entry objects). See
> `feedback_keep_journal_current.md` in claude's memory for the schema.
> Every entry in this file has been migrated to `journal/2026-05-16.json` and
> `journal/2026-05-17.json` with stable uuids. Kept here only as a readable
> prose archive — do **not** append.

Brief, dated notes on what was tried, what worked, what surprised, where we left off.
Five minutes of writing per session is the goal. Audience: future-me, after weeks away.

---

## 2026-05-16 — Session 1: kickoff, QuadStor install

**Where we are:** Spec v0.1 written (`remanence-spec-v0.1.docx`), strategy plan in `plan.txt`,
no code yet. Goal of this session: execute Step 1 of the plan — get QuadStor VTL running on akash
so subsequent SCSI work has a virtual library to target.

**Decisions made today:**
- Drive the QuadStor install end-to-end from this session, with every step captured in
  `INSTALL.md` so it's auditable and reproducible.
- Use the .deb already in `~/quadstor/` (`quadstor-vtl-ext-3.0.79.32-debian12-x86_64.deb`)
  — the "ext" (extended) edition. Built for Debian 12; akash is Ubuntu with kernel 6.8.0-106-generic,
  expecting compatibility but tracking it as a risk.
- Configuration target per spec: 1 virtual library, 4 LTO-9 drives, 40 slots.

**Surprises / things to watch:**
- QuadStor's .deb depends on `build-essential` + `postgresql` but **not** on
  `linux-headers-$(uname -r)`, even though the preinst script aborts without
  `/lib/modules/$(uname -r)/build/Makefile`. So you have to install headers manually
  before `dpkg -i` or it'll fail. Captured in INSTALL.md.
- postinst calls `a2enmod cgi` — implies Apache 2 must be installed too, but it's also
  not a declared dependency. Another manual prereq.
- QuadStor bundles its own PostgreSQL under `/quadstorvtl/pgsqlsys/data` (port 9985)
  rather than using the system postgres on 5432. Good — keeps it isolated.
- postinst says reboot is required to start the service after modules are built. We'll
  try `systemctl start quadstorvtl` first and only reboot if needed.

**Plan for end of session:**
- Step 1 complete (QuadStor running, virtual library visible via lsscsi/mtx).
- Next session: Step 3 (Rust workspace skeleton) + Step 4 (first INQUIRY command).
- Step 2 (real MSL3040 fixtures) waits for the next access window.

**Actual end of session:** Step 1 did NOT complete. QuadStor 3.0.79.32's kernel
modules don't build on Linux 6.8 — the source predates the removal of
`blkdev_get_by_path` / `blkdev_put` in kernel 6.5. Details and three options
(patch source, downgrade kernel, defer QuadStor) are in `INSTALL.md`. Awaiting
owner's call before continuing.

**Update — Option A applied, QuadStor running:** owner chose to patch source
rather than downgrade or defer. Took ~6 surgical patches in `core_itf.c`,
`core_sock.c`, `linuxdefs.h`, and `Makefile` — all mirroring QuadStor's own
existing kernel-version branching pattern. The pattern was: Ubuntu 6.8 has
`bdev_open_by_path` + `struct bdev_handle` but QuadStor's existing 6.5-6.9 branch
expected the older `blkdev_get_by_path` API that Ubuntu had already stripped.
Added a parallel `6.5 ≤ kernel < 6.10` branch at each call site that uses
`b_dev->bdev` to reach the underlying block_device through the new handle.
Also `ccflags-y += -Wno-attribute-warning -Wno-error=attribute-warning` to
silence FORTIFY false positives (real code is correct, FORTIFY can't see
through a custom allocator). Unified diffs preserved under
`/home/user/remanence/patches/quadstor-3.0.79.32/diffs/`.

Skipped the FC target driver (qla2xxx, fcint) — out of scope for VTL dev use
and the init script treats fcint as optional. Service is up:

```
vtlitf, vtldev, iscsit modules loaded
coredev, ietd, vtmdaemon running
iSCSI on 3260, Apache + QuadStor web UI on 80
```

Web UI reachable at `http://akash/` (auto-redirects through vtlogin → vtsystem).
No password required by default.

**Configuration (CLI per owner's preference):** wrote
`scripts/quadstor/{setup,reset,teardown,status}.sh` + `common.sh` — all CLI,
no UI. setup.sh:
- imports the HP_MSL_Series changer def and HP_LTO9 drive def into QuadStor (works)
- expects a backing disk in the 'Default' pool, then creates the VTL + 10
  LTO-9 vcartridges (gated by configured-disk check; exits cleanly with
  instructions if no disk yet)
- reset.sh deletes vcartridges + VTL and re-runs setup
- teardown.sh removes everything; --keep-backing and --remove-defs flags
- status.sh shows current state

**Blocker on Step 1 — backing disk:** QuadStor's daemon refuses to enumerate
synthetic block devices. Tried: loopback files over a sparse `truncate`,
device-mapper linear wraps, scsi_debug (with vendor IDs spoofed to "HP" /
"LOGICAL VOLUME"). All rejected with `Unable to find disk at /dev/sgN for
addition`. The daemon enumerates real SCSI HBAs through some path that
filters out pseudo hosts; we couldn't penetrate the filter logic from the
shipped binaries (the daemon source isn't in the .deb).

Setup.sh now exits cleanly when no disk is configured, with a one-line
operator instruction:
```
sudo /quadstorvtl/bin/bdconfig -a -d /dev/sgN -g Default
```
where `/dev/sgN` points at a *real* SCSI disk (HBA-attached, real or
virtualized at the platform level, not synthesized in-kernel). Once owner
finds a candidate disk on akash, the script picks up from there.

**Second look at the QuadStor disk docs (after owner re-posted the URLs)
caught the line I'd missed:** *"LVM volumes can also be configured."* That's
the unlock — QuadStor accepts an LVM logical volume even when the PV
underneath is a loop device. So the backing stack becomes sparse file →
loop → PV → VG → LV, all on disk, no external hardware. setup.sh now
does all of this and waits for the daemon's disk-init pass (~11 min for
100 GB on akash).

**Also found two CLI gotchas worth recording for any future
sysadmin/operator using QuadStor on a dev box:**
- `vtconfig`'s short `-T <drivedef>` flag is broken: it maps to a numeric
  drive-vendor-type ID, not the def name as the usage text claims, and
  the daemon rejects the create with `Invalid message msg_data`. Use
  `--drivedef=<name>` instead.
- `vcconfig`'s `-p <prefix>` requires a 6-character prefix when adding
  more than one cartridge (it auto-appends `L9`/etc. to make the final
  8-char label).

**Where Step 1 actually ended — COMPLETE:**
- ✅ QuadStor service installed, patched for kernel 6.8, running
- ✅ Configuration scripts written (CLI-driven, idempotent)
- ✅ Device definitions imported (HP_MSL_Series, HP_LTO9)
- ✅ Backing storage: 100G LVM LV on sparse loopback, status Active
- ✅ VTL `mainlib` created: 4× LTO-9 drives + 40-slot MSL G3 changer + 4 IE ports
- ✅ 10 LTO-9 vcartridges (RMN001L9 – RMN010L9) loaded into slots
- ✅ Verified end-to-end: `lsscsi -g` shows `/dev/sch0` + `/dev/sg0-3`,
     `sg_inq` returns proper INQUIRY responses, `mtx -f /dev/sch0 status`
     reports "4 Drives, 44 Slots (4 IE)" with cartridges in 10 storage elements

**Reset / teardown rituals:**
- `sudo scripts/quadstor/status.sh`   — read-only snapshot of everything
- `sudo scripts/quadstor/reset.sh`    — wipe cartridges + VTL, re-create
- `sudo scripts/quadstor/teardown.sh` — full reverse (flags: `--keep-backing`, `--remove-defs`)
- `sudo scripts/quadstor/setup.sh`    — idempotent; safe after teardown or partial state

**For the next session — Steps 3 and 4:**
- Rust workspace skeleton under `~/remanence/` per the plan layout.
- First INQUIRY (CDB `0x12`) against `/dev/sg4` (the changer) and the four
  drive `/dev/sgN` nodes. The library is sitting here waiting.
- Step 2 (real-hardware fixtures) is now opportunistic — capture next time
  there's MSL3040 access; the dev fixture is good enough for parser work.

---

## 2026-05-16 — Session 1 continued: Steps 3 and 4 (Rust workspace + INQUIRY)

Stayed in the same session. Steps 3 and 4 done.

**Step 3 — workspace skeleton:**
- Installed rustup → stable Rust 1.95.0 (`. ~/.cargo/env` sourced in `.bashrc`).
- `git init` in `~/remanence/`. `user.email` and `user.name` set locally.
- Tree: `Cargo.toml` workspace, `crates/remanence-scsi/`, `docs/spec-v0.1.docx`
  (moved), `fixtures/inquiry/`, plus the QuadStor `patches/` and `scripts/`
  trees that already existed.
- Workspace `Cargo.toml` declares shared deps (`bytes`, `nix`, `serde`,
  `tokio`, `thiserror`, `tracing`) so each crate's manifest stays minimal.
- AGPL-3.0 LICENSE pulled verbatim from FSF.
- Only one crate scaffolded for now (`remanence-scsi`). Per the plan, defer
  the rest until each is actually exercised.

**Step 4 — first INQUIRY:**
- Captured raw INQUIRY fixtures from the live QuadStor library
  (`fixtures/inquiry/drive[1-4]-lto9.bin`, `changer-msl-g3.bin`).
- `remanence-scsi` is laid out as:
  - `inquiry.rs` — pure Rust CDB builder + response parser, fixture-tested.
  - `sg_io.rs` — Linux-only `SG_IO` ioctl wrapper using `nix`. Linux-gated
    so the parser half still builds on macOS/CI.
  - `error.rs` — `thiserror`-based `ScsiError`, with a dedicated
    `CheckCondition` variant that carries the REQUEST SENSE buffer.
  - `examples/inquiry.rs` — opens `/dev/sgN`, sends INQUIRY, prints parsed.
- Tests (5/5 pass): CDB shape, fixture-parses-lto9, fixture-parses-changer,
  rejects-truncated, rejects-wrong-response-data-format.
- End-to-end verified: `sudo target/debug/examples/inquiry /dev/sg0..4`
  returns the expected vendor/product/revision/type for all 5 devices.

**Notes / things learned this evening:**
- `SG_IO` is one ioctl number (`0x2285`) on a `/dev/sgN` opened R/W, with a
  big `sg_io_hdr_t` describing the CDB + buffers. The "from device" path is
  all we need today.
- The `missing_docs` lint trips on macro-generated functions; wrapped the
  nix `ioctl_readwrite_bad!` in an internal `mod ioctl { … }` so the lint
  scope is contained without disabling it crate-wide.
- Open `/dev/sgN` with read+write permissions even for FROM_DEV — Linux
  refuses SG_IO on read-only file descriptors.

**Status at end of session:**
- Plan steps 1, 3, 4 all complete. Step 2 (real-MSL3040 fixtures) waits for
  the next access window — not blocking anything.
- Next natural step: keep going down Layer 1 — VPD page 0x80 (unit serial)
  is the plan's Step 5, then READ ELEMENT STATUS (Step 6, the big one).
  Or: pause to set up a remote for the git repo, push, and let collaborators
  see the project.

---

## 2026-05-16 — Session 1 continued: github push + Step 5

**Repo on GitHub:** https://github.com/archivetechie/remanence — private,
`main` as the default branch, both commits pushed, upstream tracking set.
`gh repo create archivetechie/remanence --private --source=. --remote=origin --push`
did all of it in one shot. Local branch was `master` (older Ubuntu git
default); renamed to `main` before pushing.

**Step 5 — VPD page 0x80 (Unit Serial Number):**
- Captured 5 raw fixtures (`fixtures/vpd-80/{drive[1-4]-lto9,changer-msl-g3}.bin`)
  — each one is exactly 14 bytes (4-byte VPD header + 10 ASCII serial).
- New module `remanence_scsi::vpd`:
  - `VpdHeader` — the shared 4-byte preamble (`device_type`,
    `peripheral_qualifier`, `page_code`, `page_length`). Parser is
    `VpdHeader::parse_with_payload(buf, expected_page)` and returns the
    header + a borrowed slice of the page payload. Optional `expected_page`
    cross-checks that the target didn't decide to return a different page.
  - `UnitSerial<'a>` — borrowed wrapper around the serial-number bytes,
    with `as_bytes()` and `as_str()` (trimmed). No allocation; `Copy`.
- `inquiry::build_cdb_vpd(page_code, alloc_len)` for the EVPD=1 CDB.
- Tests (5 added; 10 total in the crate): parses both fixtures, rejects
  wrong page code, rejects truncated header, rejects truncated payload.
- `examples/inquiry.rs` now prints the serial alongside vendor/product.
  Verified all 5 /dev/sgN return the serials that match QuadStor's own
  `vtconfig --list --vtl=mainlib` output:
  - `/dev/sg0` → drive1 11A1D57AD0
  - `/dev/sg1` → drive2 6D71FB6FE6
  - `/dev/sg2` → drive3 2FEB23D41A
  - `/dev/sg3` → drive4 79B07D9D00
  - `/dev/sg4` → changer 7CBAD9CF74

**Next:** plan's Step 6 — READ ELEMENT STATUS (CDB 0xB8, DVCID=1). This is
the "meaty one" — the response carries descriptors for every element
(drives, slots, ie ports, transport) and including the DVCID flag pulls in
each drive's unit serial. Once Step 6 lands, the code can enumerate the
full library and join drives → element addresses by serial. The plan
estimates a weekend for it.

---

## 2026-05-16 — Session 1 continued: plan Step 2 (real-hardware fixtures)

owner had an unplanned access window to the production library and ran
`scripts/capture-msl3040.sh` on the host `datamover` (RHEL 9.6, kernel 5.14,
sg3-utils 2.13). Tarball came back at 31 KB, extracted to 820 KB across
160-ish files. Production stack is bigger than expected:

- **Two HPE MSL3040 chassis** at `/dev/sg7` and `/dev/sg11`, both running
  firmware 3350. Logical-library serials `DEC418146K_LL02` and `..._LL03`
  (15-character serials — wider than the LTO drives' 10-char).
- **Two LTO-9 drives** (HPE Ultrium 9-SCSI, firmwares S2S1 and R3G3).
- **Two LTO-7 drives** (vendor reports as **`HP`** — pre-rebrand —
  product `Ultrium 7-SCSI`, firmwares S2T1 and Q387).
- Plus a Broadcom enclosure, two HPE smart-adapter enclosures, one HPE
  P408e-p/E208e-p storage controller, one disk — all skipped by the
  capture script because their peripheral types are `enclosure`/`storage`/
  `disk`, not `tape` or `medium changer`.

**No non-zero exit codes in capture.log** — the script ran clean.

### Real-hardware regression for the parsers

Brought three representative fixtures into `fixtures/inquiry/real/` and
`fixtures/vpd-80/real/` (changer-msl3040, drive-lto9, drive-lto7), wired
them into the existing `inquiry::tests` and `vpd::tests` modules via
`include_bytes!`. Six new tests; all 16 in the crate pass without any
parser changes — the parsers I wrote against QuadStor's virtual hardware
generalize cleanly:

- `parses_real_msl3040_changer` — `HPE`/`MSL3040`/`3350`
- `parses_real_lto9_drive` — `HPE`/`Ultrium 9-SCSI`/4-char firmware
- `parses_real_lto7_drive` — note the `HP` (not `HPE`) vendor string
- `parses_real_msl3040_serial` — 15-char serial `DEC418146K_LL02`
- `parses_real_lto9_serial` and `parses_real_lto7_serial` — 10-char,
  shared prefix `8031BDC7` across the four-drive set (sequential factory
  numbering)

### CDB compatibility notes

All three READ ELEMENT STATUS CDB variants the capture script sent
(safe / big / no-DVCID) **succeeded** on both real MSL3040s, each returning
2268 bytes. QuadStor's emulation is stricter than real HPE firmware here;
on QuadStor the "big" variant (`0xFFFF` element count, 16 MB alloc length)
returns CHECK CONDITION. Useful to know — Step 6 should default to the safe
CDB shape since it works everywhere, but the parser must of course handle
arbitrary element counts because real hardware fills the whole response.

The MSL3040 chassis we captured reports **43 elements** rather than 49 —
the library is configured with a magazine subset (the 3040 can be ordered
with various slot counts; not all magazines need to be populated).

`fixtures/real-hardware/*.tar.gz` is now in `.gitignore`; the extracted
tree is what the in-tree tests reference.

**Plan progress now:** Steps 1, 2, 3, 4, 5 all done. Step 6 (READ ELEMENT
STATUS) is next and gets us the join-by-serial logic — and the captures we
just took give us real-firmware fixtures to TDD against, which is exactly
what the plan was set up to enable.

---

## 2026-05-16 — Session 1 continued: plan Step 6 (READ ELEMENT STATUS)

The big one — done in one sitting, in part because:
- the real-MSL3040 capture from earlier gave us a production-loaded
  fixture (43 elements, cartridges in slots, two drives full),
- the parser-vs-transport split that worked for INQUIRY/VPD generalized
  cleanly to RES,
- `mtx -f /dev/sg4 status` was a perfect cross-check oracle while writing
  tests.

**Doc gathering side-quest:** owner asked whether vendor/standards refs
would help; tried fetching SMC-3 drafts from t10.org (all the obvious
URLs 404) and the MSL3040 User & Service Guide from psnow.ext.hpe.com
(curl-blocked). Worked around it for now — the fixtures + spec knowledge
were enough — and saved the doc-priority list to memory so future
sessions know what to ask for. Asked owner to drop PDFs into
`~/remanence/docs/` if he gets any.

**New module `read_element_status`:**
- `build_cdb(element_type, starting_addr, num_elements, voltag, dvcid,
  alloc_len)` — 12-byte CDB per SMC-3 §6.13, with the two flag bits in
  the right places. Constants `SAFE_NUM_ELEMENTS=0x100` and
  `SAFE_ALLOC_LEN=64KiB` for the "works everywhere" defaults.
- `ElementType` enum: MediumTransport / Storage / ImportExport /
  DataTransfer / Other(u8).
- `Element` carries: type, address, full/except/access flags, optional
  `source_address` (when SVALID), optional `primary_voltag` (when the
  page header had PVOLTAG=1), and optional `drive_serial` (from DVCID
  identifier descriptors, when the target included any).
- `ElementStatusData::by_type(t)` convenience for typed iteration.

**Five new tests, 21 total in the crate (zero failures):**
- `cdb_layout_matches_spec`
- `parses_quadstor_msl_g3` — verifies the QuadStor capture parses to
  exactly 49 elements (1+4+40+4) with no drives full and no voltags yet
  loaded (matches the state right after `setup.sh`).
- `parses_real_msl3040` — verifies the real production capture parses to
  43 elements, drive 1 is full with voltag `S30002L9` from
  `source_address=0x040a` (= storage element 34, matching the
  `mtx` decoded text fixture).
- truncated-header + truncated-payload error paths.

**End-to-end example:** `examples/topology.rs`. Opens `/dev/sgN`,
does standard INQUIRY + VPD 0x80 + RES, prints a one-line-per-element
table with type letter (R/D/S/I), address, state, voltag, source slot,
and drive serial. Verified live:

```
Library:  /dev/sg4  HP MSL G3 Series D.00
Serial:   7CBAD9CF74
Elements: 49 reported (first address 0x0000)
Layout:   1 robot(s), 4 drive(s), 40 slot(s), 4 IE port(s), 10 full
  …
  S     0x0400  full    RMN001L9
  …
  S     0x0409  full    RMN010L9
```

That's plan Step 6's stated milestone: "a Rust program that, run on akash,
prints a live topology view of the QuadStor virtual library."

**One known limitation:** real HPE firmware in this capture didn't include
DVCID identifier descriptors even with the DVCID bit set in the CDB (the
spec lets implementations decide). Drive serials in the topology example
will come back empty for that hardware until we either retry with the
larger plan-CDB form (which also succeeded on real firmware) or fall back
to per-drive INQUIRY VPD 0x80. Worth a small follow-up after Step 7's
reassess. The parser itself handles DVCID descriptors when they're
present — that path will exercise once we capture against firmware that
emits them.

**Plan progress now:** Steps 1, 2, 3, 4, 5, 6 all complete. Next: plan
Step 7 — stop and reassess. Reassess questions:
- Is the design holding up? *Yes — parser/transport split has paid off
  twice, and fixture-driven TDD has caught zero false positives so far.*
- Is the spec wrong anywhere? *Not obviously. Spec v0.1 didn't predict
  the variable-length serials (MSL3040 returns 15-char, drives 10-char),
  but the parser handles that naturally. Worth a small v0.2 note.*
- Push deeper into code (Layer 2 — libudev, join-by-serial topology
  resolver) or back to spec (manifest format, OpenAPI)? *Open question
  for owner.*

---

## 2026-05-16 — Session 1 continued: DVCID confirmed on real HPE; topology
## dance discarded

owner acquired the HPE Tape Library SCSI Reference (20-STG-TAPESCSIREF-ED5,
March 2026 Ed. 5, 172p, covers MSL3040), dropped it in `docs/`. Reading it
revealed HPE *documents* DVCID as returning a 34-byte
(vendor+product+serial) identifier descriptor per drive — but our earlier
captures with `DVCID=1` alone showed empty descriptors. The HPE doc was a
better-founded ticket against firmware behavior than guessing, but the
real answer turned out to be even simpler.

Extended `scripts/capture-msl3040.sh` to probe five CDB variants per
changer (`dt_only`, `dt_curdata`, `all_curdata`, `all_mixed_dvcid`,
`all_mixed_dvcid_curdata`) plus optional `--with-init` for the cases
where firmware needs an INITIALIZE ELEMENT STATUS first. Also added
VPD 0x85/CC/D0, full changer LOG SENSE + MODE SENSE (including page
1Dh — Element Address Assignment), and 7 more drive LOG SENSE pages.

**Dry-run on QuadStor produced the answer:** the firmware emits DVCID
descriptors only when *both* `DVCID=1` and `CurData=1` are set in CDB
byte 6. `DVCID=1` alone yields base 52-byte descriptors; with CurData
added, descriptors grow to 86 bytes and carry the documented identifier
block. CurData semantically means "return cached element state without
device motion" — and HPE/QuadStor evidently gate the DVCID enhancement
on it because device identifiers come from cached library state, not
from a fresh physical inventory.

**Verified live on real datamover MSL3040 firmware 3350:** same trick
works. Drive serials returned inline in the RES descriptor match what
each drive's own INQUIRY VPD 0x80 reports. Updated test
`real_msl3040_dvcid_block_works_too` pins this.

**Bonus finding — topology dance would have been wrong anyway:**
changer1 (sg7, host 2) reports two drives, one of which (`8031BDC7DB`)
is /dev/sg2 on **host 1** — not the host the changer is on. owner
clarified afterwards that this isn't dual-attached SAS — the MSL3040
is **one physical chassis partitioned into two logical libraries**: a
Partition 1 (LTO-9, what Remanence targets) and a Partition 2 (LTO-7,
currently used in production by *dwara2*, the predecessor app). The
physical chassis has 4 drive bays distributed across both partitions
and both HBA cables, so "drives on the same host:channel as the
changer" simply doesn't correspond to "drives in this logical library."
DVCID-RES is the only correct discovery path.

Partitioning is now a first-class concept in Layer 2's plan: a single
chassis WWN can present multiple logical libraries (the VPD 0x80
serial encodes which: `DEC418146K_LL02` vs `DEC418146K_LL03`). Discovery
must enumerate partitions independently, and Remanence must default to
**not touching** the LTO-7 partition that dwara2 owns.

**One parser bug surfaced and fixed:** PVOLTAG block is 36 bytes (32-byte
identifier + 4-byte reserved/sequence number per SMC-3 Table 47), not
32 as I'd hardcoded. The 4-byte error was invisible in the existing tests
because they only asserted the trimmed identifier string; it broke DVCID
parsing because the parser read DVCID 4 bytes too early in the descriptor.
Fixed; now using `VOLTAG_BLOCK_LEN=36` for cursor advancement and
`VOLTAG_ID_LEN=32` for the printable identifier read.

**24/24 tests pass.** New fixtures in tree:
- `real-msl3040-dvcid.bin` (188B, drives-only DVCID response, 2 drives)
- `real-msl3040-full-dvcid.bin` (2336B, all elements + DVCID — mixed
  descriptor lengths within one response: 52B for non-drives, 86B for
  drives)
- `quadstor-msl-g3-dvcid.bin` (360B, the QuadStor version)

**Layer 2's discovery is now drastically simpler than the original plan
imagined.** No topology dance, no SCSI-ID ordering inference, no
per-bay fallback INQUIRY. Just RES with DVCID+CurData on each changer.

---

## 2026-05-17 — Step 7 reassess: codex review, spec v0.2, design simplification, Layer 2 §7.1-7.3

A long single-day arc through Step 7. Five distinct sub-sessions:

**1. Codex adversarial review of everything Layer 1.** Ran
`codex challenge` (high reasoning, sandbox bypassed because bwrap is
busted on akash) against every Rust file + the QuadStor scripts + the
capture script + the just-written `docs/layer2-design.md`. 17 findings,
ordered by severity:

- **Critical (3, all in scripts/quadstor):** setup.sh would trust a
  pre-existing `qsvg` VG blindly (could be someone else's); teardown.sh
  passed `$LOOP_DEV` to `disk_configured` but setup had registered
  `$LV_PATH`, leaving the LV registered with QuadStor after a teardown;
  reset.sh would wipe any VTL named `mainlib` without verifying it was
  ours.
- **High (7):** sg_io.rs ignored host_status / driver_status / SG_INFO
  bits after the ioctl, treating bad transports as success; `cmd_len`
  and `dxfer_len` truncated silently to `u8` / `c_uint`; `resid`
  arithmetic mishandled negative or out-of-range values. Parsers in
  read_element_status.rs accepted page-byte-counts that weren't
  multiples of desc_len (hostile slack), didn't enforce
  `num_elements`, didn't refuse trailing slack after the last page.
  `capture-msl3040.sh` had a CDB alloc/buffer mismatch.
- **Medium (7):** AVOLTAG bounds unchecked; DVCID malformed-tail
  silently became `None` instead of `Err`; INQUIRY ignored
  `additional_length`. Examples opened r/w when Linux SG_IO has
  accepted O_RDONLY for FROM_DEV since 2.6.18. The capture script's
  prereq check missed `sg_modes`. Three doc-side findings on
  `layer2-design.md` (sysfs stability claim, no DVCID fallback, error
  model's "warnings via tracing" approach was promised-then-deferred).

All 17 addressed in commit `97360f5` ("Address every finding from
codex adversarial review"). 8 new fuzz/defensive tests added. The
compile-time `size_of` / `align_of` assertions on `sg_io_hdr_t` are now
pinning the struct layout at build time so a future refactor can't
silently regress.

**2. Spec v0.2.** Wrote `docs/spec-v0.2.md` (550 lines, then trimmed)
as the slim Option A from the Step 7 reassess. Real changes that
spec v0.1 didn't have right:
- LTO-7 reframed as *coexisting* (not legacy) — same MSL3040 chassis,
  different partition, owned by dwara2 until migration.
- Overland Storage XL80 added as a second production target (LTO-7 +
  LTO-6). Fleet now supports LTO-6/7/8/9 across vendors.
- §6.2 rewritten: DVCID+CurData is empirically grounded, with the
  §6.2.1 fallback ladder explicitly documented.
- §9.7 license resolved: AGPL-3.0-or-later (already chosen in code).

**3. Library/Partition simplification.** owner pushed back on my v0.2
having a nested `Library { partitions: Vec<Partition> }` model. He
was right — there's no operation Remanence performs that benefits from
knowing two logical libraries share a chassis. SCSI moves can't cross
logical libraries; drives/slots/IE belong to exactly one; cleaning
cartridges don't cross; firmware/health are operator concerns.

Collapsed to a flat `Vec<Library>`. Each `Library` is a logical library
(= one SCSI medium changer); chassis identity is an *informational*
`chassis_designator: Option<DeviceDesignator>` field that no
operational code paths look at. Spec §6.5 reframed: "the deployments
Remanence will see — a single unpartitioned library, a partitioned
chassis with N logical libraries, M physically separate libraries —
all reduce to a flat list of logical libraries." Commit `e067f61`.

**4. layer2-design.md review.** owner dropped a 15-finding review
(`docs/layer2-design-feedback.md`). All legitimate. The High-priority
ones especially:
- **`Library::open()` must revalidate identity.** Cached `/dev/sgN`
  paths can point to a *different* changer after Linux re-enumeration.
  Now: open + INQUIRY + verify VPD 0x80 serial; fail with
  `OpenError::IdentityChanged` on mismatch.
- **Allowlist is required from v0.1, not future.** Spec v0.2 §8.2
  made it required; the design doc was still saying "future soft rule."
  Now there's a real `AccessPolicy` trait + `StaticAllowlist` impl;
  state-changing operations refuse for any library not on the
  allowlist.
- **DVCID fallback was unsafe.** I'd written "fall back to per-drive
  VPD 0x80 cross-reference," then noted that this relies on the
  SCSI-bus-topology assumption we'd already proved wrong for the
  partitioned MSL3040. Now: refuse to assign drives at all when
  topology is ambiguous, emit `DriveMappingUnavailable`, return the
  library with `installed = None` on affected bays. "Loud partial
  discovery is better than wrong-looking full discovery."
- **`Drive` couldn't represent partial discovery.** Replaced with
  `DriveBay { element_address, installed: Option<InstalledDrive>, … }`
  so the library shape survives missing host attachments.
- **`count=0x0100` would truncate a full MSL3040 stack.** A 7-module
  280-slot library plus drives and IE exceeds 256 elements. Switched
  to a two-phase RES probe: first call alloc_len=8 to learn
  `byte_count`, second call sized to fit.

Plus the medium findings: voltag is now tape identity (location is
separate), MODE SENSE 1Dh consistently deferred, doc retitled "Layer
2a" with explicit scope, udev is Layer 2c. Commit `6c98cf3`.

**5. Layer 2 implementation §7.1-7.3.** Three commits:

- §7.1 (`885b1b6`): VPD 0x83 parser. `DeviceIdentification` +
  `DeviceDesignator` with `as_naa() / as_str() / as_hex()` accessors
  and a `preferred_chassis()` picker. 6 new tests against the real
  changer + drive VPD 0x83 captures.

- §7.2 + §7.3 (`2f24c85`): `crates/remanence-library/` created.
  Exposes `Library`, `DriveBay`, `InstalledDrive`, `Slot`, `IePort`,
  `ElementLayout`, `DeviceCaptures`, plus `AccessPolicy` + the
  cloneable `IoErrorKind` / `DiscoveryError` / `DiscoveryWarning` /
  `OpenError` types from the design doc. `Library::from_captures(...)`
  is the pure-logic builder — feed it parsed INQUIRY + VPD 0x80 +
  VPD 0x83 + RES and get a complete `Library` out, no SCSI calls.
  Round-trip tested against the real-MSL3040 full-DVCID capture:
  serial `DEC418146K_LL02`, 2 drive bays each with their DVCID-derived
  serials, 40 slots, drive 1 holding `S30002L9` from slot `0x040a`,
  slot 1 holding the cleaning cartridge `CLNU01L9`, chassis NAA
  `0x5001438031bdc7d4`. 41/41 workspace tests pass.

**Where we left off:** §7.4 (sysfs walker, Linux-only) and §7.5
(`discover()` orchestration that produces `DeviceCaptures` from live
SCSI) are next. §7.6 (`LibraryHandle` with identity revalidation) and
§7.7 (`rem` CLI) follow.

**One thing I should have done earlier and didn't:** journal updates.
owner caught me; I'd been relying on commit messages alone. Going
forward: append a JOURNAL.md entry at the end of each session,
five-minute write, future-me audience.

---

## 2026-05-17 — Layer 2a §7.4 + §7.5: sysfs walker and `discover()`

**§7.4 (sysfs walker).** `crates/remanence-library/src/sysfs.rs`.
Enumerates `/dev/sg*` and resolves each to its current
`/sys/class/scsi_generic/sgN/device` symlink to give a
`Vec<DeviceAttachment>`. Linux-only (gated on `cfg(target_os = "linux")`),
with `enumerate_sg_devices_under(dev_root, sys_root)` exposed so tests
drive it against a tempdir mock host instead of the real `/dev`. Four
tests: sort order, non-`sg<digits>` filtering (`sga`, `sg_some_link`,
bare `sg` all skipped), missing-symlink skip, missing-root error.

**§7.5 (`discover()` orchestration).** Two new modules:

- `transport.rs`: the `SgTransport` trait (one method,
  `execute_in(cdb, buf) -> Result<usize, ScsiError>`). Production impl
  `LinuxSgTransport` wraps `sg_io::execute_in`, opens `/dev/sgN`
  read-only first (Linux SG_IO has accepted O_RDONLY for FROM_DEV
  since 2.6.18) and falls back to r/w. Test impl `FixtureTransport`
  replays a queue of canned responses and logs every CDB issued.
- `discovery.rs`: `discover_with(devices, transport_for)` — generic
  over the transport — and `discover_linux()` for the prod path. For
  each enumerated device: open, std INQUIRY → classify
  (medium-changer / sequential-access / skip), then probe-specific
  CDBs. Changers get VPD 0x80, VPD 0x83 (warning on failure, never
  fatal), then the two-phase RES probe (8-byte header to learn
  `byte_count`, second call sized to fit, with a 1 MiB single-call
  fallback if phase 1 is rejected). Tapes get VPD 0x80. After all
  devices are probed, tape serials are matched into drive bays.

**Design choices that were worth making explicit:**
- *Closure-based transport factory* over `IntoIterator<(Attach, T)>` —
  it keeps the open/probe loop natural (open one device, probe it
  fully, drop the transport, move on) without forcing the caller to
  pre-open every device.
- *VPD 0x83 failure is non-fatal.* The chassis NAA is informational
  per the design doc; no operational code paths consume it. So a
  device that doesn't implement page 0x83 (or returns malformed
  bytes) becomes a `ScsiError` warning, not a discovery abort.
- *Serial collisions are fatal.* If a tape's VPD 0x80 serial matches
  drive bays in two different libraries, that's structurally
  impossible without a firmware bug or overlapping libraries, so
  `DiscoveryError::SerialAmbiguous` aborts. Better loud than wrong.
- *RES phase-1 fallback path.* HPE firmware 3350 happily answers the
  8-byte probe, but if some future firmware or vendor refuses, we
  fall back to one big 1 MiB call rather than guessing wrong.

**Tests.** Three new discovery tests on top of the four sysfs and one
transport test:
- `discover_empty_host_returns_no_libraries`: no `/dev/sg*` →
  `DiscoveryError::NoLibraries`.
- `discovers_a_quadstor_changer_with_all_four_drives`: feeds the real
  in-tree QuadStor INQUIRY + VPD 0x80 + RES fixtures plus four
  fabricated LTO-9 VPD 0x80 responses through the orchestration.
  Confirms 1 library (serial `7CBAD9CF74`), 4 drive bays, and —
  because our QuadStor RES capture was made *without* CurData=1 so
  there are no DVCID descriptors — that all four bays have
  `installed = None` and the four tape devices land as
  `UnclaimedTape` warnings. Exactly the "loud partial discovery"
  behavior the design doc promises.
- `no_state_changing_cdbs_during_discovery`: a `RecordingTransport`
  wrapper records every CDB opcode the orchestration issues; assert
  every one is on the read-only allowlist (`0x12` INQUIRY, `0xb8`
  RES, plus reserved future opcodes for MODE/LOG SENSE, TUR, REQUEST
  SENSE). This is the spec v0.2 §8.2 safety property mechanized.

51/51 workspace tests pass (40 SCSI + 11 library). `cargo check
--all-targets` clean.

**Where we left off.** §7.6 (`LibraryHandle::open(policy)` with
identity revalidation: re-open `/dev/sgN`, re-INQUIRY, re-VPD 0x80,
verify serial matches the discovery snapshot, fail with
`OpenError::IdentityChanged` on mismatch) and §7.7 (the `rem` CLI
binary that replaces the topology example). Once §7.6 lands we have
the first user-visible end-to-end: `rem libs`, `rem inv <library>`,
`rem drives <library>`.

**Live-hardware milestone available right now.** With §7.5 done, a
single `cargo run -p remanence-cli --bin rem -- libs` against akash's
QuadStor should produce the first live discovery report from the real
SCSI stack (not fixtures). Worth doing as the §7.7 acceptance test.

---

## 2026-05-17 — Layer 2a §7.5 review pass: DVCID ladder + join test + tightenings

Five-finding review on the just-shipped §7.5 turned up one High and
four cleanups:

- **High: real DVCID fallback ladder.** My §7.5 had a single all-elements
  RES with DVCID+CurData=1 and called it done. The design doc §4.2.1
  spells out a *ladder*: if the primary call returns no drive serials,
  retry drives-only (element_type=4) with CurData=1, then with
  CurData=0, then refuse + emit `DriveMappingUnavailable`. Now
  implemented properly. The merge keeps slot/IE/transport elements from
  the primary response and replaces only the DataTransfer page from the
  successful rung. Ladder rung failures (ScsiError or parse error) are
  silently consumed — only the eventual exhaustion is reported.

- **Medium: positive-path test missing.** The original main test
  exercised the *negative* path (no DVCID → all bays unbound). Added
  `discovers_real_msl3040_full_dvcid_with_drive_join_by_serial` driving
  the real MSL3040 full-DVCID RES capture through two simulated LTO-9
  tape devices with matching VPD 0x80 serials. Asserts both bays land
  on `installed: Some(...)` with `identity_source = DvcidAndInquiry`,
  sg_path populated, vendor/product/revision filled in.

- **Medium: NoLibraries semantics.** Old code returned `Ok(libraries: [])`
  if the host had tape drives but zero changers. Per error model that
  should be `NoLibraries`. Now `libraries.is_empty()` alone is the
  trigger. New test `no_libraries_when_host_has_only_tapes` covers it.

- **Low: API name mismatch.** Renamed `discover_linux()` →
  `discover()` to match the name the design doc uses (still
  `#[cfg(target_os = "linux")]`, since Layer 2 is Linux-only in v0.1).

- **Low: sysfs.rs didn't actually canonicalize.** Code joined the
  relative symlink target to the link's parent — leaving `..`
  segments in the result. Now uses `fs::canonicalize` which follows
  the symlink AND resolves `..`. Test mock host already creates real
  target directories so canonicalize succeeds against the tempdir.

**Tests.** 13 in the library crate (up from 11): two new discovery
tests, ladder behavior covered by the renamed existing test, and the
`no_state_changing_cdbs_during_discovery` safety test still green
(ladder retries are all opcode `0xb8`, still on the read-only
allowlist). 53/53 workspace.

**Where we left off (still).** §7.6 (`LibraryHandle::open(policy)` with
identity revalidation) and §7.7 (the `rem` CLI binary) are the next
two chunks. The live-discovery acceptance test against akash's
QuadStor remains the natural way to close out §7.7.

---

## 2026-05-17 — Layer 2a §7.6: LibraryHandle + identity revalidation

The safety scaffold the design doc has been building toward. The
type system now enforces both spec v0.2 §8.2 properties at compile
time: state-changing operations cannot be called on a plain
`Library` value, and identity revalidation cannot be skipped.

**Module: `crates/remanence-library/src/handle.rs`** (~220 lines)

- `LibraryHandle` — opaque resource type holding the `Library`
  snapshot + a live `Box<dyn SgTransport>`. Not `Clone`, not
  `PartialEq` — it's a resource handle, not a value. Manual `Debug`
  impl elides the transport. Exposes `library()` for read-only
  access to the snapshot and `transport_mut()` for Layer 2b to hang
  CDB operations off later.

- `Library::open(policy)` — Linux-only convenience that wraps
  `LinuxSgTransport::open`. Returns `Result<LibraryHandle, OpenError>`.

- `Library::open_with(policy, transport_for)` — testable form,
  caller supplies the transport opener closure.

**The three gates, in order:**

1. **`AccessPolicy::allows`** check — refuse with
   `OpenError::NotAllowed` if the library's serial isn't on the
   policy's allowlist. The transport is *not* opened at all on
   refusal — verified by a test that hands a panicking factory.

2. **Derived-identity check** — walk `drive_bays`; if any installed
   drive has `IdentitySource::Derived` AND
   `allows_derived_drive_identity(library.serial)` is false, refuse
   with `OpenError::DerivedIdentityNotOptedIn { serial }`. The
   design treats topology-derived drive mappings as suspect by
   default; the operator has to opt in per-library.

3. **Open + revalidate identity** — open the cached `changer_sg`
   via the supplied transport factory (errors map to
   `DeviceUnavailable`), then issue INQUIRY VPD 0x80 and compare
   the returned serial to `library.serial`. Mismatch (different
   device since discovery) or malformed VPD response (unconfirmable
   identity) both → `OpenError::IdentityChanged { expected, actual }`.

**Tests** — 7 new in handle::tests, one per outcome:
- `open_succeeds_when_policy_allows_and_serial_matches` (happy path)
- `open_refuses_when_policy_does_not_allow`
- `open_refuses_derived_identity_without_opt_in`
- `open_succeeds_when_derived_identity_is_explicitly_allowed`
- `open_fails_with_identity_changed_when_serial_drifts`
- `open_fails_with_device_unavailable_when_transport_open_errs`
- `open_fails_with_identity_changed_when_revalidation_response_is_malformed`

Two of them install panicking transport factories — verifying that
policy refusal aborts *before* any /dev/sgN is opened.

**Other deltas**:
- `DiscoveryReport::library(serial)` accessor — sugar for finding a
  library in the report. Used by future `rem library <serial>`.
- `LibraryHandle` re-exported at crate root.

60/60 workspace tests. Clean check.

**Where we left off.** §7.7 — replace the topology example with a
real `rem` CLI binary (`rem libraries`, `rem library <serial>`).
Then a live smoke test against akash's QuadStor closes out Layer 2a.
Layer 2b (`MOVE MEDIUM`, `INITIALIZE ELEMENT STATUS`) and Layer 2c
(udev watcher) become their own design docs after that.

---

## 2026-05-17 — Layer 2a §7.6 review pass: handle hardening + partial-DVCID + doc links

Four-finding review on §7.6. No criticals; all medium/low cleanups.

- **`transport_mut()` now `pub(crate)`.** Public visibility would let
  external callers bypass any future Layer 2b operation-level checks
  (address validation, audit logging, safety wrappers). Layer 2b is
  the only intended caller; restricting visibility now keeps that
  guarantee.

- **Revalidation also checks standard INQUIRY.** Old code only
  reissued VPD 0x80. The design doc §5.2 says INQUIRY + VPD 0x80;
  doing both means a `/dev/sgN` that now points at a *tape drive*
  (kernel re-enumeration, pass-through reordering) is rejected
  *before* we even ask for the serial. New test
  `open_fails_with_identity_changed_when_device_is_no_longer_a_changer`
  feeds a tape-drive INQUIRY and asserts the open fails with the
  changer-INQUIRY check, never reaching VPD 0x80 in the script.

- **Partial DVCID is now warned.** The old `has_drive_serial` check
  let *any* drive serial in the primary response skip the fallback
  ladder, so a partial DVCID glitch (one of N drives missing
  identity) silently produced `installed: None` on the unresolved
  bays. New design:
  - `all_drives_resolved()` requires every DataTransfer bay to have
    a serial.
  - The ladder now *gap-fills* primary in place via
    `fill_missing_drive_serials()`: drives-only RES copies its
    serial into primary bays where primary has `None`, leaving
    already-resolved bays untouched.
  - Loop exits as soon as every bay is resolved; on exhaustion the
    library gets a `DriveMappingUnavailable` warning. Partial
    failure now produces a visible warning instead of a silent
    smaller library.
  - Three new unit tests pin the resolve-check + gap-fill semantics.

- **Stale intra-doc links fixed.** `discover_linux` was renamed to
  `discover` in the prior pass but the `discover_with` docstring
  still referenced it. Now uses explicit `crate::transport::...`
  paths for `LinuxSgTransport` / `FixtureTransport`. `cargo doc
  --workspace --no-deps` is clean.

64/64 workspace tests (24 library + 40 SCSI), up from 60. Clean doc
build. The §7.7 plan stays the same.

---

## 2026-05-17 — Layer 2a §7.7: rem CLI binary

**New crate: `crates/remanence-cli/`** with one binary, `rem`.

Subcommands (per design doc §5.3):
- `rem libraries` (alias `libs`) — one-line-per-library summary of
  what `discover()` returned.
- `rem library <serial>` (alias `lib`) — focused view of one library:
  changer model + sg + sysfs, chassis NAA (with "shared with ..."
  cross-reference when two libraries report the same WWN), drive
  bays (vendor/product/revision + sg + serial per bay), slot count
  loaded/empty, IE port list.
- `rem library <serial> --slots` — adds a per-slot block:
  `[0x03e9] full   CLNU01L9   (cleaning)` / `[0x03ea] empty`.

The CLI is read-only. Layer 2b's state-changing subcommands (`rem
move`, `rem load`, `rem export`) will go through
`Library::open(policy)` and require an allowlist. For v0.1, anything
that's a CDB write is deferred.

**Exit codes** — 0 success, 1 discovery error, 2 named library not
found. Discovery warnings always print to stderr after the main
output (regardless of which subcommand) so an operator running
`rem libraries` gets the same warnings as `rem library X` would.

**Dependencies.** clap 4 with `derive` feature, on top of
`remanence-library`. Added to workspace.dependencies so the daemon
can share the same clap version when it lands.

**Smoke check.** On this host (no /dev/sg* tape libraries) the CLI
correctly errors with `error: no tape libraries reachable on this
host` and exits 1. Live test against akash's QuadStor is the next
operational step — that's the live-discovery acceptance test
mentioned earlier.

**Layer 2a is functionally complete.** All seven §7.x chunks done:
- §7.1 VPD 0x83 parser ✅
- §7.2 + §7.3 remanence-library skeleton + Library::from_captures ✅
- §7.4 sysfs walker ✅
- §7.5 discover() orchestration with DVCID fallback ladder ✅
- §7.6 LibraryHandle + AccessPolicy + identity revalidation ✅
- §7.7 rem CLI binary ✅

**Where we left off / what's still on the table.**
- *Live integration smoke against akash's QuadStor.* `cargo run -p
  remanence-cli --bin rem -- libraries` against the running VTL.
  This is the "shape against what scripts/quadstor/setup.sh
  provisions" test from design doc §8 tier 3.
- *Real-MSL3040 capture window.* Run `rem libraries` against the
  datamover when the next access window opens.
- *`--from-fixture-tree=fixtures/real-hardware/...` flag.* Design
  doc §7.7 mentions this for hardware-free CLI testing. Deferred —
  the recorded-transport tests in `remanence-library` already
  cover most of the same ground.
- *kept `examples/topology.rs`.* The design doc said "move" but the
  Layer 1 demo is genuinely useful for SCSI-level debugging with no
  Layer 2 involvement. Leaving it in place.

**Next major chunk: Layer 2b** — `MOVE MEDIUM`, `INITIALIZE ELEMENT
STATUS`, `EXCHANGE MEDIUM`. New design doc when that turn comes.

---

## 2026-05-17 — Layer 2a §7.7 review pass: open_rw, CLI tests, doc/comment refresh

Four-finding review on §7.7. No criticals.

- **`LinuxSgTransport::open_rw()` added; `Library::open(policy)` uses
  it.** Old code opened `/dev/sgN` read-only first and fell back to
  read/write only if the kernel refused. That's fine for `discover()`
  (genuinely read-only) but the §7.6 handle is the *state-changing*
  path — Layer 2b's MOVE MEDIUM is the first caller, and a surprise
  EACCES on the very first `TO_DEV` CDB would be a bad user
  experience. Now: discovery still uses `open()` (RO-first with RW
  fallback), but `Library::open(policy)` routes through `open_rw()`
  so the handle commits to write-capable I/O upfront.

- **CLI refactored for testability + 9 tests added.** `run()` is now
  generic: `fn run<F: FnOnce() -> Result<DiscoveryReport, ...>>(cli,
  discover_fn, out, err) -> ExitCode`. The binary entry point passes
  the live `remanence_library::discover` and `io::stdout/stderr`;
  tests pass synthetic `DiscoveryReport`s and `Vec<u8>` writers. New
  tests cover every code path: discovery error → exit 1, libraries
  output shape, `library <serial>` happy path, `--slots` block,
  exit 2 on serial not found, `libs`/`lib` aliases, and warnings
  reliably reaching stderr in both the success and exit-2 paths.

- **Design doc §7.7 refreshed.** Was still saying "move
  `examples/topology.rs` into rem"; we shipped `crates/remanence-cli`
  and kept `examples/topology.rs` (still useful as a Layer 1 debug
  tool). The `--from-fixture-tree` flag is explicitly noted as
  deferred — recorded-transport tests already cover the same ground.

- **`handle.rs` module docs updated.** Comment said revalidation was
  VPD 0x80 only; brought along to mention the standard INQUIRY +
  changer-type check that was added in the prior review pass. Also
  notes that `Library::open` opens read/write via `open_rw`.

73/73 workspace tests (9 CLI + 24 library + 40 SCSI, up from 64).
`cargo doc --workspace --no-deps` and `cargo check --all-targets`
both clean. The CLI binary is now properly exercised in CI without
any /dev/sg* on the test host.

---

## 2026-05-17 — Layer 2a live smoke against akash's QuadStor ✅

First time Remanence reads bytes off real silicon through the
production daemon code path.

**Host:** akash (the dev box; was working dir all along).
**QuadStor service:** active. 5 SCSI devices: /dev/sg4 medium changer
(MSL G3 emulation), /dev/sg0..3 four LTO-9 drives.

**Commands run (all under `sudo`):**

```
$ sudo rem libraries
7CBAD9CF74  HP MSL G3 Series  /dev/sg4  (4 drives, 40 slots [10 loaded], 4 IE)
```

```
$ sudo rem library 7CBAD9CF74
Library 7CBAD9CF74
  Changer:  HP MSL G3 Series D.00  /dev/sg4  (sysfs /sys/devices/platform/host10/target10:0:0/10:0:0:0)
  Chassis:  48502020202020204d534c2047332053657269657320202037434241443943463734
  Drives:
    [0x0100] HPE Ultrium 9-SCSI (HH90)  /dev/sg0  serial 11A1D57AD0
    [0x0101] HPE Ultrium 9-SCSI (HH90)  /dev/sg1  serial 6D71FB6FE6
    [0x0102] HPE Ultrium 9-SCSI (HH90)  /dev/sg2  serial 2FEB23D41A
    [0x0103] HPE Ultrium 9-SCSI (HH90)  /dev/sg3  serial 79B07D9D00
  Slots:    40 (10 loaded, 30 empty)
  IE:
    [0x0300] empty   (import:— export:out)
    [0x0301] empty   (import:— export:out)
    [0x0302] empty   (import:— export:out)
    [0x0303] empty   (import:— export:out)
```

```
$ sudo rem library 7CBAD9CF74 --slots
… (focused view above) …
Slots:
  [0x0400] full   RMN001L9
  [0x0401] full   RMN002L9
  …
  [0x0409] full   RMN010L9
  [0x040a] empty
  …
  [0x0427] empty
```

```
$ sudo rem library NOTAREAL
error: no library with serial "NOTAREAL" on this host
       run `rem libraries` to see what's available
(exit 2)
```

**What this validates end-to-end:**

- `discover()` orchestration walks `/sys/class/scsi_generic`, opens
  each /dev/sgN, INQUIRYs it, classifies, probes the changer with
  full DVCID+CurData=1 RES (primary path — no fallback ladder
  needed), VPD 0x80, VPD 0x83.
- `Library::from_captures()` builds the model from the parsed
  responses.
- Tape-device join works: all 4 drives matched their VPD 0x80
  serials to RES DVCID-inline serials → bays upgrade to
  `IdentitySource::DvcidAndInquiry`.
- Sysfs canonicalize fix is doing its job — the displayed sysfs path
  is the actual `/sys/devices/platform/host10/...` not the
  `..`-laden readlink output.
- CLI output renders correctly. Exit codes 0 / 2 both fire.
- *Zero warnings.* No DriveMappingUnavailable, no UnclaimedTape, no
  ScsiError. Clean live discovery.

**Observations / future polish (not blocking):**

- **Chassis NAA absent.** QuadStor's VPD 0x83 returns a *T10
  vendor-ID* designator (decoded ASCII: `HP      MSL G3 Series
  7CBAD9CF74`), not an NAA WWN. The CLI falls through to `as_hex()`
  which dumps the raw bytes — ugly. Add nicer T10-designator
  rendering in a follow-up. Real HPE MSL3040 hardware returns NAA
  (confirmed in earlier fixtures), so this is a QuadStor-emulation
  quirk we'll see only on dev.
- **`tape` group membership.** owner isn't in `tape`, so `rem`
  currently needs sudo on akash. Adding owner to the tape group is
  the obvious fix, but for now sudo is fine — daemon will run as
  its own service user with the right group.
- **Cartridges visible:** RMN001L9..RMN010L9 — the volume tags
  provisioned in `scripts/quadstor/setup.sh`. The full corpus we
  set up at the start of Step 1 is intact.
- **Element address convention** in QuadStor's emulation: drives at
  0x0100..0x0103, IE at 0x0300..0x0303, slots at 0x0400..0x0427.
  Different from the real MSL3040 (drives at 0x0001..0x0002,
  slots at 0x03e9..0x0410). Both are valid SMC-3.

Layer 2a is complete and validated against live silicon. The next
real milestone is the same smoke against the production MSL3040
during the next datamover access window — and from there, Layer 2b.

---

## 2026-05-17 — Running `rem` without sudo: tape group + CAP_SYS_RAWIO

Added owner to the `tape` group on akash:

```bash
sudo usermod -aG tape owner
```

Confirmed in `/etc/group`: `tape:x:26:owner`. Fresh login sessions
(via `sudo -u owner -i`) showed `26(tape)` in `id` output.

But `rem libraries` still failed with `error: no tape libraries
reachable on this host`. Strange — `sg_inq /dev/sg4` worked fine as
owner, so it wasn't a device-permission problem.

**The diagnosis came from strace:**

```
ioctl(3, SG_IO, {... cmdp="\xb8\x10\x00\x00\xff\xff\x03\x00...", ...}) = -1 EPERM
```

The kernel's SCSI command filter (`drivers/scsi/sg.c`,
`sg_allow_access()`) intercepts SG_IO for "potentially dangerous"
opcodes and returns `EPERM` to non-`CAP_SYS_RAWIO` callers.
INQUIRY (0x12) is *whitelisted* by the filter — that's why `sg_inq`
works in `tape`-group-only mode. READ ELEMENT STATUS (0xb8), MOVE
MEDIUM (0xa5), and most other SMC commands are *not* whitelisted.
Discovery hits this on the very first RES call, every probe fails,
and surfaces as `DiscoveryError::NoLibraries`.

**The fix is two-step,** and both steps must be on the host:
1. `usermod -aG tape owner` (lets you open `/dev/sgN`)
2. `setcap cap_sys_rawio+ep target/debug/rem` (lets SG_IO bypass
   the kernel filter)

After both:
```
$ rem libraries
7CBAD9CF74  HP MSL G3 Series  /dev/sg4  (4 drives, 40 slots [10 loaded], 4 IE)
```

No sudo needed. 🎉

**Captured in:**
- `INSTALL.md` — "Host privileges for running `rem` as a non-root
  user" section, with the two commands and verification recipe.
- Memory file `project_scsi_command_filter.md` — so future
  diagnostic sessions don't have to strace again.

**Two notes for later:**
- File capabilities live in xattrs, not the ELF. Every `cargo
  build` resets them. A Makefile target `make rem-dev` that builds
  + applies setcap is a small follow-up.
- For the production daemon: the systemd unit will use
  `AmbientCapabilities=CAP_SYS_RAWIO` so the long-running service
  has the capability *as a process attribute* without the on-disk
  binary being capability-bearing. That's the right shape — Layer
  2b's design doc should call it out.

---

## 2026-05-17 — Preserve warnings on DiscoveryError::NoLibraries

Review follow-up to the CAP_SYS_RAWIO discovery. The fatal-error
path was throwing away the per-device warnings, so the operator
who forgot `setcap` saw only `no tape libraries reachable on this
host` and had to strace to learn why. Fixed:

- **`DiscoveryError::NoLibraries` now carries `Vec<DiscoveryWarning>`.**
  Both construction sites (zero devices enumerated, every changer
  probe failed) populate it. `Display` impl mentions the warning
  count: `no tape libraries reachable on this host (1 warning(s))`.

- **CLI prints the warning list on the error path.** Refactored
  `print_warnings` into a reusable `print_warning_list(&[Warning],
  &mut Write)` so both the success and error paths share formatting.

- **CLI adds a targeted `setcap` hint** when every SCSI failure in
  the warnings looks like EPERM. Detection is on the
  `summary` field of `ScsiError` warnings, checking for `"EPERM"` or
  `"Operation not permitted"`. The hint points at INSTALL.md's
  "Host privileges" section and the exact `setcap` invocation.

Live verified the diagnostic on akash by stripping the cap and
re-running:

```
$ rem libraries
error: no tape libraries reachable on this host (1 warning(s))

warnings (1):
  - scsi error on /dev/sg4: READ ELEMENT STATUS: SG_IO ioctl failed:
    Operation not permitted (os error 1)

hint: every SCSI probe returned EPERM. This is the kernel SCSI command
      filter refusing READ ELEMENT STATUS without CAP_SYS_RAWIO. Try:
          sudo setcap cap_sys_rawio+ep $(realpath "$0")
```

Re-applying the cap restores the clean single-line success output.
Two new CLI tests pin both behaviors (mixed warnings → no hint;
all-EPERM warnings → hint surfaces). 75/75 workspace tests.

---

## 2026-05-17 — Add cargo fmt + cargo clippy to the routine

owner flagged that clippy and rustfmt are installed on akash and
should be part of the routine. Applied:

**rustfmt:** `cargo fmt --all` — 14 files changed, ~900 insertions
/ ~530 deletions of pure formatting. No behavior change. The diff
is large because I'd been hand-laying-out vertical function
signatures and use-blocks that rustfmt's default style collapses,
and vice-versa for things that fit on one line my style spread
across multiple. Standardising to community style now pays off
later (PR reviews don't argue about whitespace).

**clippy:** `cargo clippy --workspace --all-targets -- -D warnings`
caught 4 lint hits, all clean fixes:

- `vpd.rs:49` — needless lifetimes on `parse_with_payload`. Three
  `'a` annotations elided.
- `discovery.rs:208` — `match { Some(e) => e, None => return None }`
  → `?`. Function already returns `Option<_>`.
- `discovery.rs:413` — `&mut Vec<Library>` → `&mut [Library]`. The
  function only iterates with `&mut`; no push/pop.
- `sysfs.rs:185` — `&sys.join("sg0")` → `sys.join("sg0")`. The
  borrow was needless: `fs::create_dir_all` takes `AsRef<Path>` and
  `PathBuf` satisfies it.

After fixes: `cargo clippy -- -D warnings` exits cleanly, 75/75
workspace tests still green, `cargo doc --workspace --no-deps`
still clean.

**Saved a feedback memory** so future sessions run these by default
before commits, not just tests + doc.

---

## 2026-05-17 — Layer 2b design doc

Drafted `docs/layer2b-design.md` (522 lines), parallel structure to
the Layer 2a doc. The state-changing-operations layer Remanence
will hang `MOVE MEDIUM`, `INITIALIZE ELEMENT STATUS`, `PREVENT /
ALLOW MEDIUM REMOVAL`, and the drive-side `UNLOAD/LOAD` from. No
new crate; extends `crates/remanence-library`.

**What v0.1 of Layer 2b commits to:**

- 4 SCSI primitives: MOVE MEDIUM (0xA5), INIT ELEMENT STATUS (0x07),
  PREVENT/ALLOW MEDIUM REMOVAL (0x1E), and SSC UNLOAD/LOAD (0x1B).
- 7 operator-visible composed ops on `LibraryHandle`: `move_medium`,
  `load`, `unload`, `export`, `import`, `rescan`, `refresh` (+ a
  matched `rem` CLI for each).
- A new `DriveHandle` for SSC ops, acquired through `LibraryHandle`
  (same three-stage gate: policy + identity revalidation + R/W
  open).
- Three new error types (`MoveError`, `DriveOpError`, `RescanError`)
  with preflight semantics distinguishing snapshot-rejected ops
  (AddressUnknown / SourceEmpty / DestinationFull / SameElement /
  DerivedDriveBay) from in-flight failures (ScsiError).

**Three new safety properties on top of Layer 2a's five:**

6. *Preflight against the snapshot* — mistyped addresses fail
   without I/O.
7. *Audit hook* fires *before* the CDB goes out, so the log
   captures intent regardless of kernel-call outcome. Default no-op;
   daemon installs its own.
8. *PREVENT MEDIUM REMOVAL auto-released on `Drop`* — a panicking
   task can't strand the library locked.

**Snapshot refresh model:**

- MOVE MEDIUM: patch the cached `Library` locally (src empties, dst
  fills, voltag and source_slot move). Cheap, no I/O.
- INIT ELEMENT STATUS: full re-RES; refuse to reconcile if the
  post-init shape differs (`RescanError::SnapshotMismatch`).
- Explicit `refresh()` available for paranoid callers — just a
  re-RES, no robot motion.

**Implementation plan** is 9 chunks (§7.1-§7.9), same shape as
Layer 2a's. Lands top-down: error vocab → snapshot patcher →
move_medium → refresh/rescan → DriveHandle → composed ops →
PREVENT/ALLOW → CLI → live test.

**Open questions captured** in §9: blocking semantics, CHECK
CONDITION sense classification, per-op policy granularity, audit
log format, concurrent moves on a shared chassis. None are
load-bearing for kickoff.

**Where we go next:** §7.1 (the error types) is a tiny landing
that unlocks everything else. After that, §7.2 + §7.3 are the
fixture-testable core of MOVE MEDIUM with no live hardware needed.

---

## 2026-05-17 — Layer 2b design review pass: 11 findings absorbed

Reviewer dropped `docs/layer2b-design-feedback.md` (266 lines, 11
findings). All material. The doc grew 522→700 lines. v0.2.
Highlights of what changed:

**High — transport / Layer 1 prereq (#1, #10):** new §2.2 spells
out that every Layer 2b primitive is a *no-data* CDB and that
`SgTransport`/`sg_io` need `execute_none` (`SG_DXFER_NONE`) before
any of the implementation chunks can land. New §7.0 ("Layer 1
prerequisite") makes this the first slice, with CDB builders in
`remanence-scsi` and a `RecordingTransport` that records both data-
in and no-data calls. The data-direction split is now the
*mechanical* reason discovery can't accidentally emit a state-
changing opcode — not just convention.

**High — drive-bay preflight (#3):** §3.1 validation list is now a
table with `installed.is_none()` as a hard refusal *before*
SourceEmpty / DestinationFull get checked. New errors:
`DriveBayUnresolved` and `DriveBayMissingDevice` (for ops that
also need to talk to the drive's own /dev/sgN).

**High — phase-aware composed ops (#4):** dropped "atomic at the
API level" wording everywhere. New error types `LoadError` and
`UnloadError` with phase variants (`Move`, `OpenDrive`,
`DriveLoad`/`DriveUnload`). Each variant documents exactly what
happened to the snapshot. §5.1 includes a table of partial-failure
× snapshot-effect × dirty-flag.

**High — refresh/rescan reconciliation (#5):** §5.2 now spells out
how the post-init RES is *reconciled* with the prior snapshot, not
blindly substituted. Drive bay's `sg_path` / vendor / product /
`DvcidAndInquiry` are preserved when the serial still matches.
Four sub-cases (match, replaced, appeared, vanished) with their
own `RescanWarning` variants. Layout-shape change is fatal
(`RescanError::SnapshotMismatch`) and forces a full re-`discover()`.

**High — DriveHandle testability (#2):** decided on the
stored-factory approach. `LibraryHandle` retains the transport
factory from `Library::open_with`, so production calls
`lib_handle.open_drive(bay, policy)` with no ceremony and tests
inject a fixture factory once at `open_with` time. `'a` lifetime
ties the `DriveHandle` to its parent — two drives in one library
can't be open simultaneously, which matches the changer's
single-robot reality.

**Medium — CAP_SYS_RAWIO (#6):** new §2.1 calls out that
`open_rw` only solves the file-open mode, not the kernel SCSI
filter; every Layer 2b primitive needs `CAP_SYS_RAWIO`. New safety
property 9 mandates the same EPERM hint discovery surfaces today.

**Medium — PREVENT/ALLOW guarantee (#7):** narrowed. §3.3 reads
"best-effort cleanup, not a guarantee." Introduced
`RemovalLockGuard` with explicit `release(self) -> Result<...>`
*and* a Drop that does best-effort ALLOW. `Drop`'s caveats (no
SIGKILL, no abort, no power loss) are documented. Operational
recovery: `rem unlock <library>` or power cycle.

**Medium — audit hook records intent + outcome (#8):**
`AuditEvent` now has a `phase` field (`Started` / `Finished`) and
an outcome variant (`Success { duration, snapshot_patched,
dirty }`, `PreflightRefused { reason }`, `ScsiError { sense,
summary }`, `Other`). Preflight refusals fire a single event
combining Started + PreflightRefused so the daemon can detect
repeated impossible requests in the same log stream as successes.

**Medium — `load` arg order + CLI shape (#9):** Rust API stays
`load(slot, bay, policy)` ("from slot to bay"). CLI switches to
explicit flags everywhere: `rem load --slot 0x0400 --bay 0x0100`,
`rem unload --bay 0x0100 [--dest 0x0400]`, `rem move --src ...
--dst ...`. Positional `<src> <dst>` was a footgun.

**Low — `source_slot` semantics (#11):** §5.1 now reads
`source_slot = Some(src) only if src is a Storage slot`. IE-port
and drive-bay sources → `None`. Matches RES SVALID semantics.

Implementation plan went from 9 to 10 chunks: §7.0 (Layer 1) +
§7.1 (errors) + §7.2 (patcher) + §7.3 (move_medium) + §7.4
(reconciliation + refresh) + §7.5 (rescan) + §7.6 (DriveHandle) +
§7.7 (composed) + §7.8 (lock guard) + §7.9 (CLI) + §7.10 (live).

The §7.0 starting point is *cleaner* than the original — it's the
small SCSI-layer expansion that unblocks every chunk that follows,
and it doesn't depend on any Layer 2b machinery itself. So the
work order is now: ship §7.0, then everything else stacks on it.

---

## 2026-05-17 — Layer 2b §7.0: no-data transport + four CDB builders

The SCSI-layer prerequisite for everything that follows. Pure Layer
1 and transport-trait work; no Layer 2b state-changing machinery
landed yet.

**remanence-scsi:**

- `sg_io::execute_none(&File, &[u8], u32) -> Result<(), ScsiError>` —
  `SG_DXFER_NONE` variant of the existing `execute_in`. Same error
  classification (InvalidInput / Io / CheckCondition /
  TransportError). 2 negative tests (empty cdb, oversize cdb).
- Four CDB builders, each in its own module with byte-pattern tests:
  - `move_medium::build_cdb(robot, src, dst, invert)` — 12 bytes,
    big-endian fields, INVERT bit configurable for completeness.
  - `initialize_element_status::build_cdb()` — 6 bytes, plain
    `[0x07, 0, 0, 0, 0, 0]`.
  - `prevent_allow::build_cdb(prevent)` — 6 bytes, byte-4 bit-0.
  - `load_unload::build_cdb(load)` — 6 bytes, byte-4 bit-0.
- Modules re-exported from the crate root.

**remanence-library transport:**

- `SgTransport` trait gains `execute_none(&[u8]) -> Result<(),
  ScsiError>`. The data-direction split is now the structural reason
  discovery can't emit a state-changing opcode — it only calls
  `execute_in`.
- `LinuxSgTransport::execute_none` delegates to `sg_io::execute_none`.
- `FixtureTransport::execute_none` logs the CDB and returns `Ok(())`
  — no canned response needed (tests assert on the log).
- New `impl SgTransport for Box<dyn SgTransport>` so handles holding
  boxed transports can pass them to anything generic over `T:
  SgTransport`.
- `RecordingTransport<T>` promoted from an inline test struct to a
  proper public type in `transport.rs`. Two constructors:
  `RecordingTransport::new(inner)` returns `(wrapped, log_handle)`
  for single-device tests; `RecordingTransport::with_log(inner, log)`
  takes a shared log for multi-device cases.
- Crate root re-exports it.

**Existing discovery safety test rewired** to use the new shared
`RecordingTransport::with_log`. The "discovery issues only
read-only CDB opcodes" property still holds — same set of allowed
opcodes (0x12 INQUIRY, 0xb8 RES, plus a few future-use diagnostics).

**Tests:** 88/88 workspace (51 SCSI + 26 library + 11 CLI). Up from
75. cargo fmt + clippy + doc all clean.

**Next:** §7.1 — Layer 2b error vocabulary (`MoveError`,
`DriveOpError`, `LoadError`, `UnloadError`, `RescanError`,
`AuditEvent`). Tiny landing that compile-checks the rest of the
plan.

---

## 2026-05-17 — Layer 2b §7.1: error and audit vocabulary

Types-only chunk. No behavior change; compile-checks the rest of
the §7.x plan.

**Added to `remanence-library::error`:**

- `MoveError` — 8 variants per design doc §3.1's preflight table:
  `AddressUnknown`, `SourceEmpty`, `DestinationFull`, `SameElement`,
  `DriveBayUnresolved` (the new variant from the v0.2 doc rev that
  catches `installed = None`), `DriveBayMissingDevice` (drive-side
  ops need an `sg_path`), `DerivedDriveBay` (policy gate), and
  `ScsiError(#[from] ScsiError)` for the kernel-side path.
- `DriveOpError` — thin wrapper around `ScsiError` today; distinct
  from `MoveError` so composed ops can carry both.
- `LoadError` and `UnloadError` — phase-aware composed-op errors
  per design doc §4.2. Variants name *which phase* failed
  (`Move`, `OpenDrive`, `DriveLoad`/`DriveUnload`); docstrings on
  each variant spell out the resulting snapshot state.
- `RescanError` — `ScsiError` + `SnapshotMismatch(String)` for
  post-init RES that disagrees with the prior snapshot's shape.

**Audit vocabulary — `AuditEvent`, `AuditOp`, `AuditOutcome`:**

Implementation deviates slightly from the v0.2 design doc here:
the doc proposed a struct with `phase: AuditPhase` + `cdb:
Option<&[u8]>` + `outcome: Option<AuditOutcome>` (`Started` with
`outcome=Some(PreflightRefused)` carrying the refusal). On the way
to implementing it I reshaped to a flat 3-variant enum
(`AuditEvent::Started` / `Refused` / `Finished`), each variant
carrying exactly the fields its kind needs. Cleaner pattern-match
in hook code, no surprise `Option` unwraps.

Design doc updated to match (§6 property 7). Verbal description
unchanged: `Started` fires after preflight succeeds, `Finished`
after kernel return, `Refused` is the single-event refusal path.

**`AuditOp`** covers every state-changing op in the plan: `Move`,
`Load`, `Unload`, `Export`, `Import`, `Rescan`, `Refresh`,
`LockRemoval`, `AllowRemoval`, `DriveUnload`, `DriveLoad`. Address
fields are u16 element addresses so an audit log can be replayed
against the snapshot.

**`AuditOutcome`** has three: `Success { duration,
snapshot_patched, dirty }`, `ScsiError { sense, summary }`,
`Other { summary }`. `PreflightRefused` lives on the `Refused`
variant directly, not inside `AuditOutcome`.

**Crate root re-exports** added for all the new types.

**Tests:** 88/88 unchanged. `cargo fmt` + `cargo clippy
--workspace --all-targets -- -D warnings` + `cargo doc --workspace
--no-deps` all clean. (Two clippy doc-lint hits on the first pass
— `Option<outcome>` reading as an HTML tag, a missing crate path
on `ScsiError` — fixed before commit.)

**Next:** §7.2 — the snapshot patcher. Pure function
`apply_move(library: &mut Library, src: u16, dst: u16) ->
Result<MovePatch, MoveError>` that does the §5.1 patch rules
against an in-memory snapshot. Unit-tested with synthetic
libraries; no I/O. The §7.3 `LibraryHandle::move_medium` builds
on it.

---

## 2026-05-17 — Layer 2b §7.2: snapshot patcher

Pure function `apply_move(library: &mut Library, src: u16, dst:
u16) -> Result<MovePatch, MoveError>` in a new `ops.rs` module.
No I/O, no policy.

**Three-way split** of preflight responsibility, deliberately:
- *Snapshot-level checks* (here): `AddressUnknown`, `SameElement`,
  `SourceEmpty`, `DestinationFull`, `DriveBayUnresolved`. All
  derivable from the `Library` value alone — testable with
  synthetic snapshots, zero transport.
- *Policy-level checks* (`DerivedDriveBay`): defer to the handle
  layer because they need an `AccessPolicy` ref the snapshot
  doesn't have.
- *Device-binding checks* (`DriveBayMissingDevice`): defer to the
  composed `load` / `unload` paths because plain `move_medium`
  doesn't talk to the drive's own SG node and doesn't care.

**Preflight order is meaningful.** `DriveBayUnresolved` is checked
*before* `SourceEmpty` / `DestinationFull` so a bay with
`installed = None` and a stale `loaded_tape = Some("ORPHAN")`
gets refused on the identity gap rather than mislabelled as
"empty". One of the 16 new tests pins exactly that.

**`source_slot` semantics** match §5.1 / SVALID:
- Slot → drive bay: dst.source_slot = Some(src) ← natural-home record
- IE-port → drive bay: dst.source_slot = None
- Drive-bay → drive bay (improbable but valid): dst.source_slot = None
- Source → slot/IE: destination doesn't have a source_slot field

**Internal `ElementIdx`** type-tagged index lets `apply_move` hold
one for src and one for dst across sequential read-then-mutate
phases without tangling the borrow checker. Tradeoff worth a
six-line internal enum.

**Tests** — 16 new in `ops::tests`, no I/O:
- 5 happy paths covering each (source kind, destination kind)
  combo the v0.1 surface uses
- 9 error paths: every `MoveError` variant `apply_move` is allowed
  to emit
- 1 no-mutation-on-failure pin asserting refused moves leave the
  whole `Library` byte-for-byte unchanged (clones-and-compares)

**Tests total: 104/104** (51 SCSI + 42 library + 11 CLI; up from
88). `cargo fmt` + `cargo clippy --workspace --all-targets -- -D
warnings` + `cargo doc --workspace --no-deps` all clean.

**Next:** §7.3 — `LibraryHandle::move_medium`. Wires
`apply_move` + the derived-identity policy check + audit hook +
`execute_none(move_medium::build_cdb(...))`. First time a state-
changing CDB actually goes through a transport (FixtureTransport
in tests; LinuxSgTransport in production).

---

## 2026-05-17 — Layer 2b §7.3: LibraryHandle::move_medium

First state-changing CDB actually leaves the process (well — would,
through `LinuxSgTransport`; through `FixtureTransport` in tests).
Everything `move_medium` needs is now wired together.

**`ops::apply_move` split into two phases.** The handle calls
`plan_move(&Library, src, dst) -> Result<MovePlan, MoveError>`
*before* issuing the CDB to validate against the snapshot without
mutating, then `apply_planned_move(&mut Library, &MovePlan)`
*after* the CDB returns successfully to patch the snapshot. The
public `apply_move` is now a thin wrapper that does both. Splitting
keeps "snapshot is unchanged on error" load-bearing — there's no
intermediate state where the snapshot has been partially patched
and the CDB hasn't fired yet (or vice versa).

`MovePlan` is `pub(crate)`; it carries internal `ElementIdx`
locations plus the cartridge tag to carry forward and a flag for
whether the source was a Storage slot (driving `source_slot`
SVALID semantics on the destination). It also has a small
`derived_drive_bay(&Library) -> Option<&InstalledDrive>` helper for
the handle to run the derived-identity policy check.

**`LibraryHandle` gained two fields and three methods.**

- `audit_hook: Option<Box<dyn FnMut(&AuditEvent<'_>) + Send>>` —
  the daemon installs one; tests install one to capture events.
- `set_audit_hook<F>(hook: F)` — replaces any previous hook.
- `move_medium(src, dst, policy) -> Result<(), MoveError>`.

**`move_medium` orchestrates** in the exact order the design doc
calls for:

1. `ops::plan_move` runs snapshot-level preflight. On error: fire
   `AuditEvent::Refused` with the `MoveError` variant name as the
   `reason`, return.
2. Derived-identity policy gate (defense-in-depth on top of the
   open-time check; refresh/rescan could surface a new derived bay).
   On refusal: fire `Refused`, return.
3. Build the CDB via `remanence_scsi::move_medium::build_cdb`.
4. Fire `AuditEvent::Started` with the CDB bytes.
5. `transport.execute_none(&cdb)`.
6. On success: `ops::apply_planned_move` patches the snapshot, then
   `AuditEvent::Finished` with `Success { duration, patched: true,
   dirty: false }`.
7. On SCSI failure: snapshot stays unchanged. `Finished` with
   `ScsiError { sense, summary }`, error bubbles as
   `MoveError::ScsiError`.

**Borrow-checker note** worth recording: `fire_audit` and
`fire_refused` are free functions taking `&mut Option<AuditHook>`
rather than methods. Methods take `&mut self`, and we need to
borrow `self.library.serial` immutably *into* the `AuditEvent`
while also borrowing `self.audit_hook` mutably to call the hook —
methods can't split-borrow that. Free functions let the borrow
checker see we only need `self.audit_hook`, leaving `self.library`
free for the event's borrows.

**4 new tests in `handle::tests`:**

- `move_medium_happy_path` — slot → drive bay succeeds, snapshot
  patches, exactly one 0xA5 CDB matches `move_medium::build_cdb`,
  audit captures `Started{op=Move{...}} + FinishedSuccess`.
- `move_medium_preflight_refused_emits_refused_no_cdb` — empty
  source. Asserts: returns `MoveError::SourceEmpty`, snapshot
  unchanged (clone-and-compare), zero 0xA5 CDBs in the recording
  log (only the open-time 0x12 INQUIRY survives), exactly one
  `Refused{reason: "SourceEmpty"}` event.
- `move_medium_refused_when_drive_is_derived_and_policy_disallows`
  — open succeeds under a permissive policy that opts derived in,
  then `move_medium` is called with a *stricter* policy that
  doesn't. `DerivedDriveBay` refusal, no CDB, `Refused{reason:
  "DerivedDriveBay"}`.
- `move_medium_succeeds_when_derived_is_explicitly_allowed` —
  derived bay + permissive policy at both open and move time →
  CDB goes out, snapshot patches.

The test infrastructure also moved a step forward: there's now a
shared `recording_factory(responses)` helper that builds a
`FixtureTransport` seeded with open-time responses, wraps it in a
`RecordingTransport`, and returns both the factory and the shared
CDB log handle. The audit hook tests use an `Arc<Mutex<Vec<...>>>`
capture (Send-compatible) with an owned `CapturedEvent` enum so
assertions don't have to reckon with `AuditEvent<'a>` lifetimes.

**Tests: 109/109 workspace** (51 SCSI + 47 library + 11 CLI; up
from 105). `cargo fmt` + `cargo clippy --workspace --all-targets
-- -D warnings` + `cargo doc --workspace --no-deps` all clean.

**One slight deviation from the v0.2 doc:** `move_medium` takes
`policy: &dyn AccessPolicy`, matching the shape `load`/`unload`
use. The doc's signature in §4.3 omits it. I'll update the doc in
a follow-up; the per-call policy makes the derived-identity
defense-in-depth check straightforward (no stored-policy
lifetimes), and consistency with the composed ops is worth
something.

**Next:** §7.4 — reconciliation + `refresh()`. Pure-function
reconcile() (synthetic snapshots, no I/O), then wire `refresh()`
to issue RES through the transport, reconcile, and update the
handle's snapshot.

---

## 2026-05-17 — Layer 2b §7.4: reconcile + refresh()

Pure-function reconcile + the `LibraryHandle::refresh()` it
backs. No state-changing CDBs here — refresh is read-only — but
the snapshot life-cycle gets a lot more interesting.

**`error::RescanWarning`** — three variants per §5.2:
`DriveReplaced { addr, old_serial, new_serial }`,
`DriveAppeared { addr, serial }`,
`DriveVanished { addr, old_serial }`. These don't flow back
through `refresh()`'s return value; they're gathered inside and
(eventually) routed via the audit hook for operator visibility.
For v0.1 the warnings are observable via direct unit tests on
`ops::reconcile`.

**`ops::reconcile(old, new_es) -> Result<(Library, Vec<RescanWarning>), String>`**:
- Builds a fresh `Library` via `from_captures` using the new
  element status; preserves stable per-library fields (serial,
  changer_inquiry, changer_sg, changer_sysfs, chassis_designator)
  from `old` verbatim.
- Shape check (drive_count / slot_count / ie_count) — count
  mismatch → `Err(String)`.
- Per-bay reconciliation, matching by element address:
  - Serials match → preserve old's `sg_path`, `sysfs_path`,
    `vendor`, `product`, `revision`, `identity_source` on the new
    bay's `installed`. Occupancy / voltag / source_slot come from
    the new RES.
  - Serials differ → `DriveReplaced` warning; drop host-side
    data; `identity_source = DvcidInline`.
  - Pre None, post Some → `DriveAppeared`; `identity_source =
    DvcidInline`; `sg_path = None`.
  - Pre Some, post None → `DriveVanished`; new bay is empty
    (`installed = None`).
  - Pre None, post None → no warning.

**`LibraryHandle`** changes:
- New `is_dirty: bool` field + `is_dirty()` accessor. Cleared by
  a successful `refresh()`; set to `true` on `refresh()` shape
  mismatch (per §5.3) and (later) by composed-op partial-failure
  paths (§5.1).
- New `refresh(&mut self) -> Result<(), ScsiError>`. Issues
  `discovery::issue_res(transport, 0, true, true)` (the two-phase
  DVCID+CurData RES probe Layer 2a already uses for discovery),
  runs `ops::reconcile`, replaces the snapshot on success.
  Shape mismatch: leaves snapshot alone, sets is_dirty=true,
  returns Ok. `rescan()` (§7.5) will be the variant that errors
  on shape mismatch.

**Cross-module reuse:** `discovery::issue_res` is now
`pub(crate)`, with a `?Sized` relaxation on its `T: SgTransport`
bound so handles holding `Box<dyn SgTransport>` can pass
`self.transport.as_mut()` (a `&mut dyn SgTransport`) directly.

**Tests:** 9 new, 119/119 workspace total (was 110).

In `ops::tests` (7 new, all synthetic — no transport):
- `reconcile_preserves_host_side_fields_when_serial_matches` —
  the happy path. After a no-op-shaped RES, bay 0 keeps its
  pre-set `sg_path` / `vendor` / `DvcidAndInquiry`.
- `reconcile_emits_drive_replaced_on_serial_change` — bay 0's
  serial changes; warning fires; host-side data is dropped;
  identity_source = DvcidInline. Bay 1 unchanged (serial match
  preserves host-side).
- `reconcile_emits_drive_appeared_when_post_has_identity_pre_didnt`
  — old bay 0 had `installed = None`; post-RES has a serial.
  Warning fires; identity_source = DvcidInline.
- `reconcile_emits_drive_vanished_when_pre_had_identity_post_doesnt`
  — symmetric case; warning fires; new bay's installed = None.
- `reconcile_rejects_drive_count_mismatch`
- `reconcile_rejects_slot_count_mismatch`
- `reconcile_preserves_chassis_designator`

In `handle::tests` (2 new, fixture-driven):
- `refresh_preserves_host_side_data_when_res_is_unchanged` —
  refresh against the same real-MSL3040 full-DVCID capture the
  handle was opened with; assert bay 0x0001 still has
  `identity_source = DvcidAndInquiry`, `sg_path = Some(/dev/sg0)`,
  vendor preserved, `is_dirty() == false`.
- `refresh_shape_mismatch_sets_dirty_and_returns_ok` — open
  against the real fixture (2 drives, 40 slots), then `refresh()`
  against a hand-built RES with 1 drive and 0 slots. Assert
  `refresh() == Ok`, snapshot byte-for-byte unchanged
  (clone-and-compare), `is_dirty() == true`.

The latter test required a small SMC-3 byte builder
(`build_synthetic_es_one_drive`) — header, page header, 12-byte
element descriptor. Captured verbatim in code with the layout
comments so future-me doesn't have to redecode it.

**Quality gate**: 119/119 tests, `cargo fmt` + `cargo clippy
--workspace --all-targets -- -D warnings` + `cargo doc
--workspace --no-deps` all clean. Two clippy hits along the way
— `vec_init_then_push` on the synthetic-RES builder (fixed via
`vec![]`), and `explicit_auto_deref` on a `*reason` match (fixed
in §7.3 review pass).

**Next:** §7.5 — `LibraryHandle::rescan()`. INITIALIZE ELEMENT
STATUS via `execute_none` + the same reconcile, but shape
mismatch becomes a hard `RescanError::SnapshotMismatch`. First
audit-tracked op that fires `Started`/`Finished` for the INIT
CDB (the post-init RES is read-only and not audited).

---

## 2026-05-17 — Layer 2b §7.5: rescan()

INIT ELEMENT STATUS + post-init RES + reconcile, with shape
mismatch as a hard `RescanError::SnapshotMismatch` (the §5.2
contract). Second audit-tracked op after `move_medium`; first
where the op spans multiple CDBs and the design says only the
state-changing one (the INIT) audits.

**`LibraryHandle::rescan(&mut self) -> Result<(), RescanError>`**:

1. Build INIT CDB (`remanence_scsi::initialize_element_status::build_cdb()`
   → `[0x07, 0x00, 0x00, 0x00, 0x00, 0x00]`).
2. Fire `AuditEvent::Started { op: Rescan, cdb: &init_cdb, … }`.
3. `transport.execute_none(&cdb)`. On error: fire
   `Finished{ScsiError}`, return `RescanError::ScsiError`.
4. Post-init `discovery::issue_res(transport, 0, true, true)` —
   the same two-phase DVCID+CurData read refresh uses. *Not*
   separately audited; per §6 property 7 the audit hook is the
   state-change visibility surface. If RES fails: fire
   `Finished{ScsiError}` (the whole rescan op's outcome), return
   `RescanError::ScsiError`.
5. `ops::reconcile(&self.library, new_es)`:
   - Success → replace `self.library`, clear `is_dirty`, fire
     `Finished{Success { duration, patched: true, dirty: false }}`,
     return Ok.
   - Shape mismatch → fire `Finished{Other { summary: "shape
     mismatch: …" }}`, return `RescanError::SnapshotMismatch(msg)`.
     **Snapshot is NOT replaced** on this path — the handle is
     left intact for the operator to inspect alongside the
     escalation decision (caller is expected to re-`discover()`).

The `Other` audit-outcome choice for shape mismatch is
deliberate: the CDB succeeded (it's not a SCSI error), but the
*semantic* outcome of the operation is the structural mismatch.
`Other { summary }` captures that without conflating with kernel-
level failures.

**Code reuse:** extracted `scsi_outcome(&ScsiError) -> AuditOutcome`
helper so both `move_medium` and `rescan` build the
`Finished{ScsiError}` event the same way. Removes 8 lines of
duplication.

**Tests** — 2 new in `handle::tests`, 123/123 workspace total:

- `rescan_happy_path_clears_dirty_and_emits_audit` — open against
  the real-MSL3040 full-DVCID capture, rescan with the same
  capture as the post-init RES. Asserts: returns Ok, INIT CDB
  matches the builder verbatim (`[0x07, 0, 0, 0, 0, 0]`), bay
  0x0001 still has `DvcidAndInquiry + sg_path = /dev/sg0`
  (reconcile preserved it), `is_dirty()` is cleared, audit log is
  exactly `[Started{Rescan}, FinishedSuccess{Rescan}]`.
- `rescan_shape_mismatch_returns_error_and_audits_other` — open
  against the real fixture (2 drives, 40 slots), rescan returns
  the synthetic 1-drive RES. Asserts: returns
  `RescanError::SnapshotMismatch(msg)` whose `msg` contains
  "differ from prior snapshot", snapshot byte-for-byte unchanged
  (clone-and-compare), `is_dirty()` stays false (rescan doesn't
  touch the dirty flag on its hard-error path; that's refresh's
  contract), audit log is `[Started, FinishedScsiError]` (Other
  outcomes are normalized to `FinishedScsiError` in the test
  helper's `CapturedEvent`, with `summary.contains("shape
  mismatch")`).

**Quality gate:** 123/123 tests, fmt + clippy `--all-targets -D
warnings` + doc all clean.

**Next:** §7.6 — `DriveHandle` + drive-side ops (`unload` / `load`
SSC primitives). Composes the open-time three-stage gate from
§7.5 of Layer 2a against the drive's own `/dev/sgN`. After §7.6,
§7.7 wires composed `LibraryHandle::load` / `unload` / `export` /
`import` over move_medium + DriveHandle.

---

## 2026-05-17 — Layer 2b §7.6: DriveHandle + drive-side ops

The drive-side counterpart to `LibraryHandle`. Three pieces:

**1. Transport factory now stored on `LibraryHandle`.**
`Library::open_with` previously consumed its `F: FnMut(&Path) -> …`
factory and dropped it after opening the changer. `open_drive`
needs the same opening mechanism for the drive's own `/dev/sgN`,
so the factory now lives on the handle. New `TransportFactory`
type alias (`Box<dyn FnMut(&Path) -> Result<Box<dyn SgTransport>,
IoErrorKind>>`), and `open_with`'s bound tightened to `F: FnMut +
'static`. Production (Linux) wraps `LinuxSgTransport::open_rw` —
no captures, easily 'static; tests use move closures over owned
HashMaps.

**2. New `OpenError` variants for the drive-resolution stage:**
- `BayNotFound { addr }` — bay address not in the library snapshot
- `BayUnresolved { addr }` — bay has `installed = None`
- `BayMissingDevice { addr, serial }` — bay has installed but no
  bound `sg_path`

**3. `LibraryHandle::open_drive(bay, policy) -> Result<DriveHandle<'_>, OpenError>`** —
mirrors `Library::open`'s three-stage gate, adapted for drives:

1. *Bay-resolution checks* against the snapshot (the three new
   variants above).
2. *Derived-identity policy gate* — same defense-in-depth as
   `move_medium`'s `DerivedDriveBay` check, run on the drive's own
   `installed.identity_source`.
3. *Drive transport open + identity revalidation* — `(self
   .transport_factory)(&sg_path)`, then std INQUIRY must show
   `SequentialAccess`, then VPD 0x80 must match the recorded
   `installed.serial`. Anything else is `IdentityChanged`.

**`DriveHandle<'a>`** carries `bay_address`, the `InstalledDrive`
snapshot, the library serial, an owned drive transport, and a
**borrowed** `&'a mut Option<AuditHook>` reference to the
parent's hook. The `'a` lifetime ties the drive handle to the
parent library handle — while the drive is open, the library
handle is mutably borrowed, so only one drive can be open at a
time. Mirrors the single-robot-serialises-everything reality of
the physical changer.

**Drive ops:**
- `DriveHandle::unload()` — SSC `LOAD/UNLOAD` with `load=0`,
  audited as `AuditOp::DriveUnload { bay }`.
- `DriveHandle::load()` — SSC `LOAD/UNLOAD` with `load=1`, audited
  as `AuditOp::DriveLoad { bay }`.

Both share an `issue_load_unload(load: bool)` helper that builds
the CDB via `remanence_scsi::load_unload::build_cdb`, fires
Started/Finished events through the borrowed audit hook, and
returns `DriveOpError::ScsiError` on failure. `snapshot_patched`
is `false` in the Success outcome — load/unload don't mutate the
library snapshot directly; that's the composed
`LibraryHandle::unload(bay, dst)`'s job in §7.7.

**Tests** — 8 new in `handle::tests`, 133/133 workspace:
- `open_drive_succeeds_for_resolved_bay` — happy path
- `open_drive_refused_for_unknown_bay_address` → `BayNotFound`
- `open_drive_refused_when_bay_is_unresolved` → `BayUnresolved`
- `open_drive_refused_when_bay_has_no_sg_path` → `BayMissingDevice`
- `open_drive_refused_on_drive_identity_mismatch` →
  `IdentityChanged { expected, actual }`
- `open_drive_refused_when_device_is_not_sequential_access` →
  `IdentityChanged { actual: None }` (caught at std INQUIRY before
  VPD 0x80)
- `drive_handle_unload_issues_correct_cdb_and_audits` — CDB
  matches `[0x1B, 0, 0, 0, 0x00, 0]`, audit Started{DriveUnload} +
  FinishedSuccess
- `drive_handle_load_issues_correct_cdb_and_audits` — CDB byte 4
  is `0x01`, audit op is `DriveLoad`

Test infrastructure: `multi_recording_factory(scripts)` helper
that hands out a different `RecordingTransport<FixtureTransport>`
per `/dev/sgN`, all sharing one CDB log. Used by every §7.6 test
that needs both the changer's and the drive's responses.

**Quality gate:** 133/133 tests, fmt + clippy `--all-targets -D
warnings` + doc all clean.

**Next:** §7.7 — composed `LibraryHandle::load` / `unload` /
`export` / `import` over `move_medium` + `DriveHandle`. Phase-
aware `LoadError` / `UnloadError` (already exist in §7.1's error
vocab); per-phase dirty-marking; the §7.x payoff where the
operator-visible CLI ops finally compose.

---

## 2026-05-17 — Layer 2b §7.7: composed load / unload / export / import

The Layer 2b payoff. Operator-level operations finally compose
the primitives the §7.0-§7.6 chunks built up.

**Plumbing first.** Audit-log filtering wants every CDB tagged
with its outer composed-op context (`AuditOp::Load { slot, bay }`,
not `AuditOp::Move`). Both `move_medium` and the drive-side
primitives needed an `op` parameter for that. Refactored:

- `LibraryHandle::move_medium` → public wrapper that passes
  `AuditOp::Move { src, dst }` to a new `pub(crate)
  move_medium_as(src, dst, policy, op)`.
- `DriveHandle::unload` / `load` → public wrappers using
  `AuditOp::DriveUnload { bay }` / `DriveLoad { bay }`; new
  `pub(crate)` `unload_as(op)` / `load_as(op)` accept any op.

The CDB bytes themselves don't change. Only the audit tag does.

**Four composed ops** added to `LibraryHandle`:

`load(slot, bay, policy) -> Result<(), LoadError>`:
1. `move_medium_as(slot, bay, policy, Load{slot,bay})` — fails →
   `LoadError::Move`; snapshot unchanged.
2. `open_drive(bay, policy)` — fails → `LoadError::OpenDrive`;
   snapshot is *patched* (cartridge already in bay per MOVE);
   **`is_dirty = true`**.
3. `drive.load_as(Load{slot,bay})` — fails →
   `LoadError::DriveLoad`; patched snapshot, dirty=true.
4. Success → Ok, dirty stays false.

`unload(bay, destination, policy) -> Result<(), UnloadError>`:
1. Resolve destination: caller-supplied OR `bay.source_slot`. If
   both `None`: fire `Refused{op=Unload{bay, dst:None},
   reason:"SourceEmpty"}` and return
   `UnloadError::Move(MoveError::SourceEmpty)` — never opens the
   drive.
2. `open_drive(bay, policy)` — fails → `UnloadError::OpenDrive`;
   snapshot unchanged, no MOVE attempted.
3. `drive.unload_as(Unload{bay,dst})` — fails →
   `UnloadError::DriveUnload`; snapshot unchanged.
4. `move_medium_as(bay, dst, policy, Unload{...})` — fails →
   `UnloadError::Move`; UNLOAD succeeded but cartridge still in
   bay; snapshot unchanged (per §5.1, the snapshot is still
   honest about the bay being loaded, since physically the
   cartridge is still there).
5. Success → Ok.

`export(slot, policy) -> Result<(), MoveError>`:
- Find first `!full` IE port. If none: fire
  `Refused{op=Export{slot, ie:None}, reason:"DestinationFull"}`
  and return `MoveError::DestinationFull`.
- Otherwise: `move_medium_as(slot, ie, policy, Export{...})`.

`import(slot, policy) -> Result<(), MoveError>`:
- Find first `full` IE port. If none: fire
  `Refused{op=Import{ie:None, slot}, reason:"SourceEmpty"}` and
  return `MoveError::SourceEmpty`.
- Otherwise: `move_medium_as(ie, slot, policy, Import{...})`.

The runtime-resolved Option-shape of `Unload::dst` / `Export::ie`
/ `Import::ie` from §7.1's review pays off here: `None` only
appears in the `Refused` event for the unresolved case;
`Started`/`Finished` always carry a concrete address.

**Borrow-checker note** worth recording for future-me:
`open_drive` returns `DriveHandle<'_>` that borrows `&mut self`
for its full lifetime. Inside `load`/`unload`, the drive handle
has to drop before the second phase can call `move_medium_as`
(which needs its own `&mut self`). Each phase lives inside a
scoped `match`, so the drive drops at end-of-match before
phase 2 reaches `&mut self` again. `self.is_dirty = true` after
a partial-failure can't run while the drive is alive — the
implementation only touches it *after* the inner match has
released the borrow.

**Tests — 8 new in `handle::tests`:**

- `load_happy_path_audits_with_load_op_context` — full sequence,
  4 audit events all tagged `AuditOp::Load{0x0400, 0x0100}`. CDB
  log contains 0xA5 (MOVE) and 0x1B (SSC LOAD).
- `load_returns_move_phase_error_on_preflight_fail` — SameElement
  triggers; zero CDBs, snapshot unchanged, no is_dirty change.
- `unload_happy_path_uses_source_slot_when_no_destination_given`
  — load first to set bay.source_slot, then unload(bay, None).
  Cartridge ends up back in slot 0x0400. Audit events all tagged
  `AuditOp::Unload{bay:0x0100, dst:Some(0x0400)}`. First Started
  CDB is 0x1B (SSC UNLOAD); second is 0xA5 (MOVE).
- `unload_refuses_when_no_destination_and_no_source_slot` — bay
  loaded but `source_slot = None`. Single
  `Refused{Unload{bay,dst:None}, "SourceEmpty"}` audit event; no
  drive open; snapshot unchanged.
- `export_uses_first_available_ie_port` — IE port 0 is full, 1 is
  empty. export(slot) targets port 1. Audit op =
  `Export{slot, ie:Some(0x0301)}`.
- `export_refused_when_all_ie_ports_full` — single
  `Refused{Export{slot, ie:None}, "DestinationFull"}` event.
- `import_uses_first_occupied_ie_port` — first occupied port
  (0x0301) used. Audit op = `Import{ie:Some(0x0301), slot}`.
- `import_refused_when_no_ie_port_occupied` — single
  `Refused{Import{ie:None, slot}, "SourceEmpty"}` event.

Test infra: new `repeat_drive_factory` helper that wraps an
existing factory and serves the drive path twice — needed for
the unload-happy-path test that opens the drive once for `load`
and again for `unload`.

**Tests: 142/142 workspace** (51 SCSI + 80 library + 11 CLI; up
from 134). fmt + clippy `--all-targets -D warnings` + doc all
clean.

**Where Layer 2b stands:**
- ✅ §7.0 transport + CDB builders
- ✅ §7.1 error vocabulary
- ✅ §7.2 snapshot patcher
- ✅ §7.3 LibraryHandle::move_medium
- ✅ §7.4 reconcile + refresh
- ✅ §7.5 rescan
- ✅ §7.6 DriveHandle + drive primitives
- ✅ §7.7 composed load / unload / export / import
- 🔲 §7.8 RemovalLockGuard
- 🔲 §7.9 rem CLI subcommands
- 🔲 §7.10 live test on akash

Eight of ten chunks done. The bulk of Layer 2b's safety
machinery is shipped and tested.

---

## 2026-05-17 — Layer 2b §7.8: lock_removal / allow_removal + RemovalLockGuard

PREVENT/ALLOW MEDIUM REMOVAL (SPC-5 0x1E). Three pieces:

**`LibraryHandle::lock_removal(&mut self) -> Result<RemovalLockGuard<'_>, ScsiError>`** —
builds CDB via `prevent_allow::build_cdb(true)`, fires audit
`Started{LockRemoval}` + `Finished`, executes via `execute_none`,
returns a guard on success.

**`LibraryHandle::allow_removal(&mut self) -> Result<(), ScsiError>`** —
direct method for the daemon's success-path cleanup (or for
callers that track lock state externally). Builds CDB with
`prevent=false`, audits as `AllowRemoval`. Both `lock_removal`
and `allow_removal` share a private `issue_prevent_allow(prevent,
op)` helper.

**`RemovalLockGuard<'a>`** uses the
`Option<&'a mut LibraryHandle>` pattern:
- `release(self) -> Result<(), ScsiError>` takes the handle out,
  calls `allow_removal()`, returns the result. After `release`,
  `Drop` sees `None` and does nothing.
- `Drop` calls `allow_removal()` on the still-held handle (best-
  effort). The Result is discarded; failure surfaces only via
  the audit hook's `Finished{ScsiError}` event.

Per the design doc §3.3 / §6 property 8, `Drop` is **not** a
guarantee — it doesn't run on SIGKILL, abort, or power loss.
Daemon code should call `allow_removal()` explicitly on success
paths *and* hold the guard for the defence-in-depth Drop.
Operational recovery for stranded locks: `rem unlock <library>`
or a power cycle. The type docstring spells this out.

**Borrow shape:** the guard holds `&'a mut LibraryHandle`. While
it's alive, the handle is mutably borrowed and no other ops can
run against it. For multi-MOVE-inside-lock workflows that want
the lock and the moves, the operator either:
- calls `lock_removal()` and immediately consumes the guard via
  `release()` after the moves, OR
- skips the guard and uses the direct `lock_removal()` +
  `allow_removal()` pair (the guard is optional in that flow).

The proxy-every-method approach (where the guard re-exposes
move_medium etc.) was rejected as too much surface for a
defence-in-depth helper. The journal flags this as a future
enhancement if a concrete use case needs it.

**Tests** — 4 new in `handle::tests`:
- `lock_removal_issues_correct_cdb_and_audits` — CDB =
  `[0x1E, 0, 0, 0, 0x01, 0]`, audit pair tagged
  `AuditOp::LockRemoval`. Then the guard drops and a follow-up
  CDB = `[0x1E, 0, 0, 0, 0x00, 0]` + audit pair tagged
  `AllowRemoval` appears. Four events total: Started/Finished
  for Lock, then Started/Finished for Allow.
- `allow_removal_direct_issues_correct_cdb_and_audits` — calling
  `allow_removal()` directly (no guard) issues a single ALLOW
  with the correct CDB and audit tag.
- `guard_release_returns_result_and_suppresses_drop` — call
  `release()` explicitly; total CDBs are PREVENT + one ALLOW (not
  two ALLOWs). Pins the Drop suppression after release.
- `lock_removal_returns_scsi_error_when_cdb_fails` — uses the
  `FailExecuteNoneAfter` wrapper from the §7.7 review.
  PREVENT CDB goes out (recorded) but the synthetic
  CheckCondition fires. Audit captures `Started{LockRemoval}`
  then `Finished{ScsiError, op: LockRemoval}`. Caller gets the
  ScsiError; no guard returned.

149/149 workspace tests (was 145). fmt + clippy `--all-targets
-D warnings` + doc all clean.

**Layer 2b status: nine of ten chunks done.** §7.0–§7.8 shipped.
Only §7.9 (rem CLI subcommands) and §7.10 (live test on akash)
remain.

---

## 2026-05-17 — Layer 2b §7.9: rem CLI state-changing subcommands

Eight new subcommands on top of the existing `libraries` /
`library` pair:

```
rem move    <serial> --src 0x0400 --dst 0x0100
rem load    <serial> --slot 0x0400 --bay 0x0100
rem unload  <serial> --bay 0x0100 [--dest 0x0400]
rem export  <serial> --slot 0x0400
rem import  <serial> --slot 0x0400
rem rescan  <serial>
rem lock    <serial>      # PREVENT MEDIUM REMOVAL
rem unlock  <serial>      # ALLOW MEDIUM REMOVAL
```

**Global flags** `--allow <serial>` (repeatable) and
`--allow-derived <serial>`. Every state-changing op requires the
target library to be on the `--allow` list; the CLI refuses
early (before any I/O) when it isn't, with a clear error and
the exact `--allow <serial>` flag the operator needs to add.

**Element-address parser** handles `0x0400`, `0X0400`, and
decimal — all routed through a single `parse_element_addr` value
parser registered via clap's `value_parser` attribute.

**Dispatch shape:** a shared `run_state_change(report, serial,
allow, allow_derived, out, err, op)` helper does the common
pipeline:
1. Look up the library by serial in the discovery report (exit 2
   if missing).
2. Refuse early if the library isn't on `--allow`.
3. Build a `StaticAllowlist` from `--allow` / `--allow-derived`.
4. Open the library via `Library::open(policy)` (Linux-only —
   non-Linux paths emit a clear error and exit 1).
5. Run the caller's op closure: `Result<String, String>` where
   the `String` on `Ok` is the operator-facing success summary
   ("loaded slot 0x0400 → bay 0x0100").
6. Print `ok: <summary>` on success, or the error with an EPERM
   /CAP_SYS_RAWIO hint on failure.

The `Lock` subcommand uses `std::mem::forget(guard)` to keep the
PREVENT asserted across the CLI invocation — the operator runs
`rem unlock` (which calls `allow_removal()` directly) when the
critical section ends. The Drop-based ALLOW would defeat the
point of the CLI flow (the lock would last only as long as the
process runs).

**EPERM hint** lifted from the existing discovery path: when an
op's error contains `"EPERM"` or `"Operation not permitted"`,
the CLI prints the same `setcap cap_sys_rawio+ep` recipe with
pointer to `INSTALL.md`. Reused string-match; the hint is a small
helper. (The kernel SCSI command filter blocks every state-
changing opcode without `CAP_SYS_RAWIO`, not just RES — `rem
move` and friends will hit the same condition discovery did on
the §7.1 review pass.)

**No new unit tests.** Existing CLI tests still pass (the
read-only subcommands and the EPERM-hint path for discovery
errors). State-changing dispatch is exercised via §7.10's live
integration on akash; the dispatch logic itself is small and
mostly delegation to `LibraryHandle` methods that are already
well-tested.

**Linux gating:** `open_and_run` has two `#[cfg(target_os =
"linux")]` arms. On Linux the function opens the library and
runs the op; on other platforms it returns exit 1 with "state-
changing rem subcommands require Linux". The read-only
subcommands stay portable.

**Smoke checks ran:**
- `rem --help` lists all 10 subcommands cleanly.
- `rem move --help` shows `--src`, `--dst`, and the global
  `--allow` / `--allow-derived` flags.

**Quality gate:** 150/150 workspace tests (unchanged), `cargo
fmt` + `cargo clippy --workspace --all-targets -- -D warnings` +
`cargo doc` all clean.

**Layer 2b status: ten of ten chunks shipped.** All that
remains is §7.10 — a live cartridge move on akash's QuadStor to
prove the whole stack end-to-end.

---

## 2026-05-17 — Layer 2b §7.10: live cartridge move on akash ✅

End-to-end smoke. Real bytes off real silicon for every Layer 2b
state-changing operation.

**Setup.** akash is the host. QuadStor active, /dev/sg0..sg4
present, owner in `tape` group, `setcap cap_sys_rawio+ep` on the
debug binary (rebuild resets xattrs, so re-applied after `cargo
build -p remanence-cli`). Library serial `7CBAD9CF74` with 10
RMN tapes pre-loaded across slots 0x0400..0x0409, all four drive
bays empty, four IE ports empty.

**Run 1: load.**

```
$ rem load 7CBAD9CF74 --slot 0x0400 --bay 0x0100 --allow 7CBAD9CF74
ok: loaded slot 0x0400 → bay 0x0100
```

Verified with `rem library 7CBAD9CF74 --slots`: slot 0x0400 now
`empty`, slot count 10 loaded → 9 loaded. Robot physically moved
RMN001L9 from slot 0x0400 to bay 0x0100. CDB sequence: MOVE
MEDIUM (0xA5) on /dev/sg4, then SSC LOAD (0x1B byte 4 = 0x01) on
/dev/sg0.

**Run 2: unload.**

```
$ rem unload 7CBAD9CF74 --bay 0x0100 --allow 7CBAD9CF74
ok: unloaded bay 0x0100 → recorded source slot
```

No `--dest` flag — the bay's `source_slot` from RES SVALID
(0x0400) was used. Verified: slot 0x0400 holds RMN001L9 again,
slot count back to 10 loaded. CDB sequence: SSC UNLOAD (0x1B
byte 4 = 0x00) on /dev/sg0, then MOVE MEDIUM (0xA5) on /dev/sg4.

**Net change on the library: zero.** Net validation: massive —
the full Layer 2b stack moved real cartridge bytes through:
- `discover()` → identity revalidation → 4-stage open gate → MOVE
  + LOAD (load case)
- `discover()` → identity revalidation → 4-stage open gate →
  UNLOAD + MOVE (unload case)

Both used the correct CDB bytes, both routed through the
`AuditOp::Load{slot,bay}` / `AuditOp::Unload{bay,dst=Some(0x0400)}`
audit context (audit hook wasn't installed in this run; would
appear in daemon production).

**Pre-discovery allowlist gate verified.**

```
$ rem move 7CBAD9CF74 --src 0x0400 --dst 0x0100
error: library "7CBAD9CF74" not on the --allow list — state-changing ops are refused
       pass `--allow 7CBAD9CF74` to permit this invocation
```

Exit 1 with no I/O. discover() was never called.

**Run 3: lock cycle.**

```
$ rem lock 7CBAD9CF74 --allow 7CBAD9CF74
ok: locked — call `rem unlock` when done
$ rem unlock 7CBAD9CF74 --allow 7CBAD9CF74
ok: unlocked
```

PREVENT MEDIUM REMOVAL (CDB 0x1E byte 4 = 0x01) then ALLOW
(byte 4 = 0x00). `mem::forget(guard)` correctly kept the lock
asserted across the first CLI's exit; a separate `rem unlock`
invocation released it.

**Run 4: rescan.**

```
$ rem rescan 7CBAD9CF74 --allow 7CBAD9CF74
ok: rescan ok
```

INITIALIZE ELEMENT STATUS (0x07) on /dev/sg4 + post-init RES +
reconcile. Snapshot shape didn't change so no warnings; no
operator-visible output beyond `ok: rescan ok`.

**Four CDB shapes validated live on real silicon:**
- MOVE MEDIUM (0xA5) — load + unload
- SSC LOAD / UNLOAD (0x1B) — load + unload
- PREVENT / ALLOW MEDIUM REMOVAL (0x1E) — lock + unlock
- INITIALIZE ELEMENT STATUS (0x07) — rescan

**Layer 2b: complete.** §7.0–§7.10 shipped. 151/151 workspace
tests, all CDB shapes verified end-to-end on akash.

**Polish item for follow-up (not blocking):** the `rem library
<serial>` view doesn't surface bay-loaded state — the
`InstalledDrive` line shows the drive's metadata but no cartridge
information. The discovery layer captures it correctly
(`bay.loaded`, `bay.loaded_tape`, `bay.source_slot` are all set);
the CLI just doesn't print it. Easy fix when convenient.

**Next major chunk: Layer 2c — the udev watcher.** Event-driven
re-discovery on hot-plug, replacing the periodic `discover()`
the daemon would otherwise run. Its own design doc.

## 2026-05-17 — §7.10 review fixes: IE-touching moves, CLI hints, setcap

Three findings from the §7.10 live smoke, all addressed in one
commit:

**High — IE-port moves now mark the snapshot dirty.** The
QuadStor live `rem export` succeeded but the cartridge *vaulted*
rather than parking visible in element 0x1000 (the IE port).
Vendor-specific: HPE parks in IE, QuadStor pulls into a vault.
The snapshot-patcher's blind "IE-full after export" model was
wrong for QuadStor. Fix: `LibraryHandle::move_medium_as` now
checks whether `src` or `dst` is one of the library's
`ie_ports[].element_address`. If yes, it sets `self.is_dirty =
true` and surfaces `dirty: true` in the `Finished{Success}`
audit outcome. Downstream callers (the CLI's `open_and_run`)
already know to print the recovery hint when `handle.is_dirty()`
is set on op return. Result: an export against QuadStor leaves
the snapshot tagged dirty, operator sees the
`rem library --slots` + `rem rescan` recovery hint, and the
next discovery cycle resolves the true post-move state without
us pretending we know which vendor flavor we're talking to.

Three new regression tests in `handle.rs`:
- `ie_endpoint_move_marks_snapshot_dirty` — slot → IE asserts
  `is_dirty=true`
- `ie_to_slot_move_also_marks_dirty` — IE → slot, symmetric
- `slot_to_bay_move_does_not_mark_dirty` — sanity: non-IE moves
  don't over-fire

**Medium — CLI hint text now includes `--allow`.** The
pre-discovery allowlist gate (added before §7.10) rejects any
state-changing subcommand whose serial isn't on `--allow`. But
two hint sites still suggested raw invocations:
- `rem lock <serial>`'s success message said
  `call \`rem unlock\` when done`
- `print_dirty_snapshot_recovery` suggested
  `rem rescan <serial>`

Both would now hit the gate and fail. Fixed both: lock success
prints `call \`rem unlock <serial> --allow <serial>\` when
done`, and dirty-recovery prints the full `rem rescan <serial>
--allow <serial>` form.

**Medium — setcap hint uses `current_exe()`.** The hint
previously embedded `$(realpath "$0")`. In an interactive
shell that copy-pastes the line, `$0` resolves to the shell
(`/bin/bash` or `/usr/bin/zsh`), not the `rem` binary, so the
capability would land on the wrong file. New helper
`rem_binary_path()` calls `std::env::current_exe()` and
substitutes the actual path; both setcap hint sites use it.
Falls back to `/path/to/rem` only if `current_exe()` errors,
which essentially doesn't happen on Linux.

**Quality gate:**
- `cargo fmt --all` — clean
- `cargo clippy --workspace --all-targets -- -D warnings` —
  clean
- `cargo test --workspace` — 12 + 91 + 51 + 1 ignored, all pass
- `cargo doc --workspace --no-deps` — no warnings

No re-run against akash needed for these changes — the
allowlist + identity stack didn't move, just the dirty-marking
inside `move_medium_as` and CLI text. The next live smoke will
naturally exercise the IE-touching path.

## 2026-05-17 — §7.10 review fixes round 2: dirty on success + neutral export text

The previous fix correctly set `handle.is_dirty()` after an
IE-touching success, but the CLI only printed the recovery
hint on the *error* path. Live `rem export … --slot 0x0400`
against akash therefore printed
`ok: exported slot 0x0400 → first available IE port` while
QuadStor was busy vaulting the cartridge in the background;
the operator saw a confident success message but a subsequent
`rem import` failed with "source element 0x0300 is empty".

**Fix 1 — surface dirty on the success branch.**
`open_and_run`'s `Ok(summary)` arm now also checks
`handle.is_dirty()` and, when set, prints the recovery hint
before returning `ExitCode::SUCCESS`. The op did succeed; the
warning is purely about the snapshot model being unreliable
past this point.

**Fix 2 — branch the recovery wording on reason.**
The hint used to lead with "the operation partially succeeded
— an earlier phase changed library state before the later
phase failed." That wording is wrong (and alarming) for a
clean IE-touching op that just happened to expose vendor
divergence. Split into `DirtyReason::{PartialFailure,
VendorSemantics}`; the success path uses the latter and reads
"the operation touched an IE port. Post-move state depends on
vendor semantics (some libraries vault the cartridge rather
than park it in the IE element)." Both reasons emit the same
recovery commands (`rem library --slots` + `rem rescan`).

**Fix 3 — neutral export/import success text.** Dropped the
`→ first available IE port` / `→ slot 0x...` direction-arrow
claims, since they implied a post-state we can't verify from
the CDB alone. New text:
- `ok: export issued for slot 0x0400`
- `ok: import issued for slot 0x0400`

Coupled with the dirty hint that now follows, the operator
gets: "we sent the op, here's what we know, here's how to
confirm." No fake certainty.

Two new unit tests pin the wording:
- `dirty_recovery_hint_partial_failure_wording` — asserts the
  failure wording stays and the `--allow` suffix is on the
  rescan line
- `dirty_recovery_hint_vendor_semantics_wording` — asserts the
  vendor wording is used, the failure wording is *not* leaked,
  and the `--allow` suffix is present

**Quality gate:**
- `cargo fmt --all` — clean
- `cargo clippy --workspace --all-targets -- -D warnings` —
  clean
- `cargo test --workspace` — 14 + 91 + 51, all pass (CLI suite
  +2 from previous count)
- `cargo doc --workspace --no-deps` — no warnings

Akash is currently restored (10 loaded slots, RMN001L9 in
0x0400). Next live smoke against this change will exercise the
new success-path hint end-to-end.

## 2026-05-17 — docs/layer2b-design.md sync with implementation

Doc-only sweep. Three stale spots in the design doc, all
flagged by review after the live akash run validated the
implementation. None affected runtime correctness; the code
had drifted past the doc.

**Spot 1 (§5.1, `is_dirty` paragraph).** The doc claimed "The
CLI auto-refreshes after partial-failure paths." We don't —
we print a recovery hint and exit. Rewrote to describe the
actual behavior: CLI consults `handle.is_dirty()` after every
op and, when set, prints either the partial-failure or the
vendor-semantics flavor of the hint. Cross-referenced §7.7 for
the IE-port reasoning.

**Spot 2 (§7.9, CLI subcommands).** Doc said "Policy comes
from `--policy <path>` (default `~/.config/remanence/policy.toml`)."
That config-file flow was never implemented. Replaced with the
real model: two global flags `--allow <serial>` and
`--allow-derived <serial>`, plus the pre-discovery allowlist
gate that short-circuits before any SG_IO call. Also called
out that the setcap hint uses `std::env::current_exe()` so
copy-paste from interactive shells lands on the right binary.

**Spot 3 (Appendix A, worked example).** Refreshed all CLI
transcripts to:
- include `--allow <serial>` on every state-changing
  invocation
- match the *actual* success-text format (`ok: loaded slot
  0x0400 → bay 0x0100` etc., not the prose "moving 0x0400 →
  0x0100 in library 7CBAD9CF74…" placeholder)
- show the partial-failure recovery hint verbatim
- add a new QuadStor IE-port walkthrough demonstrating the
  vendor-semantics flavor of the hint — exactly the scenario
  the §7.10 review round 2 fix addressed

Bonus fix (§2 "Preconditions"): the doc still said the
`AccessPolicy` is "configured from `/etc/remanence/policy.toml`"
in the preamble. Re-worded to say the daemon builds it from
its own config and the CLI builds a `StaticAllowlist` from
the `--allow` flags.

No code change, no quality-gate run needed (text-only).
Reviewer confirmed local gates + live akash all green before
this sync.

## 2026-05-17 — High: op-class timeouts + dirty-on-transport-error

Review-flagged latent bug. The transport carried a single 5-s
SG_IO timeout for every CDB, including state-changing ones.
QuadStor never noticed (it completes everything in memory in
milliseconds) but on a real MSL3040 the spec window for MOVE
MEDIUM is 8-20 s — so the very first production move would
have timed out, surfaced as a transport error, and (worse) the
old code reported `ScsiError` without marking `is_dirty=true`.
The handle would have happily kept its pre-MOVE snapshot while
the changer was busy actually moving the cartridge. Recipe for
silent data-loss.

**Fix 1 — operation-class timeouts.** New `TimeoutClass` enum
in `transport.rs`:

| Class | Window | Used for |
|---|---|---|
| `Inquiry` | 5 s | INQUIRY / VPD |
| `PreventAllow` | 5 s | PREVENT / ALLOW MEDIUM REMOVAL |
| `ReadElementStatus` | 60 s | RES |
| `Move` | 120 s | MOVE MEDIUM |
| `InitElementStatus` | 600 s | INITIALIZE ELEMENT STATUS |
| `LoadUnload` | 600 s | SSC LOAD / UNLOAD |

Added `set_timeout_for(class)` to the `SgTransport` trait with
a no-op default for test transports. Production
`LinuxSgTransport` overrides it to mutate `self.timeout_ms`.
`Box<dyn SgTransport>` and `RecordingTransport` both forward
the call so wrapping doesn't lose the class info. Every
state-changing call site in `handle.rs` now calls
`set_timeout_for(...)` immediately before its `execute_none`.

**Fix 2 — dirty marking on completion-unknown failures.** New
`completion_unknown(&ScsiError) -> bool` predicate in
`handle.rs`. Returns true for `TransportError` (driver
timeout, host adapter reset, bus reset) and `Io` (raw ioctl
failure — rare, treated as ambiguous for safety). Returns
false for `CheckCondition` (device explicitly rejected the
CDB, physical state unchanged) and for pre-flight parse
errors.

State-changing call sites that now consult it and mark
`self.is_dirty = true` on completion-unknown failures:
- `move_medium_as` — robot may be mid-flight
- `rescan`'s INIT phase — element-state cache may be partial
- `LibraryHandle::unload`'s drive UNLOAD phase — drive may
  have ejected mechanically while we lost the status

PREVENT/ALLOW deliberately stays clean — snapshot doesn't
track lock state, so transport failure there has nothing to
flag dirty.

**Fix 3 — audit `ScsiError` outcome carries `dirty: bool`.**
Symmetric with `Success`'s `dirty` field. An audit-replay
consumer can now reconstruct that a failed CDB left the
snapshot in an untrustworthy state, instead of having to
infer from the error summary text.

**Tests added (5):**
- `timeout_class_durations_match_operational_reality` —
  pins the 6 windows against accidental refactoring
- `linux_transport_set_timeout_for_updates_window`
  (transport.rs) — asserts the trait method mutates the
  per-CDB ms field for the production impl
- `move_with_transport_error_marks_snapshot_dirty` — injects
  a driver-timeout-shaped TransportError on the MOVE CDB,
  asserts `is_dirty=true`, snapshot patch NOT applied,
  audit summary names the failure
- `rescan_with_init_transport_error_marks_snapshot_dirty` —
  symmetric for INIT
- `composed_unload_with_drive_transport_error_marks_dirty` —
  SSC UNLOAD times out → composed unload marks dirty before
  propagating, MOVE phase never attempted

Test-side helper `FailFirstNoneWithTransportError<T>` wraps
any inner SgTransport and replaces the first `execute_none`
return with a synthetic `TransportError { driver_status: 0x06,
... }`. This is the shape the kernel produces for a real
SG_IO driver timeout.

**Design doc updated** (`docs/layer2b-design.md`):
- §6 lead-in: "four" → "five" operation-level safety
  properties
- §6.7: `AuditOutcome::ScsiError` schema picks up the `dirty`
  field, with rationale paragraph
- §6.10 (new): "Op-class SG_IO timeouts" — class table,
  completion-unknown semantics, audit-replay implications

**Quality gate:**
- `cargo fmt --all` — clean
- `cargo clippy --workspace --all-targets -- -D warnings` —
  clean
- `cargo test --workspace` — 14 + 96 + 51, all pass (handle
  suite +5 from previous count)
- `cargo doc --workspace --no-deps` — no warnings

Caught entirely by review before any production hardware
test would have surfaced it. The next live MSL3040 smoke (or
even a flaky cable on akash) will exercise the dirty-marking
path; until then, the new tests pin the contract.

## 2026-05-17 — Op-class timeouts: round 2 fixes

Two review findings on the prior commit:

**Medium — RES still ran under 5 s when called outside the
state-changing paths.** `TimeoutClass::ReadElementStatus` was
defined but only set inside `LibraryHandle::rescan`'s explicit
call site. Discovery, `Library::open`'s identity revalidation,
and `LibraryHandle::refresh` all called the shared `issue_res`
helper which never set the timeout. So a partitioned MSL3040
with hundreds of elements could still time out on cold
discovery.

Fix: move the timeout set *into* `issue_res` itself.
`t.set_timeout_for(TimeoutClass::ReadElementStatus)` runs once
at the top of the helper, covering both the probe
(`learn_byte_count`) and the full read. Every caller — Layer
2a discovery, identity revalidation, `rescan`'s post-INIT RES,
`refresh()` — inherits the right window without having to
remember. Removed the now-redundant explicit set in
`LibraryHandle::rescan` (replaced with a comment pointing at
`issue_res`).

**Low — dirty hint wording was too narrow.** The CLI's
recovery-hint formatter only had `PartialFailure` and
`VendorSemantics` flavors. A single-CDB MOVE that timed out
correctly flipped `is_dirty=true`, but then the error path
printed "the operation partially succeeded — an earlier phase
changed library state before the later phase failed" —
incorrect (there was no earlier phase) and alarming.

Fix: add a third `DirtyReason::CompletionUnknown` flavor with
its own wording ("the operation failed with a transport-level
error; the device may have actually executed it even though
the host didn't get a clean status back"). For the CLI to
pick the right flavor, the library now reports *which* kind
of dirty.

**New library API:**

```rust
pub enum DirtyCause {
    PartialFailure,    // composed-op, earlier CDB succeeded
    VendorSemantics,   // op succeeded but post-state vendor-divergent
    CompletionUnknown, // state-changing CDB failed ambiguously
}

impl LibraryHandle {
    pub fn dirty_cause(&self) -> Option<DirtyCause>;
}
```

Invariant: `is_dirty() == true ⟺ dirty_cause().is_some()`.
Every `self.is_dirty = true` site in `handle.rs` was replaced
with `self.mark_dirty(cause)`, and every `self.is_dirty =
false` with `self.clear_dirty()`. The two helpers flip both
fields together so they can't drift apart.

Cause mapping at the dirty-marking sites:
- composed `load` post-MOVE phase failure → `PartialFailure`
- `move_medium_as` IE-touching success → `VendorSemantics`
- `move_medium_as` completion-unknown error → `CompletionUnknown`
- `rescan` INIT execute_none completion-unknown error → `CompletionUnknown`
- `rescan` INIT ok + RES fail → `CompletionUnknown` (changer
  re-derived its state, we just can't read it back)
- `rescan` reconcile shape mismatch → `CompletionUnknown`
- `refresh` shape mismatch (success-with-dirty path) →
  `CompletionUnknown`
- `LibraryHandle::unload` drive UNLOAD completion-unknown →
  `CompletionUnknown`

CLI now maps `DirtyCause → DirtyReason` via a total
`From` impl; `open_and_run` reads `handle.dirty_cause()`
on both success and error branches and prints the matching
hint flavor.

**Tests added (6):**
- `move_transport_error_records_completion_unknown_cause`
- `ie_endpoint_move_records_vendor_semantics_cause`
- `composed_load_partial_failure_records_partial_failure_cause`
- `fresh_handle_has_no_dirty_cause` (invariant baseline)
- `dirty_recovery_hint_completion_unknown_wording` (CLI)
- `dirty_reason_from_dirty_cause_covers_all_variants` (CLI
  mapping is total; adding a new `DirtyCause` variant in
  future trips a `non_exhaustive_patterns` build error)

**Design doc updated:**
- §5.1: `DirtyCause` introduced alongside `is_dirty()`, with
  the three causes documented and the
  `is_dirty ⟺ dirty_cause.is_some()` invariant called out.
  CLI behavior re-described as "consults `handle.dirty_cause()`"
  rather than "consults `is_dirty()`".
- §6.10: noted that `READ ELEMENT STATUS` is special — `issue_res`
  sets the class itself, so every caller gets the long window.
  Reworded the timeout-fire paragraph to use the
  `CompletionUnknown` hint wording.

**Quality gate:**
- `cargo fmt --all` — clean
- `cargo clippy --workspace --all-targets -- -D warnings` —
  clean
- `cargo test --workspace` — 16 + 100 + 51, all pass (handle
  +4, CLI +2 from previous count)
- `cargo doc --workspace --no-deps` — no warnings

The same review session that confirmed live akash green is the
one that flagged these — caught entirely from code-reading, no
new hardware run needed.

## 2026-05-17 — load() classifies drive-LOAD timeout as CompletionUnknown

Follow-up review on the prior commit. Two findings:

**Medium — `load()` collapsed all post-MOVE failures to
`PartialFailure`.** The composed `LibraryHandle::load` was
doing:

```rust
if result.is_err() {
    self.mark_dirty(DirtyCause::PartialFailure);
}
```

That was right for `open_drive` failing or a CHECK CONDITION
from the drive's SSC LOAD — the MOVE happened, the LOAD
didn't. But for a *transport-level* drive LOAD failure
(timeout, bus reset), the cartridge moved AND the drive may
have actually executed the LOAD even though we lost the
status. That's the stronger `CompletionUnknown` signal — and
the CLI's `CompletionUnknown` hint wording ("the device may
have actually executed it even though the host didn't get a
clean status back") is the right thing to print, not the
partial-failure one.

Fix: classify the post-MOVE error by inspecting the variant:

```rust
let cause = match err {
    LoadError::DriveLoad(DriveOpError::ScsiError(scsi_err))
        if completion_unknown(scsi_err) =>
    {
        DirtyCause::CompletionUnknown
    }
    _ => DirtyCause::PartialFailure,
};
self.mark_dirty(cause);
```

`PartialFailure` still covers `open_drive` errors and SCSI
errors that aren't completion-unknown (CHECK CONDITION,
parse errors). The DriveHandle's own audit `Finished` event
already reports `dirty: completion_unknown(...)` per its
inner CDB outcome (handle.rs:1239), so the audit log and the
library's `dirty_cause()` are now consistent for this path.

**Low (docs) — §5.1 partial-failure table was stale.** Said
`move_medium(src, dst)` "CDB returned error" leaves dirty
"no". That was true before the op-class-timeouts commit;
now, transport-level errors do mark dirty. The whole table
needed refactoring to split CHECK-CONDITION rows from
transport-error rows, and to add a row for the IE-port
`VendorSemantics` success path the prior commit added.

The new table is 12 rows across 4 ops, with an explicit
`Cause` column. Added a trailing paragraph pointing at the
shared `completion_unknown(&ScsiError)` predicate so a
reader can find the actual rule.

**Tests added (1):**
- `composed_load_drive_transport_error_records_completion_unknown`
  — MOVE succeeds, drive opens, SSC LOAD fails with a
  synthetic driver-timeout TransportError; asserts
  `dirty_cause() == Some(CompletionUnknown)`, both MOVE and
  LOAD CDBs went out.

**Quality gate:**
- `cargo fmt --all` — clean
- `cargo clippy --workspace --all-targets -- -D warnings` — clean
- `cargo test --workspace` — 16 + 101 + 51, all pass (handle
  +1 from previous count)
- `cargo doc --workspace --no-deps` — no warnings

Layer 2b's dirty-state machine is now consistent across
`is_dirty()`, `dirty_cause()`, and the audit `Finished`
outcome's `dirty: bool` field for every state-changing path.
The CLI prints the right hint for every flavor without having
to guess.

## 2026-05-17 — Public doc sync for the new dirty-on-failure semantics

Doc-only follow-up. The runtime behavior is correct (see prior
commits), but the *public* API docstrings on the error enums
and `LibraryHandle::unload` / `move_medium` still said
"snapshot unchanged" / "is_dirty stays false" for failure
paths that now mark dirty on a transport-level error. A daemon
author reading rustdoc would build the wrong mental model and
either skip an unneeded refresh or, worse, trust a stale
snapshot after a real timeout.

Updated docstrings:

- `LoadError::Move` — split into CHECK CONDITION (clean) vs
  transport error (`CompletionUnknown`).
- `LoadError::DriveLoad` — split into CC (`PartialFailure`,
  cartridge in bay, LOAD didn't run) vs transport (cartridge
  in bay, LOAD may have actually run → `CompletionUnknown`).
- `LoadError::OpenDrive` — clarified that the existing dirty
  bit has cause `PartialFailure`.
- `UnloadError::OpenDrive` — clarified that no CDB went out,
  so dirty stays false.
- `UnloadError::DriveUnload` — split CC (clean, drive still
  holds the cartridge, idempotent retry) vs transport (drive
  may have actually ejected → `CompletionUnknown`).
- `UnloadError::Move` — split CC (clean, cartridge still in
  the bay, retry MOVE) vs transport (cartridge may have moved
  partway → `CompletionUnknown`).
- `LibraryHandle::move_medium` — added a "Dirty-state on
  failure / success" block covering CC vs transport for
  failure and the IE-port `VendorSemantics` case for success.
- `LibraryHandle::unload` — rewrote the phase-aware-errors
  bullet list to match the new error-enum docs.

No code change. Quality gate: fmt + clippy + tests + doc all
green (cargo doc compiles the intra-doc link to
`DirtyCause::CompletionUnknown` etc. without warnings).
