//! Standalone storage contracts through the production command dispatcher.
//!
//! | ID | Observable behaviour | Boundary coverage and limits |
//! | --- | --- | --- |
//! | B01 | Acknowledged objects survive process exit and remain listed and readable. | API crash, restart and read tests; CLI fresh-command reads. |
//! | B02 | Later sessions and cartridge removal preserve earlier committed data. | API successive-session and restart tests; CLI successive puts. |
//! | B03 | Checkpoint and close return the durable objects the caller can release. | API explicit and automatic checkpoint tests. |
//! | B04 | Retrying the same object returns its identity without writing another copy. | API retry and CLI unchanged-file tests. |
//! | B05 | Abort preserves committed objects and hides unfinished objects. | API abort test; standalone exposes no abort command. |
//! | B06 | Conflicting access is refused while a tape is active and succeeds after close. | API conflicting-open test; internal handoff races remain lower-level tests. |
//! | B07 | Loading waits without data commands and eventually admits access without operator action. | API delayed-load and interrupted-unload tests. |
//! | B08 | Operators can observe loading, cancel work and resume safely after interruption. | API health during loading; cancellation/restart state matrix remains lower-level coverage. |
//! | B09 | Wrong pool, wrong cartridge and unsafe append targets are refused before media writes. | API pool, identity and unexpected-tail tests; CLI pool refusal. |
//! | B10 | Uncertain or failed writes do not become successful committed copies. | API lost-write-reply test; host journal and catalog failure cuts remain lower-level tests. |
//! | B11 | Finalized tapes remain readable and refuse more writes. | API finalization and bounded read-reopen tests; standalone lifecycle routing changes intentionally. |
//! | B12 | Catalog replay preserves committed copies, tape state and operator decisions. | Restart object/copy reads are covered; operator-state rebuild matrix has no public rebuild RPC. |
//! | B13 | State opens only when its format can be interpreted without losing data. | Format and migration tests remain; historical migration is outside the retained contract. |
//! | B14 | Resource exhaustion refuses work without damaging committed data. | API oversized-append test; internal memory-pressure cuts remain lower-level tests. |
//! | B15 | Standalone commands list and restore the files they acknowledge. | CLI dispatcher tests on fresh command invocations. |
//!
//! Each command opens fresh state and a fresh model transport. The model path
//! never reaches a hardware opener. Session-level checkpoint and abort are not
//! exposed by the standalone CLI; `put` commits before it returns.

use std::path::Path;
use std::process::ExitCode;
use std::sync::{Arc, Mutex};

use remanence_chaos::model::{
    DeviceRole, ModelTransport, SharedVirtualWorld, VirtualTape, VirtualWorld,
};
use remanence_library::transport::TransferOutcome;
use remanence_library::{DeviceAttachment, FixtureTransport, ScsiError, SgTransport, TimeoutClass};

const DEVICE: &str = "/model/characterisation-drive";
const SERIAL: &str = "CHARACTERISATION-CLI";
const BAY: u16 = 0x100;
const BARCODE: &str = "CHAR02L9";

struct Transport(ModelTransport);
impl SgTransport for Transport {
    fn execute_in(&mut self, cdb: &[u8], buf: &mut [u8]) -> Result<TransferOutcome, ScsiError> {
        self.0.execute_in(cdb, buf)
    }
    fn execute_out(&mut self, cdb: &[u8], buf: &[u8]) -> Result<TransferOutcome, ScsiError> {
        self.0.execute_out(cdb, buf)
    }
    fn execute_none(&mut self, cdb: &[u8]) -> Result<(), ScsiError> {
        if cdb[0] == 0x1e {
            assert_eq!(cdb.len(), 6);
            assert!(cdb[4] <= 1);
            FixtureTransport::new().execute_none(cdb)
        } else {
            self.0.execute_none(cdb)
        }
    }
    fn set_timeout_for(&mut self, class: TimeoutClass) {
        self.0.set_timeout_for(class);
    }
}

