//! P1 C6 CLI oracles using the production parser and dispatcher with injected IO.
//! `run_with_standalone_test_io` is an intentionally missing integration-test seam:
//! it must wrap normal `rem` dispatch, injecting config search paths, device
//! enumeration/transport and the device-lock directory, never command outcomes.
//! Each invocation gets an isolated HOME/XDG state root and absent system config;
//! no process-global environment or actual /dev/sg device is touched.

use std::path::Path;
use std::process::ExitCode;
use std::sync::{Arc, Mutex};

use remanence_chaos::model::{
    DeviceRole, ModelTransport, SharedVirtualWorld, VirtualTape, VirtualWorld,
};
use remanence_library::transport::TransferOutcome;
use remanence_library::{DeviceAttachment, FixtureTransport, ScsiError, SgTransport, TimeoutClass};
use sha2::{Digest, Sha256};

const DEVICE: &str = "/dev/sg-chaos-drive-0100";
const SERIAL: &str = "CLI-ORACLE-DRIVE";
const BAY: u16 = 0x0100;

// ModelTransport handles every media operation. Its only missing command
// needed here is PREVENT/ALLOW; use FixtureTransport's no-data completion.
struct RemovalTransport(ModelTransport);
impl SgTransport for RemovalTransport {
    fn execute_in(&mut self, cdb: &[u8], buf: &mut [u8]) -> Result<TransferOutcome, ScsiError> {
        self.0.execute_in(cdb, buf)
    }
    fn execute_none(&mut self, cdb: &[u8]) -> Result<(), ScsiError> {
        if cdb.first() == Some(&0x1e) {
            assert_eq!(cdb.len(), 6);
            assert!(cdb[4] <= 1);
            return FixtureTransport::new().execute_none(cdb);
        }
        self.0.execute_none(cdb)
    }
    fn execute_out(&mut self, cdb: &[u8], buf: &[u8]) -> Result<TransferOutcome, ScsiError> {
        self.0.execute_out(cdb, buf)
    }
    fn set_timeout_for(&mut self, class: TimeoutClass) {
        self.0.set_timeout_for(class);
    }
}

fn world() -> SharedVirtualWorld {
    let mut world = VirtualWorld::single_drive("UNEXPOSED-CHANGER", BAY, SERIAL, 0x0400, 0);
    world.put_tape_in_drive(
        BAY,
        "ORACLEL9",
        None,
        VirtualTape::empty(128 * 1024 * 1024, 262144),
    );
    Arc::new(Mutex::new(world))
}

fn run(root: &Path, world: &SharedVirtualWorld, args: &[&str]) -> (ExitCode, String) {
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let code = remanence_cli::run_with_standalone_test_io(
        args,
        &root.join("home"),
        &root.join("xdg-state"),
        &root.join("absent-etc-rem-config.toml"),
        &root.join("device-locks"),
        vec![DeviceAttachment {
            sg_path: DEVICE.into(),
            sysfs_path: "/sys/devices/oracle-drive".into(),
        }],
        |path: &Path| {
            assert_eq!(path, Path::new(DEVICE), "CLI opened an unexpected device");
            Ok(Box::new(RemovalTransport(ModelTransport::new(
                Arc::clone(world),
                DeviceRole::Drive { bay: BAY },
            ))) as Box<dyn SgTransport>)
        },
        &mut stdout,
        &mut stderr,
    );
    (
        code,
        format!(
            "{}\n{}",
            String::from_utf8(stdout).unwrap(),
            String::from_utf8(stderr).unwrap()
        ),
    )
}

#[test]
fn c6_tape_init_device_requires_allow_device() {
    let temp = tempfile::tempdir().unwrap();
    let world = world();
    let (code, output) = run(
        temp.path(),
        &world,
        &[
            "rem",
            "--allow-untrusted-state-volume",
            "tape",
            "init",
            "--device",
            DEVICE,
            "--label",
            "ORACLEL9",
            "--generation",
            "L9",
        ],
    );
    assert_ne!(code, ExitCode::SUCCESS);
    assert!(
        output.contains("--allow-device"),
        "must name the missing authorization: {output}"
    );
    assert!(
        !output.contains("unexpected argument"),
        "parser rejection is not a policy refusal: {output}"
    );
    let world = world.lock().unwrap();
    assert!(world.tapes["ORACLEL9"].records.is_empty());
    assert_eq!(world.tapes["ORACLEL9"].written_bytes, 0);
    assert!(world
        .command_log
        .iter()
        .all(|c| !matches!(c.opcode, 0x0a | 0x8a | 0x10 | 0x80)));
}

#[test]
fn c6_home_mode_persists_default_pool_without_config() {
    let temp = tempfile::tempdir().unwrap();
    let world = world();
    let config_path = temp.path().join("xdg-state/rem/config.toml");
    assert!(!config_path.exists());
    let (code, output) = run(
        temp.path(),
        &world,
        &[
            "rem",
            "--allow-untrusted-state-volume",
            "pool",
            "new",
            "other",
        ],
    );
    assert_eq!(code, ExitCode::SUCCESS, "{output}");
    let config =
        remanence_state::load_config(&config_path).expect("home mode creates durable config");
    assert_eq!(config.daemon.state_dir, temp.path().join("xdg-state/rem"));
    assert_eq!(
        config
            .tape_pools
            .iter()
            .filter(|p| p.id == "default")
            .count(),
        1
    );
    assert_eq!(
        config.tape_pools.iter().filter(|p| p.id == "other").count(),
        1
    );
    assert!(config.libraries.is_empty());
    assert!(config.tape_pool_rules.is_empty());
    assert!(world.lock().unwrap().tapes["ORACLEL9"].records.is_empty());
}

