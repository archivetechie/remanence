//! State-changing operations on the in-memory library snapshot.
//!
//! Pure functions that the Layer 2b handle types build on. Nothing
//! here touches `/dev/sg*` or runs any policy check — those are the
//! handle's job. This module is purely about "given a [`Library`]
//! value and the parameters of an operation, validate the request
//! against the snapshot and (if valid) patch the snapshot to reflect
//! what the operation would have done."
//!
//! The split is deliberate:
//! - **Snapshot-level preflight** lives here. Address validity, full /
//!   empty checks, the [`MoveError::DriveBayUnresolved`] check for
//!   drive bays whose `installed` is `None`. Testable with synthetic
//!   snapshots and no transport.
//! - **Policy-level checks** ([`MoveError::DerivedDriveBay`]) live in
//!   the handle layer, since they need an [`AccessPolicy`] reference
//!   the snapshot doesn't have.
//! - **Device-binding checks** ([`MoveError::DriveBayMissingDevice`])
//!   live in the composed `load` / `unload` paths, where the drive's
//!   `/dev/sgN` is actually needed. Plain `move_medium` only talks to
//!   the changer and doesn't care whether the bay has a bound
//!   `sg_path`.
//!
//! [`AccessPolicy`]: crate::AccessPolicy

use remanence_scsi::ElementStatusData;

use crate::error::{MoveError, RescanWarning};
use crate::model::{DeviceCaptures, IdentitySource, InstalledDrive, Library};

/// Record of what `apply_move` patched on the snapshot, returned on
/// success. Today it's just the (src, dst) pair the caller already
/// supplied; the type exists so future expansions (cartridge tag
/// that moved, rollback helper) have a stable surface to extend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MovePatch {
    /// Source element address that the snapshot's view drained.
    pub src: u16,
    /// Destination element address that the snapshot's view filled.
    pub dst: u16,
}

/// Apply the §5.1 MOVE MEDIUM patch to the given `Library` snapshot.
///
/// Runs the snapshot-level preflight checks first; on failure returns
/// the appropriate [`MoveError`] variant and the snapshot is left
/// unmodified. On success, `src` is drained and `dst` is filled
/// according to the patch rules in `docs/layer2b-design.md` §5.1:
///
/// - Slot / IE source clears `full` and `cartridge`. Drive-bay source
///   clears `loaded`, `loaded_tape`, and `source_slot`.
/// - Slot / IE destination sets `full = true` and copies the
///   cartridge tag. Drive-bay destination sets `loaded = true`,
///   copies `loaded_tape`, and — *only when the source was a Storage
///   slot* — sets `source_slot = Some(src)`. For IE-port or drive-bay
///   sources, the destination's `source_slot` is left `None`. (This
///   matches the SMC-3 SVALID convention: the changer reports
///   source-address only when the cartridge came from a storage slot,
///   since that's the "natural home" relationship.)
///
/// Occupancy is always tracked separately from the cartridge tag —
/// a full slot with no readable barcode (`full = true, cartridge =
/// None`) moved into a bay produces `loaded = true, loaded_tape =
/// None`, which faithfully represents the physical state.
pub fn apply_move(library: &mut Library, src: u16, dst: u16) -> Result<MovePatch, MoveError> {
    let plan = plan_move(library, src, dst)?;
    apply_planned_move(library, &plan);
    Ok(MovePatch { src, dst })
}

/// Validate a MOVE MEDIUM against the snapshot without mutating it.
/// Returns a [`MovePlan`] that [`apply_planned_move`] can later use to
/// patch the snapshot exactly as `apply_move` would have.
///
/// `LibraryHandle::move_medium` uses this two-phase form: it calls
/// `plan_move` before issuing the CDB so the policy / preflight
/// vocabulary lives outside the patch path, then `apply_planned_move`
/// after the CDB returns successfully. Splitting also keeps the
/// "snapshot is unchanged on error" property load-bearing — there's
/// no intermediate state where the snapshot has been partially
/// patched.
pub(crate) fn plan_move(library: &Library, src: u16, dst: u16) -> Result<MovePlan, MoveError> {
    // -- Preflight ---------------------------------------------------
    if src == dst {
        return Err(MoveError::SameElement { addr: src });
    }
    let src_idx = find_element(library, src).ok_or_else(|| MoveError::AddressUnknown {
        library: library.serial.clone(),
        addr: src,
    })?;
    let dst_idx = find_element(library, dst).ok_or_else(|| MoveError::AddressUnknown {
        library: library.serial.clone(),
        addr: dst,
    })?;

    // DriveBayUnresolved before SourceEmpty / DestinationFull, per
    // §3.1: a bay with installed=None is operationally unsafe
    // regardless of what loaded says.
    if let ElementIdx::Drive(i) = src_idx {
        if library.drive_bays[i].installed.is_none() {
            return Err(MoveError::DriveBayUnresolved { addr: src });
        }
    }
    if let ElementIdx::Drive(i) = dst_idx {
        if library.drive_bays[i].installed.is_none() {
            return Err(MoveError::DriveBayUnresolved { addr: dst });
        }
    }

    // SourceEmpty / DestinationFull checks. Occupancy is tracked
    // separately from the cartridge tag (slots/IE/bays all support a
    // "full but no readable barcode" state). The cartridge tag is
    // carried forward as-is; `None` is preserved through the move.
    let (cartridge, src_is_storage_slot) = match src_idx {
        ElementIdx::Drive(i) => {
            let b = &library.drive_bays[i];
            if !b.loaded {
                return Err(MoveError::SourceEmpty { addr: src });
            }
            (b.loaded_tape.clone(), false)
        }
        ElementIdx::Slot(i) => {
            let s = &library.slots[i];
            if !s.full {
                return Err(MoveError::SourceEmpty { addr: src });
            }
            (s.cartridge.clone(), true)
        }
        ElementIdx::Ie(i) => {
            let p = &library.ie_ports[i];
            if !p.full {
                return Err(MoveError::SourceEmpty { addr: src });
            }
            (p.cartridge.clone(), false)
        }
    };

    let dst_full = match dst_idx {
        ElementIdx::Drive(i) => library.drive_bays[i].loaded,
        ElementIdx::Slot(i) => library.slots[i].full,
        ElementIdx::Ie(i) => library.ie_ports[i].full,
    };
    if dst_full {
        return Err(MoveError::DestinationFull { addr: dst });
    }

    Ok(MovePlan {
        src,
        src_idx,
        dst_idx,
        cartridge,
        src_is_storage_slot,
    })
}