struct Harness {
    root: tempfile::TempDir,
    world: SharedVirtualWorld,
}
impl Harness {
    fn new() -> Self {
        let mut world = VirtualWorld::single_drive("UNUSED", BAY, SERIAL, 0x400, 0);
        world.put_tape_in_drive(
            BAY,
            BARCODE,
            None,
            VirtualTape::empty(18_000_000_000_000, 262144),
        );
        Self {
            root: tempfile::tempdir().unwrap(),
            world: Arc::new(Mutex::new(world)),
        }
    }
    fn run(&self, args: &[&str]) -> (ExitCode, String) {
        let root = self.root.path();
        let mut out = Vec::new();
        let mut errors = Vec::new();
        let mut argv = vec!["rem", "--allow-untrusted-state-volume"];
        argv.extend(args);
        let code = remanence_cli::run_with_standalone_test_io(
            &argv,
            &root.join("home"),
            &root.join("state"),
            &root.join("absent-config"),
            &root.join("locks"),
            vec![DeviceAttachment {
                sg_path: DEVICE.into(),
                sysfs_path: "/model/sys/drive".into(),
            }],
            |path| {
                assert_eq!(path, Path::new(DEVICE));
                Ok(Box::new(Transport(ModelTransport::new(
                    Arc::clone(&self.world),
                    DeviceRole::Drive { bay: BAY },
                ))) as Box<dyn SgTransport>)
            },
            &mut out,
            &mut errors,
        );
        (
            code,
            format!(
                "{}\n{}",
                String::from_utf8(out).unwrap(),
                String::from_utf8(errors).unwrap()
            ),
        )
    }
    fn ok(&self, args: &[&str]) -> String {
        let (code, output) = self.run(args);
        assert_eq!(code, ExitCode::SUCCESS, "{args:?}: {output}");
        output
    }
    fn init(&self, parity: bool) {
        self.ok(&[
            "pool",
            "new",
            "archive",
            "--scheme",
            if parity { "custom:2,1,3" } else { "none" },
        ]);
        self.ok(&[
            "tape",
            "init",
            "--device",
            DEVICE,
            "--allow-device",
            SERIAL,
            "--label",
            BARCODE,
            "--generation",
            "L9",
            "--pool",
            "archive",
        ]);
    }
    fn put(&self, name: &str, bytes: &[u8]) -> String {
        let source = self.root.path().join(name);
        std::fs::write(&source, bytes).unwrap();
        self.ok(&[
            "put",
            source.to_str().unwrap(),
            "--device",
            DEVICE,
            "--allow-device",
            SERIAL,
        ])
    }
    fn assert_read(&self, name: &str, bytes: &[u8]) {
        let out = tempfile::tempdir_in(self.root.path()).unwrap();
        self.ok(&[
            "get",
            name,
            "--out",
            out.path().to_str().unwrap(),
            "--device",
            DEVICE,
            "--allow-device",
            SERIAL,
        ]);
        assert_eq!(std::fs::read(out.path().join(name)).unwrap(), bytes);
    }
}

#[test]
fn acknowledged_files_are_listed_and_readable_by_later_commands() {
    for parity in [false, true] {
        let fixture = Harness::new();
        fixture.init(parity);
        fixture.put("first.txt", b"first command bytes");
        fixture.put("second.txt", b"second command bytes");
        let listing = fixture.ok(&["list"]);
        assert!(listing.contains("first.txt"), "{listing}");
        assert!(listing.contains("second.txt"), "{listing}");
        fixture.assert_read("first.txt", b"first command bytes");
        fixture.assert_read("second.txt", b"second command bytes");
    }
}

#[test]
fn retrying_unchanged_file_does_not_duplicate_media() {
    let fixture = Harness::new();
    fixture.init(false);
    fixture.put("same.txt", b"same bytes");
    let before = fixture.world.lock().unwrap().tapes[BARCODE].records.clone();
    let output = fixture.put("same.txt", b"same bytes");
    assert!(output.contains("already_stored"), "{output}");
    assert_eq!(fixture.world.lock().unwrap().tapes[BARCODE].records, before);
    fixture.assert_read("same.txt", b"same bytes");
}

#[test]
fn wrong_pool_refuses_write_and_preserves_existing_file() {
    let fixture = Harness::new();
    fixture.init(false);
    fixture.put("safe.txt", b"safe bytes");
    let source = fixture.root.path().join("wrong.txt");
    std::fs::write(&source, b"wrong pool bytes").unwrap();
    let before = fixture.world.lock().unwrap().tapes[BARCODE].records.clone();
    let (code, output) = fixture.run(&[
        "put",
        source.to_str().unwrap(),
        "--pool",
        "default",
        "--device",
        DEVICE,
        "--allow-device",
        SERIAL,
    ]);
    assert_ne!(code, ExitCode::SUCCESS);
    assert!(
        output.contains("default") && output.contains("archive"),
        "{output}"
    );
    assert_eq!(fixture.world.lock().unwrap().tapes[BARCODE].records, before);
    fixture.assert_read("safe.txt", b"safe bytes");
}
