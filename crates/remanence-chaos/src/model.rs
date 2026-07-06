//! Stateful virtual SSC tape and SMC changer transport for L1b chaos tests.
//!
//! `ModelTransport` is intentionally small: it implements only the SCSI
//! commands Remanence's public drive/changer handles need for hermetic
//! Phase C tests. It is a record-oriented model, so one WRITE(6) buffer
//! becomes one tape record, filemarks are records of their own, and READ
//! POSITION reports the current record index.

use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use remanence_library::transport::{SgTransport, TransferOutcome};
use remanence_library::{DeviceCaptures, ElementLayout, IdentitySource, Library};
use remanence_scsi::{Inquiry, ScsiError};

const DEFAULT_BLOCK_SIZE: u32 = 1024 * 1024;
const DEFAULT_CAPACITY_BYTES: u64 = 1024 * 1024 * 1024;
const MAX_BLOCK_LENGTH: u32 = 0x00ff_ffff;
const MIN_BLOCK_LENGTH: u16 = 1;
const CHANGER_PATH: &str = "/dev/sg-chaos-changer";
const DRIVE_PATH_PREFIX: &str = "/dev/sg-chaos-drive-";

/// Shared virtual library state cloned into changer and drive transports.
pub type SharedVirtualWorld = Arc<Mutex<VirtualWorld>>;

/// One virtual tape record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Record {
    /// One data block written by a single WRITE(6) command.
    Block(Vec<u8>),
    /// One tape filemark.
    Filemark,
}

/// A virtual cartridge held by the model.
#[derive(Clone, Debug)]
pub struct VirtualTape {
    /// Tape records in physical order.
    pub records: Vec<Record>,
    /// EOM early-warning threshold in bytes.
    pub capacity_bytes: u64,
    /// Total data bytes written through this model.
    pub written_bytes: u64,
    /// Whether the model reports the medium as write-protected.
    pub write_protected: bool,
    /// Whether the model reports WORM media.
    pub worm: bool,
    /// Current fixed block size reported by MODE SENSE.
    pub block_size: u32,
    /// Hardware compression flag reported by MODE SENSE.
    pub compression: bool,
    /// TapeAlert flags currently reported for this cartridge.
    pub tape_alert_flags: BTreeSet<u8>,
    /// Whether this cartridge is a cleaning cartridge.
    pub cleaning_cart: bool,
    /// If set, the cartridge is considered expired and fast-ejects when loaded.
    pub cleaning_cart_expired: bool,
    /// If set, the cartridge clears the drive's dirty state after this many ops.
    pub cleaning_cycle_ops: Option<u64>,
}

impl VirtualTape {
    /// Build an empty writable virtual tape.
    pub fn empty(capacity_bytes: u64, block_size: u32) -> Self {
        Self {
            records: Vec::new(),
            capacity_bytes,
            written_bytes: 0,
            write_protected: false,
            worm: false,
            block_size,
            compression: false,
            tape_alert_flags: BTreeSet::new(),
            cleaning_cart: false,
            cleaning_cart_expired: false,
            cleaning_cycle_ops: None,
        }
    }
}

fn record_data_bytes(records: &[Record]) -> u64 {
    records
        .iter()
        .map(|record| match record {
            Record::Block(block) => block.len() as u64,
            Record::Filemark => 0,
        })
        .sum()
}

fn recompute_written_bytes(tape: &mut VirtualTape) {
    tape.written_bytes = record_data_bytes(&tape.records);
}

impl Default for VirtualTape {
    fn default() -> Self {
        Self::empty(DEFAULT_CAPACITY_BYTES, DEFAULT_BLOCK_SIZE)
    }
}

/// One storage slot in the virtual changer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SlotState {
    /// SCSI element address of this storage slot.
    pub address: u16,
    /// Whether the model reports a cartridge present in this slot.
    pub full: bool,
    /// Barcode currently in the slot.
    pub barcode: Option<String>,
    /// Whether the accessor can reach this slot.
    pub access: bool,
    /// Whether the slot reports an element exception.
    pub except: bool,
}

/// Shared virtual changer/tape state.
#[derive(Clone, Debug)]
pub struct VirtualWorld {
    /// Barcode-keyed virtual tapes.
    pub tapes: HashMap<String, VirtualTape>,
    /// Storage slots in element-address order.
    pub slots: Vec<SlotState>,
    /// Drive bay element address to loaded barcode.
    pub drive_bays: HashMap<u16, Option<String>>,
    /// Drive bay element address to source slot, when known.
    pub drive_source_slots: HashMap<u16, Option<u16>>,
    /// Virtual changer serial returned by VPD 0x80.
    pub changer_serial: String,
    /// Drive bay element address to drive serial returned by VPD 0x80.
    pub drive_serials: HashMap<u16, String>,
    /// Drive bay element address to drive-level TapeAlert flags.
    pub drive_alert_flags: HashMap<u16, BTreeSet<u8>>,
    /// Per-bay cleaning simulation state.
    pub drive_states: HashMap<u16, VirtualDriveState>,
    /// Changer element layout.
    pub element_layout: ElementLayout,
}

/// Per-drive cleaning simulation state.
#[derive(Clone, Debug, Default)]
pub struct VirtualDriveState {
    /// After how many drive ops the drive becomes dirty.
    pub dirty_after_ops: Option<u64>,
    /// Number of drive ops since the last clean-cycle reset.
    pub op_count: u64,
    /// Remaining ops before a cleaning cartridge auto-ejects.
    pub cleaning_cycle_ops_remaining: Option<u64>,
    /// Whether the drive has already reported flag 20 for the current dirty cycle.
    pub dirty_alert_reported: bool,
}

impl VirtualWorld {
    /// Build a single-drive virtual library with `slot_count` storage slots.
    pub fn single_drive(
        changer_serial: impl Into<String>,
        bay: u16,
        drive_serial: impl Into<String>,
        slot_start: u16,
        slot_count: u16,
    ) -> Self {
        let slots = (0..slot_count)
            .map(|offset| SlotState {
                address: slot_start.saturating_add(offset),
                full: false,
                barcode: None,
                access: true,
                except: false,
            })
            .collect::<Vec<_>>();
        let mut drive_serials = HashMap::new();
        drive_serials.insert(bay, drive_serial.into());
        let mut drive_bays = HashMap::new();
        drive_bays.insert(bay, None);
        let mut drive_source_slots = HashMap::new();
        drive_source_slots.insert(bay, None);
        let mut drive_alert_flags = HashMap::new();
        drive_alert_flags.insert(bay, BTreeSet::new());
        let mut drive_states = HashMap::new();
        drive_states.insert(bay, VirtualDriveState::default());
        Self {
            tapes: HashMap::new(),
            slots,
            drive_bays,
            drive_source_slots,
            changer_serial: changer_serial.into(),
            drive_serials,
            drive_alert_flags,
            drive_states,
            element_layout: ElementLayout {
                robot_address: 0,
                drive_start: bay,
                drive_count: 1,
                slot_start,
                slot_count,
                ie_start: 0,
                ie_count: 0,
            },
        }
    }

    /// Return the role to use for a transport factory path.
    pub fn role_for_path(&self, path: impl AsRef<std::path::Path>) -> Option<DeviceRole> {
        let path = path.as_ref().to_string_lossy();
        if path == CHANGER_PATH {
            return Some(DeviceRole::Changer);
        }
        let suffix = path.strip_prefix(DRIVE_PATH_PREFIX)?;
        let bay = u16::from_str_radix(suffix, 16).ok()?;
        self.drive_serials
            .contains_key(&bay)
            .then_some(DeviceRole::Drive { bay })
    }

    /// Insert a tape and place its barcode in the given storage slot.
    pub fn put_tape_in_slot(
        &mut self,
        slot_address: u16,
        barcode: impl Into<String>,
        tape: VirtualTape,
    ) {
        let barcode = barcode.into();
        self.tapes.insert(barcode.clone(), tape);
        let slot = self
            .slots
            .iter_mut()
            .find(|slot| slot.address == slot_address)
            .expect("slot address belongs to virtual world");
        slot.full = true;
        slot.barcode = Some(barcode);
    }

    /// Insert a tape and mark it loaded in `bay`.
    pub fn put_tape_in_drive(
        &mut self,
        bay: u16,
        barcode: impl Into<String>,
        source_slot: Option<u16>,
        tape: VirtualTape,
    ) {
        let barcode = barcode.into();
        self.tapes.insert(barcode.clone(), tape);
        self.drive_bays.insert(bay, Some(barcode));
        self.drive_source_slots.insert(bay, source_slot);
    }

    /// Loaded barcode in `bay`, if any.
    pub fn loaded_barcode(&self, bay: u16) -> Option<&str> {
        self.drive_bays.get(&bay)?.as_deref()
    }

    /// Set one TapeAlert flag on a virtual cartridge.
    pub fn set_tape_alert(&mut self, barcode: impl AsRef<str>, flag: u8) -> bool {
        if !(1..=remanence_scsi::log_sense::TAPE_ALERT_FLAG_COUNT).contains(&flag) {
            return false;
        }
        let Some(tape) = self.tapes.get_mut(barcode.as_ref()) else {
            return false;
        };
        tape.tape_alert_flags.insert(flag)
    }

    /// Set one drive-level TapeAlert flag on a virtual bay.
    pub fn set_drive_alert(&mut self, bay: u16, flag: u8) -> bool {
        if !(1..=remanence_scsi::log_sense::TAPE_ALERT_FLAG_COUNT).contains(&flag) {
            return false;
        }
        self.drive_alert_flags.entry(bay).or_default().insert(flag)
    }

    /// Arm one bay to become dirty after `dirty_after_ops` drive commands.
    pub fn arm_drive_dirty_after_ops(&mut self, bay: u16, dirty_after_ops: u64) -> bool {
        let Some(state) = self.drive_states.get_mut(&bay) else {
            return false;
        };
        state.dirty_after_ops = Some(dirty_after_ops);
        state.op_count = 0;
        state.dirty_alert_reported = false;
        true
    }

    /// Mark one storage slot inaccessible in the honest model state.
    pub fn set_slot_inaccessible(&mut self, address: u16) -> bool {
        let Some(slot) = self.slots.iter_mut().find(|slot| slot.address == address) else {
            return false;
        };
        slot.access = false;
        slot.except = true;
        true
    }

    /// Mark one storage slot full while leaving its barcode unreadable.
    pub fn set_slot_full_without_voltag(&mut self, address: u16) -> bool {
        let Some(slot) = self.slots.iter_mut().find(|slot| slot.address == address) else {
            return false;
        };
        slot.full = true;
        slot.barcode = None;
        true
    }

    /// Build a public `Library` snapshot consistent with this virtual world.
    pub fn library_snapshot(&self) -> Library {
        let changer_inquiry = parse_inquiry(changer_inquiry_response());
        let elements = self.element_status_bytes();
        let element_status =
            remanence_scsi::read_element_status::parse(&elements).expect("model RES parses");
        let captures = DeviceCaptures {
            changer_inquiry,
            unit_serial: self.changer_serial.clone(),
            device_id: None,
            element_status,
            changer_sg: PathBuf::from(CHANGER_PATH),
            changer_sysfs: PathBuf::from("/sys/chaos/changer"),
        };
        let mut library = Library::from_captures(captures);
        for bay in &mut library.drive_bays {
            if let Some(installed) = &mut bay.installed {
                installed.identity_source = IdentitySource::DvcidAndInquiry;
                installed.sg_path = Some(drive_path(bay.element_address));
                installed.sysfs_path = Some(PathBuf::from(format!(
                    "/sys/chaos/drive/{:04x}",
                    bay.element_address
                )));
            }
        }
        library
    }

