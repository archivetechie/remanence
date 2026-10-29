//! Exercise tape and file durability independently of recovery policy.
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use remanence_chaos::file_model::MemoryDirectory;
use remanence_chaos::model::{DeviceRole, ModelTransport, VirtualTape, VirtualWorld};
use remanence_library::durable_file::DurableDirectory;
use remanence_library::SgTransport;
use remanence_scsi::ScsiError;

fn drive() -> (Arc<Mutex<VirtualWorld>>, ModelTransport) {
    let mut world = VirtualWorld::single_drive("LIB", 1, "DRIVE", 100, 1);
    world.put_tape_in_drive(1, "TAPE", Some(100), VirtualTape::empty(1024, 4));
    let world = Arc::new(Mutex::new(world));
    let transport = ModelTransport::new(world.clone(), DeviceRole::Drive { bay: 1 });
    (world, transport)
}

#[test]
fn buffered_records_allow_every_partial_flush_and_reset_requires_positioning() {
    for keep in 1..=3 {
        let (world, mut drive) = drive();
        drive.execute_out(&[0x0a, 0, 0, 0, 4, 0], b"base").unwrap();
        drive.execute_none(&[0x10, 0, 0, 0, 0, 0]).unwrap();
        drive.execute_out(&[0x0a, 0, 0, 0, 4, 0], b"tail").unwrap();
        drive.execute_none(&[0x10, 1, 0, 0, 1, 0]).unwrap();
        {
            let mut w = world.lock().unwrap();
            assert_eq!(w.tapes["TAPE"].durable_frontier, 1);
            assert!(w.power_cut(1, 0).is_err());
            assert!(w.power_cut(1, 4).is_err());
            w.power_cut(1, keep).unwrap();
            assert_eq!(w.tapes["TAPE"].records.len(), keep);
        }
        // Reopening a transport cannot hide the reset or manufacture BOT position.
        let mut drive = ModelTransport::new(world.clone(), DeviceRole::Drive { bay: 1 });
        drive
            .execute_in(&[0x12, 0, 0, 0, 36, 0], &mut [0; 36])
            .unwrap();
        let ScsiError::CheckCondition { sense, .. } = drive.execute_none(&[0; 6]).unwrap_err()
        else {
            panic!("reset unit attention");
        };
        assert_eq!((sense[2], sense[12], sense[13]), (6, 0x29, 0));
        let mut position = [0; 32];
        drive
            .execute_in(&[0x34, 6, 0, 0, 0, 0, 0, 0, 0, 0], &mut position)
            .unwrap();
        assert_eq!(position[0] & 0x0c, 0x0c);
        assert!(drive.execute_out(&[0x0a, 0, 0, 0, 4, 0], b"nope").is_err());
        drive.execute_none(&[1, 0, 0, 0, 0, 0]).unwrap();
        drive
            .execute_in(&[0x34, 6, 0, 0, 0, 0, 0, 0, 0, 0], &mut position)
            .unwrap();
        assert_eq!(position[0] & 0x0c, 0);
    }
}

#[test]
fn zero_count_barrier_preserves_tail_and_overwrite_retracts_frontier() {
    let (world, mut drive) = drive();
    drive
        .execute_out(&[0x0a, 1, 0, 0, 3, 0], b"abcdefghijkl")
        .unwrap();
    drive.execute_none(&[1, 0, 0, 0, 0, 0]).unwrap();
    drive.execute_none(&[0x10, 0, 0, 0, 0, 0]).unwrap();
    assert_eq!(world.lock().unwrap().tapes["TAPE"].records.len(), 3);
    drive.execute_out(&[0x0a, 0, 0, 0, 0, 0], b"").unwrap();
    assert!(drive.execute_out(&[0x0a, 0, 0, 0, 4, 0], b"short").is_err());
    assert_eq!(world.lock().unwrap().tapes["TAPE"].records.len(), 3);
    assert_eq!(world.lock().unwrap().tapes["TAPE"].durable_frontier, 3);
    drive.execute_out(&[0x0a, 0, 0, 0, 4, 0], b"abcd").unwrap();
    assert_eq!(world.lock().unwrap().tapes["TAPE"].durable_frontier, 0);
    world.lock().unwrap().power_cut(1, 0).unwrap();
    assert!(world.lock().unwrap().tapes["TAPE"].records.is_empty());
}

