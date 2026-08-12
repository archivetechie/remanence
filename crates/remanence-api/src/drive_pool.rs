//! Collision-safe drive/changer actor registry and session lifecycle state.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::{mpsc, oneshot};
use tonic::Status;
use uuid::Uuid;

use crate::write_owner::{ChangerCommand, DriveCommand};
use remanence_state::DriveHealthSnapshotRecord;

use crate::pool_write::TapeUuid;

/// A drive bay is only unique within one logical library.
#[derive(Clone, Debug, Hash, Ord, PartialOrd, PartialEq, Eq)]
pub(crate) struct DriveKey {
    pub(crate) library_serial: String,
    pub(crate) bay: u16,
}

impl DriveKey {
    pub(crate) fn new(library_serial: impl Into<String>, bay: u16) -> Self {
        Self {
            library_serial: library_serial.into(),
            bay,
        }
    }

    fn label(&self) -> String {
        format!("{} drive bay 0x{:04x}", self.library_serial, self.bay)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct MountedSession {
    pub(crate) bay: u16,
    pub(crate) library_serial: String,
    pub(crate) barcode: Option<String>,
    pub(crate) home_slot: Option<u16>,
    pub(crate) tape_uuid: TapeUuid,
    pub(crate) drive_uuid: Option<Vec<u8>>,
}

impl MountedSession {
    pub(crate) fn drive_key(&self) -> DriveKey {
        DriveKey::new(self.library_serial.clone(), self.bay)
    }
}

/// A library cartridge intentionally left seated after its session closes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SeatedCartridge {
    pub(crate) bay: u16,
    pub(crate) library_serial: String,
    pub(crate) barcode: Option<String>,
    pub(crate) home_slot: u16,
    pub(crate) tape_uuid: Option<TapeUuid>,
    pub(crate) prior_session_id: Option<Uuid>,
}

impl SeatedCartridge {
    pub(crate) fn drive_key(&self) -> DriveKey {
        DriveKey::new(self.library_serial.clone(), self.bay)
    }
}

/// Generation-tagged idle record used to invalidate stale timeout tasks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ParkedCartridge {
    pub(crate) seated: SeatedCartridge,
    pub(crate) generation: u64,
}

#[derive(Default)]
pub(crate) struct ParkedState {
    pub(crate) next_generation: u64,
    pub(crate) by_drive: HashMap<DriveKey, ParkedCartridge>,
}

/// Shared actor/pool lifecycle maps used by timer-driven close-and-park.
#[derive(Clone, Default)]
pub(crate) struct DrivePoolLifecycle {
    pub(crate) sessions: Arc<Mutex<HashMap<Uuid, MountedSession>>>,
    pub(crate) parked: Arc<Mutex<ParkedState>>,
    pub(crate) timer_park_tx: Option<mpsc::UnboundedSender<ParkedCartridge>>,
}

impl DrivePoolLifecycle {
    pub(crate) fn with_timer_park_sender(
        timer_park_tx: mpsc::UnboundedSender<ParkedCartridge>,
    ) -> Self {
        Self {
            timer_park_tx: Some(timer_park_tx),
            ..Self::default()
        }
    }
}

#[derive(Clone)]
pub(crate) struct DrivePool {
    changers: Arc<HashMap<String, mpsc::Sender<ChangerCommand>>>,
    drives: Arc<HashMap<DriveKey, mpsc::Sender<DriveCommand>>>,
    reservations: Arc<HashMap<DriveKey, AtomicBool>>,
    pub(crate) sessions: Arc<Mutex<HashMap<Uuid, MountedSession>>>,
    tape_reservations: Arc<Mutex<HashSet<TapeUuid>>>,
    parked: Arc<Mutex<ParkedState>>,
    shutting_down: Arc<AtomicBool>,
}

impl DrivePool {
    #[cfg(test)]
    pub(crate) fn new(
        changer: mpsc::Sender<ChangerCommand>,
        drives: HashMap<u16, mpsc::Sender<DriveCommand>>,
        reservations: Arc<HashMap<u16, AtomicBool>>,
    ) -> Self {
        Self::new_for_library("LIB001", changer, drives, reservations)
    }