    fn element_status_bytes(&self) -> Vec<u8> {
        let mut pages = Vec::new();
        append_element_page(
            &mut pages,
            1,
            COMMON_DESC_LEN,
            vec![common_descriptor(
                self.element_layout.robot_address,
                false,
                None,
                None,
            )],
            false,
        );

        let slot_descs = self
            .slots
            .iter()
            .map(|slot| {
                descriptor_with_voltag(
                    slot.address,
                    slot.full,
                    slot.barcode.as_deref(),
                    None,
                    None,
                    slot.access,
                    slot.except,
                )
            })
            .collect::<Vec<_>>();
        append_element_page(&mut pages, 2, DESC_WITH_VOLTAG_LEN, slot_descs, true);

        let drive_descs = self
            .drive_serials
            .iter()
            .map(|(&bay, serial)| {
                let barcode = self.drive_bays.get(&bay).and_then(Option::as_deref);
                let source = self.drive_source_slots.get(&bay).copied().flatten();
                descriptor_with_voltag(
                    bay,
                    barcode.is_some(),
                    barcode,
                    source,
                    Some(serial.as_str()),
                    true,
                    false,
                )
            })
            .collect::<Vec<_>>();
        append_element_page(
            &mut pages,
            4,
            DESC_WITH_VOLTAG_AND_DVCID_LEN,
            drive_descs,
            true,
        );

        let num_elements = 1 + self.slots.len() + self.drive_serials.len();
        let mut response = vec![0u8; 8];
        response[0..2].copy_from_slice(&self.element_layout.robot_address.to_be_bytes());
        response[2..4].copy_from_slice(&(num_elements as u16).to_be_bytes());
        response[5] = ((pages.len() >> 16) & 0xff) as u8;
        response[6] = ((pages.len() >> 8) & 0xff) as u8;
        response[7] = (pages.len() & 0xff) as u8;
        response.extend_from_slice(&pages);
        response
    }
}

/// Device role for a `ModelTransport` instance.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeviceRole {
    /// Medium changer device.
    Changer,
    /// Sequential-access tape drive in a bay element.
    Drive {
        /// Drive bay element address.
        bay: u16,
    },
}

/// Stateful virtual SCSI transport for one changer or drive device.
#[derive(Debug)]
pub struct ModelTransport {
    world: SharedVirtualWorld,
    role: DeviceRole,
    position: u64,
}

impl ModelTransport {
    /// Construct a model transport for one virtual device.
    pub fn new(world: SharedVirtualWorld, role: DeviceRole) -> Self {
        Self {
            world,
            role,
            position: 0,
        }
    }

    /// Return the role this transport is serving.
    pub fn role(&self) -> DeviceRole {
        self.role
    }

    fn drive_bay(&self) -> Result<u16, ScsiError> {
        match self.role {
            DeviceRole::Drive { bay } => Ok(bay),
            DeviceRole::Changer => Err(ScsiError::InvalidInput("drive command sent to changer")),
        }
    }

    fn ensure_changer(&self) -> Result<(), ScsiError> {
        match self.role {
            DeviceRole::Changer => Ok(()),
            DeviceRole::Drive { .. } => {
                Err(ScsiError::InvalidInput("changer command sent to drive"))
            }
        }
    }

    fn advance_drive_cleaning_state(&self, world: &mut VirtualWorld) -> Result<bool, ScsiError> {
        let bay = self.drive_bay()?;
        let Some(barcode) = world.loaded_barcode(bay).map(str::to_string) else {
            return Ok(false);
        };
        let Some(tape) = world.tapes.get(&barcode) else {
            return Ok(false);
        };
        if tape.cleaning_cart {
            let mut auto_eject = false;
            {
                let state = world.drive_states.entry(bay).or_default();
                if let Some(remaining) = state.cleaning_cycle_ops_remaining.as_mut() {
                    if *remaining > 0 {
                        *remaining -= 1;
                    }
                    if *remaining == 0 {
                        auto_eject = true;
                        state.cleaning_cycle_ops_remaining = None;
                    }
                }
            }
            if auto_eject {
                let source_slot = world.drive_source_slots.get(&bay).copied().flatten();
                if let Some(source_slot) = source_slot {
                    let barcode = world
                        .drive_bays
                        .get_mut(&bay)
                        .and_then(Option::take)
                        .ok_or(ScsiError::InvalidInput("cleaning cart vanished"))?;
                    put_into_element(&mut *world, source_slot, barcode, Some(source_slot))?;
                    world.drive_source_slots.insert(bay, None);
                }
            }
            return Ok(auto_eject);
        }

        let dirty_reached = {
            let state = world.drive_states.entry(bay).or_default();
            state.op_count = state.op_count.saturating_add(1);
            if let Some(dirty_after_ops) = state.dirty_after_ops {
                state.op_count >= dirty_after_ops
            } else {
                false
            }
        };
        if dirty_reached {
            world.drive_alert_flags.entry(bay).or_default().insert(20);
        }
        Ok(false)
    }

    fn with_loaded_tape<R>(
        &self,
        world: &mut VirtualWorld,
        f: impl FnOnce(&mut VirtualTape) -> Result<R, ScsiError>,
    ) -> Result<R, ScsiError> {
        let bay = self.drive_bay()?;
        let barcode = world
            .drive_bays
            .get(&bay)
            .and_then(Option::as_ref)
            .cloned()
            .ok_or(ScsiError::InvalidInput("drive bay has no loaded tape"))?;
        let tape = world
            .tapes
            .get_mut(&barcode)
            .ok_or(ScsiError::InvalidInput(
                "loaded barcode has no virtual tape",
            ))?;
        f(tape)
    }

    fn inquiry(&self, buf: &mut [u8], evpd: bool, page: u8) -> Result<TransferOutcome, ScsiError> {
        let world = self.world.lock().expect("virtual world lock");
        let response = if evpd && page == 0x80 {
            match self.role {
                DeviceRole::Changer => vpd80_response(&world.changer_serial),
                DeviceRole::Drive { bay } => {
                    let serial = world
                        .drive_serials
                        .get(&bay)
                        .ok_or(ScsiError::InvalidInput("unknown drive bay"))?;
                    vpd80_response(serial)
                }
            }
        } else if evpd {
            return Err(ScsiError::InvalidInput("unsupported VPD page"));
        } else {
            match self.role {
                DeviceRole::Changer => changer_inquiry_response(),
                DeviceRole::Drive { .. } => tape_inquiry_response(),
            }
        };
        copy_response(buf, &response)
    }

    fn read_block_limits(&self, buf: &mut [u8]) -> Result<TransferOutcome, ScsiError> {
        self.drive_bay()?;
        copy_response(buf, &rbl_response(MAX_BLOCK_LENGTH, MIN_BLOCK_LENGTH))
    }

    fn mode_sense(&self, buf: &mut [u8]) -> Result<TransferOutcome, ScsiError> {
        let mut world = self.world.lock().expect("virtual world lock");
        let block_length = self.with_loaded_tape(&mut world, |tape| Ok(tape.block_size))?;
        let compression = self.with_loaded_tape(&mut world, |tape| Ok(tape.compression))?;
        copy_response(buf, &mode_sense_response(block_length, compression))
    }

    fn read_position(&self, buf: &mut [u8]) -> Result<TransferOutcome, ScsiError> {
        self.drive_bay()?;
        let flags = if self.position == 0 { 0x80 } else { 0x00 };
        copy_response(buf, &rp_long_response(flags, 0, self.position))
    }

    fn log_sense(&self, cdb: &[u8], buf: &mut [u8]) -> Result<TransferOutcome, ScsiError> {
        let bay = self.drive_bay()?;
        let page_code = cdb.get(2).copied().unwrap_or(0) & 0x3f;
        if page_code != remanence_scsi::log_sense::PAGE_TAPE_ALERT {
            return Err(ScsiError::InvalidInput("unsupported LOG SENSE page"));
        }

        let world = self.world.lock().expect("virtual world lock");
        let mut flags = world
            .drive_alert_flags
            .get(&bay)
            .cloned()
            .unwrap_or_default();
        if let Some(barcode) = world.loaded_barcode(bay) {
            if let Some(tape) = world.tapes.get(barcode) {
                flags.extend(tape.tape_alert_flags.iter().copied());
            }
        }

        let page = remanence_scsi::log_sense::synthesize_tape_alert_page(&flags);
        let n = page.len().min(buf.len()).min(log_sense_alloc_len(cdb));
        buf[..n].copy_from_slice(&page[..n]);
        Ok(TransferOutcome::clean(n as u32))
    }

    fn test_unit_ready(&self) -> Result<(), ScsiError> {
        let bay = self.drive_bay()?;
        let world = self.world.lock().expect("virtual world lock");
        match world.drive_bays.get(&bay) {
            Some(Some(barcode)) if world.tapes.contains_key(barcode) => Ok(()),
            Some(_) => Err(check_condition(no_medium_sense(), 0)),
            None => Err(ScsiError::InvalidInput("unknown drive bay")),
        }
    }

    fn read_6(&mut self, buf: &mut [u8]) -> Result<TransferOutcome, ScsiError> {
        let mut world = self.world.lock().expect("virtual world lock");
        let position = usize::try_from(self.position)
            .map_err(|_| ScsiError::InvalidInput("model position exceeds usize"))?;
        let outcome = self.with_loaded_tape(&mut world, |tape| {
            let Some(record) = tape.records.get(position).cloned() else {
                return Err(check_condition(blank_check_eod_sense(), 0));
            };
            match record {
                Record::Block(block) => {
                    let n = block.len().min(buf.len());
                    buf[..n].copy_from_slice(&block[..n]);
                    Ok(TransferOutcome::clean(n as u32))
                }
                Record::Filemark => Err(check_condition(filemark_sense(), 0)),
            }
        });
        let outcome = outcome
            .inspect(|_| {
                self.position = self.position.saturating_add(1);
            })
            .inspect_err(|err| {
                if is_filemark_check_condition(err) {
                    self.position = self.position.saturating_add(1);
                }
            })?;
        if self.advance_drive_cleaning_state(&mut world)? {
            self.position = 0;
        }
        Ok(outcome)
    }

    fn write_6(&mut self, buf: &[u8]) -> Result<TransferOutcome, ScsiError> {
        let mut world = self.world.lock().expect("virtual world lock");
        let position = usize::try_from(self.position)
            .map_err(|_| ScsiError::InvalidInput("model position exceeds usize"))?;
        let mut crossed_eom = false;
        self.with_loaded_tape(&mut world, |tape| {
            if tape.write_protected {
                return Err(ScsiError::InvalidInput("virtual tape is write protected"));
            }
            if position < tape.records.len() {
                tape.records.truncate(position);
                recompute_written_bytes(tape);
            }
            tape.records.push(Record::Block(buf.to_vec()));
            tape.written_bytes = tape.written_bytes.saturating_add(buf.len() as u64);
            crossed_eom = tape.written_bytes > tape.capacity_bytes;
            Ok(())
        })?;
        self.position = self.position.saturating_add(1);
        if self.advance_drive_cleaning_state(&mut world)? {
            self.position = 0;
        }
        if crossed_eom {
            Err(check_condition(eom_sense(0x00, 0), buf.len() as u32))
        } else {
            Ok(TransferOutcome::clean(buf.len() as u32))
        }
    }

    fn write_filemarks(&mut self, cdb: &[u8]) -> Result<(), ScsiError> {
        let count = read_u24(cdb, 2).ok_or(ScsiError::InvalidInput("short WRITE FILEMARKS CDB"))?;
        let mut world = self.world.lock().expect("virtual world lock");
        let position = usize::try_from(self.position)
            .map_err(|_| ScsiError::InvalidInput("model position exceeds usize"))?;
        self.with_loaded_tape(&mut world, |tape| {
            if position < tape.records.len() {
                tape.records.truncate(position);
                recompute_written_bytes(tape);
            }
            for _ in 0..count {
                tape.records.push(Record::Filemark);
            }
            Ok(())
        })?;
        self.position = self.position.saturating_add(u64::from(count));
        if self.advance_drive_cleaning_state(&mut world)? {
            self.position = 0;
        }
        Ok(())
    }