#[test]
fn each_synchronizing_command_flushes_and_queries_do_not() {
    for command in [
        vec![1, 0, 0, 0, 0, 0],
        vec![0x10, 0, 0, 0, 1, 0],
        vec![0x10, 0, 0, 0, 0, 0],
        vec![0x11, 3, 0, 0, 0, 0],
        vec![0x1b, 0, 0, 0, 1, 0],
        vec![0x2b, 0, 0, 0, 0, 0, 0, 0, 0, 0],
        vec![0x92, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
        vec![0x91, 3, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    ] {
        let (world, mut drive) = drive();
        drive.execute_out(&[0x0a, 0, 0, 0, 4, 0], b"data").unwrap();
        drive.execute_none(&[0; 6]).unwrap();
        drive.execute_none(&[0x11, 0, 0, 0, 0, 0]).unwrap();
        drive
            .execute_in(&[0x34, 6, 0, 0, 0, 0, 0, 0, 0, 0], &mut [0; 32])
            .unwrap();
        assert_eq!(world.lock().unwrap().tapes["TAPE"].durable_frontier, 0);
        drive.execute_none(&command).unwrap();
        let w = world.lock().unwrap();
        assert_eq!(
            w.tapes["TAPE"].durable_frontier,
            w.tapes["TAPE"].records.len()
        );
    }
    let (world, mut drive) = drive();
    drive.execute_out(&[0x0a, 0, 0, 0, 4, 0], b"data").unwrap();
    assert!(drive.execute_in(&[8, 0, 0, 0, 4, 0], &mut [0; 4]).is_err());
    assert_eq!(world.lock().unwrap().tapes["TAPE"].durable_frontier, 1);
}

#[test]
fn every_byte_of_unsynced_tail_can_tear() {
    let model = MemoryDirectory::default();
    let path = Path::new("/journal/log");
    let mut file = model.create(path).unwrap();
    file.append(b"durable").unwrap();
    file.sync().unwrap();
    model.sync(path.parent().unwrap()).unwrap();
    file.append(b"unsynced").unwrap();
    for keep in 0..=8 {
        let mut cut = model.snapshot();
        cut.crash(&BTreeMap::from([(path.to_owned(), keep)]), false)
            .unwrap();
        let (_, bytes) = cut.files().next().unwrap();
        assert_eq!(bytes.written, b"durableunsynced"[..7 + keep]);
        assert_eq!(bytes.written, bytes.durable);
    }
    model.fail_next_sync(path).unwrap();
    assert!(file.sync().is_err());
    file.sync().unwrap();
    assert_eq!(
        model.snapshot().files().next().unwrap().1.durable,
        b"durable"
    );
}

#[test]
fn synced_bytes_need_a_synced_name_and_rename_preserves_inode() {
    let model = MemoryDirectory::default();
    let old = Path::new("/journal/temporary");
    let new = Path::new("/journal/final");
    let mut file = model.create(old).unwrap();
    file.append(b"header").unwrap();
    file.sync().unwrap();
    let mut vanished = model.snapshot();
    vanished.crash(&BTreeMap::new(), false).unwrap();
    assert_eq!(vanished.files().count(), 0);
    model.sync(Path::new("/journal")).unwrap();
    model.rename(old, new).unwrap();
    file.append(b"tail").unwrap();
    file.sync().unwrap();
    let mut old_name = model.snapshot();
    old_name.crash(&BTreeMap::new(), false).unwrap();
    let (path, bytes) = old_name.files().next().unwrap();
    assert_eq!(path, old);
    assert_eq!(bytes.durable, b"headertail");
    model.sync(Path::new("/journal")).unwrap();
    let mut new_name = model.snapshot();
    new_name.crash(&BTreeMap::new(), false).unwrap();
    assert_eq!(new_name.files().next().unwrap().0, new);
}
