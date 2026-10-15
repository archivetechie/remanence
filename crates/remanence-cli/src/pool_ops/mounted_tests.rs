//! Exercise the common CLI consumers against real model-written cartridges,
//! including their sole BOT-relative positioning boundary.
use super::*;
use remanence_api::{LtoGen, OperatorMount, WriteObjectInputKind, WriteObjectSource};
use remanence_chaos::model::{
    DeviceRole, ModelTransport, Record, SharedVirtualWorld, VirtualTape, VirtualWorld,
};
use remanence_library::{transport::TransferOutcome, ScsiError, SgTransport, TimeoutClass};
use std::sync::{Arc, Mutex};

const UUID: TapeUuid = [0x81; 16];
const BLOCK: u32 = 262144;
const BAY: u16 = 0x100;
const SERIAL: &str = "CLI-READ-MODEL";
const DEVICE: &str = "/dev/sg-cli-read-model";

struct Transport {
    inner: ModelTransport,
    commands: Option<Arc<Mutex<Vec<Vec<u8>>>>>,
}
impl Transport {
    fn record(&self, cdb: &[u8]) {
        if let Some(commands) = &self.commands {
            commands.lock().unwrap().push(cdb.to_vec());
        }
    }
}
impl SgTransport for Transport {
    fn execute_in(&mut self, cdb: &[u8], data: &mut [u8]) -> Result<TransferOutcome, ScsiError> {
        self.record(cdb);
        self.inner.execute_in(cdb, data)
    }
    fn execute_out(&mut self, cdb: &[u8], data: &[u8]) -> Result<TransferOutcome, ScsiError> {
        self.record(cdb);
        self.inner.execute_out(cdb, data)
    }
    fn execute_none(&mut self, cdb: &[u8]) -> Result<(), ScsiError> {
        self.record(cdb);
        if cdb[0] == 0x1e {
            return Ok(());
        }
        self.inner.execute_none(cdb)
    }
    fn set_timeout_for(&mut self, class: TimeoutClass) {
        self.inner.set_timeout_for(class);
    }
}
fn mount<'a>(
    state: &'a mut StateHandle,
    world: &SharedVirtualWorld,
    root: &Path,
) -> (OperatorMount<'a>, MountedTape) {
    let mut backend = OperatorMount::with_transport_for_test(
        Path::new(DEVICE),
        &StaticAllowlist::new([SERIAL]),
        Box::new(Transport {
            inner: ModelTransport::new(Arc::clone(world), DeviceRole::Drive { bay: BAY }),
            commands: None,
        }),
        state,
        &root.join("locks"),
    )
    .unwrap();
    let mounted = backend
        .mount(MountTarget::Device {
            path: DEVICE.into(),
            expected_uuid: Some(UUID),
        })
        .unwrap();
    (backend, mounted)
}