    #[cfg(test)]
    pub(crate) fn new_for_library(
        library_serial: &str,
        changer: mpsc::Sender<ChangerCommand>,
        drives: HashMap<u16, mpsc::Sender<DriveCommand>>,
        reservations: Arc<HashMap<u16, AtomicBool>>,
    ) -> Self {
        Self::new_with_lifecycle(
            HashMap::from([(library_serial.to_string(), changer)]),
            drives
                .into_iter()
                .map(|(bay, sender)| (DriveKey::new(library_serial, bay), sender))
                .collect(),
            Arc::new(
                reservations
                    .iter()
                    .map(|(bay, reserved)| {
                        (
                            DriveKey::new(library_serial, *bay),
                            AtomicBool::new(reserved.load(Ordering::SeqCst)),
                        )
                    })
                    .collect(),
            ),
            DrivePoolLifecycle::default(),
        )
    }

    pub(crate) fn new_with_lifecycle(
        changers: HashMap<String, mpsc::Sender<ChangerCommand>>,
        drives: HashMap<DriveKey, mpsc::Sender<DriveCommand>>,
        reservations: Arc<HashMap<DriveKey, AtomicBool>>,
        lifecycle: DrivePoolLifecycle,
    ) -> Self {
        Self {
            changers: Arc::new(changers),
            drives: Arc::new(drives),
            reservations,
            sessions: lifecycle.sessions,
            tape_reservations: Arc::new(Mutex::new(HashSet::new())),
            parked: lifecycle.parked,
            shutting_down: Arc::new(AtomicBool::new(false)),
        }
    }

    pub(crate) fn operates_library(&self, library_serial: &str) -> bool {
        self.changers.contains_key(library_serial)
    }

    pub(crate) fn changer_tx(
        &self,
        library_serial: &str,
    ) -> Result<mpsc::Sender<ChangerCommand>, Status> {
        self.changers.get(library_serial).cloned().ok_or_else(|| {
            Status::not_found(format!(
                "library {library_serial} is not operated by this daemon"
            ))
        })
    }

    pub(crate) fn drive_tx(&self, drive: &DriveKey) -> Result<mpsc::Sender<DriveCommand>, Status> {
        self.drives
            .get(drive)
            .cloned()
            .ok_or_else(|| Status::not_found(format!("{} not available", drive.label())))
    }

    #[cfg(test)]
    pub(crate) fn reserve_free_drive(&self, library_serial: &str) -> Result<DriveKey, Status> {
        let mut drives = self
            .reservations
            .keys()
            .filter(|drive| drive.library_serial == library_serial)
            .cloned()
            .collect::<Vec<_>>();
        drives.sort();
        drives
            .into_iter()
            .find(|drive| {
                self.reservations.get(drive).is_some_and(|reservation| {
                    reservation
                        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                        .is_ok()
                })
            })
            .ok_or_else(|| Status::failed_precondition("all drives are busy"))
    }

    pub(crate) fn reserve_drive(&self, drive: &DriveKey) -> Result<DriveReservation, Status> {
        if self.is_shutting_down() {
            return Err(Status::unavailable("drive pool is shutting down"));
        }
        self.reserve_drive_inner(drive)
    }

    pub(crate) fn reserve_drive_for_shutdown(
        &self,
        drive: &DriveKey,
    ) -> Result<DriveReservation, Status> {
        self.reserve_drive_inner(drive)
    }

    fn reserve_drive_inner(&self, drive: &DriveKey) -> Result<DriveReservation, Status> {
        let reservation = self
            .reservations
            .get(drive)
            .ok_or_else(|| Status::not_found(format!("{} not available", drive.label())))?;
        reservation
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .map_err(|_| Status::failed_precondition(format!("{} is busy", drive.label())))?;
        Ok(DriveReservation {
            drive: drive.clone(),
            reservations: self.reservations.clone(),
            armed: true,
        })
    }

    pub(crate) fn release(&self, drive: &DriveKey) {
        if let Some(reservation) = self.reservations.get(drive) {
            reservation.store(false, Ordering::SeqCst);
        }
    }

    #[cfg(test)]
    pub(crate) fn reserve_all_exclusive(&self) -> Result<(), Status> {
        self.reserve_matching_exclusive(|_| true)
    }

    pub(crate) fn reserve_library_exclusive(&self, library_serial: &str) -> Result<(), Status> {
        self.reserve_matching_exclusive(|drive| drive.library_serial == library_serial)
    }