    fn mode_select(&mut self, buf: &[u8]) -> Result<TransferOutcome, ScsiError> {
        let mut world = self.world.lock().expect("virtual world lock");
        self.with_loaded_tape(&mut world, |tape| {
            if buf.len() >= 12 {
                let block_length =
                    ((buf[9] as u32) << 16) | ((buf[10] as u32) << 8) | buf[11] as u32;
                if block_length != 0 {
                    tape.block_size = block_length;
                }
            }
            let page_offset = buf.get(3).copied().map(|bdl| 4 + bdl as usize);
            if let Some(page_offset) = page_offset {
                if buf.len() > page_offset + 2 && (buf[page_offset] & 0x3f) == 0x0f {
                    tape.compression = buf[page_offset + 2] & 0x80 != 0;
                }
            }
            Ok(())
        })?;
        Ok(TransferOutcome::clean(buf.len() as u32))
    }

    fn load_unload(&mut self, cdb: &[u8]) -> Result<(), ScsiError> {
        let load = cdb.get(4).copied().unwrap_or(0) & 0x01 != 0;
        if load {
            let mut world = self.world.lock().expect("virtual world lock");
            let bay = self.drive_bay()?;
            if !world.drive_bays.contains_key(&bay) {
                return Err(ScsiError::InvalidInput("unknown drive bay"));
            }
            let Some(barcode) = world.drive_bays[&bay].clone() else {
                return Err(ScsiError::InvalidInput("load requested with empty bay"));
            };
            let expired = world
                .tapes
                .get(&barcode)
                .is_some_and(|tape| tape.cleaning_cart_expired);
            let cleaning_cycle_ops = world
                .tapes
                .get(&barcode)
                .and_then(|tape| tape.cleaning_cycle_ops);
            self.position = 0;
            if expired {
                world.drive_alert_flags.entry(bay).or_default().insert(22);
                let source_slot = world.drive_source_slots.get(&bay).copied().flatten();
                if let Some(source_slot) = source_slot {
                    let barcode = world
                        .drive_bays
                        .get_mut(&bay)
                        .and_then(Option::take)
                        .ok_or(ScsiError::InvalidInput("expired cleaning cart vanished"))?;
                    put_into_element(&mut world, source_slot, barcode, Some(source_slot))?;
                    world.drive_source_slots.insert(bay, None);
                }
                world
                    .drive_states
                    .entry(bay)
                    .or_default()
                    .cleaning_cycle_ops_remaining = None;
                return Ok(());
            }
            let state = world.drive_states.entry(bay).or_default();
            state.op_count = 0;
            state.dirty_alert_reported = false;
            state.cleaning_cycle_ops_remaining = cleaning_cycle_ops;
            if let Some(flags) = world.drive_alert_flags.get_mut(&bay) {
                flags.remove(&20);
            }
            Ok(())
        } else {
            self.position = 0;
            Ok(())
        }
    }

    fn rewind(&mut self) {
        self.position = 0;
    }

    fn locate(&mut self, cdb: &[u8]) -> Result<(), ScsiError> {
        let lba = match cdb.first().copied() {
            Some(0x92) if cdb.len() >= 12 => u64::from_be_bytes([
                cdb[4], cdb[5], cdb[6], cdb[7], cdb[8], cdb[9], cdb[10], cdb[11],
            ]),
            Some(0x2b) if cdb.len() >= 7 => {
                u32::from_be_bytes([cdb[3], cdb[4], cdb[5], cdb[6]]) as u64
            }
            _ => return Err(ScsiError::InvalidInput("short LOCATE CDB")),
        };
        self.position = lba;
        Ok(())
    }

    fn space(&mut self, cdb: &[u8]) -> Result<(), ScsiError> {
        let (code, count) = match cdb.first().copied() {
            Some(0x11) if cdb.len() >= 5 => {
                let raw = read_u24(cdb, 2).expect("length checked");
                ((cdb[1] & 0x07), sign_extend_24(raw) as i64)
            }
            Some(0x91) if cdb.len() >= 12 => (
                cdb[1] & 0x07,
                i64::from_be_bytes([
                    cdb[4], cdb[5], cdb[6], cdb[7], cdb[8], cdb[9], cdb[10], cdb[11],
                ]),
            ),
            _ => return Err(ScsiError::InvalidInput("short SPACE CDB")),
        };
        match code {
            0 => self.space_blocks(count),
            1 => self.space_filemarks(count),
            3 => {
                let world = self.world.lock().expect("virtual world lock");
                let bay = self.drive_bay()?;
                let barcode = world.drive_bays.get(&bay).and_then(Option::as_ref);
                if let Some(tape) = barcode.and_then(|barcode| world.tapes.get(barcode)) {
                    self.position = tape.records.len() as u64;
                }
                Ok(())
            }
            _ => Err(ScsiError::InvalidInput("unsupported SPACE code")),
        }
    }

    fn space_blocks(&mut self, count: i64) -> Result<(), ScsiError> {
        let world = self.world.lock().expect("virtual world lock");
        let bay = self.drive_bay()?;
        let end = world
            .drive_bays
            .get(&bay)
            .and_then(Option::as_ref)
            .and_then(|barcode| world.tapes.get(barcode))
            .map_or(0, |tape| tape.records.len() as u64);
        let start = self.position;
        let requested_end = if count >= 0 {
            start.saturating_add(count as u64)
        } else {
            let magnitude = count.unsigned_abs();
            if magnitude > start {
                self.position = 0;
                let residual = residual_after_partial_space(count, start, self.position);
                return Err(check_condition(space_residual_sense(residual, 0x00), 0));
            }
            start - magnitude
        };
        self.position = requested_end.min(end);
        if requested_end != self.position {
            let residual = residual_after_partial_space(count, start, self.position);
            return Err(check_condition(space_residual_sense(residual, 0x08), 0));
        }
        Ok(())
    }

    fn space_filemarks(&mut self, count: i64) -> Result<(), ScsiError> {
        if count == 0 {
            return Ok(());
        }
        let world = self.world.lock().expect("virtual world lock");
        let bay = self.drive_bay()?;
        let records = world
            .drive_bays
            .get(&bay)
            .and_then(Option::as_ref)
            .and_then(|barcode| world.tapes.get(barcode))
            .map(|tape| tape.records.clone())
            .unwrap_or_default();
        let start = self.position;
        let mut remaining = count.unsigned_abs();
        if count > 0 {
            while remaining > 0 {
                let mut pos = self.position as usize;
                let mut found = false;
                while pos < records.len() {
                    if matches!(records[pos], Record::Filemark) {
                        self.position = (pos + 1) as u64;
                        found = true;
                        break;
                    }
                    pos += 1;
                }
                if !found {
                    self.position = records.len() as u64;
                    let traversed = count.unsigned_abs() - remaining;
                    let residual = count - i64::try_from(traversed).unwrap_or(i64::MAX);
                    return Err(check_condition(space_residual_sense(residual, 0x08), 0));
                }
                remaining -= 1;
            }
        } else {
            while remaining > 0 {
                if self.position == 0 {
                    let traversed = start as i64 - self.position as i64;
                    let residual = count + traversed;
                    return Err(check_condition(space_residual_sense(residual, 0x00), 0));
                }
                let mut pos = self.position.saturating_sub(1) as usize;
                let mut found = false;
                loop {
                    if matches!(records.get(pos), Some(Record::Filemark)) {
                        self.position = pos as u64;
                        found = true;
                        break;
                    }
                    if pos == 0 {
                        break;
                    }
                    pos -= 1;
                }
                if !found {
                    self.position = 0;
                    let traversed = start as i64 - self.position as i64;
                    let residual = count + traversed;
                    return Err(check_condition(space_residual_sense(residual, 0x00), 0));
                }
                remaining -= 1;
            }
        }
        Ok(())
    }

    fn move_medium(&mut self, cdb: &[u8]) -> Result<(), ScsiError> {
        if cdb.len() < 8 {
            return Err(ScsiError::InvalidInput("short MOVE MEDIUM CDB"));
        }
        let src = u16::from_be_bytes([cdb[4], cdb[5]]);
        let dst = u16::from_be_bytes([cdb[6], cdb[7]]);
        let mut world = self.world.lock().expect("virtual world lock");
        let barcode = take_from_element(&mut world, src)?;
        put_into_element(&mut world, dst, barcode, Some(src))
    }
}

impl SgTransport for ModelTransport {
    fn execute_in(&mut self, cdb: &[u8], buf: &mut [u8]) -> Result<TransferOutcome, ScsiError> {
        match cdb.first().copied() {
            Some(0x05) => self.read_block_limits(buf),
            Some(0x08) => self.read_6(buf),
            Some(0x12) => self.inquiry(
                buf,
                cdb.get(1).copied().unwrap_or(0) & 0x01 != 0,
                cdb.get(2).copied().unwrap_or(0),
            ),
            Some(0x1a) => self.mode_sense(buf),
            Some(0x34) => self.read_position(buf),
            Some(0x4d) => self.log_sense(cdb, buf),
            Some(0xb8) => {
                self.ensure_changer()?;
                let world = self.world.lock().expect("virtual world lock");
                copy_response(buf, &world.element_status_bytes())
            }
            _ => Err(ScsiError::InvalidInput("unsupported data-in CDB")),
        }
    }

    fn execute_none(&mut self, cdb: &[u8]) -> Result<(), ScsiError> {
        match cdb.first().copied() {
            Some(0x00) => self.test_unit_ready(),
            Some(0x01) => {
                self.drive_bay()?;
                self.rewind();
                Ok(())
            }
            Some(0x10) => self.write_filemarks(cdb),
            Some(0x11 | 0x91) => self.space(cdb),
            Some(0x1b) => self.load_unload(cdb),
            Some(0x2b | 0x92) => {
                self.drive_bay()?;
                self.locate(cdb)
            }
            Some(0xa5) => {
                self.ensure_changer()?;
                self.move_medium(cdb)
            }
            _ => Err(ScsiError::InvalidInput("unsupported no-data CDB")),
        }
    }

    fn execute_out(&mut self, cdb: &[u8], buf: &[u8]) -> Result<TransferOutcome, ScsiError> {
        match cdb.first().copied() {
            Some(0x0a) => self.write_6(buf),
            Some(0x15) => self.mode_select(buf),
            _ => Err(ScsiError::InvalidInput("unsupported data-out CDB")),
        }
    }
}

fn copy_response(buf: &mut [u8], response: &[u8]) -> Result<TransferOutcome, ScsiError> {
    let n = buf.len().min(response.len());
    buf[..n].copy_from_slice(&response[..n]);
    Ok(TransferOutcome::clean(n as u32))
}

fn log_sense_alloc_len(cdb: &[u8]) -> usize {
    if cdb.len() >= 9 {
        u16::from_be_bytes([cdb[7], cdb[8]]) as usize
    } else {
        usize::MAX
    }
}

fn drive_path(bay: u16) -> PathBuf {
    PathBuf::from(format!("{DRIVE_PATH_PREFIX}{bay:04x}"))
}

fn parse_inquiry(bytes: Vec<u8>) -> Inquiry {
    Inquiry::parse(&bytes).expect("bundled model inquiry fixture parses")
}

fn changer_inquiry_response() -> Vec<u8> {
    include_bytes!("../../../fixtures/inquiry/changer-msl-g3.bin").to_vec()
}

fn tape_inquiry_response() -> Vec<u8> {
    include_bytes!("../../../fixtures/inquiry/drive1-lto9.bin").to_vec()
}

fn vpd80_response(serial: &str) -> Vec<u8> {
    let bytes = serial.as_bytes();
    let mut v = vec![0x00, 0x80, 0x00, bytes.len() as u8];
    v.extend_from_slice(bytes);
    v
}

fn rbl_response(max_block_length: u32, min_block_length: u16) -> Vec<u8> {
    let mut buf = vec![0u8; 6];
    let max = max_block_length.to_be_bytes();
    buf[1] = max[1];
    buf[2] = max[2];
    buf[3] = max[3];
    let min = min_block_length.to_be_bytes();
    buf[4] = min[0];
    buf[5] = min[1];
    buf
}

fn mode_sense_response(block_length: u32, dce: bool) -> Vec<u8> {
    let mut buf = vec![0u8; 28];
    buf[0] = 27;
    buf[1] = 0x98;
    buf[2] = 0x10;
    buf[3] = 8;
    let bl = block_length.to_be_bytes();
    buf[9] = bl[1];
    buf[10] = bl[2];
    buf[11] = bl[3];
    buf[12] = 0x0f;
    buf[13] = 14;
    buf[14] = if dce { 0x80 } else { 0x00 };
    buf
}