#[test]
fn barcode_golden_commands_and_standalone_consumers_cover_both_representations() {
    let root = tempfile::tempdir().unwrap();
    let config = remanence_state::parse_config_toml(&format!(
        r#"
[daemon]
state_dir = "{0}"
default_idle_timeout_seconds = 1800
io_memory_ceiling = "64 MiB"
append_ring_bytes = "16 MiB"
[tape_io]
read_reservoir_bytes = "16 MiB"
[[tape_pools]]
id = "default"
block_size = 262144
[journal]
dir = "{0}/journals"
require_trusted_volume = false
[audit]
dir = "{0}/audit"
fsync = true
[index]
sqlite_path = "{0}/index.sqlite"
[cache]
tape_catalog_dir = "{0}/cache"
"#,
        root.path().display()
    ))
    .unwrap();
    let mut state = StateHandle::open_with_config(
        remanence_state::StatePaths::from_config(root.path().join("config.toml"), &config),
        config,
    )
    .unwrap();
    state
        .catalog_index()
        .provision_tape(remanence_state::ProvisionTapeInput {
            tape_uuid: UUID,
            voltag: "HAND".into(),
            block_size: BLOCK,
            parity: remanence_parity::ParityConfig::None,
            force: false,
        })
        .unwrap();
    state
        .catalog_index()
        .set_tape_generation(
            UUID,
            remanence_state::MediaObservation {
                generation: LtoGen::Lto9,
                source: remanence_state::GenerationSource::Operator,
            },
        )
        .unwrap();
    state
        .catalog_index()
        .project_tape_pool_membership(UUID, "default")
        .unwrap();
    let bootstrap = remanence_api::build_tape_bootstrap(
        UUID,
        BLOCK,
        remanence_parity::ParityConfig::None,
        "2026-10-15T00:00:00Z",
        "mounted-read-model",
    );
    let mut block = vec![0; BLOCK as usize];
    remanence_parity::bootstrap::write_bootstrap_block(&bootstrap, &mut block).unwrap();
    let mut tape = VirtualTape::empty(128 * 1024 * 1024, BLOCK);
    tape.records = vec![Record::Block(block), Record::Filemark];
    tape.written_bytes = u64::from(BLOCK);
    let mut world = VirtualWorld::single_drive("UNUSED", BAY, SERIAL, 0x400, 0);
    world.put_tape_in_drive(BAY, "HAND", None, tape);
    let world = Arc::new(Mutex::new(world));
    let key = RecipientPrivateKey::new([1; 16], "primary", [3; 32]).unwrap();
    let backup = RecipientPrivateKey::new([2; 16], "backup", [4; 32]).unwrap();
    let key_path = root.path().join("private.remp");
    std::fs::write(&key_path, key.serialize().as_slice()).unwrap();
    for encrypted in [false, true] {
        let name = if encrypted { "encrypted" } else { "plaintext" };
        let input = root.path().join(name);
        std::fs::write(&input, name).unwrap();
        let (mut backend, mut mounted) = mount(&mut state, &world, root.path());
        let result = backend
            .write_checkpointed(
                &mut mounted,
                WriteObjectToPoolRequest {
                    pool_id: "default".into(),
                    source: WriteObjectSource::Path(input),
                    archive_path: name.into(),
                    caller_object_id: name.into(),
                    expected_content_sha256: None,
                    expected_object_id: None,
                    input_kind: WriteObjectInputKind::LogicalFile,
                    representation: if encrypted {
                        PoolWriteRepresentation::Encrypted {
                            recipients: vec![
                                key.public_key(0).unwrap(),
                                backup.public_key(1).unwrap(),
                            ],
                        }
                    } else {
                        PoolWriteRepresentation::Plaintext
                    },
                },
            )
            .unwrap();
        backend.release(mounted);
        let mut locator = Vec::new();
        print_locator_json(&result.object, "default", &mut locator);
        let locator = String::from_utf8(locator).unwrap();
        let private_key = encrypted.then_some(key_path.as_path());
        for verb in ["read", "verify", "export"] {
            let cases: &[&str] = if verb == "export" {
                &["valid"]
            } else if encrypted {
                &["valid", "missing", "invalid"]
            } else {
                &["valid", "unexpected"]
            };
            for case in cases {
                let commands = Arc::new(Mutex::new(Vec::new()));
                let snapshot = world.lock().unwrap().library_snapshot();
                let policy = StaticAllowlist::new(["UNUSED"]);
                let shared = Arc::clone(&world);
                let trace = Arc::clone(&commands);
                let mut library = snapshot
                    .open_with(&policy, move |path| {
                        let role = shared.lock().unwrap().role_for_path(path).unwrap();
                        Ok(Box::new(Transport {
                            inner: ModelTransport::new(Arc::clone(&shared), role),
                            commands: Some(Arc::clone(&trace)),
                        }))
                    })
                    .unwrap();
                let mounted = ChangerMount {
                    library: &mut library,
                    policy: &policy,
                    index: state.catalog_index(),
                }
                .mount(MountTarget::Tape { tape_uuid: UUID })
                .unwrap();
                let invalid = root.path().join("invalid.remp");
                std::fs::write(&invalid, b"not a key").unwrap();
                let key = match *case {
                    "missing" => None,
                    "invalid" => Some(invalid.as_path()),
                    "unexpected" => Some(key_path.as_path()),
                    _ => private_key,
                };
                let mut output = Vec::new();
                let result = match verb {
                    "read" => read_mounted_tape_object(
                        mounted,
                        state.catalog_index(),
                        &locator,
                        key,
                        &mut output,
                    ),
                    "verify" => {
                        verify_mounted_tape_object(mounted, state.catalog_index(), &locator, key)
                    }
                    _ => export_mounted_tape_object(
                        mounted,
                        state.catalog_index(),
                        &locator,
                        &mut output,
                    ),
                };
                assert_eq!(
                    result.is_ok(),
                    *case == "valid",
                    "{name}/{verb}/{case}: {result:?}"
                );
                let commands = commands.lock().unwrap();
                assert_eq!(
                    *commands,
                    pre_p4_commands(encrypted, verb == "export", *case == "valid"),
                    "{name}/{verb}/{case}: complete command stream including release"
                );
                if *case == "valid" && verb == "read" {
                    assert_eq!(output, name.as_bytes());
                }
            }
        }

        for verb in ["read", "verify", "export"] {
            let (mut backend, mounted) = mount(&mut state, &world, root.path());
            world.lock().unwrap().command_log.clear();
            let mut bytes = Vec::new();
            let report = match verb {
                "read" => read_mounted_tape_object(
                    mounted,
                    backend.catalog_index(),
                    &locator,
                    private_key,
                    &mut bytes,
                ),
                "verify" => verify_mounted_tape_object(
                    mounted,
                    backend.catalog_index(),
                    &locator,
                    private_key,
                ),
                _ => export_mounted_tape_object(
                    mounted,
                    backend.catalog_index(),
                    &locator,
                    &mut bytes,
                ),
            }
            .unwrap();
            backend.finish().unwrap();
            if verb == "read" {
                assert_eq!(bytes, name.as_bytes());
            }
            if verb == "export" {
                assert_eq!(
                    report["bytes_written"].as_u64().unwrap(),
                    bytes.len() as u64
                );
                assert!(!bytes.is_empty());
            } else {
                assert_eq!(report["verified"], true);
            }
            let world = world.lock().unwrap();
            assert_eq!(
                world
                    .command_log
                    .iter()
                    .filter(|c| c.opcode == 0x01)
                    .count(),
                0,
                "{name}/{verb}: reuse verified bootstrap origin"
            );
            assert_eq!(
                world
                    .command_log
                    .iter()
                    .filter(|c| matches!(c.opcode, 0x11 | 0x91))
                    .count(),
                1,
                "{name}/{verb}: SPACE"
            );
        }
    }
}