    fn reserve_matching_exclusive(
        &self,
        include: impl Fn(&DriveKey) -> bool,
    ) -> Result<(), Status> {
        if self.is_shutting_down() {
            return Err(Status::unavailable("drive pool is shutting down"));
        }
        let mut acquired = Vec::new();
        let mut drives = self
            .reservations
            .keys()
            .filter(|drive| include(drive))
            .cloned()
            .collect::<Vec<_>>();
        drives.sort();
        for drive in drives {
            let Some(reservation) = self.reservations.get(&drive) else {
                continue;
            };
            if reservation
                .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                acquired.push(drive);
            } else {
                for acquired_drive in acquired {
                    self.release(&acquired_drive);
                }
                return Err(Status::failed_precondition("drives are busy"));
            }
        }
        Ok(())
    }

    pub(crate) fn release_library(&self, library_serial: &str) {
        release_library_reservations(&self.reservations, library_serial);
    }

    pub(crate) fn busy_drives(&self) -> HashSet<DriveKey> {
        self.reservations
            .iter()
            .filter(|(_, reservation)| reservation.load(Ordering::SeqCst))
            .map(|(drive, _)| drive.clone())
            .collect()
    }

    pub(crate) fn busy_bays(&self, library_serial: &str) -> HashSet<u16> {
        self.busy_drives()
            .into_iter()
            .filter(|drive| drive.library_serial == library_serial)
            .map(|drive| drive.bay)
            .collect()
    }

    pub(crate) fn sessions_by_drive(&self) -> HashMap<DriveKey, (Uuid, MountedSession)> {
        self.sessions
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .iter()
            .map(|(session_id, mounted)| (mounted.drive_key(), (*session_id, mounted.clone())))
            .collect()
    }

    pub(crate) fn drive_keys(&self) -> Vec<DriveKey> {
        let mut keys = self.drives.keys().cloned().collect::<Vec<_>>();
        keys.sort();
        keys
    }

    pub(crate) fn mounted_tape_uuids(&self) -> HashSet<TapeUuid> {
        let mut in_use = self
            .sessions
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .values()
            .map(|mounted| mounted.tape_uuid)
            .collect::<HashSet<_>>();
        in_use.extend(
            self.tape_reservations
                .lock()
                .unwrap_or_else(|err| err.into_inner())
                .iter()
                .copied(),
        );
        in_use
    }

    pub(crate) fn reserve_tape(&self, tape_uuid: TapeUuid) -> Result<TapeReservation, Status> {
        self.reserve_tape_with_after_insert(tape_uuid, |_| {})
    }

    pub(crate) fn reserve_tape_with_after_insert(
        &self,
        tape_uuid: TapeUuid,
        after_insert: impl FnOnce(&HashSet<TapeUuid>),
    ) -> Result<TapeReservation, Status> {
        let sessions = self.sessions.lock().unwrap_or_else(|err| err.into_inner());
        if sessions
            .values()
            .any(|mounted| mounted.tape_uuid == tape_uuid)
        {
            return Err(Status::failed_precondition("tape is already mounted"));
        }
        let mut reservations = self
            .tape_reservations
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        if !reservations.insert(tape_uuid) {
            return Err(Status::failed_precondition("tape is already mounted"));
        }
        after_insert(&reservations);
        drop(sessions);
        Ok(TapeReservation {
            tape_uuid,
            reservations: self.tape_reservations.clone(),
        })
    }

    pub(crate) fn record_session(&self, session_id: Uuid, mounted: MountedSession) {
        self.parked
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .by_drive
            .remove(&mounted.drive_key());
        self.sessions
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .insert(session_id, mounted);
    }

    pub(crate) fn session(&self, session_id: Uuid) -> Result<MountedSession, Status> {
        self.sessions
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .get(&session_id)
            .cloned()
            .ok_or_else(|| Status::not_found("session not found"))
    }

    pub(crate) fn forget_session(&self, session_id: Uuid) {
        self.sessions
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .remove(&session_id);
    }

    pub(crate) fn finish_session(
        &self,
        session_id: Uuid,
        mounted: MountedSession,
    ) -> Option<ParkedCartridge> {
        let drive = mounted.drive_key();
        self.forget_session(session_id);
        let parked = mounted.home_slot.map(|home_slot| {
            self.park_cartridge(SeatedCartridge {
                bay: mounted.bay,
                library_serial: mounted.library_serial,
                barcode: mounted.barcode,
                home_slot,
                tape_uuid: Some(mounted.tape_uuid),
                prior_session_id: Some(session_id),
            })
        });
        self.release(&drive);
        parked
    }

    pub(crate) fn park_cartridge(&self, seated: SeatedCartridge) -> ParkedCartridge {
        let mut state = self.parked.lock().unwrap_or_else(|err| err.into_inner());
        state.next_generation = state.next_generation.wrapping_add(1).max(1);
        let parked = ParkedCartridge {
            seated,
            generation: state.next_generation,
        };
        state
            .by_drive
            .insert(parked.seated.drive_key(), parked.clone());
        parked
    }

    pub(crate) fn parked_at(&self, drive: &DriveKey) -> Option<ParkedCartridge> {
        self.parked
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .by_drive
            .get(drive)
            .cloned()
    }

    pub(crate) fn parked_is_current(&self, parked: &ParkedCartridge) -> bool {
        self.parked_at(&parked.seated.drive_key())
            .is_some_and(|current| current.generation == parked.generation)
    }

    pub(crate) fn forget_parked(&self, parked: &ParkedCartridge) {
        let mut state = self.parked.lock().unwrap_or_else(|err| err.into_inner());
        if state
            .by_drive
            .get(&parked.seated.drive_key())
            .is_some_and(|current| current.generation == parked.generation)
        {
            state.by_drive.remove(&parked.seated.drive_key());
        }
    }

    pub(crate) fn begin_shutdown(&self) {
        self.shutting_down.store(true, Ordering::SeqCst);
    }

    pub(crate) fn is_shutting_down(&self) -> bool {
        self.shutting_down.load(Ordering::SeqCst)
    }

    pub(crate) async fn poll_drive_health(
        &self,
        drive: &DriveKey,
        drive_uuid: Vec<u8>,
    ) -> Result<DriveHealthSnapshotRecord, Status> {
        let tx = self.drive_tx(drive)?;
        let (reply, rx) = oneshot::channel();
        tx.send(DriveCommand::PollHealth {
            drive_uuid,
            trigger: "manual",
            session_id: None,
            tape_uuid: None,
            reply,
        })
        .await
        .map_err(|_| Status::unavailable("drive actor is unavailable"))?;
        rx.await
            .map_err(|_| Status::unavailable("drive actor stopped"))?
    }

    pub(crate) fn heartbeat_drive(
        &self,
        drive: &DriveKey,
        drive_uuid: Vec<u8>,
    ) -> Result<(), Status> {
        let tx = self.drive_tx(drive)?;
        let (reply, rx) = oneshot::channel();
        tx.blocking_send(DriveCommand::Heartbeat { drive_uuid, reply })
            .map_err(|_| Status::unavailable("drive actor is unavailable"))?;
        rx.blocking_recv()
            .map_err(|_| Status::unavailable("drive actor stopped"))?
    }
}

