//! Policy refusal coverage uses the production parser and dispatcher. No command
//! may open a transport before the explicit device authorization is supplied.
use std::{path::Path, process::ExitCode};

#[test]
fn every_device_verb_refuses_missing_authorization_before_io() {
    let uuid = "31313131-3131-4131-8131-313131313131";
    let commands: Vec<Vec<&str>> = vec![
        vec!["tape", "identify"],
        vec!["tape", "init", "--label", "HOME", "--generation", "L9"],
        vec!["tape", "adopt"],
        vec!["tape", "verify"],
        vec!["tape", "verify", "--full"],
        vec!["tape", "restore", "--out", "/tmp/unused"],
        vec![
            "tape",
            "finalize",
            "--tape",
            uuid,
            "--acknowledge-tape",
            uuid,
            "--reason",
            "close",
            "--idempotency-key",
            uuid,
        ],
        vec!["tape", "assign-pool", "--pool", "default"],
        vec!["tape", "assign-pool", "--unassign"],
        vec!["tape", "rebind", "--label", "NEW", "--previous-uuid", uuid],
        vec!["tape", "set-generation", "--generation", "L7"],
        vec!["tape", "import-journals", "--from", "/tmp/unused"],
        vec!["tape", "release"],
        vec!["put", "/tmp/unused"],
        vec!["get", uuid, "--out", "/tmp/unused"],
        vec!["diagnostics", "capture", "--out", "/tmp/unused.tar"],
    ];
    for command in commands {
        let temp = tempfile::tempdir().unwrap();
        let mut args = vec!["rem", "--json", "--allow-untrusted-state-volume"];
        args.extend(command.clone());
        args.extend(["--device", "/dev/not-opened"]);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code = remanence_cli::run_with_standalone_test_io(
            &args,
            temp.path(),
            &temp.path().join("state"),
            &temp.path().join("absent"),
            &temp.path().join("locks"),
            vec![],
            |_: &Path| panic!("policy must precede device IO"),
            &mut stdout,
            &mut stderr,
        );
        assert_ne!(code, ExitCode::SUCCESS, "{command:?}");
        let response: serde_json::Value = serde_json::from_slice(
            stdout
                .split(|b| *b == b'\n')
                .rfind(|line| !line.is_empty())
                .unwrap(),
        )
        .unwrap_or_else(|_| {
            panic!(
                "parser rejected {command:?}: {}",
                String::from_utf8_lossy(&stderr)
            )
        });
        assert!(
            response["error"]
                .as_str()
                .unwrap()
                .contains("--allow-device"),
            "{response}"
        );
        assert_eq!(
            response["supported_endpoint_kinds"],
            serde_json::json!(["library", "standalone"])
        );
        assert!(!temp.path().join("state/rem/config.toml").exists());
    }
}

#[test]
fn unserialized_override_refuses_all_mutations_before_io() {
    for command in [
        vec!["put", "/tmp/unused"],
        vec!["tape", "release"],
        vec!["tape", "adopt"],
        vec!["tape", "init", "--label", "HOME"],
    ] {
        let temp = tempfile::tempdir().unwrap();
        let mut args = vec![
            "rem",
            "--device",
            "/dev/not-opened",
            "--allow-device",
            "/dev/not-opened",
            "--allow-unserialized-device",
        ];
        args.extend(command);
        let mut out = Vec::new();
        let mut errors = Vec::new();
        let code = remanence_cli::run_with_standalone_test_io(
            &args,
            temp.path(),
            temp.path(),
            &temp.path().join("absent"),
            temp.path(),
            vec![],
            |_: &Path| panic!("no IO permitted"),
            &mut out,
            &mut errors,
        );
        assert_ne!(code, ExitCode::SUCCESS);
        assert!(String::from_utf8_lossy(&errors).contains("only permitted for read verbs"));
    }
}

/// Keep the model responsible for media contents; add only its missing removal command.
struct RemovalTransport {
    inner: remanence_chaos::model::ModelTransport,
    config: std::path::PathBuf,
    fail_writes: bool,
}
impl RemovalTransport {
    /// Inspect persisted evidence at the dispatch boundary, before model IO.
    fn assert_write_guard(&self, opcode: u8) {
        if !matches!(opcode, 0x0a | 0x10) {
            return;
        }
        let config = remanence_state::load_config(&self.config).unwrap();
        let paths = remanence_state::StatePaths::from_config(&self.config, &config);
        let index = remanence_state::CatalogIndex::open(&config.index.sqlite_path).unwrap();
        let sessions = index.non_terminal_sessions().unwrap();
        let session = sessions
            .iter()
            .find(|s| s.session_kind == "write")
            .expect("write session must precede bootstrap dispatch");
        assert!(
            matches!(&session.drive_key, Some(remanence_library::DriveKey::Standalone { drive_serial }) if drive_serial == "HOME-DRIVE")
        );
        let uuid: [u8; 16] = session.tape_uuid.as_deref().unwrap().try_into().unwrap();
        let durable =
            remanence_state::CalibrationControlStore::open(&paths.calibration_dir).unwrap();
        assert!(
            durable.row(uuid).write_epoch > 0,
            "calibration fence must reach disk before the first bootstrap write"
        );
        let audit = remanence_state::FileAuditLog::replay(&paths.audit_dir).unwrap();
        assert!(audit
            .iter()
            .any(|r| r.event == remanence_state::AuditEvent::SessionOpened
                && r.session_id == Some(session.session_id)));
    }
}
impl remanence_library::SgTransport for RemovalTransport {
    fn execute_in(
        &mut self,
        cdb: &[u8],
        buf: &mut [u8],
    ) -> Result<remanence_library::transport::TransferOutcome, remanence_library::ScsiError> {
        self.inner.execute_in(cdb, buf)
    }
    fn execute_none(&mut self, cdb: &[u8]) -> Result<(), remanence_library::ScsiError> {
        self.assert_write_guard(cdb[0]);
        if cdb[0] == 0x1e {
            Ok(())
        } else {
            self.inner.execute_none(cdb)
        }
    }
    fn execute_out(
        &mut self,
        cdb: &[u8],
        buf: &[u8],
    ) -> Result<remanence_library::transport::TransferOutcome, remanence_library::ScsiError> {
        self.assert_write_guard(cdb[0]);
        if self.fail_writes && cdb[0] == 0x0a {
            return Err(remanence_library::ScsiError::TransportError {
                status: 0,
                host_status: 3,
                driver_status: 0,
                info: 1,
                sense: vec![],
            });
        }
        self.inner.execute_out(cdb, buf)
    }
    fn set_timeout_for(&mut self, class: remanence_library::TimeoutClass) {
        self.inner.set_timeout_for(class)
    }
}