fn rp_long_response(flags: u8, partition: u32, lba: u64) -> Vec<u8> {
    let mut v = vec![0u8; 32];
    v[0] = flags;
    v[4..8].copy_from_slice(&partition.to_be_bytes());
    v[8..16].copy_from_slice(&lba.to_be_bytes());
    v
}

const COMMON_DESC_LEN: usize = 12;
const VOLTAG_BLOCK_LEN: usize = 36;
const DESC_WITH_VOLTAG_LEN: usize = COMMON_DESC_LEN + VOLTAG_BLOCK_LEN;
const DVCID_DESCRIPTOR_HEADER_LEN: usize = 4;
const DESC_WITH_VOLTAG_AND_DVCID_LEN: usize =
    DESC_WITH_VOLTAG_LEN + DVCID_DESCRIPTOR_HEADER_LEN + 32;

fn append_element_page(
    out: &mut Vec<u8>,
    element_type: u8,
    desc_len: usize,
    descriptors: Vec<Vec<u8>>,
    pvoltag: bool,
) {
    if descriptors.is_empty() {
        return;
    }
    let page_bytes = desc_len * descriptors.len();
    out.push(element_type);
    out.push(if pvoltag { 0x80 } else { 0x00 });
    out.extend_from_slice(&(desc_len as u16).to_be_bytes());
    out.push(0);
    out.push(((page_bytes >> 16) & 0xff) as u8);
    out.push(((page_bytes >> 8) & 0xff) as u8);
    out.push((page_bytes & 0xff) as u8);
    for mut desc in descriptors {
        desc.resize(desc_len, 0);
        out.extend_from_slice(&desc);
    }
}

fn common_descriptor(
    address: u16,
    full: bool,
    source: Option<u16>,
    serial: Option<&str>,
) -> Vec<u8> {
    let mut desc = vec![0u8; COMMON_DESC_LEN];
    desc[0..2].copy_from_slice(&address.to_be_bytes());
    if full {
        desc[2] |= 0x01;
    }
    if let Some(source) = source {
        desc[9] |= 0x80;
        desc[10..12].copy_from_slice(&source.to_be_bytes());
    }
    if let Some(serial) = serial {
        append_dvcid_descriptor(&mut desc, serial);
    }
    desc
}

fn descriptor_with_voltag(
    address: u16,
    full: bool,
    barcode: Option<&str>,
    source: Option<u16>,
    serial: Option<&str>,
    access: bool,
    except: bool,
) -> Vec<u8> {
    let mut desc = common_descriptor(address, full, source, None);
    if access {
        desc[2] |= 0x08;
    }
    if except {
        desc[2] |= 0x04;
    }
    let mut voltag = [b' '; VOLTAG_BLOCK_LEN];
    if let Some(barcode) = barcode {
        for (dst, src) in voltag.iter_mut().take(32).zip(barcode.as_bytes()) {
            *dst = *src;
        }
    }
    desc.extend_from_slice(&voltag);
    if let Some(serial) = serial {
        append_dvcid_descriptor(&mut desc, serial);
    }
    desc
}

fn append_dvcid_descriptor(out: &mut Vec<u8>, serial: &str) {
    let mut ident = [b' '; 32];
    for (dst, src) in ident.iter_mut().zip(serial.as_bytes()) {
        *dst = *src;
    }
    out.extend_from_slice(&[0x02, 0x00, 0x00, ident.len() as u8]);
    out.extend_from_slice(&ident);
}

fn take_from_element(world: &mut VirtualWorld, element: u16) -> Result<String, ScsiError> {
    if let Some(slot) = world.slots.iter_mut().find(|slot| slot.address == element) {
        if !slot.full {
            return Err(ScsiError::InvalidInput("source slot is empty"));
        }
        let Some(barcode) = slot.barcode.take() else {
            return Err(ScsiError::InvalidInput(
                "source slot has no readable barcode",
            ));
        };
        slot.full = false;
        return Ok(barcode);
    }
    if let Some(entry) = world.drive_bays.get_mut(&element) {
        world.drive_source_slots.insert(element, None);
        return entry
            .take()
            .ok_or(ScsiError::InvalidInput("source bay is empty"));
    }
    Err(ScsiError::InvalidInput("unknown source element"))
}

fn put_into_element(
    world: &mut VirtualWorld,
    element: u16,
    barcode: String,
    source: Option<u16>,
) -> Result<(), ScsiError> {
    if let Some(slot) = world.slots.iter_mut().find(|slot| slot.address == element) {
        if slot.full {
            return Err(ScsiError::InvalidInput("destination slot is occupied"));
        }
        slot.full = true;
        slot.barcode = Some(barcode);
        return Ok(());
    }
    if let Some(entry) = world.drive_bays.get_mut(&element) {
        if entry.is_some() {
            return Err(ScsiError::InvalidInput("destination bay is occupied"));
        }
        *entry = Some(barcode);
        world.drive_source_slots.insert(element, source);
        return Ok(());
    }
    Err(ScsiError::InvalidInput("unknown destination element"))
}

fn read_u24(cdb: &[u8], offset: usize) -> Option<u32> {
    Some(
        ((*cdb.get(offset)? as u32) << 16)
            | ((*cdb.get(offset + 1)? as u32) << 8)
            | (*cdb.get(offset + 2)? as u32),
    )
}

fn sign_extend_24(value: u32) -> i32 {
    if value & 0x0080_0000 != 0 {
        (value | 0xff00_0000) as i32
    } else {
        value as i32
    }
}

fn residual_after_partial_space(requested: i64, start: u64, actual_end: u64) -> i64 {
    let traversed = actual_end as i128 - start as i128;
    let residual = requested as i128 - traversed;
    residual.clamp(i64::MIN as i128, i64::MAX as i128) as i64
}

#[cfg(target_os = "linux")]
fn check_condition(sense: Vec<u8>, bytes_transferred: u32) -> ScsiError {
    ScsiError::CheckCondition {
        sense,
        bytes_transferred,
    }
}

#[cfg(not(target_os = "linux"))]
fn check_condition(_sense: Vec<u8>, _bytes_transferred: u32) -> ScsiError {
    ScsiError::InvalidInput("CHECK CONDITION synthesis is Linux-only")
}

#[cfg(target_os = "linux")]
fn is_filemark_check_condition(err: &ScsiError) -> bool {
    matches!(err, ScsiError::CheckCondition { sense, .. } if sense.get(2).copied().unwrap_or(0) & 0x80 != 0)
}

#[cfg(not(target_os = "linux"))]
fn is_filemark_check_condition(_err: &ScsiError) -> bool {
    false
}

fn fixed_sense(key: u8, asc: u8, ascq: u8, information: Option<i64>, flags: u8) -> Vec<u8> {
    let mut sense = vec![0u8; 32];
    sense[0] = 0x70;
    sense[2] = (key & 0x0f) | flags;
    if let Some(information) = information {
        sense[0] |= 0x80;
        sense[3..7].copy_from_slice(&(information as i32).to_be_bytes());
    }
    sense[7] = 24;
    sense[12] = asc;
    sense[13] = ascq;
    sense
}

fn filemark_sense() -> Vec<u8> {
    fixed_sense(0x00, 0x00, 0x01, Some(0), 0x80)
}

fn blank_check_eod_sense() -> Vec<u8> {
    fixed_sense(0x08, 0x00, 0x05, Some(0), 0x00)
}

fn no_medium_sense() -> Vec<u8> {
    fixed_sense(0x02, 0x3a, 0x00, None, 0x00)
}

fn eom_sense(key: u8, residual: i64) -> Vec<u8> {
    fixed_sense(key, 0x00, 0x00, Some(residual), 0x40)
}

fn space_residual_sense(residual: i64, key: u8) -> Vec<u8> {
    fixed_sense(key, 0x00, 0x05, Some(residual), 0x00)
}

#[cfg(test)]
mod tests {
    use super::*;
    use remanence_library::{AccessPolicy, StaticAllowlist};
    use remanence_scsi::{read_element_status, read_position, read_write, space, write_filemarks};

    fn world() -> SharedVirtualWorld {
        let mut world = VirtualWorld::single_drive("LIB-MODEL", 0x0100, "DRV-MODEL", 0x0400, 2);
        world.put_tape_in_slot(0x0400, "TAPE001", VirtualTape::default());
        Arc::new(Mutex::new(world))
    }

    #[test]
    fn library_snapshot_opens_and_loads_through_public_api() {
        let world = world();
        let library = world.lock().unwrap().library_snapshot();
        let policy = StaticAllowlist::new([library.serial.clone()]);
        assert!(policy.allows("LIB-MODEL"));
        let factory_world = Arc::clone(&world);
        let mut handle = library
            .open_with(&policy, move |path| {
                let role = factory_world
                    .lock()
                    .unwrap()
                    .role_for_path(path)
                    .expect("known model path");
                Ok(Box::new(ModelTransport::new(
                    Arc::clone(&factory_world),
                    role,
                )))
            })
            .expect("open model library");

        handle.load(0x0400, 0x0100, &policy).expect("load slot");

        assert_eq!(
            world.lock().unwrap().loaded_barcode(0x0100),
            Some("TAPE001")
        );
    }

    #[test]
    fn drive_test_unit_ready_reports_ready_and_no_medium() {
        let world = Arc::new(Mutex::new(VirtualWorld::single_drive(
            "LIB-MODEL",
            0x0100,
            "DRV-MODEL",
            0x0400,
            1,
        )));
        let mut drive = ModelTransport::new(Arc::clone(&world), DeviceRole::Drive { bay: 0x0100 });

        let err = drive.execute_none(&[0, 0, 0, 0, 0, 0]).unwrap_err();
        let ScsiError::CheckCondition { sense, .. } = err else {
            panic!("empty bay should report no-medium check condition");
        };
        assert_eq!(sense[2] & 0x0f, 0x02);
        assert_eq!(sense[12], 0x3a);
        assert_eq!(sense[13], 0x00);

        world.lock().unwrap().put_tape_in_drive(
            0x0100,
            "TAPE001",
            Some(0x0400),
            VirtualTape::default(),
        );
        drive
            .execute_none(&[0, 0, 0, 0, 0, 0])
            .expect("loaded tape is ready in the honest model");
    }

    #[test]
    fn drive_read_write_filemarks_and_position_are_record_oriented() {
        let mut world = VirtualWorld::single_drive("LIB-MODEL", 0x0100, "DRV-MODEL", 0x0400, 1);
        world.put_tape_in_drive(0x0100, "TAPE001", Some(0x0400), VirtualTape::default());
        let world = Arc::new(Mutex::new(world));
        let mut drive = ModelTransport::new(Arc::clone(&world), DeviceRole::Drive { bay: 0x0100 });

        let first = b"alpha";
        let second = b"beta";
        drive
            .execute_out(
                &read_write::build_write_variable_cdb(first.len() as u32),
                first,
            )
            .expect("write first");
        drive
            .execute_out(
                &read_write::build_write_variable_cdb(second.len() as u32),
                second,
            )
            .expect("write second");
        drive
            .execute_none(&write_filemarks::build_cdb_6(1))
            .expect("filemark");
        let mut rp = [0u8; 32];
        drive
            .execute_in(&read_position::build_cdb_long(), &mut rp)
            .expect("read position");
        assert_eq!(u64::from_be_bytes(rp[8..16].try_into().unwrap()), 3);

        drive.rewind();
        let err = drive
            .execute_none(&space::build_cdb_6(space::SpaceCode::Blocks, -1))
            .expect_err("negative SPACE from BOT reports residual");
        assert!(matches!(
            err,
            ScsiError::CheckCondition { ref sense, .. }
                if sense.get(7).copied() == Some(24)
        ));
        let mut buf = [0u8; 16];
        let n = drive
            .execute_in(
                &read_write::build_read_variable_cdb(buf.len() as u32),
                &mut buf,
            )
            .expect("read first")
            .bytes_transferred as usize;
        assert_eq!(&buf[..n], first);
        let n = drive
            .execute_in(
                &read_write::build_read_variable_cdb(buf.len() as u32),
                &mut buf,
            )
            .expect("read second")
            .bytes_transferred as usize;
        assert_eq!(&buf[..n], second);
        let err = drive
            .execute_in(
                &read_write::build_read_variable_cdb(buf.len() as u32),
                &mut buf,
            )
            .expect_err("filemark sense");
        assert!(is_filemark_check_condition(&err));
    }