#[derive(Debug)]
pub(crate) struct DriveReservation {
    drive: DriveKey,
    reservations: Arc<HashMap<DriveKey, AtomicBool>>,
    armed: bool,
}

impl DriveReservation {
    pub(crate) fn disarm(mut self) {
        self.armed = false;
    }
}

impl Drop for DriveReservation {
    fn drop(&mut self) {
        if self.armed {
            if let Some(reservation) = self.reservations.get(&self.drive) {
                reservation.store(false, Ordering::SeqCst);
            }
        }
    }
}

#[derive(Debug)]
pub(crate) struct TapeReservation {
    tape_uuid: TapeUuid,
    reservations: Arc<Mutex<HashSet<TapeUuid>>>,
}

impl Drop for TapeReservation {
    fn drop(&mut self) {
        self.reservations
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .remove(&self.tape_uuid);
    }
}

pub(crate) struct ExclusiveGuard {
    reservations: Arc<HashMap<DriveKey, AtomicBool>>,
    library_serial: Option<String>,
}

impl ExclusiveGuard {
    #[cfg(test)]
    pub(crate) fn from_reserved(reservations: Arc<HashMap<DriveKey, AtomicBool>>) -> Self {
        Self {
            reservations,
            library_serial: None,
        }
    }

    pub(crate) fn from_reserved_library(
        reservations: Arc<HashMap<DriveKey, AtomicBool>>,
        library_serial: impl Into<String>,
    ) -> Self {
        Self {
            reservations,
            library_serial: Some(library_serial.into()),
        }
    }
}