#[test]
fn home_roundtrip_and_metadata_use_real_dispatch() {
    use remanence_chaos::model::{DeviceRole, ModelTransport, VirtualTape, VirtualWorld};
    use std::sync::{Arc, Mutex};
    let mut model = VirtualWorld::single_drive("UNEXPOSED", 0x100, "HOME-DRIVE", 0x400, 0);
    model.put_tape_in_drive(
        0x100,
        "HOMEL9",
        None,
        VirtualTape::empty(128 * 1024 * 1024, 262144),
    );
    let world = Arc::new(Mutex::new(model));
    let root = tempfile::tempdir().unwrap();
    let run_result = |extra: &[&str]| {
        let mut args = vec!["rem", "--json", "--allow-untrusted-state-volume"];
        let device = extra[0] != "pool" && !extra.contains(&"--tape");
        if device {
            args.extend([
                "--device",
                "/dev/home-model",
                "--allow-device",
                "HOME-DRIVE",
            ]);
        }
        args.extend_from_slice(extra);
        let mut out = Vec::new();
        let mut errors = Vec::new();
        let code = remanence_cli::run_with_standalone_test_io(
            &args,
            root.path(),
            &root.path().join("state"),
            &root.path().join("absent"),
            &root.path().join("locks"),
            vec![remanence_library::DeviceAttachment {
                sg_path: "/dev/home-model".into(),
                sysfs_path: "/sys/home-model".into(),
            }],
            |_: &Path| {
                Ok(Box::new(RemovalTransport {
                    inner: ModelTransport::new(
                        Arc::clone(&world),
                        DeviceRole::Drive { bay: 0x100 },
                    ),
                    config: root.path().join("state/rem/config.toml"),
                    fail_writes: false,
                })
                    as Box<dyn remanence_library::SgTransport>)
            },
            &mut out,
            &mut errors,
        );
        let response: serde_json::Value = serde_json::from_slice(
            out.split(|b| *b == b'\n')
                .rfind(|line| !line.is_empty())
                .expect("response envelope"),
        )
        .unwrap_or_else(|_| panic!("{}", String::from_utf8_lossy(&errors)));
        if device {
            assert_eq!(
                response["observed_endpoints"][0]["serial"], "HOME-DRIVE",
                "{response}"
            );
        }
        (code, response)
    };
    let run = |extra: &[&str]| {
        let (code, response) = run_result(extra);
        assert_eq!(code, ExitCode::SUCCESS, "{extra:?}: {response}");
        response["result"].clone()
    };
    run(&["tape", "init", "--label", "HOMEL9", "--generation", "L9"]);
    let config_path = root.path().join("state/rem/config.toml");
    let config = remanence_state::load_config(&config_path).unwrap();
    let paths = remanence_state::StatePaths::from_config(&config_path, &config);
    let audit = remanence_state::FileAuditLog::replay(&paths.audit_dir).unwrap();
    let events: Vec<_> = audit
        .iter()
        .filter(|r| {
            matches!(
                r.event,
                remanence_state::AuditEvent::TapeProvisioned
                    | remanence_state::AuditEvent::TapePoolAssigned
            )
        })
        .collect();
    assert_eq!(events.len(), 2);
    assert_eq!(
        events[0].event,
        remanence_state::AuditEvent::TapeProvisioned
    );
    assert_eq!(
        events[1].event,
        remanence_state::AuditEvent::TapePoolAssigned
    );
    assert_eq!(
        events[0].detail.get("voltag_source"),
        Some(&ciborium::Value::Text("operator".into()))
    );
    assert_eq!(
        events[1].detail.get("assignment_source"),
        Some(&ciborium::Value::Text("operator".into()))
    );
    let mut rebuilt =
        remanence_state::CatalogIndex::open(root.path().join("rebuilt.sqlite")).unwrap();
    rebuilt.replay_audit_records(&audit).unwrap();
    let recovered = rebuilt.get_tape_by_voltag("HOMEL9").unwrap().unwrap();
    assert_eq!(recovered.pool_id.as_deref(), Some("default"));
    assert_eq!(
        recovered.voltag_source,
        Some(remanence_state::LabelProvenance::Operator)
    );
    assert_eq!(recovered.cartridge_generation.as_deref(), Some("L9"));
    assert_eq!(recovered.stripes_per_neighborhood, Some(1));
    drop(rebuilt);
    let (code, verification) = run_result(&["tape", "verify"]);
    assert_ne!(code, ExitCode::SUCCESS);
    assert_eq!(verification["ok"], false);
    assert_eq!(verification["status"], "failed");
    assert!(verification["error"]
        .as_str()
        .unwrap()
        .contains("verification failed"));
    run(&["tape", "set-generation", "--generation", "L9"]);
    // Later operator intent must survive loss of the SQLite cache, even when
    // old init records and pool rules are replayed again.
    run(&["pool", "new", "second"]);
    let assert_recovered = |pool: Option<&str>, generation: &str| {
        let config = remanence_state::load_config(&config_path).unwrap();
        let live = remanence_state::CatalogIndex::open(&config.index.sqlite_path).unwrap();
        let expected = live.get_tape_by_voltag("HOMEL9").unwrap().unwrap();
        assert_eq!(expected.pool_id.as_deref(), pool);
        assert_eq!(expected.cartridge_generation.as_deref(), Some(generation));
        let audit = remanence_state::FileAuditLog::replay(&paths.audit_dir).unwrap();
        let mut rebuilt = remanence_state::CatalogIndex::open(":memory:").unwrap();
        for full_rebuild in [false, false, true] {
            if full_rebuild {
                rebuilt
                    .rebuild_from_authoritative_sources(&audit, &[])
                    .unwrap();
            } else {
                rebuilt.replay_audit_records(&audit).unwrap();
            }
            rebuilt
                .reconcile_tape_pool_projection_from_rules(&[], &[])
                .unwrap();
            let actual = rebuilt.get_tape(&expected.tape_uuid).unwrap().unwrap();
            assert_eq!(actual.pool_id, expected.pool_id);
            assert_eq!(actual.assignment_generation, expected.assignment_generation);
            assert_eq!(
                remanence_state::tape_generation(&actual),
                remanence_state::tape_generation(&expected)
            );
        }
        uuid::Uuid::from_slice(&expected.tape_uuid)
            .unwrap()
            .to_string()
    };
    run(&["tape", "assign-pool", "--pool", "second"]);
    let uuid = assert_recovered(Some("second"), "L9");
    run(&["tape", "assign-pool", "--tape", &uuid, "--unassign"]);
    assert_recovered(None, "L9");
    run(&["tape", "set-generation", "--generation", "L8"]);
    assert_recovered(None, "L8");

    run(&["tape", "assign-pool", "--pool", "default"]);
    run(&["tape", "set-generation", "--generation", "L9"]);
    let source = root.path().join("hello.txt");
    std::fs::write(&source, b"standalone member roundtrip").unwrap();
    let result = run(&["put", source.to_str().unwrap()]);
    let object = result["objects"][0]["object_id"].as_str().unwrap();
    let destination = root.path().join("restore");
    run(&["get", object, "--out", destination.to_str().unwrap()]);
    assert_eq!(
        std::fs::read(destination.join("hello.txt")).unwrap(),
        b"standalone member roundtrip"
    );
    let config_path = root.path().join("state/rem/config.toml");
    let config = remanence_state::load_config(&config_path).unwrap();
    let mut index = remanence_state::CatalogIndex::open(&config.index.sqlite_path).unwrap();
    let tape = index.get_tape_by_voltag("HOMEL9").unwrap().unwrap();
    index
        .record_tape_io_fence(remanence_state::TapeIoFenceInput {
            tape_uuid: tape.tape_uuid.as_slice().try_into().unwrap(),
            barcode: Some("HOMEL9".into()),
            reason: "test_uncertain_write".into(),
            evidence_json: None,
        })
        .unwrap();
    drop(index);
    run(&["tape", "release"]);
    let index = remanence_state::CatalogIndex::open(&config.index.sqlite_path).unwrap();
    assert_eq!(
        index.list_active_tape_io_fences().unwrap().len(),
        1,
        "release must not clear quarantine"
    );
    drop(index);
    let health = run(&["doctor"]);
    assert_eq!(health["drives"][0]["ownership"], "standalone");
    assert_eq!(
        health["state_dir"],
        root.path().join("state/rem").to_str().unwrap()
    );
    let before_capture = world.lock().unwrap().command_log.len();
    let capture = root.path().join("diagnostics.tar");
    run(&["diagnostics", "capture", "--out", capture.to_str().unwrap()]);
    assert!(std::fs::metadata(&capture).unwrap().len() > 0);
    assert!(
        world.lock().unwrap().command_log[before_capture..]
            .iter()
            .all(|command| matches!(command.opcode, 0x12 | 0x1a | 0x4d)),
        "diagnostics must never read tape data or position tape"
    );
}

