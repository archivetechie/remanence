# Review Feedback: `docs/layer2-design.md`

**Reviewed against:** `docs/spec-v0.2.md`  
**Review date:** 2026-05-17  
**Scope:** Discovery/topology design, safety model, identity model, and implementation readiness.

## Summary

The design is directionally strong. It correctly centers the important Layer 2 idea from spec v0.2: Remanence operates on a flat list of logical libraries and joins library-reported drive bays to host device nodes by stable serial identity rather than SCSI enumeration order.

The main fixes needed before implementation are safety and consistency issues. In particular, the doc should treat `/dev/sgN` paths as stale immediately after discovery unless revalidated, make the library allowlist a hard requirement rather than a future soft rule, avoid unsafe drive assignment in the DVCID fallback path, and represent partially discovered drive bays without dropping topology.

## High Priority Feedback

### 1. `Library::open()` must revalidate the changer identity before returning a handle

`docs/layer2-design.md:329-332` says `Library::open()` returns an error if the library's `/dev/sgN` has gone away. That is not enough. A stale `/dev/sgN` can also still exist but now refer to a different medium changer after reboot, HBA rescan, cable changes, or hotplug churn.

This matters because the safety section explicitly says operators do not target by `/dev/sgN` (`docs/layer2-design.md:383-385`), but the proposed handle still opens the cached path from the snapshot.

Recommended change:

- `Library::open()` should open the current `changer_sg`, issue standard INQUIRY plus VPD 0x80, and verify that the returned serial still equals `library.serial`.
- If the serial differs, return a hard error such as `OpenError::IdentityChanged { expected, actual, path }`.
- Future drive handles should do the same for drive serials before LOAD, READ, WRITE, REWIND, LOCATE, or reservation operations.

This turns `/dev/sgN` into a current attachment hint, not a trusted identity.

### 2. The library allowlist is now required by spec v0.2, not a future soft rule

The Layer 2 doc says a future config file "may allowlist libraries" and marks it out of scope for v0.1 (`docs/layer2-design.md:386`, `docs/layer2-design.md:473-474`). Spec v0.2 makes this a core defense-in-depth requirement: the daemon configuration carries an explicit list of library serials it may issue state-changing commands against, and it refuses commands for anything else.

Recommended change:

- Replace "future config file may allowlist libraries" with a hard design requirement.
- Make handle acquisition policy-aware, for example `Library::open(policy: &LibraryAccessPolicy)` or a daemon-owned `LibraryRegistry::open_owned(serial)`.
- Discovery can and should report every reachable logical library, but state-changing handles should only be obtainable for allowlisted library serials in daemon mode.

The current "explicit CLI spelling" safety property is useful, but it is not a substitute for daemon-level ownership enforcement.

### 3. The DVCID fallback path is under-specified and can misassign drives

The doc correctly says bus topology does not map cleanly to logical-library membership on `datamover` (`docs/layer2-design.md:41`). Later, the final DVCID fallback says to cross-reference per-drive VPD 0x80 and fill `drive.serial`, while acknowledging that this relies on the old SCSI-bus-topology assumption (`docs/layer2-design.md:224-227`).

Those statements are in tension. If DVCID is absent and multiple logical libraries share HBA paths, there may be no safe way to assign host tape drives to changer drive elements automatically.

Recommended change:

- Spell out the exact final fallback algorithm from spec v0.2: drives on the same `host:channel` as the changer, sorted by SCSI ID.
- Mark that mapping as `derived` / low confidence in the model or report.
- Refuse to use topology-derived mappings for state-changing operations unless the deployment explicitly enables that fallback for a vendor/topology where it has been validated.
- If multiple logical libraries are visible on the same host/channel, prefer `DiscoveryWarning::DriveMappingUnavailable` over manufacturing an apparently authoritative mapping.

The current text risks producing a plausible-looking but wrong bay-to-drive mapping, which is worse than a loud partial discovery.

### 4. The model cannot represent unresolved or partially resolved drive bays