impl Drop for ExclusiveGuard {
    fn drop(&mut self) {
        if let Some(library_serial) = self.library_serial.as_deref() {
            release_library_reservations(&self.reservations, library_serial);
        } else {
            release_all_reservations(&self.reservations);
        }
    }
}

pub(crate) fn release_all_reservations(reservations: &HashMap<DriveKey, AtomicBool>) {
    for reservation in reservations.values() {
        reservation.store(false, Ordering::SeqCst);
    }
}

pub(crate) fn release_library_reservations(
    reservations: &HashMap<DriveKey, AtomicBool>,
    library_serial: &str,
) {
    for (drive, reservation) in reservations {
        if drive.library_serial == library_serial {
            reservation.store(false, Ordering::SeqCst);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_pool() -> DrivePool {
        let (changer_a, _) = mpsc::channel(1);
        let (changer_b, _) = mpsc::channel(1);
        let key_a = DriveKey::new("LIB-A", 0x0100);
        let key_b = DriveKey::new("LIB-B", 0x0100);
        let (drive_a, _) = mpsc::channel(1);
        let (drive_b, _) = mpsc::channel(1);
        DrivePool::new_with_lifecycle(
            HashMap::from([
                ("LIB-A".to_string(), changer_a),
                ("LIB-B".to_string(), changer_b),
            ]),
            HashMap::from([(key_a.clone(), drive_a), (key_b.clone(), drive_b)]),
            Arc::new(HashMap::from([
                (key_a, AtomicBool::new(false)),
                (key_b, AtomicBool::new(false)),
            ])),
            DrivePoolLifecycle::default(),
        )
    }

    #[test]
    fn equal_bays_in_different_libraries_reserve_independently() {
        let pool = test_pool();
        let key_a = DriveKey::new("LIB-A", 0x0100);
        let key_b = DriveKey::new("LIB-B", 0x0100);
        let reservation_a = pool.reserve_drive(&key_a).expect("reserve A");
        let reservation_b = pool.reserve_drive(&key_b).expect("reserve B");
        assert_eq!(pool.busy_bays("LIB-A"), HashSet::from([0x0100]));
        assert_eq!(pool.busy_bays("LIB-B"), HashSet::from([0x0100]));
        drop(reservation_a);
        assert!(pool.busy_bays("LIB-A").is_empty());
        assert_eq!(pool.busy_bays("LIB-B"), HashSet::from([0x0100]));
        drop(reservation_b);
    }

    #[test]
    fn parked_state_does_not_collide_on_equal_bays() {
        let pool = test_pool();
        let parked_a = pool.park_cartridge(SeatedCartridge {
            bay: 0x0100,
            library_serial: "LIB-A".to_string(),
            barcode: Some("A001".to_string()),
            home_slot: 1,
            tape_uuid: None,
            prior_session_id: None,
        });
        let parked_b = pool.park_cartridge(SeatedCartridge {
            bay: 0x0100,
            library_serial: "LIB-B".to_string(),
            barcode: Some("B001".to_string()),
            home_slot: 2,
            tape_uuid: None,
            prior_session_id: None,
        });
        assert_eq!(
            pool.parked_at(&parked_a.seated.drive_key())
                .and_then(|parked| parked.seated.barcode),
            Some("A001".to_string())
        );
        assert_eq!(
            pool.parked_at(&parked_b.seated.drive_key())
                .and_then(|parked| parked.seated.barcode),
            Some("B001".to_string())
        );
    }

    #[test]
    fn library_exclusive_reservation_does_not_block_another_library() {
        let pool = test_pool();
        let key_a = DriveKey::new("LIB-A", 0x0100);
        let key_b = DriveKey::new("LIB-B", 0x0100);
        pool.reserve_library_exclusive("LIB-A")
            .expect("reserve library A");
        assert_eq!(
            pool.reserve_drive(&key_a)
                .expect_err("library A is exclusively reserved")
                .code(),
            tonic::Code::FailedPrecondition
        );
        let reservation_b = pool
            .reserve_drive(&key_b)
            .expect("library B remains independent");
        drop(ExclusiveGuard::from_reserved_library(
            pool.reservations.clone(),
            "LIB-A",
        ));
        assert!(
            pool.reservations
                .get(&key_b)
                .expect("library B reservation")
                .load(Ordering::SeqCst),
            "releasing library A must not release library B"
        );
        drop(reservation_b);
        pool.reserve_drive(&key_a)
            .expect("library A guard releases its drives");
    }
}