/// Exercise the real binary selector, including Clap's alternate help spelling.
#[test]
fn every_new_verb_routes_help_through_the_entry_point() {
    let verbs = [
        vec!["doctor"],
        vec!["drives"],
        vec!["diagnostics", "capture"],
        vec!["pool", "new"],
        vec!["tape", "identify"],
        vec!["tape", "init"],
        vec!["tape", "adopt"],
        vec!["tape", "verify"],
        vec!["tape", "restore"],
        vec!["tape", "finalize"],
        vec!["tape", "assign-pool"],
        vec!["tape", "rebind"],
        vec!["tape", "set-generation"],
        vec!["tape", "import-journals"],
        vec!["tape", "release"],
        vec!["put"],
        vec!["get"],
    ];
    for verb in verbs {
        for prefix in [false, true] {
            let mut args = vec![];
            if prefix {
                args.push("help");
            }
            args.extend_from_slice(&verb);
            if !prefix {
                args.push("--help");
            }
            let result = std::process::Command::new(env!("CARGO_BIN_EXE_rem"))
                .args(&args)
                .output()
                .unwrap();
            assert!(
                result.status.success(),
                "{args:?}: {}",
                String::from_utf8_lossy(&result.stderr)
            );
            let help = String::from_utf8(result.stdout).unwrap();
            assert!(help.contains("--allow-device"), "{args:?}: {help}");
            let library_option = match verb.as_slice() {
                ["put"] | ["get"] | ["tape", "finalize"] => Some("--endpoint"),
                ["tape", "init"] => Some("--library"),
                _ => None,
            };
            if let Some(option) = library_option {
                for expected in [
                    "Library/daemon interface:",
                    "Standalone device interface:",
                    option,
                    "--device",
                ] {
                    assert!(
                        help.contains(expected),
                        "{args:?}: missing {expected}: {help}"
                    );
                }
            }
            if verb == ["pool", "new"] || verb == ["tape", "init"] {
                for expected in [
                    "custom:128,4,1",
                    "1 MiB",
                    "512 MiB",
                    "256 KiB",
                    "--scheme default",
                ] {
                    assert!(
                        help.contains(expected),
                        "{args:?}: missing {expected}: {help}"
                    );
                }
            }
        }
    }
}