`Drive` requires `serial: String` and `sg_path: PathBuf` (`docs/layer2-design.md:127-149`), but the error model says a library with no reachable drives is still valid and should be returned (`docs/layer2-design.md:264-266`). The warning enum also includes `UnresolvedDrive` (`docs/layer2-design.md:312-313`).

Those cannot all be true with the current model. If the changer reports a configured drive bay but the corresponding host tape device is unreadable or missing, the library topology should still contain that bay.

Recommended change:

- Split "bay" from "host device attachment", or make attachment optional.
- A shape like this would preserve topology:

```rust
pub struct DriveBay {
    pub element_address: u16,
    pub installed_drive: Option<InstalledDrive>,
    pub loaded_tape: Option<String>,
    pub source_slot: Option<u16>,
}

pub struct InstalledDrive {
    pub serial: String,
    pub vendor: Option<String>,
    pub product: Option<String>,
    pub revision: Option<String>,
    pub sg_path: Option<PathBuf>,
    pub sysfs_path: Option<PathBuf>,
}
```

This lets discovery return the library shape even when host-side drive matching is incomplete.

### 5. `READ ELEMENT STATUS` with `count=0x0100` may not scale to the spec target

`docs/layer2-design.md:217` proposes `READ ELEMENT STATUS` with `starting=0`, `count=0x0100`, and `element_type=0`. Spec v0.2 requires scaling to a fully populated MSL3040 configuration, up to 280 slots plus drives and other elements.

If `count` is interpreted as the number of elements to return from the starting element address, `0x0100` can truncate discovery for a full multi-module library.

Recommended change:

- Use MODE SENSE page 1Dh first and query each element type range explicitly, or
- Use a maximum element count that covers the full SMC range and rely on allocation length/page parsing, or
- Iterate per element type using discovered page headers until all element descriptors are returned.

This is a correctness issue because missing slots would make the discovery snapshot incomplete while appearing successful.

## Medium Priority Feedback

### 6. `Drive.sysfs_path` is described as stable, but sysfs paths are not stable identities

`docs/layer2-design.md:148-149` says the drive sysfs path is "stable across reboots." Spec v0.2 says `/dev/sgN`, SCSI ID, and sysfs path are not stable and should be treated as labels-of-the-day.

Recommended change:

- Reword the field as "current sysfs attachment path observed at discovery time."
- Do not call it a bay path or durable identity.
- Key durable drive-bay state only by `(library.serial, element_address)`.

### 7. Cartridge identity should not be keyed by `(library.serial, voltag)`

`docs/layer2-design.md:192` says the catalog keys cartridge/manifest records off `(library.serial, voltag)`. Spec v0.2 identifies the cartridge by barcode/volume tag. Library serial is part of the cartridge's current location, not part of the tape's identity.

Recommended change:

- Treat `voltag` as the tape identity within a Remanence deployment.
- Store location separately, e.g. `{ library_serial, element_address | drive_serial | exported }`.
- Detect duplicate visible barcodes across libraries as a warning or hard error depending on policy.

Otherwise, a tape physically exported from one library and imported into another can look like a different tape to the catalog.

### 8. MODE SENSE page 1Dh is both required and deferred in different sections

The domain model says `ElementLayout` is sourced from MODE SENSE page 1Dh and cross-checked against RES (`docs/layer2-design.md:87-118`). The implementation plan says the first version does not query MODE SENSE 1Dh (`docs/layer2-design.md:414-416`). The open questions then ask whether discovery should attempt MODE SENSE at all (`docs/layer2-design.md:458-459`).

Recommended change:

- Pick one v0.1 behavior.
- If MODE SENSE is deferred, say `ElementLayout` is initially derived from READ ELEMENT STATUS page headers, with MODE SENSE as a later validation enhancement.
- If MODE SENSE is required, move the parser/command support into the implementation prerequisites and remove the open question.

### 9. The header and status metadata are stale relative to spec v0.2

`docs/layer2-design.md:6` still says the companion top-level spec is `docs/spec-v0.1.docx` and `plan.txt`. The body already references spec v0.2 in places, so this is likely stale metadata.