    #[test]
    fn overwrite_recomputes_written_bytes_for_eom() {
        let mut world = VirtualWorld::single_drive("LIB-MODEL", 0x0100, "DRV-MODEL", 0x0400, 1);
        world.put_tape_in_drive(
            0x0100,
            "TAPE001",
            Some(0x0400),
            VirtualTape::empty(12, DEFAULT_BLOCK_SIZE),
        );
        let world = Arc::new(Mutex::new(world));
        let mut drive = ModelTransport::new(Arc::clone(&world), DeviceRole::Drive { bay: 0x0100 });
        let block = [0xa5; 8];

        drive
            .execute_out(
                &read_write::build_write_variable_cdb(block.len() as u32),
                &block,
            )
            .expect("first write below EOM threshold");
        drive
            .execute_out(
                &read_write::build_write_variable_cdb(block.len() as u32),
                &block,
            )
            .expect_err("second append crosses EOM threshold");

        drive.rewind();
        drive
            .execute_out(
                &read_write::build_write_variable_cdb(block.len() as u32),
                &block,
            )
            .expect("overwrite after rewind truncates stale bytes before EOM check");

        let guard = world.lock().expect("virtual world lock");
        let tape = guard.tapes.get("TAPE001").expect("tape exists");
        assert_eq!(tape.records.len(), 1);
        assert_eq!(tape.written_bytes, block.len() as u64);
    }

    #[test]
    fn filemark_overwrite_recomputes_written_bytes() {
        let mut world = VirtualWorld::single_drive("LIB-MODEL", 0x0100, "DRV-MODEL", 0x0400, 1);
        world.put_tape_in_drive(0x0100, "TAPE001", Some(0x0400), VirtualTape::default());
        let world = Arc::new(Mutex::new(world));
        let mut drive = ModelTransport::new(Arc::clone(&world), DeviceRole::Drive { bay: 0x0100 });
        let block = [0x5a; 8];

        for _ in 0..2 {
            drive
                .execute_out(
                    &read_write::build_write_variable_cdb(block.len() as u32),
                    &block,
                )
                .expect("write block");
        }

        drive.rewind();
        drive
            .execute_none(&write_filemarks::build_cdb_6(1))
            .expect("filemark overwrite");

        let guard = world.lock().expect("virtual world lock");
        let tape = guard.tapes.get("TAPE001").expect("tape exists");
        assert_eq!(tape.records, vec![Record::Filemark]);
        assert_eq!(tape.written_bytes, 0);
    }

    #[test]
    fn read_element_status_response_parses_after_move() {
        let world = world();
        {
            let mut changer = ModelTransport::new(Arc::clone(&world), DeviceRole::Changer);
            changer
                .execute_none(&remanence_scsi::move_medium::build_cdb(
                    0, 0x0400, 0x0100, false,
                ))
                .expect("move");
        }
        let mut changer = ModelTransport::new(Arc::clone(&world), DeviceRole::Changer);
        let mut buf = vec![0u8; 4096];
        let n = changer
            .execute_in(
                &read_element_status::build_cdb(0, 0, 16, true, true, true, 4096),
                &mut buf,
            )
            .expect("RES")
            .bytes_transferred as usize;
        let parsed = read_element_status::parse(&buf[..n]).expect("parse RES");
        let drive = parsed
            .elements
            .iter()
            .find(|element| element.address == 0x0100)
            .expect("drive element");
        assert!(drive.full);
        assert_eq!(drive.primary_voltag.as_deref(), Some("TAPE001"));
        assert_eq!(drive.source_address, Some(0x0400));
        assert_eq!(drive.drive_serial.as_deref(), Some("DRV-MODEL"));
    }

    #[test]
    fn slot_seeders_round_trip_access_and_blank_voltag() {
        let mut world = VirtualWorld::single_drive("LIB-MODEL", 0x0100, "DRV-MODEL", 0x0400, 2);

        assert!(world.set_slot_inaccessible(0x0400));
        assert!(world.set_slot_full_without_voltag(0x0401));

        let library = world.library_snapshot();
        let inaccessible = library
            .slots
            .iter()
            .find(|slot| slot.element_address == 0x0400)
            .expect("inaccessible slot");
        let blank = library
            .slots
            .iter()
            .find(|slot| slot.element_address == 0x0401)
            .expect("blank-voltag slot");
        assert!(!inaccessible.accessible);
        assert!(!inaccessible.full);
        assert!(blank.accessible);
        assert!(blank.full);
        assert_eq!(blank.cartridge, None);
    }

    #[test]
    fn dirty_drive_sets_flag_20_after_configured_ops() {
        let mut world = VirtualWorld::single_drive("LIB-MODEL", 0x0100, "DRV-MODEL", 0x0400, 1);
        world.put_tape_in_drive(0x0100, "TAPE001", Some(0x0400), VirtualTape::default());
        assert!(world.arm_drive_dirty_after_ops(0x0100, 2));
        let world = Arc::new(Mutex::new(world));
        let mut drive = ModelTransport::new(Arc::clone(&world), DeviceRole::Drive { bay: 0x0100 });
        let block = b"dirty";

        drive
            .execute_out(
                &read_write::build_write_variable_cdb(block.len() as u32),
                block,
            )
            .expect("first write");
        drive
            .execute_out(
                &read_write::build_write_variable_cdb(block.len() as u32),
                block,
            )
            .expect("second write");

        let guard = world.lock().expect("virtual world lock");
        assert!(guard
            .drive_alert_flags
            .get(&0x0100)
            .is_some_and(|flags| flags.contains(&20)));
    }

    #[test]
    fn cleaning_cart_clears_dirty_and_auto_ejects_after_cycle() {
        let mut world = VirtualWorld::single_drive("LIB-MODEL", 0x0100, "DRV-MODEL", 0x0400, 1);
        let cleaning = VirtualTape {
            cleaning_cart: true,
            cleaning_cycle_ops: Some(1),
            ..VirtualTape::default()
        };
        world.put_tape_in_slot(0x0400, "CLN001L9", VirtualTape::default());
        world.put_tape_in_drive(0x0100, "CLN001L9", Some(0x0400), cleaning);
        if let Some(slot) = world.slots.iter_mut().find(|slot| slot.address == 0x0400) {
            slot.full = false;
            slot.barcode = None;
        }
        world.arm_drive_dirty_after_ops(0x0100, 1);
        let world = Arc::new(Mutex::new(world));
        let mut drive = ModelTransport::new(Arc::clone(&world), DeviceRole::Drive { bay: 0x0100 });

        drive
            .execute_none(&remanence_scsi::load_unload::build_cdb(true))
            .expect("load cleaning cart");
        drive
            .execute_out(&read_write::build_write_variable_cdb(5), b"clean")
            .expect("cleaning cycle op");

        let guard = world.lock().expect("virtual world lock");
        assert_eq!(guard.loaded_barcode(0x0100), None);
        let slot = guard
            .slots
            .iter()
            .find(|slot| slot.address == 0x0400)
            .expect("source slot");
        assert_eq!(slot.barcode.as_deref(), Some("CLN001L9"));
    }

    #[test]
    fn expired_cleaning_cart_fast_ejects_with_flag_22() {
        let mut world = VirtualWorld::single_drive("LIB-MODEL", 0x0100, "DRV-MODEL", 0x0400, 1);
        let cleaning = VirtualTape {
            cleaning_cart: true,
            cleaning_cart_expired: true,
            ..VirtualTape::default()
        };
        world.put_tape_in_slot(0x0400, "CLN002L9", VirtualTape::default());
        world.put_tape_in_drive(0x0100, "CLN002L9", Some(0x0400), cleaning);
        if let Some(slot) = world.slots.iter_mut().find(|slot| slot.address == 0x0400) {
            slot.full = false;
            slot.barcode = None;
        }
        let world = Arc::new(Mutex::new(world));
        let mut drive = ModelTransport::new(Arc::clone(&world), DeviceRole::Drive { bay: 0x0100 });

        drive
            .execute_none(&remanence_scsi::load_unload::build_cdb(true))
            .expect("load expired cleaning cart");

        let guard = world.lock().expect("virtual world lock");
        assert!(guard
            .drive_alert_flags
            .get(&0x0100)
            .is_some_and(|flags| flags.contains(&22)));
        assert_eq!(guard.loaded_barcode(0x0100), None);
        let slot = guard
            .slots
            .iter()
            .find(|slot| slot.address == 0x0400)
            .expect("source slot");
        assert_eq!(slot.barcode.as_deref(), Some("CLN002L9"));
    }
}

#[cfg(all(test, target_os = "linux"))]
mod l1b_tests {
    use super::*;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    use remanence_api::read_core::CapturePayloadSink;
    use remanence_format::{
        stream_rem_tar_object_with_manifest_anchor, write_rem_tar_object, FormatError, RemTarFile,
        RemTarObjectOptions,
    };
    use remanence_library::{
        AuditEvent, AuditOutcome, BlockSource, DriveHandle, IoErrorKind, LibraryHandle, MoveError,
        StaticAllowlist, TapeIoError,
    };
    use remanence_parity::{
        scan_reconstruct_filemark_map, CapacityReserveInput, DriveHandleRawSink,
        DriveHandleRawSource, ObjectParitySource, OpenTrust, ParityAuditHook, ParityError,
        ParityScheme, ParitySink, RecoveryEvent, RecoveryOutcome, SchemeId, ScopedFilemarkMap,
        TapeFilePosition,
    };
    use rusqlite::{params, Connection};
    use serde_json::{json, Value};
    use tempfile::TempDir;

    use crate::{ChaosTransport, DeviceCtx, FaultEngine};
    use remanence_scsi::ScsiError;

    const LIB_SERIAL: &str = "LIB-L1B";
    const DRIVE_SERIAL: &str = "DRV-L1B";
    const BAY: u16 = 0x0100;
    const SLOT: u16 = 0x0400;
    const BARCODE: &str = "TAPE001";
    const BLOCK_SIZE: u32 = 4096;
    const TAPE_UUID: [u8; 16] = [0xc7; 16];

    #[derive(Clone)]
    struct WrittenObject {
        tape_file_number: u32,
        block_count: u64,
        manifest_sha256: [u8; 32],
        payload_first_body_lba: u64,
        payload: Vec<u8>,
        scoped_map: ScopedFilemarkMap,
    }

    #[derive(Default)]
    struct RecordingHook {
        events: Mutex<Vec<RecoveryEvent>>,
    }

    impl RecordingHook {
        fn events(&self) -> Vec<RecoveryEvent> {
            self.events.lock().expect("hook lock").clone()
        }
    }

    impl ParityAuditHook for RecordingHook {
        fn on_recovery(&self, event: &RecoveryEvent) {
            self.events.lock().expect("hook lock").push(event.clone());
        }
    }

    fn loaded_world(capacity_bytes: u64) -> SharedVirtualWorld {
        let mut world = VirtualWorld::single_drive(LIB_SERIAL, BAY, DRIVE_SERIAL, SLOT, 1);
        world.put_tape_in_drive(
            BAY,
            BARCODE,
            Some(SLOT),
            VirtualTape::empty(capacity_bytes, BLOCK_SIZE),
        );
        Arc::new(Mutex::new(world))
    }

    fn slotted_world(capacity_bytes: u64) -> SharedVirtualWorld {
        let mut world = VirtualWorld::single_drive(LIB_SERIAL, BAY, DRIVE_SERIAL, SLOT, 1);
        world.put_tape_in_slot(
            SLOT,
            BARCODE,
            VirtualTape::empty(capacity_bytes, BLOCK_SIZE),
        );
        Arc::new(Mutex::new(world))
    }

    fn policy() -> StaticAllowlist {
        StaticAllowlist::new([LIB_SERIAL])
    }