/// A literal operand named help must reach the library parser/runtime without
/// requiring a standalone device. No hardware or daemon is needed for refusal.
#[test]
fn literal_help_operands_keep_library_entry_point_routing() {
    let root = tempfile::tempdir().unwrap();
    for args in [
        vec!["put", "help", "--pool", "example"],
        vec!["get", "help", "--dest", "out"],
        vec!["tape", "init", "help", "--config", "missing.toml"],
        vec!["tape", "finalize", "--reason", "help"],
    ] {
        let result = std::process::Command::new(env!("CARGO_BIN_EXE_rem"))
            .current_dir(root.path())
            .args(&args)
            .output()
            .unwrap();
        assert!(
            !result.status.success(),
            "{args:?}: expected library refusal"
        );
        let output = format!(
            "{}{}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
        assert!(!output.contains("requires --device"), "{args:?}: {output}");
        assert!(
            !output.contains("Standalone device interface:"),
            "{args:?}: {output}"
        );
    }
}

/// Discovery supplies the serial for changers and installed library drives too.
#[test]
fn diagnostics_accepts_discovered_library_device_serials() {
    use remanence_chaos::model::{DeviceRole, ModelTransport, VirtualWorld};
    use std::sync::{Arc, Mutex};
    let world = Arc::new(Mutex::new(VirtualWorld::single_drive(
        "CAPTURE-CHANGER",
        0x100,
        "CAPTURE-DRIVE",
        0x400,
        0,
    )));
    for (device, serial) in [
        ("/dev/capture-changer", "CAPTURE-CHANGER"),
        ("/dev/capture-drive", "CAPTURE-DRIVE"),
    ] {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("capture.tar");
        let args = [
            "rem",
            "--json",
            "--allow-untrusted-state-volume",
            "--device",
            device,
            "--allow-device",
            serial,
            "diagnostics",
            "capture",
            "--out",
            target.to_str().unwrap(),
        ];
        let mut out = Vec::new();
        let mut errors = Vec::new();
        let code = remanence_cli::run_with_standalone_test_io(
            &args,
            root.path(),
            root.path(),
            &root.path().join("missing"),
            &root.path().join("locks"),
            ["/dev/capture-changer", "/dev/capture-drive"]
                .iter()
                .map(|p| remanence_library::DeviceAttachment {
                    sg_path: p.into(),
                    sysfs_path: format!("/sys{p}").into(),
                })
                .collect(),
            |path: &Path| {
                Ok(Box::new(ModelTransport::new(
                    Arc::clone(&world),
                    if path.ends_with("capture-changer") {
                        DeviceRole::Changer
                    } else {
                        DeviceRole::Drive { bay: 0x100 }
                    },
                ))
                    as Box<dyn remanence_library::SgTransport>)
            },
            &mut out,
            &mut errors,
        );
        assert_eq!(
            code,
            ExitCode::SUCCESS,
            "{} {}",
            String::from_utf8_lossy(&out),
            String::from_utf8_lossy(&errors)
        );
        assert!(std::fs::metadata(target).unwrap().len() > 0);
    }
}

/// An actual bootstrap uses the requested block-size geometry, and a failed
/// bootstrap leaves durable quarantine instead of usable initialization facts.
#[test]
fn init_geometry_and_failed_bootstrap_are_guarded() {
    use remanence_chaos::model::{DeviceRole, ModelTransport, VirtualTape, VirtualWorld};
    use std::sync::{Arc, Mutex};
    for fail in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let mut model = VirtualWorld::single_drive("UNEXPOSED", 0x100, "HOME-DRIVE", 0x400, 0);
        model.put_tape_in_drive(
            0x100,
            "HOMEL9",
            None,
            VirtualTape::empty(128 * 1024 * 1024, 262144),
        );
        let world = Arc::new(Mutex::new(model));
        let run = |extra: &[&str]| {
            let mut args = vec!["rem", "--json", "--allow-untrusted-state-volume"];
            args.extend_from_slice(extra);
            let mut out = Vec::new();
            let mut errors = Vec::new();
            let code = remanence_cli::run_with_standalone_test_io(
                &args,
                root.path(),
                &root.path().join("state"),
                &root.path().join("absent"),
                &root.path().join("locks"),
                vec![remanence_library::DeviceAttachment {
                    sg_path: "/dev/home-model".into(),
                    sysfs_path: "/sys/home-model".into(),
                }],
                |_: &Path| {
                    Ok(Box::new(RemovalTransport {
                        inner: ModelTransport::new(
                            Arc::clone(&world),
                            DeviceRole::Drive { bay: 0x100 },
                        ),
                        config: root.path().join("state/rem/config.toml"),
                        fail_writes: fail,
                    })
                        as Box<dyn remanence_library::SgTransport>)
                },
                &mut out,
                &mut errors,
            );
            let response: serde_json::Value = serde_json::from_slice(&out)
                .unwrap_or_else(|_| panic!("{}", String::from_utf8_lossy(&errors)));
            (code, response)
        };
        let (code, output) = run(&["pool", "new", "library-geometry", "--scheme", "default"]);
        assert_eq!(code, ExitCode::SUCCESS, "{output}");
        let init = [
            "tape",
            "init",
            "--device",
            "/dev/home-model",
            "--allow-device",
            "HOME-DRIVE",
            "--label",
            "HOMEL9",
            "--generation",
            "L9",
            "--pool",
            "library-geometry",
            "--block-size",
            "1048576",
        ];
        let (code, output) = run(&init);
        let config =
            remanence_state::load_config(root.path().join("state/rem/config.toml")).unwrap();
        let index = remanence_state::CatalogIndex::open(&config.index.sqlite_path).unwrap();
        if fail {
            assert_ne!(code, ExitCode::SUCCESS, "{output}");
            assert_eq!(index.list_active_tape_io_fences().unwrap().len(), 1);
            assert!(index.get_tape_by_voltag("HOMEL9").unwrap().is_none());
            drop(index);
            let before = world
                .lock()
                .unwrap()
                .command_log
                .iter()
                .filter(|c| c.opcode == 0x0a)
                .count();
            let (code, output) = run(&init);
            assert_ne!(code, ExitCode::SUCCESS, "{output}");
            assert_eq!(
                world
                    .lock()
                    .unwrap()
                    .command_log
                    .iter()
                    .filter(|c| c.opcode == 0x0a)
                    .count(),
                before
            );
        } else {
            assert_eq!(code, ExitCode::SUCCESS, "{output}");
            let tape = index.get_tape_by_voltag("HOMEL9").unwrap().unwrap();
            assert_eq!(tape.block_size, Some(1048576));
            assert_eq!(tape.stripes_per_neighborhood, Some(128));
        }
    }
}

/// Informational skips remain complete and readable in both CLI output modes.
#[test]
fn skipped_disk_is_informational_without_any_tape_endpoints() {
    for json in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let mut args = vec!["rem", "--allow-untrusted-state-volume", "drives"];
        if json {
            args.push("--json");
        }
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code = remanence_cli::run_with_standalone_test_enumeration(
            &args,
            temp.path(),
            &temp.path().join("state"),
            &temp.path().join("absent"),
            &temp.path().join("locks"),
            remanence_library::sysfs::DeviceEnumeration {
                devices: vec![],
                warnings: vec![remanence_library::DiscoveryWarning::NotTapeOrChanger {
                    path: "/dev/sg2".into(),
                    peripheral_type: 0,
                }],
            },
            |_: &Path| panic!("disk must never be opened"),
            &mut stdout,
            &mut stderr,
        );
        assert_eq!(
            code,
            ExitCode::SUCCESS,
            "{}",
            String::from_utf8_lossy(&stderr)
        );
        assert!(String::from_utf8_lossy(&stderr).contains("skipped /dev/sg2: disk (type 0)"));
        if json {
            let response: serde_json::Value = serde_json::from_slice(&stdout).unwrap();
            assert_eq!(response["result"]["complete"], true, "{response}");
            assert_eq!(
                response["result"]["warnings"],
                serde_json::json!(["skipped /dev/sg2: disk (type 0)"])
            );
        }
    }
}