Recommended change:

- Update the companion line to `docs/spec-v0.2.md`.
- Consider renaming the status to `draft v0.2` or explicitly saying this is "Layer 2a discovery draft" if the doc is intentionally narrower than the full Layer 2 in spec v0.2.

### 10. udev scope does not match spec v0.2

Spec v0.2 says Layer 2 subscribes to udev events on both `scsi_generic` and `scsi_tape`. The Layer 2 doc makes event-driven rediscovery a non-goal (`docs/layer2-design.md:28-29`) and later sketches a future watcher for only `scsi_generic` (`docs/layer2-design.md:446-449`).

Recommended change:

- Keep `discover()` one-shot, but explicitly define the watcher as part of Layer 2 runtime scope if this doc is meant to cover all Layer 2.
- Watch both `scsi_generic` and `scsi_tape`, or explain why `scsi_generic` alone is sufficient.
- If udev is intentionally deferred, mark the document as Layer 2a discovery rather than the whole Layer 2 design.

## Lower Priority Feedback

### 11. Warning and error boundaries need one more pass

The doc says a failing single device should be recorded in `DiscoveryReport.warnings` (`docs/layer2-design.md:264-266`), but `DiscoveryError` includes `PermissionDenied(PathBuf)` and `Scsi { dev, source }` (`docs/layer2-design.md:244-251`). That boundary needs to be precise.

Recommended change:

- Use top-level `Err` only when discovery cannot produce a meaningful report at all.
- Use warnings for per-device failures.
- Add warning variants for layout mismatch, identity revalidation failure, DVCID fallback confidence, and inaccessible changer if other changers were successfully discovered.

### 12. `std::io::Error` in `DiscoveryWarning` may not fit the stated value-type goals

The domain model emphasizes plain cloneable value structs (`docs/layer2-design.md:51`), but `DiscoveryWarning::DeviceUnreadable` stores `std::io::Error` (`docs/layer2-design.md:305-307`), which is awkward to clone, compare, serialize, or include in snapshot tests.

Recommended change:

- Store a small structured error summary instead: `{ kind: ErrorKind, message: String, raw_os_error: Option<i32> }`.

### 13. VPD 0x83 NAA should probably be modeled as normalized opaque identity, not `u64`

The current model uses `Option<u64>` for `chassis_wwn` (`docs/layer2-design.md:81-85`). That is probably fine for the observed HPE NAA value, but VPD 0x83 descriptors are variable-shape and vendor-dependent.

Recommended change:

- Use a normalized string or byte vector for the designator, for example `Option<DeviceDesignator>`, and render it as hex in the CLI.
- Keep `u64` only if Layer 1 deliberately exposes exactly one 64-bit NAA form.

### 14. Barcode normalization needs to be specified

Slots and IE ports expose `Option<String>` for cartridge tags (`docs/layer2-design.md:164-178`), but the doc does not define how the 32-byte VOLTAG field is normalized.

Recommended change:

- Specify whether trailing spaces are trimmed.
- Preserve or expose raw tag bytes if non-ASCII or malformed tags are possible.
- Document how cleaning cartridges are recognized, if the CLI intends to show `(cleaning)` as in the example.

### 15. The section numbering skips 3.2

The doc jumps from `3.1 Library` to `3.3 ElementLayout` (`docs/layer2-design.md:53-104`). Minor, but worth fixing during the next edit.

## Suggested Next Edit Order

1. Update the document metadata to point at spec v0.2 and clarify whether this is Layer 2 or Layer 2a.
2. Make allowlisting and identity revalidation non-negotiable safety requirements.
3. Fix the drive model so unresolved bays and missing host attachments can be represented.
4. Resolve the DVCID fallback behavior and explicitly prevent unsafe topology-derived mappings on shared-HBA partitioned deployments.
5. Resolve the MODE SENSE / READ ELEMENT STATUS layout strategy and increase the element-count strategy for full-size libraries.
6. Tighten warnings/errors and add tests for stale `/dev/sgN`, partial discovery, and "no state-changing CDBs during discovery."