/// Apply a previously-validated [`MovePlan`] to the snapshot. Cannot
/// fail — all validation happened in [`plan_move`]. Patches `src`
/// (drain) then `dst` (fill) per `docs/layer2b-design.md` §5.1.
pub(crate) fn apply_planned_move(library: &mut Library, plan: &MovePlan) {
    match plan.src_idx {
        ElementIdx::Drive(i) => {
            let b = &mut library.drive_bays[i];
            b.loaded = false;
            b.loaded_tape = None;
            b.source_slot = None;
        }
        ElementIdx::Slot(i) => {
            let s = &mut library.slots[i];
            s.full = false;
            s.cartridge = None;
        }
        ElementIdx::Ie(i) => {
            let p = &mut library.ie_ports[i];
            p.full = false;
            p.cartridge = None;
        }
    }

    match plan.dst_idx {
        ElementIdx::Drive(i) => {
            let b = &mut library.drive_bays[i];
            b.loaded = true;
            b.loaded_tape = plan.cartridge.clone();
            // SVALID convention: source_slot only when src was a
            // Storage slot. IE / drive-bay sources → None.
            b.source_slot = if plan.src_is_storage_slot {
                Some(plan.src)
            } else {
                None
            };
        }
        ElementIdx::Slot(i) => {
            let s = &mut library.slots[i];
            s.full = true;
            s.cartridge = plan.cartridge.clone();
        }
        ElementIdx::Ie(i) => {
            let p = &mut library.ie_ports[i];
            p.full = true;
            p.cartridge = plan.cartridge.clone();
        }
    }
}

/// Internal type carrying everything `apply_planned_move` needs to
/// patch the snapshot. Crate-visible so `handle.rs` can pass one
/// across the CDB call.
#[derive(Debug, Clone)]
pub(crate) struct MovePlan {
    pub(crate) src: u16,
    src_idx: ElementIdx,
    dst_idx: ElementIdx,
    cartridge: Option<String>,
    src_is_storage_slot: bool,
}

impl MovePlan {
    /// If either side of the move is a drive bay whose identity was
    /// inferred from topology rather than read inline from RES DVCID,
    /// return the bay's element address paired with the installed
    /// drive's metadata. The handle layer uses this for the
    /// `DerivedDriveBay` policy check; returning the bay's address
    /// (rather than just the drive) keeps the resulting `MoveError`
    /// accurate when the derived bay is the destination rather than
    /// the source.
    pub(crate) fn derived_drive_bay<'a>(
        &self,
        library: &'a Library,
    ) -> Option<(u16, &'a InstalledDrive)> {
        for idx in [self.src_idx, self.dst_idx] {
            if let ElementIdx::Drive(i) = idx {
                let bay = &library.drive_bays[i];
                if let Some(installed) = &bay.installed {
                    if matches!(installed.identity_source, IdentitySource::Derived) {
                        return Some((bay.element_address, installed));
                    }
                }
            }
        }
        None
    }
}

/// Internal: type-tagged index into one of the three element
/// collections on a `Library`. Cheap (one byte + a usize), Copy, so
/// we can hold one for src and one for dst without lifetimes getting
/// in the way of the sequential-mutation pattern `apply_move` uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ElementIdx {
    Drive(usize),
    Slot(usize),
    Ie(usize),
}

fn find_element(library: &Library, addr: u16) -> Option<ElementIdx> {
    if let Some(i) = library
        .drive_bays
        .iter()
        .position(|b| b.element_address == addr)
    {
        return Some(ElementIdx::Drive(i));
    }
    if let Some(i) = library.slots.iter().position(|s| s.element_address == addr) {
        return Some(ElementIdx::Slot(i));
    }
    if let Some(i) = library
        .ie_ports
        .iter()
        .position(|p| p.element_address == addr)
    {
        return Some(ElementIdx::Ie(i));
    }
    None
}