/// Persistent virtual media with fresh production dispatch and state locks per invocation.
struct VersionFixture {
    fail_after_path: std::sync::Mutex<Option<String>>,
    root: tempfile::TempDir,
    world: std::sync::Arc<std::sync::Mutex<remanence_chaos::model::VirtualWorld>>,
}
impl VersionFixture {
    fn new() -> Self {
        Self::with_parity(true)
    }

    fn with_parity(parity: bool) -> Self {
        let fixture = Self {
            fail_after_path: std::sync::Mutex::new(None),
            root: tempfile::tempdir().unwrap(),
            world: std::sync::Arc::new(std::sync::Mutex::new(
                remanence_chaos::model::VirtualWorld::single_drive(
                    "UNEXPOSED",
                    0x100,
                    "HOME-DRIVE",
                    0x400,
                    0,
                ),
            )),
        };
        fixture.new_tape("FIRST");
        if !parity {
            fixture.ok(&["pool", "new", "unused", "--scheme", "none"]);
            let text = std::fs::read_to_string(fixture.config())
                .unwrap()
                .replace("custom:128,4,1", "none");
            std::fs::write(fixture.config(), text).unwrap();
        }
        fixture.ok(&["tape", "init", "--label", "FIRST", "--generation", "L9"]);
        fixture
    }
    fn new_tape(&self, label: &str) {
        self.world.lock().unwrap().put_tape_in_drive(
            0x100,
            label,
            None,
            remanence_chaos::model::VirtualTape::empty(256 * 1024 * 1024, 262144),
        );
    }
    fn config(&self) -> std::path::PathBuf {
        self.root.path().join("state/rem/config.toml")
    }
    fn index(&self) -> remanence_state::CatalogIndex {
        let config = remanence_state::load_config(self.config()).unwrap();
        remanence_state::CatalogIndex::open(config.index.sqlite_path).unwrap()
    }
    fn writes(&self) -> usize {
        self.world
            .lock()
            .unwrap()
            .command_log
            .iter()
            .filter(|c| matches!(c.opcode, 0x0a | 0x10))
            .count()
    }
    fn run(&self, extra: &[&str], json: bool) -> (ExitCode, String, String) {
        self.run_with_mutation(extra, json, None)
    }
    fn run_with_mutation(
        &self,
        extra: &[&str],
        json: bool,
        mutation: Option<(std::path::PathBuf, Vec<u8>, bool)>,
    ) -> (ExitCode, String, String) {
        let mut args = vec!["rem", "--allow-untrusted-state-volume"];
        if json {
            args.push("--json");
        }
        if extra[0] != "pool" {
            args.extend([
                "--device",
                "/dev/home-model",
                "--allow-device",
                "HOME-DRIVE",
            ]);
        }
        args.extend_from_slice(extra);
        let mut out = Vec::new();
        let mut errors = Vec::new();
        let mutation = std::sync::Arc::new(std::sync::Mutex::new(mutation));
        let code = remanence_cli::run_with_standalone_test_io(
            &args,
            self.root.path(),
            &self.root.path().join("state"),
            &self.root.path().join("absent"),
            &self.root.path().join("locks"),
            vec![remanence_library::DeviceAttachment {
                sg_path: "/dev/home-model".into(),
                sysfs_path: "/sys/home-model".into(),
            }],
            |_: &Path| {
                Ok(Box::new(VersionMutationTransport {
                    fail_after_path: self.fail_after_path.lock().unwrap().clone(),
                    inner: RemovalTransport {
                        inner: remanence_chaos::model::ModelTransport::new(
                            std::sync::Arc::clone(&self.world),
                            remanence_chaos::model::DeviceRole::Drive { bay: 0x100 },
                        ),
                        config: self.config(),
                        fail_writes: false,
                    },
                    mutation: std::sync::Arc::clone(&mutation),
                })
                    as Box<dyn remanence_library::SgTransport>)
            },
            &mut out,
            &mut errors,
        );
        (
            code,
            String::from_utf8(out).unwrap(),
            String::from_utf8(errors).unwrap(),
        )
    }
    fn envelope(text: &str) -> serde_json::Value {
        let lines: Vec<serde_json::Value> = text
            .lines()
            .map(|l| serde_json::from_str(l).expect("each stdout line is JSON"))
            .collect();
        let envelope = lines.last().expect("final envelope").clone();
        if let Some(receipts) = envelope["result"]["receipts"]
            .as_array()
            .filter(|_| envelope["result"]["summary"].is_object())
        {
            assert_eq!(&lines[..receipts.len()], receipts);
            assert_eq!(lines[receipts.len()], envelope["result"]["summary"]);
        }
        envelope
    }
    fn ok(&self, args: &[&str]) -> serde_json::Value {
        let (code, out, errors) = self.run(args, true);
        assert_eq!(code, ExitCode::SUCCESS, "{args:?}\n{out}\n{errors}");
        Self::envelope(&out)["result"].clone()
    }
}

/// Change a source after version preparation but before the admitted writer opens it.
type SourceMutation = (std::path::PathBuf, Vec<u8>, bool);

struct VersionMutationTransport {
    fail_after_path: Option<String>,
    inner: RemovalTransport,
    mutation: std::sync::Arc<std::sync::Mutex<Option<SourceMutation>>>,
}
impl remanence_library::SgTransport for VersionMutationTransport {
    fn execute_in(
        &mut self,
        cdb: &[u8],
        buf: &mut [u8],
    ) -> Result<remanence_library::transport::TransferOutcome, remanence_library::ScsiError> {
        self.inner.execute_in(cdb, buf)
    }
    fn execute_out(
        &mut self,
        cdb: &[u8],
        buf: &[u8],
    ) -> Result<remanence_library::transport::TransferOutcome, remanence_library::ScsiError> {
        if cdb[0] == 0x0a {
            if let Some(path) = &self.fail_after_path {
                let config = remanence_state::load_config(&self.inner.config).unwrap();
                let index = remanence_state::CatalogIndex::open(config.index.sqlite_path).unwrap();
                if !index
                    .list_committed_versions_by_path("default", path)
                    .unwrap()
                    .is_empty()
                {
                    self.inner.fail_writes = true;
                }
            }
        }
        self.inner.execute_out(cdb, buf)
    }
    fn execute_none(&mut self, cdb: &[u8]) -> Result<(), remanence_library::ScsiError> {
        if cdb[0] == 0x00 && self.mutation.lock().unwrap().is_some() {
            let config = remanence_state::load_config(&self.inner.config).unwrap();
            let index = remanence_state::CatalogIndex::open(config.index.sqlite_path).unwrap();
            if index
                .non_terminal_sessions()
                .unwrap()
                .iter()
                .any(|s| s.session_kind == "write")
            {
                let (path, bytes, replace) = self.mutation.lock().unwrap().take().unwrap();
                if replace {
                    std::fs::remove_file(&path).unwrap();
                }
                std::fs::write(path, bytes).unwrap();
            }
        }
        self.inner.execute_none(cdb)
    }
    fn set_timeout_for(&mut self, class: remanence_library::TimeoutClass) {
        self.inner.set_timeout_for(class);
    }
}