#[test]
fn c6_put_uses_loaded_pool_and_refuses_mismatched_override() {
    let temp = tempfile::tempdir().unwrap();
    let world = world();
    let (code, output) = run(
        temp.path(),
        &world,
        &[
            "rem",
            "--allow-untrusted-state-volume",
            "pool",
            "new",
            "other",
        ],
    );
    assert_eq!(
        code,
        ExitCode::SUCCESS,
        "configure both pools before testing their mismatch: {output}"
    );
    let (code, output) = run(
        temp.path(),
        &world,
        &[
            "rem",
            "--allow-untrusted-state-volume",
            "tape",
            "init",
            "--device",
            DEVICE,
            "--allow-device",
            SERIAL,
            "--label",
            "ORACLEL9",
            "--generation",
            "L9",
            "--pool",
            "other",
        ],
    );
    assert_eq!(
        code,
        ExitCode::SUCCESS,
        "positive authorized init must really run: {output}"
    );
    let before = world.lock().unwrap().tapes["ORACLEL9"].records.clone();
    assert!(!before.is_empty(), "init must actually write BOT");
    let written_before = world.lock().unwrap().tapes["ORACLEL9"].written_bytes;
    let source = temp.path().join("source.txt");
    std::fs::write(&source, b"must not reach the wrong pool").unwrap();
    let (code, output) = run(
        temp.path(),
        &world,
        &[
            "rem",
            "--allow-untrusted-state-volume",
            "put",
            source.to_str().unwrap(),
            "--device",
            DEVICE,
            "--allow-device",
            SERIAL,
            "--pool",
            "default",
        ],
    );
    assert_ne!(code, ExitCode::SUCCESS);
    assert!(
        output.contains("default") && output.contains("other"),
        "name both pools: {output}"
    );
    assert!(
        !output.contains("unexpected argument") && !output.contains("unrecognized subcommand"),
        "{output}"
    );
    assert_eq!(world.lock().unwrap().tapes["ORACLEL9"].records, before);
    assert_eq!(
        world.lock().unwrap().tapes["ORACLEL9"].written_bytes,
        written_before
    );
    // C6 Amendment 1 authorizes the optional --pool signature. The ordinary
    // put must select the loaded tape's NON-default pool without an override.
    for pool_override in [false, true] {
        let content = if pool_override {
            b"explicit matching pool".as_slice()
        } else {
            b"loaded pool selection".as_slice()
        };
        // Distinct filenames: `put` defaults the caller id to the archive path,
        // and the checkpointed writer rejects differing content under one id
        // (gate round 3), which would make the second branch red for the
        // wrong reason.
        let source = temp.path().join(if pool_override {
            "matching-pool.txt"
        } else {
            "loaded-pool.txt"
        });
        std::fs::write(&source, content).unwrap();
        let written_before = world.lock().unwrap().tapes["ORACLEL9"].written_bytes;
        let mut args = vec![
            "rem",
            "--allow-untrusted-state-volume",
            "put",
            source.to_str().unwrap(),
            "--device",
            DEVICE,
            "--allow-device",
            SERIAL,
        ];
        if pool_override {
            args.extend(["--pool", "other"]);
        }
        let (code, output) = run(temp.path(), &world, &args);
        assert_eq!(code, ExitCode::SUCCESS, "loaded pool write: {output}");
        assert!(
            world.lock().unwrap().tapes["ORACLEL9"].written_bytes > written_before,
            "put must grow media independently: pool_override={pool_override}"
        );
        let config_path = temp.path().join("xdg-state/rem/config.toml");
        let config = remanence_state::load_config(&config_path).unwrap();
        let paths = remanence_state::StatePaths::from_config(config_path, &config);
        let index = remanence_state::CatalogIndex::open(&paths.sqlite_path).unwrap();
        let tape = index.get_tape_by_voltag("ORACLEL9").unwrap().unwrap();
        let object = index
            .get_native_object_by_content_hash(&Sha256::digest(content))
            .unwrap()
            .expect("this invocation's content must be cataloged");
        assert!(
            object.copies.iter().any(|copy| {
                copy.tape_uuid == tape.tape_uuid
                    && copy.status == "committed"
                    && copy.pool_id.as_deref() == Some("other")
            }),
            "this invocation must commit its object to the loaded tape's pool: pool_override={pool_override}"
        );
    }
    assert!(world.lock().unwrap().tapes["ORACLEL9"].written_bytes > written_before);
    let config_path = temp.path().join("xdg-state/rem/config.toml");
    let config = remanence_state::load_config(&config_path).unwrap();
    let paths = remanence_state::StatePaths::from_config(config_path, &config);
    let index = remanence_state::CatalogIndex::open(&paths.sqlite_path).unwrap();
    let tape = index.get_tape_by_voltag("ORACLEL9").unwrap().unwrap();
    assert_eq!(tape.pool_id.as_deref(), Some("other"));
    assert_eq!(
        index
            .committed_copy_pool_snapshots(&tape.tape_uuid)
            .unwrap(),
        vec![Some("other".into())]
    );
}
