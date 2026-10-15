Standalone oracle inputs (P1)

The drives-only host is one enumerated attachment, /dev/sg-oracle-lto7.
The discovery test supplies drive-inquiry.bin then drive-vpd80.bin through
FixtureTransport. It adds no synthetic changer to a successful discovery.
Incomplete-discovery cases inject failed opens, failed INQUIRY, malformed
changer VPD, and unresolved changer bays. The last case uses the existing
fixtures/element-status/quadstor-msl-g3.bin no-DVCID capture for each of the
three explicit RES ladder replies, plus fixtures/inquiry/changer-msl-g3.bin
and fixtures/vpd-80/changer-msl-g3.bin. A second unresolved case uses
quadstor-msl-g3-dvcid.bin: identified bays have no matching device attachment.
Each failure must leave identifying evidence and Unknown ownership on the
otherwise usable drive. No real hardware is addressed by these tests.

Byte-identical capture sources:

  drive-inquiry.bin
    fixtures/inquiry/real/drive-lto7.bin
    Also inquiry/standard/drive2.bin in the capture directory below.
    SHA-256 fef9953b45987ee8394904aafc0d65a328ba3019f3d22b5a0524bcd39763ded1

  drive-vpd80.bin
    inquiry/vpd-80/drive2.bin in the capture directory below.
    SHA-256 37ea851526aa5d34dbe4c80bf272023125ef5d90d9d984d52a8567cdabdd0d8a

  changer-inquiry.bin
    inquiry/standard/changer1.bin in the capture directory below.
    SHA-256 fa296ceb9973f043a1f4b1ad68ab52e9a8470ce974a0eba345ab1e393e470981

Capture directory:
  fixtures/real-hardware/remanence-fixtures-datamover-20260516T172045Z

These captures identify an HP Ultrium 7-SCSI drive, revision Q387,
VPD serial 8031BDC7EF. They are discovery/parser evidence, not tape-write
hardware qualification. MODE SENSE test bytes are constructed explicitly
in the API oracle; they are not claimed as captured hardware output.

index-v0.1.0.sql is the released SQLite shape (user_version 16), taken from
crates/remanence-state/src/index.rs at tag v0.1.0,
commit 3dd5afdfc9e774bffa96dd340cb34a69988fcdbf. It includes MINIMUM_SCHEMA
and the additive columns, object_copies_pool_idx, objects_content_digest_idx,
tapes_pool_idx, and schema_meta schema_version BLOB row of that release.
The migration test inserts labelled, unparseable-label, and unlabelled rows
across three pools and an unassigned tape. A second pre-migration variant
adds assignment_generation and seeds nonzero versions; that additive variant
is not claimed to be the unmodified v0.1.0 shape. No production migration
code or current schema generator is used to construct this historical input.

P1-fix interface obligations (test-only dispatch)

unattached_drives is the contract C3 and design v4.1 section 3.1 spelling;
there is no naming deviation. standalone_drives names configuration.

The CLI run_with_standalone_test_io seam is still an implementation obligation:
it must inject IO into the real parser/dispatcher, never manufacture outcomes.
Implementing that seam would edit production code, excluded by this dispatch.
The optional put --pool mismatch and matching-pool tests follow contract C6
Amendment 1 (2026-10-15); ordinary put also checks the loaded non-default pool.

OperatorMount::with_transport_and_lock_io_for_test is a proposed filesystem
boundary seam. It runs normal production root selection and lock acquisition,
with no lock-root override. Its callback receives each operation name
(create_dir_all, open, lock_exclusive) and logical production path, returning
an IO error or a physical path to use for that operation. Every attempted
fallback must pass through the same callback. It must never swallow errors,
implement alternate policy, or synthesize successful lock acquisition. The
oracle redirects permitted IO into its temporary directory and injects EACCES
for root creation and EAGAIN for exclusive acquisition, independently.

The write transport observer independently replays the on-disk calibration
journal before every media-modifying dispatch. A separate mounted case replaces
the journal path with a directory to force an append failure even under root.
Ephemeral mount coverage uses the same valid BOT/catalog/pool state and checks
writable(), write refusal, SCSI dispatch, sessions, copies, and journal authority.

Restore member fixtures are generated with the real archive writer and, for
invalid paths only, mutated at equal length with manifest SHA-256 and tar
checksums recomputed. The reader must reject these fixtures for path syntax.
Valid member paths also traverse pre-existing candidate, member-directory,
and leaf-file symlinks to test filesystem containment independently of parsing.
Ordinal destinations are checked for ordinary, parent-relative, absolute,
and repeated object IDs in one multi-candidate tape.