// =====================================================================
//  reconcile — §5.2 / §5.3
// =====================================================================

/// Reconcile an existing `Library` snapshot against a freshly-parsed
/// `ElementStatusData`. Returns the new snapshot value plus any
/// drive-bay reconciliation warnings; or `Err(String)` when the new
/// element-status shape differs from the old (different drive_count,
/// slot_count, or ie_count) — that's the structural-mismatch case
/// the caller has to escalate.
///
/// Implements the four sub-cases in `docs/layer2b-design.md` §5.2 for
/// drive-bay reconciliation:
///
/// - **Match by element address.** For each post-RES bay, look up the
///   pre-RES bay at the same address.
/// - **Serials match** → preserve the pre-RES `sg_path`, `sysfs_path`,
///   `vendor`, `product`, `revision`, and `identity_source` on the
///   post-RES bay's `InstalledDrive` (the host-side data Layer 2a's
///   tape-device join filled in). The post-RES `loaded`,
///   `loaded_tape`, and `source_slot` win.
/// - **Serials differ** → drop the pre-RES host-side data; the bay
///   holds a different drive now. `identity_source = DvcidInline`,
///   `sg_path = None`. Emit `DriveReplaced` warning.
/// - **Post has identity, pre didn't** → fresh DVCID. Emit
///   `DriveAppeared` warning. `identity_source = DvcidInline`.
/// - **Pre had identity, post doesn't** → emit `DriveVanished`
///   warning. New bay's `installed` is `None`.
///
/// Non-drive elements (slots, IE ports) come directly from the new
/// RES — RES is the source of truth for `full` / `cartridge` and
/// reconciliation has nothing to preserve.
///
/// Stable per-library fields (`serial`, `changer_inquiry`,
/// `changer_sg`, `changer_sysfs`, `chassis_designator`) are
/// preserved from `old`.
pub(crate) fn reconcile(
    old: &Library,
    new_es: ElementStatusData,
) -> Result<(Library, Vec<RescanWarning>), String> {
    // Build a fresh Library skeleton from the new element status,
    // reusing `Library::from_captures`. We pass `device_id: None`
    // because reconcile preserves the chassis designator from `old`
    // unconditionally (it's stable across reseats of the same
    // changer).
    let captures = DeviceCaptures {
        changer_inquiry: old.changer_inquiry.clone(),
        unit_serial: old.serial.clone(),
        device_id: None,
        element_status: new_es,
        changer_sg: old.changer_sg.clone(),
        changer_sysfs: old.changer_sysfs.clone(),
    };
    let mut new_lib = Library::from_captures(captures);
    new_lib.chassis_designator = old.chassis_designator.clone();

    // -- Shape check ------------------------------------------------
    // The §5.2 contract treats the library "shape" as the *set of
    // element addresses*, not just the counts. A reconfiguration that
    // keeps the same drive/slot/IE counts but shifts addresses (e.g.
    // a firmware update remaps storage slots from 0x0400.. to
    // 0x1000..) is still a structural change — match-by-address
    // reconciliation would otherwise emit a flurry of DriveAppeared
    // and DriveVanished warnings and silently drop the host-side
    // bindings Layer 2a worked hard to establish.
    if let Some(reason) = shape_mismatch(old, &new_lib) {
        return Err(reason);
    }

    // -- Drive-bay reconciliation ----------------------------------
    let mut warnings = Vec::new();
    for new_bay in new_lib.drive_bays.iter_mut() {
        let old_bay = old
            .drive_bays
            .iter()
            .find(|b| b.element_address == new_bay.element_address);
        match (
            old_bay.and_then(|b| b.installed.as_ref()),
            new_bay.installed.as_ref(),
        ) {
            (Some(old_inst), Some(new_inst)) if old_inst.serial == new_inst.serial => {
                // Same drive: preserve host-side data + identity_source
                // from old; new occupancy/voltag/source_slot already
                // came from the new RES.
                new_bay.installed = Some(InstalledDrive {
                    serial: new_inst.serial.clone(),
                    identity_source: old_inst.identity_source,
                    vendor: old_inst.vendor.clone(),
                    product: old_inst.product.clone(),
                    revision: old_inst.revision.clone(),
                    sg_path: old_inst.sg_path.clone(),
                    sysfs_path: old_inst.sysfs_path.clone(),
                });
            }
            (Some(old_inst), Some(new_inst)) => {
                warnings.push(RescanWarning::DriveReplaced {
                    addr: new_bay.element_address,
                    old_serial: old_inst.serial.clone(),
                    new_serial: new_inst.serial.clone(),
                });
                // Drop host-side data — the drive is different now.
                // new_bay.installed already has identity_source =
                // DvcidInline and None host-side fields from
                // Library::from_captures, so leave it as-is.
            }
            (None, Some(new_inst)) => {
                warnings.push(RescanWarning::DriveAppeared {
                    addr: new_bay.element_address,
                    serial: new_inst.serial.clone(),
                });
                // new_bay.installed already correct.
            }
            (Some(old_inst), None) => {
                warnings.push(RescanWarning::DriveVanished {
                    addr: new_bay.element_address,
                    old_serial: old_inst.serial.clone(),
                });
                // new_bay.installed already None.
            }
            (None, None) => {
                // Empty bay both times — no warning.
            }
        }
    }

    Ok((new_lib, warnings))
}