    fn scheme() -> ParityScheme {
        ParityScheme {
            id: SchemeId::new_static("chaos-l1b-rs"),
            data_blocks_per_stripe: 2,
            parity_blocks_per_stripe: 1,
            stripes_per_neighborhood: 2,
        }
    }

    fn open_handle(world: SharedVirtualWorld, engine: Option<FaultEngine>) -> LibraryHandle {
        let library = world.lock().expect("world lock").library_snapshot();
        let policy = policy();
        library
            .open_with(&policy, move |path| {
                let role = world
                    .lock()
                    .expect("world lock")
                    .role_for_path(path)
                    .ok_or_else(|| IoErrorKind {
                        kind: "NotFound",
                        message: format!("unknown model path {}", path.display()),
                        raw_os_error: None,
                    })?;
                let ctx = device_ctx(&world, role);
                let model = ModelTransport::new(Arc::clone(&world), role);
                let transport: Box<dyn SgTransport> = if let Some(engine) = engine.clone() {
                    Box::new(ChaosTransport::new(model, engine, ctx))
                } else {
                    Box::new(ChaosTransport::disabled(model))
                };
                Ok(transport)
            })
            .expect("open model library")
    }

    fn open_drive(world: SharedVirtualWorld, engine: Option<FaultEngine>) -> DriveHandle {
        let policy = policy();
        let mut handle = open_handle(world, engine);
        handle.open_drive(BAY, &policy).expect("open model drive")
    }

    fn device_ctx(world: &SharedVirtualWorld, role: DeviceRole) -> DeviceCtx {
        let mut ctx = DeviceCtx::new().with_backend("model");
        match role {
            DeviceRole::Changer => {
                let library_id = world.lock().expect("world lock").changer_serial.clone();
                ctx = ctx.with_library_id(library_id);
            }
            DeviceRole::Drive { bay } => {
                ctx = ctx.with_drive_id(format!("bay-{bay:04x}"));
                if let Some(barcode) = world
                    .lock()
                    .expect("world lock")
                    .loaded_barcode(bay)
                    .map(str::to_string)
                {
                    ctx = ctx.with_tape_id(barcode.clone()).with_barcode(barcode);
                }
            }
        }
        ctx
    }

    fn test_payload() -> Vec<u8> {
        (0..37_000).map(|i| ((i * 31 + 17) % 251) as u8).collect()
    }

    fn capacity_input(projected_object_blocks: u64) -> CapacityReserveInput {
        let scheme = scheme();
        CapacityReserveInput {
            projected_object_blocks,
            block_size_bytes: u64::from(BLOCK_SIZE),
            current_epoch_fill_blocks: 0,
            data_shards_per_epoch: u64::from(scheme.data_blocks_per_stripe)
                * u64::from(scheme.stripes_per_neighborhood),
            parity_shards_per_epoch: u64::from(scheme.parity_blocks_per_stripe)
                * u64::from(scheme.stripes_per_neighborhood),
            sidecar_index_block_count: 2,
            object_filemark_blocks: 1,
            sidecar_filemark_blocks: 1,
            bootstrap_filemark_blocks: 1,
            pending_completed_sidecars: 0,
            remaining_bootstrap_count: 1,
            safety_margin_blocks: 3,
            remaining_tape_blocks: 100_000,
            empty_tape_usable_blocks: 100_000,
            pending_completed_epoch_parity_bytes: 0,
            remaining_spool_bytes: 16 * 1024 * 1024,
        }
    }

    fn write_object(world: SharedVirtualWorld) -> WrittenObject {
        let payload = test_payload();
        let (tape_file_number, block_count, manifest_sha256, payload_first_body_lba) = {
            let mut drive = open_drive(Arc::clone(&world), None);
            drive.load().expect("load drive");
            let mut raw = DriveHandleRawSink::new(&mut drive);
            raw.configure_parity_write_session(BLOCK_SIZE)
                .expect("configure parity session");
            let mut sink = ParitySink::new_sidecar_only(&mut raw, scheme(), TAPE_UUID, BLOCK_SIZE)
                .expect("open parity sink");
            sink.write_bootstrap().expect("write bootstrap");
            let (tape_file_number, _) = sink
                .begin_object_with_capacity_reserve(capacity_input(128))
                .expect("begin object");

            let mut options = RemTarObjectOptions::new(
                "00000000-0000-4000-8000-00000000c001",
                "chaos-l1b-object",
                "2026-06-21T00:00:00Z",
                "00000000-0000-4000-8000-00000000c002",
            );
            options.chunk_size = BLOCK_SIZE as usize;
            let files = [RemTarFile {
                path: "payload.bin",
                file_id: "payload",
                data: &payload,
                mtime: Some("0"),
                executable: Some(false),
            }];
            let layout =
                write_rem_tar_object(&mut sink, &options, &files).expect("write RAO object");
            let summary = sink.finish_object().expect("finish object");
            assert_eq!(summary.tape_file_number, tape_file_number);
            assert_eq!(summary.data_block_count, layout.projected_size_blocks);
            sink.finish().expect("finish tape parity session");
            (
                tape_file_number,
                summary.data_block_count,
                layout.manifest_sha256,
                layout.files[0]
                    .first_chunk_lba
                    .expect("payload has data chunks")
                    .0,
            )
        };
        let scoped_map = scoped_map_from_world(Arc::clone(&world));
        WrittenObject {
            tape_file_number,
            block_count,
            manifest_sha256,
            payload_first_body_lba,
            payload,
            scoped_map,
        }
    }

    fn scoped_map_from_world(world: SharedVirtualWorld) -> ScopedFilemarkMap {
        let mut drive = open_drive(world, None);
        let mut raw = DriveHandleRawSource::new(&mut drive);
        let map =
            scan_reconstruct_filemark_map(&mut raw, &TAPE_UUID, BLOCK_SIZE).expect("scan map");
        let highest = map.max_sidecar_end_exclusive();
        assert!(highest > 0, "finished test object should be protected");
        ScopedFilemarkMap::from_catalog(map, highest)
    }

    fn physical_lba(written: &WrittenObject, body_lba: u64) -> u64 {
        written
            .scoped_map
            .map
            .physical_position(TapeFilePosition {
                tape_file_number: written.tape_file_number,
                block_within_file: body_lba,
            })
            .expect("physical position")
            .lba
    }

