# Chaos Phase C ModelTransport Report - 2026-06-21

## Summary

Phase C is implemented in `remanence-chaos`: `ModelTransport` provides a
stateful in-memory virtual tape drive and medium changer, and the L1b tests run
the real Remanence write/read stack through `LibraryHandle`/`DriveHandle`,
`ChaosTransport`, `ParitySink`, `ObjectParitySource`, and the RAO reader.

No `remanence-library` production hook was added. The model uses the existing
public `Library::open_with` and `LibraryHandle::open_drive` factory seam.

## Implemented

- `crates/remanence-chaos/src/model.rs`: shared `VirtualWorld`, `VirtualTape`,
  `Record`, `DeviceRole`, and `ModelTransport`.
- Drive CDBs sized for L1b: INQUIRY, VPD 0x80, READ BLOCK LIMITS, MODE
  SENSE/SELECT, READ/WRITE(6), WRITE FILEMARKS, SPACE(6/16), LOCATE(10/16),
  READ POSITION long, LOAD/UNLOAD, and REWIND.
- Changer CDBs sized for L1b: INQUIRY, VPD 0x80, READ ELEMENT STATUS, and MOVE
  MEDIUM.
- L1b tests for clean round trip, MED-05 digest-layer detection, EOM
  early-warning sense mapping, MED-01 recovery and unrecoverability, MED-05 peer
  rejection during reconstruction, and changer loaded-barcode coupling.
- `ObjectParitySource::space(0, ...)` now accepts a no-op. This lets
  `read_object_payload(..., tape_file_number = 0, ...)` compose with the
  object-local parity source without changing object-local positioning
  semantics.
- Chaos fixed-format sense synthesis now returns a 32-byte sense buffer with
  additional length 24, matching the Phase C sense-shape requirement.

## Design Notes

The implementation keeps media location ownership with changer MOVE MEDIUM.
The Phase C table said drive UNLOAD should clear the bay, but
`LibraryHandle::unload` is composed as drive UNLOAD followed by changer MOVE
MEDIUM from bay to slot. Clearing bay occupancy on drive UNLOAD would make that
second step fail. The model therefore treats LOAD/UNLOAD as drive mechanical
state/positioning and treats MOVE MEDIUM as the changer inventory mutation.

The MED-05 test targets a payload data block. The expected integrity failure is
therefore `FormatError::FileDigestMismatch` for `payload.bin`; a manifest-targeted
mutation could instead produce `ManifestDigestMismatch`. This matches the design
decision that GOOD-status silent corruption is a digest-layer property, not an
RS erasure-recovery property.

## Verification

```text
cargo test
test result: ok across workspace unit and doc tests

cargo test -p remanence-chaos
test result: ok. 19 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out

cargo fmt --check
ok

cargo clippy -- -D warnings
Finished `dev` profile [unoptimized + debuginfo]

cargo build --release
Finished `release` profile [optimized]
```