#[test]
fn put_versions_are_pool_scoped_and_get_is_loaded_tape_scoped() {
    let f = VersionFixture::new();
    let directory = f.root.path().join("photos");
    std::fs::create_dir(&directory).unwrap();
    let source = directory.join("a.txt");
    std::fs::write(&source, b"A").unwrap();
    std::fs::write(directory.join("b.txt"), b"unchanged").unwrap();
    std::os::unix::fs::symlink(&source, directory.join("link")).unwrap();
    let args = ["put", directory.to_str().unwrap()];
    let first = f.ok(&args);
    assert_eq!(first["summary"]["written"], 2);
    assert_eq!(first["summary"]["skipped"], 1);
    let original = first["receipts"][0]["object_id"].as_str().unwrap();
    let before = f.writes();
    let (code, human, errors) = f.run(&args, false);
    assert_eq!(code, ExitCode::SUCCESS, "{human}\n{errors}");
    println!("HUMAN PUT TRANSCRIPT\n{human}");
    assert!(human.contains("Total: 0 written, 2 already_stored, 1 skipped"));
    assert_eq!(f.writes(), before);
    std::fs::write(&source, b"B").unwrap();
    let changed = f.ok(&args);
    assert_eq!(changed["summary"]["written"], 1);
    assert_eq!(changed["summary"]["already_stored"], 1);
    assert_ne!(changed["receipts"][0]["object_id"], original);
    let out = f.root.path().join("newest");
    f.ok(&["get", "photos/a.txt", "--out", out.to_str().unwrap()]);
    assert_eq!(std::fs::read(out.join("photos/a.txt")).unwrap(), b"B");
    let out = f.root.path().join("older");
    f.ok(&[
        "get",
        "photos/a.txt",
        "--version",
        original,
        "--out",
        out.to_str().unwrap(),
    ]);
    assert_eq!(std::fs::read(out.join("photos/a.txt")).unwrap(), b"A");
    std::fs::write(&source, b"A").unwrap();
    let third = f.ok(&args);
    assert_eq!(third["summary"]["written"], 1);
    assert_ne!(third["receipts"][0]["object_id"], original);
    assert_eq!(
        f.index()
            .list_committed_versions_by_path("default", "photos/a.txt")
            .unwrap()
            .len(),
        3
    );
    let out = f.root.path().join("third");
    f.ok(&["get", "photos/a.txt", "--out", out.to_str().unwrap()]);
    assert_eq!(std::fs::read(out.join("photos/a.txt")).unwrap(), b"A");
    f.new_tape("SECOND");
    f.ok(&["tape", "init", "--label", "SECOND", "--generation", "L9"]);
    let before = f.writes();
    let (code, json, errors) = f.run(&args, true);
    assert_eq!(code, ExitCode::SUCCESS, "{json}\n{errors}");
    println!("JSON PUT TRANSCRIPT\n{json}");
    let skipped = VersionFixture::envelope(&json);
    assert_eq!(skipped["result"]["receipts"][0]["label"], "FIRST");
    assert_eq!(skipped["result"]["summary"]["written"], 0);
    assert_eq!(f.writes(), before);
    let (code, refusal, _) = f.run(
        &["get", "photos/a.txt", "--out", out.to_str().unwrap()],
        true,
    );
    assert_ne!(code, ExitCode::SUCCESS);
    let refusal = VersionFixture::envelope(&refusal);
    assert!(refusal["error"]
        .as_str()
        .unwrap()
        .contains("is not on this tape; it is on FIRST ("));
    let stored_id = skipped["result"]["receipts"][0]["object_id"]
        .as_str()
        .unwrap();
    let (code, refusal, _) = f.run(&["get", stored_id, "--out", out.to_str().unwrap()], true);
    assert_ne!(code, ExitCode::SUCCESS);
    assert!(VersionFixture::envelope(&refusal)["error"]
        .as_str()
        .unwrap()
        .contains("is not on this tape; it is on FIRST ("));
    let (code, refusal, _) = f.run(&["put", source.to_str().unwrap(), "--tape", "FIRST"], true);
    assert!(
        refusal.contains("requested tape FIRST differs from loaded tape SECOND"),
        "{refusal}"
    );
    assert!(VersionFixture::envelope(&refusal)["result"]["summary"]["failed"].is_object());
    assert_ne!(code, ExitCode::SUCCESS);
    assert_eq!(f.writes(), before);
    f.ok(&["pool", "new", "offsite"]);
    f.ok(&["tape", "assign-pool", "--pool", "offsite"]);
    let second_pool = f.ok(&args);
    assert_eq!(second_pool["summary"]["written"], 2);
    assert_eq!(second_pool["receipts"][0]["label"], "SECOND");
}

