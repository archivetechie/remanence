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
        let response: serde_json::Value = serde_json::from_slice(&stdout).unwrap_or_else(|_| {
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
        let response: serde_json::Value = serde_json::from_slice(&out)
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