/// Return `Some(reason)` if `new` differs from `old` in element
/// counts *or* in the sorted address lists for any of drive bays,
/// storage slots, or IE ports. `None` means the shapes match
/// element-for-element by address and `reconcile` may proceed to the
/// per-bay merge.
fn shape_mismatch(old: &Library, new: &Library) -> Option<String> {
    if new.layout.drive_count != old.layout.drive_count
        || new.layout.slot_count != old.layout.slot_count
        || new.layout.ie_count != old.layout.ie_count
    {
        return Some(format!(
            "post-RES counts ({} drives, {} slots, {} IE) differ from prior snapshot \
             ({} drives, {} slots, {} IE)",
            new.layout.drive_count,
            new.layout.slot_count,
            new.layout.ie_count,
            old.layout.drive_count,
            old.layout.slot_count,
            old.layout.ie_count,
        ));
    }
    // Counts match — now check that the address *sets* match too.
    // Sorted Vec<u16> equality is fine: counts are equal, so two
    // vectors are set-equal iff they're sequence-equal after sort.
    if let Some(diff) = addr_set_diff(
        "drive bay",
        &addrs(&old.drive_bays, |b| b.element_address),
        &addrs(&new.drive_bays, |b| b.element_address),
    ) {
        return Some(diff);
    }
    if let Some(diff) = addr_set_diff(
        "storage slot",
        &addrs(&old.slots, |s| s.element_address),
        &addrs(&new.slots, |s| s.element_address),
    ) {
        return Some(diff);
    }
    if let Some(diff) = addr_set_diff(
        "IE port",
        &addrs(&old.ie_ports, |p| p.element_address),
        &addrs(&new.ie_ports, |p| p.element_address),
    ) {
        return Some(diff);
    }
    None
}

fn addrs<T>(items: &[T], pick: impl Fn(&T) -> u16) -> Vec<u16> {
    let mut v: Vec<u16> = items.iter().map(pick).collect();
    v.sort_unstable();
    v
}

fn addr_set_diff(kind: &str, old: &[u16], new: &[u16]) -> Option<String> {
    if old != new {
        Some(format!(
            "post-RES {kind} addresses {new:#06x?} differ from prior snapshot {old:#06x?}"
        ))
    } else {
        None
    }
}