/// Wire-level golden from the pre-P4 library choreography: open changer/drive,
/// rewind and verify BOT, configure, validate key, then SPACE and consume.
/// Plaintext's second post-SPACE READ POSITION is the existing pipeline frontier
/// proof, not a fresh position query by the representation reader.
fn pre_p4_commands(encrypted: bool, export: bool, valid_key: bool) -> Vec<Vec<u8>> {
    let read_position = vec![0x34, 6, 0, 0, 0, 0, 0, 0, 0, 0];
    let mut commands = vec![
        vec![0x12, 0, 0, 0, 0x60, 0], // changer INQUIRY
        vec![0x12, 1, 0x80, 1, 0, 0],
        vec![0x12, 0, 0, 0, 0x60, 0], // drive INQUIRY
        vec![0x12, 1, 0x80, 1, 0, 0],
        vec![1, 0, 0, 0, 0, 0],
        vec![0x92, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
        read_position.clone(),
        vec![8, 0, 0x10, 0, 0, 0], // variable BOT read, 1 MiB limit
        vec![5, 0, 0, 0, 0, 0],
        vec![0x1a, 0, 0x0f, 0, 0x40, 0],
        vec![0x15, 0x10, 0, 0, 0x1c, 0],
    ];
    if !valid_key {
        return commands;
    }
    commands.push(vec![0x11, 1, 0, 0, if encrypted { 2 } else { 1 }, 0]);
    commands.push(read_position.clone());
    if !encrypted && !export {
        commands.push(read_position);
        commands.push(vec![8, 1, 0, 0, 3, 0]); // one three-record fixed batch
    } else {
        commands.extend(std::iter::repeat_n(
            vec![8, 0, 4, 0, 0, 0],
            if encrypted { 4 } else { 3 },
        ));
    }
    commands
}