    fn open_source<'a>(
        raw: &'a mut DriveHandleRawSource<'_>,
        written: &WrittenObject,
    ) -> ObjectParitySource<'a> {
        ObjectParitySource::open(
            raw,
            scheme(),
            TAPE_UUID,
            written.scoped_map.clone(),
            BLOCK_SIZE,
            written.tape_file_number,
            OpenTrust::RequireValidated,
        )
        .expect("open object parity source")
    }

    fn read_payload(
        world: SharedVirtualWorld,
        engine: Option<FaultEngine>,
        written: &WrittenObject,
    ) -> Result<Vec<u8>, FormatError> {
        let mut drive = open_drive(world, engine);
        let mut raw = DriveHandleRawSource::new(&mut drive);
        let mut source = open_source(&mut raw, written);
        let mut captured = Vec::new();
        let result = {
            let mut sink = CapturePayloadSink::new(&mut captured);
            let result = stream_rem_tar_object_with_manifest_anchor(
                &mut source,
                BLOCK_SIZE as usize,
                written.block_count,
                &mut sink,
                Some(written.manifest_sha256),
            );
            if result.is_ok() {
                sink.finish().expect("captured payload finalizes");
            }
            result
        };
        result.map(|_| captured)
    }

    fn chaos_state(prefix: &str, scenario_id: &str, seed: &str) -> (TempDir, PathBuf) {
        let temp = tempfile::Builder::new()
            .prefix(prefix)
            .tempdir()
            .expect("tempdir");
        let state_path = temp.path().join("state.db");
        let conn = create_state(&state_path, scenario_id, seed);
        drop(conn);
        (temp, state_path)
    }

    fn create_state(path: &Path, scenario_id: &str, seed: &str) -> Connection {
        let conn = Connection::open(path).expect("open sqlite state");
        conn.execute_batch(
            r#"
            CREATE TABLE scenarios (
                id TEXT PRIMARY KEY,
                seed TEXT NOT NULL,
                time_scale REAL NOT NULL DEFAULT 1.0,
                source_path TEXT,
                source_json TEXT NOT NULL,
                armed_at TEXT NOT NULL DEFAULT '2026-06-21T00:00:00.000Z'
            );
            CREATE TABLE faults (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                scenario_id TEXT NOT NULL,
                ordinal INTEGER NOT NULL,
                catalogue_id TEXT NOT NULL,
                target_json TEXT NOT NULL,
                trigger_json TEXT NOT NULL,
                action_json TEXT NOT NULL,
                scope TEXT NOT NULL
            );
            CREATE TABLE corrupt_ranges (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                scenario_id TEXT NOT NULL,
                fault_id INTEGER,
                tape_id TEXT,
                drive_id TEXT,
                lba INTEGER,
                offset INTEGER NOT NULL,
                length INTEGER NOT NULL,
                mode TEXT NOT NULL,
                scope TEXT NOT NULL,
                seed TEXT,
                created_at TEXT NOT NULL DEFAULT '2026-06-21T00:00:00.000Z'
            );
            CREATE TABLE events (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                ts TEXT NOT NULL,
                scenario_id TEXT,
                fault_id INTEGER,
                catalogue_id TEXT,
                event_json TEXT NOT NULL
            );
            "#,
        )
        .expect("create sqlite schema");
        conn.execute(
            "INSERT INTO scenarios(id, seed, source_json) VALUES (?1, ?2, '{}')",
            params![scenario_id, seed],
        )
        .expect("insert scenario");
        conn
    }

    fn insert_fault(
        conn: &Connection,
        scenario_id: &str,
        catalogue_id: &str,
        target: Value,
        trigger: Value,
        action: Value,
        scope: &str,
    ) {
        let ordinal: i64 = conn
            .query_row("SELECT COUNT(*) FROM faults", [], |row| row.get(0))
            .expect("fault count");
        conn.execute(
            r#"
            INSERT INTO faults(
                scenario_id, ordinal, catalogue_id, target_json,
                trigger_json, action_json, scope
            )
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
            "#,
            params![
                scenario_id,
                ordinal,
                catalogue_id,
                target.to_string(),
                trigger.to_string(),
                action.to_string(),
                scope
            ],
        )
        .expect("insert fault");
    }

    fn event_log_path(state_path: &Path) -> PathBuf {
        PathBuf::from(format!("{}.events.jsonl", state_path.display()))
    }

    fn read_jsonl(path: &Path) -> Vec<Value> {
        fs::read_to_string(path)
            .expect("read event log")
            .lines()
            .map(|line| serde_json::from_str(line).expect("parse event"))
            .collect()
    }

    fn arm_med05(state_path: &Path, scenario_id: &str, lba: u64) -> FaultEngine {
        let conn = Connection::open(state_path).expect("open state");
        insert_fault(
            &conn,
            scenario_id,
            "MED-05",
            json!({"tape": BARCODE}),
            json!({"op": "read", "lba": lba}),
            json!({"status": "good", "mutate": {"mode": "xor", "offset": 32, "length": 64}}),
            "transient",
        );
        drop(conn);
        FaultEngine::from_state_path(state_path).expect("load fault engine")
    }

    fn arm_med01(state_path: &Path, scenario_id: &str, lba: u64) -> FaultEngine {
        let conn = Connection::open(state_path).expect("open state");
        insert_fault(
            &conn,
            scenario_id,
            "MED-01",
            json!({"tape": BARCODE}),
            json!({"op": "read", "lba": lba}),
            json!({}),
            "transient",
        );
        drop(conn);
        FaultEngine::from_state_path(state_path).expect("load fault engine")
    }

    fn arm_med01_and_med05(
        state_path: &Path,
        scenario_id: &str,
        erasure_lba: u64,
        mutation_lba: u64,
    ) -> FaultEngine {
        let conn = Connection::open(state_path).expect("open state");
        insert_fault(
            &conn,
            scenario_id,
            "MED-01",
            json!({"tape": BARCODE}),
            json!({"op": "read", "lba": erasure_lba}),
            json!({}),
            "transient",
        );
        insert_fault(
            &conn,
            scenario_id,
            "MED-05",
            json!({"tape": BARCODE}),
            json!({"op": "read", "lba": mutation_lba}),
            json!({"status": "good", "mutate": {"mode": "xor", "offset": 32, "length": 64}}),
            "transient",
        );
        drop(conn);
        FaultEngine::from_state_path(state_path).expect("load fault engine")
    }

    fn arm_tape_alert(
        state_path: &Path,
        scenario_id: &str,
        catalogue_id: &str,
        target: Value,
        flags: &[u8],
        scope: &str,
    ) -> FaultEngine {
        let conn = Connection::open(state_path).expect("open state");
        insert_fault(
            &conn,
            scenario_id,
            catalogue_id,
            target,
            json!({"op": "log_sense"}),
            json!({"tape_alert": flags}),
            scope,
        );
        drop(conn);
        FaultEngine::from_state_path(state_path).expect("load fault engine")
    }

    fn arm_library_fault(
        state_path: &Path,
        scenario_id: &str,
        catalogue_id: &str,
        op: &str,
        action: Value,
        scope: &str,
    ) -> FaultEngine {
        let conn = Connection::open(state_path).expect("open state");
        insert_fault(
            &conn,
            scenario_id,
            catalogue_id,
            json!({"library": LIB_SERIAL}),
            json!({"op": op}),
            action,
            scope,
        );
        drop(conn);
        FaultEngine::from_state_path(state_path).expect("load fault engine")
    }

    fn arm_element_status_fault(
        state_path: &Path,
        scenario_id: &str,
        catalogue_id: &str,
        slot: u16,
    ) -> FaultEngine {
        let conn = Connection::open(state_path).expect("open state");
        insert_fault(
            &conn,
            scenario_id,
            catalogue_id,
            json!({"library": LIB_SERIAL, "slot": slot}),
            json!({"op": "read_element_status"}),
            json!({}),
            "library",
        );
        drop(conn);
        FaultEngine::from_state_path(state_path).expect("load fault engine")
    }

    fn arm_lib11_no_medium(state_path: &Path, scenario_id: &str) -> FaultEngine {
        let conn = Connection::open(state_path).expect("open state");
        insert_fault(
            &conn,
            scenario_id,
            "LIB-11",
            json!({"drive": format!("bay-{BAY:04x}")}),
            json!({"op": "read"}),
            json!({}),
            "drive",
        );
        drop(conn);
        FaultEngine::from_state_path(state_path).expect("load fault engine")
    }

    fn assert_alert_flags(alerts: &remanence_library::TapeAlerts, expected: &[u8]) {
        let actual = alerts.active().iter().copied().collect::<Vec<_>>();
        assert_eq!(actual, expected);
    }

    fn event_for<'a>(events: &'a [Value], catalogue_id: &str) -> &'a Value {
        events
            .iter()
            .find(|event| event["catalogue_id"] == catalogue_id)
            .unwrap_or_else(|| panic!("{catalogue_id} event missing from {events:#?}"))
    }

    fn sense_tuple(sense: &[u8]) -> (u8, u8, u8) {
        let decoded = remanence_scsi::decode_sense(sense).expect("decode fixed sense");
        (decoded.key, decoded.asc, decoded.ascq)
    }

    fn scsi_error_tuple(err: &ScsiError) -> (u8, u8, u8) {
        match err {
            ScsiError::CheckCondition { sense, .. } => sense_tuple(sense),
            other => panic!("expected CHECK CONDITION, got {other:?}"),
        }
    }

    type CapturedSense = Arc<Mutex<Vec<(u8, u8, u8, bool)>>>;

    fn capture_finished_sense(handle: &mut LibraryHandle) -> CapturedSense {
        let captured = Arc::new(Mutex::new(Vec::new()));
        let captured_for_hook = Arc::clone(&captured);
        handle.set_audit_hook(move |event| {
            if let AuditEvent::Finished {
                outcome:
                    AuditOutcome::ScsiError {
                        sense: Some(sense),
                        dirty,
                        ..
                    },
                ..
            } = event
            {
                let (key, asc, ascq) = sense_tuple(sense);
                captured_for_hook
                    .lock()
                    .expect("capture lock")
                    .push((key, asc, ascq, *dirty));
            }
        });
        captured
    }

    #[test]
    fn changer_honest_refresh_and_move_succeed() {
        let world = slotted_world(64 * 1024 * 1024);
        let mut handle = open_handle(Arc::clone(&world), None);

        handle.refresh().expect("honest refresh");
        let slot = handle
            .library()
            .slots
            .iter()
            .find(|slot| slot.element_address == SLOT)
            .expect("slot");
        assert!(slot.full);
        assert!(slot.accessible);
        assert_eq!(slot.cartridge.as_deref(), Some(BARCODE));

        handle
            .move_medium(SLOT, BAY, &policy())
            .expect("honest move");

        let bay = handle
            .library()
            .drive_bays
            .iter()
            .find(|bay| bay.element_address == BAY)
            .expect("bay");
        assert!(bay.loaded);
        assert_eq!(bay.loaded_tape.as_deref(), Some(BARCODE));
        assert!(!handle.is_dirty());
    }

    #[test]
    fn lib01_move_medium_returns_source_empty_sense_without_dirtying_snapshot() {
        let world = slotted_world(64 * 1024 * 1024);
        let (_temp, state_path) =
            chaos_state("remanence-chaos-l1b-lib01", "l1b-lib01", "seed-lib01");
        let engine = arm_library_fault(
            &state_path,
            "l1b-lib01",
            "LIB-01",
            "move_medium",
            json!({}),
            "library",
        );
        let mut handle = open_handle(Arc::clone(&world), Some(engine));
        let captured = capture_finished_sense(&mut handle);

        let err = handle
            .move_medium(SLOT, BAY, &policy())
            .expect_err("LIB-01 fails move");

        match err {
            MoveError::ScsiError(err) => assert_eq!(scsi_error_tuple(&err), (0x05, 0x3b, 0x0e)),
            other => panic!("expected ScsiError, got {other:?}"),
        }
        assert!(!handle.is_dirty());
        assert_eq!(
            captured.lock().expect("capture lock").as_slice(),
            &[(0x05, 0x3b, 0x0e, false)]
        );
    }

    #[test]
    fn lib08_move_medium_returns_door_open_sense() {
        let world = slotted_world(64 * 1024 * 1024);
        let (_temp, state_path) =
            chaos_state("remanence-chaos-l1b-lib08", "l1b-lib08", "seed-lib08");
        let engine = arm_library_fault(
            &state_path,
            "l1b-lib08",
            "LIB-08",
            "move_medium",
            json!({}),
            "library",
        );
        let mut handle = open_handle(Arc::clone(&world), Some(engine));
        let captured = capture_finished_sense(&mut handle);

        let err = handle
            .move_medium(SLOT, BAY, &policy())
            .expect_err("LIB-08 fails move");

        match err {
            MoveError::ScsiError(err) => assert_eq!(scsi_error_tuple(&err), (0x02, 0x04, 0x18)),
            other => panic!("expected ScsiError, got {other:?}"),
        }
        assert_eq!(
            captured.lock().expect("capture lock").as_slice(),
            &[(0x02, 0x04, 0x18, false)]
        );
    }

    #[test]
    fn lib08_refresh_returns_door_open_sense() {
        let world = slotted_world(64 * 1024 * 1024);
        let (_temp, state_path) = chaos_state(
            "remanence-chaos-l1b-lib08-refresh",
            "l1b-lib08-refresh",
            "seed-lib08-refresh",
        );
        let engine = arm_library_fault(
            &state_path,
            "l1b-lib08-refresh",
            "LIB-08",
            "read_element_status",
            json!({}),
            "library",
        );
        let mut handle = open_handle(Arc::clone(&world), Some(engine));

        let err = handle.refresh().expect_err("LIB-08 fails refresh");

        assert_eq!(scsi_error_tuple(&err), (0x02, 0x04, 0x18));
    }

    #[test]
    fn lib05_refresh_reports_full_slot_without_readable_barcode() {
        let world = slotted_world(64 * 1024 * 1024);
        let (_temp, state_path) =
            chaos_state("remanence-chaos-l1b-lib05", "l1b-lib05", "seed-lib05");
        let engine = arm_element_status_fault(&state_path, "l1b-lib05", "LIB-05", SLOT);
        let mut handle = open_handle(Arc::clone(&world), Some(engine));

        handle.refresh().expect("LIB-05 refresh");

        let slot = handle
            .library()
            .slots
            .iter()
            .find(|slot| slot.element_address == SLOT)
            .expect("slot");
        assert!(slot.full);
        assert_eq!(slot.cartridge, None);
        assert!(slot.accessible);
        let events = read_jsonl(&event_log_path(&state_path));
        let event = event_for(&events, "LIB-05");
        assert_eq!(event["operation"], "read_element_status");
        assert_eq!(event["element_status"]["action"], "blank_voltag");
        assert_eq!(event["element_status"]["applied"], json!([SLOT]));
    }

    #[test]
    fn lib09_refresh_reports_inaccessible_slot() {
        let world = slotted_world(64 * 1024 * 1024);
        let (_temp, state_path) =
            chaos_state("remanence-chaos-l1b-lib09", "l1b-lib09", "seed-lib09");
        let engine = arm_element_status_fault(&state_path, "l1b-lib09", "LIB-09", SLOT);
        let mut handle = open_handle(Arc::clone(&world), Some(engine));

        handle.refresh().expect("LIB-09 refresh");

        let slot = handle
            .library()
            .slots
            .iter()
            .find(|slot| slot.element_address == SLOT)
            .expect("slot");
        assert!(slot.full);
        assert!(!slot.accessible);
        assert_eq!(slot.cartridge.as_deref(), Some(BARCODE));
        let events = read_jsonl(&event_log_path(&state_path));
        let event = event_for(&events, "LIB-09");
        assert_eq!(event["operation"], "read_element_status");
        assert_eq!(event["element_status"]["action"], "set_exception");
        assert_eq!(event["element_status"]["applied"], json!([SLOT]));
    }

    #[test]
    fn lib03_successful_move_queues_one_unit_attention_on_next_changer_command() {
        let world = slotted_world(64 * 1024 * 1024);
        let (_temp, state_path) =
            chaos_state("remanence-chaos-l1b-lib03", "l1b-lib03", "seed-lib03");
        let engine = arm_library_fault(
            &state_path,
            "l1b-lib03",
            "LIB-03",
            "move_medium",
            json!({}),
            "library",
        );
        let mut handle = open_handle(Arc::clone(&world), Some(engine));

        handle
            .move_medium(SLOT, BAY, &policy())
            .expect("move succeeds before UA");

        let err = handle
            .move_medium(BAY, SLOT, &policy())
            .expect_err("next changer command gets UA");
        match err {
            MoveError::ScsiError(err) => assert_eq!(scsi_error_tuple(&err), (0x06, 0x28, 0x00)),
            other => panic!("expected ScsiError, got {other:?}"),
        }
        handle
            .move_medium(BAY, SLOT, &policy())
            .expect("UA is one-shot");
    }

    #[test]
    fn lib11_drive_read_without_medium_maps_to_no_medium() {
        let world = Arc::new(Mutex::new(VirtualWorld::single_drive(
            LIB_SERIAL,
            BAY,
            DRIVE_SERIAL,
            SLOT,
            1,
        )));
        let (_temp, state_path) =
            chaos_state("remanence-chaos-l1b-lib11", "l1b-lib11", "seed-lib11");
        let engine = arm_lib11_no_medium(&state_path, "l1b-lib11");
        let mut drive = open_drive(Arc::clone(&world), Some(engine));
        let mut buf = vec![0u8; BLOCK_SIZE as usize];

        let err = drive.read_block(&mut buf).expect_err("LIB-11 read fails");

        assert!(matches!(err, TapeIoError::NoMedium));
    }

    #[test]
    fn tape_alert_honest_device_reports_seeded_tape_flags() {
        let world = loaded_world(64 * 1024 * 1024);
        let mut drive = open_drive(Arc::clone(&world), None);

        let clean = drive.read_tape_alerts().expect("read clean alerts");
        assert_alert_flags(&clean, &[]);

        {
            let mut world = world.lock().expect("world lock");
            assert!(world.set_tape_alert(BARCODE, 7));
            assert!(world.set_tape_alert(BARCODE, 19));
        }
        let seeded = drive.read_tape_alerts().expect("read seeded alerts");
        assert_alert_flags(&seeded, &[7, 19]);
    }

    #[test]
    fn tape_alert_med07_tape_scope_sets_flags_and_jsonl() {
        let world = loaded_world(64 * 1024 * 1024);
        let (_temp, state_path) =
            chaos_state("remanence-chaos-l1b-med07", "l1b-med07", "seed-med07");
        let engine = arm_tape_alert(
            &state_path,
            "l1b-med07",
            "MED-07",
            json!({"tape": BARCODE}),
            &[7, 19],
            "tape",
        );
        let mut drive = open_drive(Arc::clone(&world), Some(engine));

        let alerts = drive.read_tape_alerts().expect("read MED-07 alerts");

        assert_alert_flags(&alerts, &[7, 19]);
        let events = read_jsonl(&event_log_path(&state_path));
        let event = event_for(&events, "MED-07");
        assert_eq!(event["operation"], "log_sense");
        assert_eq!(event["seed"], "seed-med07");
        assert_eq!(event["tape_alert"], json!([7, 19]));
    }

    #[test]
    fn tape_alert_cln01_drive_scope_sets_drive_flag() {
        let world = loaded_world(64 * 1024 * 1024);
        let (_temp, state_path) =
            chaos_state("remanence-chaos-l1b-cln01", "l1b-cln01", "seed-cln01");
        let engine = arm_tape_alert(
            &state_path,
            "l1b-cln01",
            "CLN-01",
            json!({"drive": format!("bay-{BAY:04x}")}),
            &[20],
            "drive",
        );
        let mut drive = open_drive(Arc::clone(&world), Some(engine));

        let alerts = drive.read_tape_alerts().expect("read CLN-01 alerts");

        assert_alert_flags(&alerts, &[20]);
        let events = read_jsonl(&event_log_path(&state_path));
        let event = event_for(&events, "CLN-01");
        assert_eq!(event["operation"], "log_sense");
        assert_eq!(event["tape_alert"], json!([20]));
    }

    #[test]
    fn tape_alert_hw04_reports_multiple_drive_flags() {
        let world = loaded_world(64 * 1024 * 1024);
        let (_temp, state_path) = chaos_state("remanence-chaos-l1b-hw04", "l1b-hw04", "seed-hw04");
        let engine = arm_tape_alert(
            &state_path,
            "l1b-hw04",
            "HW-04",
            json!({"drive": format!("bay-{BAY:04x}")}),
            &[58, 16],
            "drive",
        );
        let mut drive = open_drive(Arc::clone(&world), Some(engine));

        let alerts = drive.read_tape_alerts().expect("read HW-04 alerts");

        assert_alert_flags(&alerts, &[16, 58]);
        let events = read_jsonl(&event_log_path(&state_path));
        let event = event_for(&events, "HW-04");
        assert_eq!(event["tape_alert"], json!([58, 16]));
    }

    #[test]
    fn tape_alert_fault_persists_across_repeated_reads() {
        let world = loaded_world(64 * 1024 * 1024);
        let (_temp, state_path) = chaos_state(
            "remanence-chaos-l1b-alert-persist",
            "l1b-alert-persist",
            "seed-persist",
        );
        let engine = arm_tape_alert(
            &state_path,
            "l1b-alert-persist",
            "MED-07",
            json!({"tape": BARCODE}),
            &[7, 19],
            "tape",
        );
        let mut drive = open_drive(Arc::clone(&world), Some(engine));

        let first = drive.read_tape_alerts().expect("first TapeAlert read");
        let second = drive.read_tape_alerts().expect("second TapeAlert read");

        assert_alert_flags(&first, &[7, 19]);
        assert_alert_flags(&second, &[7, 19]);
        let events = read_jsonl(&event_log_path(&state_path));
        let count = events
            .iter()
            .filter(|event| event["catalogue_id"] == "MED-07")
            .count();
        assert_eq!(count, 2, "persistent alert should emit on both reads");
    }

    #[test]
    fn faithful_device_round_trip_through_parity_and_format() {
        let world = loaded_world(64 * 1024 * 1024);
        let written = write_object(Arc::clone(&world));

        let payload = read_payload(Arc::clone(&world), None, &written).expect("read object");

        assert_eq!(payload, written.payload);
        let records = world
            .lock()
            .expect("world lock")
            .tapes
            .get(BARCODE)
            .expect("tape")
            .records
            .clone();
        assert!(records
            .iter()
            .any(|record| matches!(record, Record::Filemark)));
        assert!(
            records
                .iter()
                .filter(|record| matches!(record, Record::Block(_)))
                .count()
                >= written.block_count as usize
        );
    }

    #[test]
    fn med05_good_status_corruption_surfaces_at_digest_layer() {
        let world = loaded_world(64 * 1024 * 1024);
        let written = write_object(Arc::clone(&world));
        let target_lba = physical_lba(&written, written.payload_first_body_lba);
        let (_temp, state_path) =
            chaos_state("remanence-chaos-l1b-med05", "l1b-med05", "seed-med05");
        let engine = arm_med05(&state_path, "l1b-med05", target_lba);

        let err = read_payload(Arc::clone(&world), Some(engine), &written)
            .expect_err("silent corruption must fail digest validation");

        assert!(
            matches!(err, FormatError::FileDigestMismatch { ref path, .. } if path == "payload.bin")
                || matches!(err, FormatError::ManifestDigestMismatch),
            "unexpected digest error: {err:?}"
        );
        let events = read_jsonl(&event_log_path(&state_path));
        let med05 = events
            .iter()
            .find(|event| event["catalogue_id"] == "MED-05")
            .expect("MED-05 event");
        assert_eq!(med05["lba_before"], target_lba);
        assert_eq!(med05["seed"], "seed-med05");
        assert_eq!(med05["mutation_summary"]["offset"], 32);
        assert_eq!(med05["mutation_summary"]["length"], 64);
    }

    #[test]
    fn eom_early_warning_flows_through_fixed_sense() {
        let world = loaded_world(1);
        let mut drive = open_drive(world, None);
        drive.load().expect("load drive");

        let outcome = drive
            .write_block(&vec![0xa5; BLOCK_SIZE as usize])
            .expect("write crosses EOM as early warning");

        assert!(outcome.early_warning);
        assert!(!outcome.end_of_medium);
        assert_eq!(outcome.bytes_written, BLOCK_SIZE);
    }

    #[test]
    fn med01_erasure_recovers_and_reports_unrecoverable_damage() {
        let world = loaded_world(64 * 1024 * 1024);
        let written = write_object(Arc::clone(&world));
        let target_lba = physical_lba(&written, 0);
        let peer_lba = physical_lba(&written, 2);

        let (_recover_temp, recover_state) = chaos_state(
            "remanence-chaos-l1b-med01-ok",
            "l1b-med01-ok",
            "seed-med01-ok",
        );
        let recover_engine = arm_med01(&recover_state, "l1b-med01-ok", target_lba);
        let hook = Arc::new(RecordingHook::default());
        {
            let mut drive = open_drive(Arc::clone(&world), Some(recover_engine));
            let mut raw = DriveHandleRawSource::new(&mut drive);
            let mut source = open_source(&mut raw, &written);
            source.set_audit_hook(Some(hook.clone() as Arc<dyn ParityAuditHook>));
            let mut block = vec![0u8; BLOCK_SIZE as usize];
            let bytes = source.read_block(&mut block).expect("recovered read");
            assert_eq!(bytes, BLOCK_SIZE as usize);
        }
        assert!(hook
            .events()
            .iter()
            .any(|event| matches!(event.outcome, RecoveryOutcome::Recovered)));

        let (_lost_temp, lost_state) = chaos_state(
            "remanence-chaos-l1b-med01-lost",
            "l1b-med01-lost",
            "seed-med01-lost",
        );
        let lost_engine = arm_med01(&lost_state, "l1b-med01-lost", peer_lba);
        let lost_hook = Arc::new(RecordingHook::default());
        let err = {
            let mut drive = open_drive(Arc::clone(&world), Some(lost_engine));
            let mut raw = DriveHandleRawSource::new(&mut drive);
            let mut source = open_source(&mut raw, &written);
            source.set_audit_hook(Some(lost_hook.clone() as Arc<dyn ParityAuditHook>));
            source
                .recover_block_at(0)
                .expect_err("peer loss exceeds m=1")
        };
        assert!(matches!(
            err,
            ParityError::Unrecoverable {
                lost_count: 2,
                limit: 1,
                ..
            }
        ));
        assert!(lost_hook.events().iter().any(|event| matches!(
            event.outcome,
            RecoveryOutcome::Unrecoverable { lost_count: 2 }
        )));
    }

    #[test]
    fn med05_peer_corruption_during_reconstruction_is_unrecoverable() {
        let world = loaded_world(64 * 1024 * 1024);
        let written = write_object(Arc::clone(&world));
        let target_lba = physical_lba(&written, 0);
        let peer_lba = physical_lba(&written, 2);
        let (_temp, state_path) = chaos_state(
            "remanence-chaos-l1b-med05-peer",
            "l1b-med05-peer",
            "seed-med05-peer",
        );
        let engine = arm_med01_and_med05(&state_path, "l1b-med05-peer", target_lba, peer_lba);
        let hook = Arc::new(RecordingHook::default());

        {
            let mut drive = open_drive(Arc::clone(&world), Some(engine));
            let mut raw = DriveHandleRawSource::new(&mut drive);
            let mut source = open_source(&mut raw, &written);
            source.set_audit_hook(Some(hook.clone() as Arc<dyn ParityAuditHook>));
            let mut block = vec![0u8; BLOCK_SIZE as usize];
            source
                .read_block(&mut block)
                .expect_err("MED-01 target plus corrupt peer should be unrecoverable");
        }

        assert!(hook.events().iter().any(|event| matches!(
            event.outcome,
            RecoveryOutcome::Unrecoverable { lost_count: 2 }
        )));
    }

    #[test]
    fn changer_load_couples_barcode_to_per_tape_fault_targeting() {
        let world = slotted_world(64 * 1024 * 1024);
        {
            let mut handle = open_handle(Arc::clone(&world), None);
            let policy = policy();
            handle.load(SLOT, BAY, &policy).expect("load from slot");
        }
        assert_eq!(
            world.lock().expect("world lock").loaded_barcode(BAY),
            Some(BARCODE)
        );
        let (_temp, state_path) =
            chaos_state("remanence-chaos-l1b-changer", "l1b-changer", "seed-changer");
        let conn = Connection::open(&state_path).expect("open state");
        insert_fault(
            &conn,
            "l1b-changer",
            "MED-05",
            json!({"tape": BARCODE}),
            json!({"op": "read"}),
            json!({"status": "good", "mutate": {"mode": "xor", "offset": 0, "length": 16}}),
            "transient",
        );
        drop(conn);
        let engine = FaultEngine::from_state_path(&state_path).expect("load fault engine");
        let mut drive = open_drive(Arc::clone(&world), Some(engine));
        let original = vec![0x5a; BLOCK_SIZE as usize];
        drive.write_block(&original).expect("write block");
        drive.rewind().expect("rewind");
        let _ = drive.position().expect("refresh chaos LBA after rewind");
        let mut read_back = vec![0u8; BLOCK_SIZE as usize];
        let bytes = drive.read_block(&mut read_back).expect("read block");

        assert_eq!(bytes, BLOCK_SIZE as usize);
        assert_ne!(read_back, original);
        let events = read_jsonl(&event_log_path(&state_path));
        let med05 = events
            .iter()
            .find(|event| event["catalogue_id"] == "MED-05")
            .expect("MED-05 event");
        assert_eq!(med05["barcode"], BARCODE);
        assert_eq!(med05["mutation_summary"]["applied_length"], 16);
    }
}