#[test]
fn put_partial_failure_preserves_receipts_and_summary() {
    let f = VersionFixture::new();
    let source = f.root.path().join("first.txt");
    std::fs::write(&source, b"committed").unwrap();
    let missing = f.root.path().join("missing.txt");
    let (code, output, _) = f.run(
        &["put", source.to_str().unwrap(), missing.to_str().unwrap()],
        true,
    );
    assert_ne!(code, ExitCode::SUCCESS);
    let envelope = VersionFixture::envelope(&output);
    assert_eq!(envelope["ok"], false);
    assert_eq!(envelope["result"]["receipts"][0]["event"], "written");
    assert_eq!(envelope["result"]["summary"]["written"], 1);
    assert_eq!(
        envelope["result"]["summary"]["failed"]["path"],
        "missing.txt"
    );
    assert!(envelope["result"]["summary"]["next"].is_null());
    assert_eq!(
        f.index()
            .list_committed_versions_by_path("default", "first.txt")
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn prepared_source_mutations_never_report_false_success() {
    for (bytes, replace) in [
        (b"BBBB".to_vec(), false),
        (b"growth".to_vec(), false),
        (b"x".to_vec(), false),
        (b"CCCC".to_vec(), true),
        (vec![b'B'; 2 * 1024 * 1024], false),
    ] {
        let f = VersionFixture::new();
        let source = f.root.path().join("file.txt");
        std::fs::write(
            &source,
            vec![b'A'; if bytes.len() > 1024 { bytes.len() } else { 4 }],
        )
        .unwrap();
        let (code, output, _) = f.run_with_mutation(
            &["put", source.to_str().unwrap()],
            true,
            Some((source.clone(), bytes.clone(), replace)),
        );
        assert_eq!(
            std::fs::read(&source).unwrap(),
            bytes,
            "mutation hook must run: {output}"
        );
        assert_ne!(code, ExitCode::SUCCESS, "{output}");
        if bytes.len() > 1024 {
            assert!(
                !f.index().list_active_tape_io_fences().unwrap().is_empty(),
                "streamed mutation must fence"
            );
        }
        let envelope = VersionFixture::envelope(&output);
        assert_eq!(envelope["result"]["summary"]["written"], 0);
        assert_eq!(envelope["result"]["summary"]["failed"]["path"], "file.txt");
        assert!(f
            .index()
            .list_committed_versions_by_path("default", "file.txt")
            .unwrap()
            .is_empty());
    }
}

#[test]
fn checkpoints_restore_versions_after_projection_loss_and_full_rebuild() {
    for (encrypted, parity) in [(false, true), (true, true), (false, false), (true, false)] {
        let f = VersionFixture::with_parity(parity);
        let source = f.root.path().join("file.txt");
        std::fs::write(&source, b"checkpoint authority").unwrap();
        let primary =
            remanence_aead::RecipientPrivateKey::new([0x31; 16], "primary", [0x51; 32]).unwrap();
        let recovery =
            remanence_aead::RecipientPrivateKey::new([0x32; 16], "recovery", [0x52; 32]).unwrap();
        let recipient = f.root.path().join("primary.remr");
        let recovery_path = f.root.path().join("recovery.remr");
        std::fs::write(
            &recipient,
            primary.public_key(0).unwrap().serialize().unwrap(),
        )
        .unwrap();
        std::fs::write(
            &recovery_path,
            recovery.public_key(1).unwrap().serialize().unwrap(),
        )
        .unwrap();
        let mut args = vec!["put", source.to_str().unwrap()];
        if encrypted {
            args.extend([
                "--recipient",
                recipient.to_str().unwrap(),
                "--recipient",
                recovery_path.to_str().unwrap(),
            ]);
        }
        let written = f.ok(&args);
        let id = written["receipts"][0]["object_id"].as_str().unwrap();
        let expected = f.index().get_native_object(id).unwrap().unwrap();
        let timestamp = time::OffsetDateTime::parse(
            &expected.created_at_utc,
            &time::format_description::well_known::Rfc3339,
        )
        .unwrap();
        assert_eq!(timestamp.offset(), time::UtcOffset::UTC);
        assert_eq!(
            timestamp
                .format(&time::format_description::well_known::Rfc3339)
                .unwrap(),
            expected.created_at_utc
        );
        let files = f.index().list_native_object_files(id).unwrap();
        let versions = f
            .index()
            .list_committed_versions_by_path("default", "file.txt")
            .unwrap();
        assert!(expected
            .caller_object_id
            .as_deref()
            .unwrap()
            .starts_with("put:"));
        // Preserve journal authority but remove the SQLite object projection.
        let text = std::fs::read_to_string(f.config())
            .unwrap()
            .replace("[journal]\n", "[journal]\nrequire_trusted_volume = false\n");
        std::fs::write(f.config(), text).unwrap();
        {
            let mut state =
                remanence_state::StateHandle::open_from_config_file(f.config()).unwrap();
            state.rebuild_index_from_journals().unwrap();
        }
        assert!(f.index().get_native_object(id).unwrap().is_none());
        let before = f.writes();
        for _ in 0..2 {
            let replayed = f.ok(&args);
            assert_eq!(replayed["summary"]["written"], 0);
            assert_eq!(replayed["receipts"][0]["object_id"], id);
            assert_eq!(f.writes(), before);
        }
        assert_eq!(f.index().get_native_object(id).unwrap().unwrap(), expected);
        assert_eq!(f.index().list_native_object_files(id).unwrap(), files);
        // Exercise the public rebuild command, including parity journal replay.
        let rebuilt = std::process::Command::new(env!("CARGO_BIN_EXE_rem"))
            .args([
                "rebuild-catalog-from-journals",
                "--config",
                f.config().to_str().unwrap(),
            ])
            .output()
            .unwrap();
        assert!(
            rebuilt.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&rebuilt.stdout),
            String::from_utf8_lossy(&rebuilt.stderr)
        );
        assert_eq!(f.index().get_native_object(id).unwrap().unwrap(), expected);
        assert_eq!(f.index().list_native_object_files(id).unwrap(), files);
        assert_eq!(
            f.index()
                .list_committed_versions_by_path("default", "file.txt")
                .unwrap(),
            versions
        );
    }
}

#[test]
fn media_failure_after_first_commit_keeps_the_written_receipt() {
    let f = VersionFixture::new();
    let first = f.root.path().join("first.txt");
    let second = f.root.path().join("second.txt");
    std::fs::write(&first, b"committed").unwrap();
    std::fs::write(&second, b"fails on media").unwrap();
    *f.fail_after_path.lock().unwrap() = Some("first.txt".into());
    let (code, output, _) = f.run(
        &["put", first.to_str().unwrap(), second.to_str().unwrap()],
        true,
    );
    assert_ne!(code, ExitCode::SUCCESS, "{output}");
    let envelope = VersionFixture::envelope(&output);
    assert_eq!(envelope["result"]["receipts"].as_array().unwrap().len(), 1);
    assert_eq!(envelope["result"]["receipts"][0]["event"], "written");
    assert_eq!(envelope["result"]["summary"]["written"], 1);
    assert_eq!(
        envelope["result"]["summary"]["failed"]["path"],
        "second.txt"
    );
    assert!(f
        .index()
        .list_committed_versions_by_path("default", "second.txt")
        .unwrap()
        .is_empty());
    *f.fail_after_path.lock().unwrap() = None;
    let before = f.writes();
    let (code, output, _) = f.run(&["put", first.to_str().unwrap()], true);
    assert_ne!(
        code,
        ExitCode::SUCCESS,
        "all-skip invocation must report active fence"
    );
    assert!(
        VersionFixture::envelope(&output)["result"]["summary"]["failed"]["error"]
            .as_str()
            .unwrap()
            .contains("fence")
    );
    assert_eq!(f.writes(), before);
}

/// Representation changes create versions even when the plaintext is unchanged.
#[test]
fn representation_changes_are_versions_and_get_selectors_normalize() {
    let f = VersionFixture::new();
    let source = f.root.path().join("file.txt");
    std::fs::write(&source, b"same bytes").unwrap();
    assert_eq!(
        f.ok(&["put", source.to_str().unwrap()])["receipts"][0]["reason"],
        "new"
    );
    for round in [1u8, 2] {
        let mut paths = Vec::new();
        for slot in [0u8, 1] {
            let key = remanence_aead::RecipientPrivateKey::new(
                [round * 10 + slot; 16],
                "recipient",
                [round * 10 + slot; 32],
            )
            .unwrap();
            let path = f.root.path().join(format!("key{slot}.remr"));
            std::fs::write(&path, key.public_key(slot).unwrap().serialize().unwrap()).unwrap();
            paths.push(path);
        }
        let args = [
            "put",
            source.to_str().unwrap(),
            "--recipient",
            paths[0].to_str().unwrap(),
            "--recipient",
            paths[1].to_str().unwrap(),
        ];
        let result = f.ok(&args);
        assert_eq!(result["receipts"][0]["reason"], "representation_changed");
        assert_eq!(f.ok(&args)["receipts"][0]["event"], "already_stored");
    }
    let versions = f
        .index()
        .list_committed_versions_by_path("default", "file.txt")
        .unwrap();
    assert_eq!(versions.len(), 3);
    for (n, selector) in ["file.txt", "./file.txt", "/file.txt"].iter().enumerate() {
        let out = f.root.path().join(format!("out{n}"));
        f.ok(&[
            "get",
            selector,
            "--version",
            &versions[2].object_id,
            "--out",
            out.to_str().unwrap(),
        ]);
        assert_eq!(std::fs::read(out.join("file.txt")).unwrap(), b"same bytes");
    }
}

/// Special files are receipts, and never opened as payload sources.
#[test]
fn fifo_is_skipped_without_opening_it() {
    let f = VersionFixture::new();
    let path = f.root.path().join("pipe");
    assert!(std::process::Command::new("mkfifo")
        .arg(&path)
        .status()
        .unwrap()
        .success());
    let result = f.ok(&["put", path.to_str().unwrap()]);
    assert_eq!(result["receipts"][0]["event"], "skipped_unsupported");
    assert_eq!(result["receipts"][0]["kind"], "fifo");
}

#[test]
fn socket_is_skipped_without_opening_it() {
    let f = VersionFixture::new();
    let path = f.root.path().join("socket");
    let _socket = std::os::unix::net::UnixListener::bind(&path)
        .expect("socket fixture requires local bind permission");
    let result = f.ok(&["put", path.to_str().unwrap()]);
    assert_eq!(result["receipts"][0]["event"], "skipped_unsupported");
    assert_eq!(result["receipts"][0]["kind"], "socket");
}

/// Legacy checkpoints may have a path caller id without member-file projection.
#[test]
fn legacy_caller_path_fallback_stays_on_loaded_tape() {
    let f = VersionFixture::with_parity(false);
    let source = f.root.path().join("legacy.txt");
    std::fs::write(&source, b"legacy").unwrap();
    let written = f.ok(&["put", source.to_str().unwrap()]);
    let tape = uuid::Uuid::parse_str(written["tape"].as_str().unwrap()).unwrap();
    let config = remanence_state::load_config(f.config()).unwrap();
    let paths = remanence_state::StatePaths::from_config(f.config(), &config);
    let directory = paths.journal_dir.join("checkpoints");
    let journal =
        remanence_state::FileCheckpointJournal::open(&directory, *tape.as_bytes()).unwrap();
    let mut records = journal.replay().unwrap();
    drop(journal);
    assert_eq!(records.len(), 1);
    records[0].objects[0].object.caller_object_id = Some("legacy.txt".into());
    records[0].objects[0].files.clear();
    std::fs::remove_dir_all(&directory).unwrap();
    let journal =
        remanence_state::FileCheckpointJournal::open(&directory, *tape.as_bytes()).unwrap();
    journal.append(&records[0]).unwrap();
    drop(journal);
    let text = std::fs::read_to_string(f.config())
        .unwrap()
        .replace("[journal]\n", "[journal]\nrequire_trusted_volume = false\n");
    std::fs::write(f.config(), text).unwrap();
    // Construct the older projection in a fresh catalog so no current member
    // rows survive to contradict the deliberately member-less checkpoint.
    std::fs::remove_file(&config.index.sqlite_path).unwrap();
    {
        let mut state = remanence_state::StateHandle::open_from_config_file(f.config()).unwrap();
        state
            .catalog_index()
            .provision_tape(remanence_state::ProvisionTapeInput {
                tape_uuid: *tape.as_bytes(),
                voltag: "FIRST".into(),
                block_size: 262144,
                parity: remanence_parity::ParityConfig::None,
                force: false,
            })
            .unwrap();
        state
            .catalog_index()
            .project_tape_pool_membership(*tape.as_bytes(), "default")
            .unwrap();
        state
            .catalog_index()
            .project_checkpoint_record(&records[0])
            .unwrap();
    }
    let out = f.root.path().join("restored");
    f.ok(&["get", "legacy.txt", "--out", out.to_str().unwrap()]);
    assert_eq!(std::fs::read(out.join("legacy.txt")).unwrap(), b"legacy");
    assert!(f
        .index()
        .list_committed_versions_by_path_any_pool("legacy.txt")
        .unwrap()
        .is_empty());
    f.new_tape("SECOND");
    f.ok(&["tape", "init", "--label", "SECOND", "--generation", "L9"]);
    let (code, output, _) = f.run(&["get", "legacy.txt", "--out", out.to_str().unwrap()], true);
    assert_ne!(code, ExitCode::SUCCESS);
    assert!(VersionFixture::envelope(&output)["error"]
        .as_str()
        .unwrap()
        .contains("is not on this tape; it is on FIRST ("));
}