// =====================================================================
//  Tests
// =====================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{DriveBay, ElementLayout, IePort, InstalledDrive, Slot};
    use crate::IdentitySource;
    use std::path::PathBuf;

    /// Build a synthetic library with 2 drive bays, 2 storage slots,
    /// 1 IE port, in a known starting state. Tests mutate it to set
    /// up specific scenarios.
    fn fake_library() -> Library {
        Library {
            serial: "LIB001".into(),
            changer_sg: PathBuf::from("/dev/sg-mock"),
            changer_sysfs: PathBuf::from("/sys/class/scsi_device/mock"),
            changer_inquiry: remanence_scsi::Inquiry::parse(include_bytes!(
                "../../../fixtures/inquiry/changer-msl-g3.bin"
            ))
            .unwrap(),
            chassis_designator: None,
            layout: ElementLayout {
                robot_address: 0,
                drive_start: 0x0100,
                drive_count: 2,
                slot_start: 0x0400,
                slot_count: 2,
                ie_start: 0x0300,
                ie_count: 1,
            },
            drive_bays: vec![
                DriveBay {
                    element_address: 0x0100,
                    installed: Some(InstalledDrive {
                        serial: "DRV_A".into(),
                        identity_source: IdentitySource::DvcidAndInquiry,
                        vendor: None,
                        product: None,
                        revision: None,
                        sg_path: None,
                        sysfs_path: None,
                    }),
                    loaded: false,
                    loaded_tape: None,
                    source_slot: None,
                },
                DriveBay {
                    element_address: 0x0101,
                    installed: Some(InstalledDrive {
                        serial: "DRV_B".into(),
                        identity_source: IdentitySource::DvcidAndInquiry,
                        vendor: None,
                        product: None,
                        revision: None,
                        sg_path: None,
                        sysfs_path: None,
                    }),
                    loaded: false,
                    loaded_tape: None,
                    source_slot: None,
                },
            ],
            slots: vec![
                Slot {
                    element_address: 0x0400,
                    full: true,
                    cartridge: Some("TAPE_A".into()),
                },
                Slot {
                    element_address: 0x0401,
                    full: false,
                    cartridge: None,
                },
            ],
            ie_ports: vec![IePort {
                element_address: 0x0300,
                full: false,
                cartridge: None,
                import_enabled: true,
                export_enabled: true,
            }],
        }
    }

    // -- Happy paths -------------------------------------------------

    #[test]
    fn slot_to_drive_bay_moves_cartridge_and_sets_source_slot() {
        let mut lib = fake_library();
        let patch = apply_move(&mut lib, 0x0400, 0x0100).expect("patch ok");
        assert_eq!(
            patch,
            MovePatch {
                src: 0x0400,
                dst: 0x0100
            }
        );

        let bay = &lib.drive_bays[0];
        assert!(bay.loaded);
        assert_eq!(bay.loaded_tape.as_deref(), Some("TAPE_A"));
        // Source slot recorded — the bay can be unloaded back to 0x0400.
        assert_eq!(bay.source_slot, Some(0x0400));

        let slot = &lib.slots[0];
        assert!(!slot.full);
        assert!(slot.cartridge.is_none());
    }

    #[test]
    fn slot_with_unreadable_barcode_to_drive_bay_keeps_bay_loaded() {
        // Regression for the §7.2 review High finding: a full slot with
        // cartridge = None (full but no readable barcode) must land in
        // the bay with loaded = true and loaded_tape = None — NOT
        // loaded = false, which would read as "empty bay" everywhere
        // else and corrupt the cached snapshot.
        let mut lib = fake_library();
        lib.slots[0].cartridge = None; // still full: true, just no voltag

        apply_move(&mut lib, 0x0400, 0x0100).expect("move ok");

        let bay = &lib.drive_bays[0];
        assert!(
            bay.loaded,
            "bay must be flagged loaded even without a voltag"
        );
        assert!(bay.loaded_tape.is_none(), "no voltag carried through");
        assert_eq!(bay.source_slot, Some(0x0400));

        let slot = &lib.slots[0];
        assert!(!slot.full);
        assert!(slot.cartridge.is_none());

        // Sanity: the bay now passes the "is full" preflight, so a
        // subsequent unload to a different slot succeeds. Pre-fix,
        // this would have failed with SourceEmpty.
        apply_move(&mut lib, 0x0100, 0x0401).expect("unload of unbarcoded ok");
        assert!(!lib.drive_bays[0].loaded);
        assert!(lib.slots[1].full);
        assert!(lib.slots[1].cartridge.is_none());
    }

    #[test]
    fn drive_bay_to_slot_clears_source_slot_on_destination() {
        // Pre-load drive bay 0x0100 with TAPE_A from slot 0x0400.
        let mut lib = fake_library();
        apply_move(&mut lib, 0x0400, 0x0100).unwrap();
        // Now unload to slot 0x0401 (a different slot). The destination
        // slot should NOT carry source_slot — that's a drive-bay-only
        // field. Drive bay 0x0100 must end up empty.
        let patch = apply_move(&mut lib, 0x0100, 0x0401).expect("unload ok");
        assert_eq!(
            patch,
            MovePatch {
                src: 0x0100,
                dst: 0x0401
            }
        );

        let bay = &lib.drive_bays[0];
        assert!(!bay.loaded);
        assert!(bay.loaded_tape.is_none());
        assert!(bay.source_slot.is_none());

        let slot = &lib.slots[1];
        assert!(slot.full);
        assert_eq!(slot.cartridge.as_deref(), Some("TAPE_A"));
    }

    #[test]
    fn slot_to_slot_carries_cartridge() {
        let mut lib = fake_library();
        apply_move(&mut lib, 0x0400, 0x0401).expect("slot-to-slot ok");
        assert!(!lib.slots[0].full);
        assert!(lib.slots[1].full);
        assert_eq!(lib.slots[1].cartridge.as_deref(), Some("TAPE_A"));
    }

    #[test]
    fn slot_to_ie_marks_ie_full() {
        let mut lib = fake_library();
        apply_move(&mut lib, 0x0400, 0x0300).expect("slot-to-ie ok");
        assert!(!lib.slots[0].full);
        assert!(lib.ie_ports[0].full);
        assert_eq!(lib.ie_ports[0].cartridge.as_deref(), Some("TAPE_A"));
    }

    #[test]
    fn ie_to_drive_bay_does_not_set_source_slot() {
        // Seed IE port full first.
        let mut lib = fake_library();
        lib.ie_ports[0].full = true;
        lib.ie_ports[0].cartridge = Some("IMPORT_X".into());

        apply_move(&mut lib, 0x0300, 0x0100).expect("ie-to-bay ok");

        let bay = &lib.drive_bays[0];
        assert!(bay.loaded);
        assert_eq!(bay.loaded_tape.as_deref(), Some("IMPORT_X"));
        // Source was IE, not a Storage slot → source_slot stays None.
        assert!(bay.source_slot.is_none());
    }

    // -- Snapshot-level error paths ----------------------------------

    #[test]
    fn same_element_refused() {
        let mut lib = fake_library();
        let e = apply_move(&mut lib, 0x0400, 0x0400).unwrap_err();
        assert!(matches!(e, MoveError::SameElement { addr: 0x0400 }));
    }

    #[test]
    fn address_unknown_refused() {
        let mut lib = fake_library();
        let e = apply_move(&mut lib, 0x9999, 0x0100).unwrap_err();
        match e {
            MoveError::AddressUnknown { library, addr } => {
                assert_eq!(library, "LIB001");
                assert_eq!(addr, 0x9999);
            }
            other => panic!("expected AddressUnknown, got {other:?}"),
        }
    }

    #[test]
    fn source_empty_refused_for_slot() {
        let mut lib = fake_library();
        // Slot 0x0401 starts empty.
        let e = apply_move(&mut lib, 0x0401, 0x0100).unwrap_err();
        assert!(matches!(e, MoveError::SourceEmpty { addr: 0x0401 }));
    }

    #[test]
    fn source_empty_refused_for_drive_bay() {
        let mut lib = fake_library();
        // Bay 0x0100 starts empty.
        let e = apply_move(&mut lib, 0x0100, 0x0401).unwrap_err();
        assert!(matches!(e, MoveError::SourceEmpty { addr: 0x0100 }));
    }

    #[test]
    fn source_empty_refused_for_ie_port() {
        let mut lib = fake_library();
        let e = apply_move(&mut lib, 0x0300, 0x0100).unwrap_err();
        assert!(matches!(e, MoveError::SourceEmpty { addr: 0x0300 }));
    }

    #[test]
    fn destination_full_refused_for_slot() {
        let mut lib = fake_library();
        // Move slot→slot but destination already full (load 0x0401 first).
        lib.slots[1].full = true;
        lib.slots[1].cartridge = Some("BLOCK".into());
        let e = apply_move(&mut lib, 0x0400, 0x0401).unwrap_err();
        assert!(matches!(e, MoveError::DestinationFull { addr: 0x0401 }));
    }

    #[test]
    fn destination_full_refused_for_drive_bay() {
        let mut lib = fake_library();
        lib.drive_bays[0].loaded = true;
        lib.drive_bays[0].loaded_tape = Some("ALREADY".into());
        let e = apply_move(&mut lib, 0x0400, 0x0100).unwrap_err();
        assert!(matches!(e, MoveError::DestinationFull { addr: 0x0100 }));
    }

    #[test]
    fn destination_full_refused_for_ie_port() {
        let mut lib = fake_library();
        lib.ie_ports[0].full = true;
        lib.ie_ports[0].cartridge = Some("BLOCK".into());
        let e = apply_move(&mut lib, 0x0400, 0x0300).unwrap_err();
        assert!(matches!(e, MoveError::DestinationFull { addr: 0x0300 }));
    }

    #[test]
    fn drive_bay_unresolved_source_refused() {
        let mut lib = fake_library();
        // Pretend bay 0x0100 has a tape loaded but unresolved identity.
        lib.drive_bays[0].loaded = true;
        lib.drive_bays[0].loaded_tape = Some("ORPHAN".into());
        lib.drive_bays[0].installed = None;
        let e = apply_move(&mut lib, 0x0100, 0x0401).unwrap_err();
        // DriveBayUnresolved must win over SourceEmpty/DestinationFull
        // even when the bay is loaded.
        assert!(matches!(e, MoveError::DriveBayUnresolved { addr: 0x0100 }));
    }

    #[test]
    fn drive_bay_unresolved_destination_refused() {
        let mut lib = fake_library();
        lib.drive_bays[0].installed = None;
        let e = apply_move(&mut lib, 0x0400, 0x0100).unwrap_err();
        assert!(matches!(e, MoveError::DriveBayUnresolved { addr: 0x0100 }));
    }

    // -- No-mutation-on-failure pin ----------------------------------

    #[test]
    fn refused_move_leaves_snapshot_unchanged() {
        let mut lib = fake_library();
        let before = lib.clone();
        let _ = apply_move(&mut lib, 0x0401, 0x0100); // source empty
        assert_eq!(lib, before);
        let _ = apply_move(&mut lib, 0x0400, 0x0400); // same element
        assert_eq!(lib, before);
        let _ = apply_move(&mut lib, 0x0400, 0x9999); // address unknown
        assert_eq!(lib, before);
    }

    // =====================================================================
    //  reconcile — §5.2 / §5.3
    // =====================================================================

    use remanence_scsi::read_element_status::{Element, ElementStatusData, ElementType};

    /// Build a synthetic ElementStatusData mirroring the
    /// `fake_library()` layout: 2 drive bays (0x0100/0x0101),
    /// 2 storage slots (0x0400/0x0401), 1 IE port (0x0300). Caller
    /// passes per-element overrides; defaults mirror fake_library's
    /// initial state (slot 0x0400 full with TAPE_A, rest empty).
    fn synthetic_es(drive_serials: [Option<&str>; 2]) -> ElementStatusData {
        let elements = vec![
            Element {
                element_type: ElementType::DataTransfer,
                address: 0x0100,
                full: false,
                impexp: false,
                except: false,
                access: true,
                export_enabled: false,
                import_enabled: false,
                source_address: None,
                primary_voltag: None,
                drive_serial: drive_serials[0].map(|s| s.to_string()),
            },
            Element {
                element_type: ElementType::DataTransfer,
                address: 0x0101,
                full: false,
                impexp: false,
                except: false,
                access: true,
                export_enabled: false,
                import_enabled: false,
                source_address: None,
                primary_voltag: None,
                drive_serial: drive_serials[1].map(|s| s.to_string()),
            },
            Element {
                element_type: ElementType::ImportExport,
                address: 0x0300,
                full: false,
                impexp: false,
                except: false,
                access: true,
                export_enabled: true,
                import_enabled: true,
                source_address: None,
                primary_voltag: None,
                drive_serial: None,
            },
            Element {
                element_type: ElementType::Storage,
                address: 0x0400,
                full: true,
                impexp: false,
                except: false,
                access: true,
                export_enabled: false,
                import_enabled: false,
                source_address: None,
                primary_voltag: Some("TAPE_A".into()),
                drive_serial: None,
            },
            Element {
                element_type: ElementType::Storage,
                address: 0x0401,
                full: false,
                impexp: false,
                except: false,
                access: true,
                export_enabled: false,
                import_enabled: false,
                source_address: None,
                primary_voltag: None,
                drive_serial: None,
            },
        ];
        let num = elements.len() as u16;
        ElementStatusData {
            first_element_address: 0x0100,
            num_elements: num,
            elements,
        }
    }

    /// Old library with bay 0x0100 fully resolved (DvcidAndInquiry +
    /// sg_path) and bay 0x0101 with a different drive. The fake
    /// helper's drive serials don't reflect realistic shape; what
    /// matters is the reconcile behavior.
    fn fake_old_for_reconcile() -> Library {
        let mut lib = fake_library();
        lib.drive_bays[0].installed.as_mut().unwrap().serial = "DRV_A".into();
        lib.drive_bays[0].installed.as_mut().unwrap().sg_path =
            Some(std::path::PathBuf::from("/dev/sg0"));
        lib.drive_bays[0].installed.as_mut().unwrap().vendor = Some("HPE".into());
        lib.drive_bays[1].installed.as_mut().unwrap().serial = "DRV_B".into();
        lib.drive_bays[1].installed.as_mut().unwrap().sg_path =
            Some(std::path::PathBuf::from("/dev/sg1"));
        lib
    }

    #[test]
    fn reconcile_preserves_host_side_fields_when_serial_matches() {
        let old = fake_old_for_reconcile();
        let new_es = synthetic_es([Some("DRV_A"), Some("DRV_B")]);
        let (new_lib, warnings) = reconcile(&old, new_es).expect("reconcile ok");

        assert!(warnings.is_empty(), "no warnings on identical reconcile");
        assert_eq!(new_lib.drive_bays.len(), 2);

        let bay_a = &new_lib.drive_bays[0];
        let inst_a = bay_a.installed.as_ref().expect("bay 0 installed");
        assert_eq!(inst_a.serial, "DRV_A");
        // Preserved from old:
        assert_eq!(inst_a.identity_source, IdentitySource::DvcidAndInquiry);
        assert_eq!(
            inst_a.sg_path.as_deref(),
            Some(std::path::Path::new("/dev/sg0"))
        );
        assert_eq!(inst_a.vendor.as_deref(), Some("HPE"));

        // Library identity preserved.
        assert_eq!(new_lib.serial, old.serial);
        assert_eq!(new_lib.changer_sg, old.changer_sg);
    }

    #[test]
    fn reconcile_emits_drive_replaced_on_serial_change() {
        let old = fake_old_for_reconcile();
        let new_es = synthetic_es([Some("DRV_A_PRIME"), Some("DRV_B")]); // bay 0 swapped
        let (new_lib, warnings) = reconcile(&old, new_es).expect("reconcile ok");

        // One warning, for bay 0x0100.
        assert_eq!(warnings.len(), 1);
        assert!(matches!(
            &warnings[0],
            RescanWarning::DriveReplaced {
                addr: 0x0100,
                old_serial,
                new_serial,
            } if old_serial == "DRV_A" && new_serial == "DRV_A_PRIME"
        ));

        // Bay 0 now has the new serial, NO old host-side data, identity_source = DvcidInline.
        let bay_a = &new_lib.drive_bays[0];
        let inst_a = bay_a.installed.as_ref().unwrap();
        assert_eq!(inst_a.serial, "DRV_A_PRIME");
        assert_eq!(inst_a.identity_source, IdentitySource::DvcidInline);
        assert!(inst_a.sg_path.is_none());
        assert!(inst_a.vendor.is_none());

        // Bay 1 is unchanged (serial matches → host-side preserved).
        let inst_b = new_lib.drive_bays[1].installed.as_ref().unwrap();
        assert_eq!(inst_b.serial, "DRV_B");
        assert_eq!(inst_b.identity_source, IdentitySource::DvcidAndInquiry);
        assert!(inst_b.sg_path.is_some());
    }

    #[test]
    fn reconcile_emits_drive_appeared_when_post_has_identity_pre_didnt() {
        let mut old = fake_old_for_reconcile();
        old.drive_bays[0].installed = None; // bay 0 unresolved at discovery
        let new_es = synthetic_es([Some("DRV_A_FRESH"), Some("DRV_B")]);
        let (new_lib, warnings) = reconcile(&old, new_es).expect("reconcile ok");

        assert_eq!(warnings.len(), 1);
        assert!(matches!(
            &warnings[0],
            RescanWarning::DriveAppeared { addr: 0x0100, serial } if serial == "DRV_A_FRESH"
        ));
        let inst_a = new_lib.drive_bays[0].installed.as_ref().unwrap();
        assert_eq!(inst_a.serial, "DRV_A_FRESH");
        assert_eq!(inst_a.identity_source, IdentitySource::DvcidInline);
        assert!(inst_a.sg_path.is_none());
    }

    #[test]
    fn reconcile_emits_drive_vanished_when_pre_had_identity_post_doesnt() {
        let old = fake_old_for_reconcile();
        let new_es = synthetic_es([None, Some("DRV_B")]); // bay 0 lost its serial
        let (new_lib, warnings) = reconcile(&old, new_es).expect("reconcile ok");

        assert_eq!(warnings.len(), 1);
        assert!(matches!(
            &warnings[0],
            RescanWarning::DriveVanished { addr: 0x0100, old_serial } if old_serial == "DRV_A"
        ));
        assert!(new_lib.drive_bays[0].installed.is_none());
    }

    #[test]
    fn reconcile_rejects_drive_count_mismatch() {
        let old = fake_old_for_reconcile();
        // ElementStatusData with only one DataTransfer element.
        let one_bay = ElementStatusData {
            first_element_address: 0x0100,
            num_elements: 1,
            elements: vec![Element {
                element_type: ElementType::DataTransfer,
                address: 0x0100,
                full: false,
                impexp: false,
                except: false,
                access: true,
                export_enabled: false,
                import_enabled: false,
                source_address: None,
                primary_voltag: None,
                drive_serial: Some("DRV_A".into()),
            }],
        };
        let err = reconcile(&old, one_bay).unwrap_err();
        assert!(err.contains("differ from prior snapshot"));
    }

    #[test]
    fn reconcile_rejects_slot_count_mismatch() {
        let old = fake_old_for_reconcile();
        // Drop the second storage slot from synthetic_es.
        let mut es = synthetic_es([Some("DRV_A"), Some("DRV_B")]);
        es.elements
            .retain(|e| !(e.element_type == ElementType::Storage && e.address == 0x0401));
        es.num_elements = es.elements.len() as u16;
        let err = reconcile(&old, es).unwrap_err();
        assert!(err.contains("differ from prior snapshot"));
    }

    #[test]
    fn reconcile_preserves_chassis_designator() {
        // Build a real DeviceDesignator from the real-hardware VPD 0x83
        // fixture so the test actually proves preservation of a
        // populated value — not just None == None.
        let did = remanence_scsi::DeviceIdentification::parse(include_bytes!(
            "../../../fixtures/real-hardware/remanence-fixtures-datamover-20260516T172906Z/inquiry/vpd-83/changer1.bin"
        ))
        .expect("VPD 0x83 fixture parses");
        let chassis = did
            .preferred_chassis()
            .cloned()
            .expect("real MSL3040 VPD 0x83 has a preferred chassis designator");

        // Sanity: the captured chassis is the HP NAA we've been
        // working with (0x5001438031bdc7d4) — if this changes the
        // fixture changed, not the reconcile logic.
        assert_eq!(chassis.as_naa(), Some(0x5001_4380_31bd_c7d4));

        let mut old = fake_old_for_reconcile();
        old.chassis_designator = Some(chassis.clone());
        let new_es = synthetic_es([Some("DRV_A"), Some("DRV_B")]);
        let (new_lib, _) = reconcile(&old, new_es).unwrap();

        assert_eq!(
            new_lib.chassis_designator,
            Some(chassis),
            "reconcile must preserve the populated chassis designator from old"
        );
    }

    #[test]
    fn reconcile_rejects_drive_bay_address_shift_at_same_count() {
        // Same count, different addresses → structural mismatch.
        // Pre-fix this would have reconciled "successfully" with
        // DriveVanished for the old addresses and DriveAppeared for
        // the new ones, silently dropping the host-side bindings.
        let old = fake_old_for_reconcile();
        // Synthesise a 2-drive RES whose bay addresses are 0x0200 and
        // 0x0201 instead of 0x0100/0x0101 — same count, shifted.
        let mut es = synthetic_es([Some("DRV_A"), Some("DRV_B")]);
        for e in es.elements.iter_mut() {
            if matches!(e.element_type, ElementType::DataTransfer) {
                e.address += 0x0100;
            }
        }
        let err = reconcile(&old, es).unwrap_err();
        assert!(
            err.contains("drive bay addresses"),
            "expected drive-bay address-set mismatch, got {err:?}"
        );
    }

    #[test]
    fn reconcile_rejects_slot_address_shift_at_same_count() {
        // Storage slots: same count (2), shifted addresses
        // (0x0400/0x0401 → 0x1000/0x1001).
        let old = fake_old_for_reconcile();
        let mut es = synthetic_es([Some("DRV_A"), Some("DRV_B")]);
        for e in es.elements.iter_mut() {
            if matches!(e.element_type, ElementType::Storage) {
                e.address = e.address.wrapping_add(0x0c00);
            }
        }
        let err = reconcile(&old, es).unwrap_err();
        assert!(
            err.contains("storage slot addresses"),
            "expected storage-slot address-set mismatch, got {err:?}"
        );
    }
}
