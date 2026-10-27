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
    check_all_writes: bool,
    writes_observed: u64,
}
impl RemovalTransport {
    /// Inspect persisted evidence at the dispatch boundary, before model IO.
    fn assert_write_guard(&mut self, opcode: u8) {
        // Finalization writes 2 GiB of separation records. Replaying the complete
        // audit log before every zero block makes this observer quadratic.
        // Inspect every 256th media write and every filemark boundary;
        // production append admission remains active for every session.
        if opcode == 0x0a {
            self.writes_observed += 1;
        }
        if opcode == 0x0a && self.writes_observed % 256 != 1 && !self.check_all_writes {
            return;
        }
        if !matches!(opcode, 0x0a | 0x10) {
            return;
        }
        let config = remanence_state::load_config(&self.config).unwrap();
        let paths = remanence_state::StatePaths::from_config(&self.config, &config);
        let index =
            remanence_state::CatalogIndex::open_read_only(&config.index.sqlite_path).unwrap();
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
                    check_all_writes: true,
                    writes_observed: 0,
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
    let (code, health) = run_result(&["doctor"]);
    assert_ne!(code, ExitCode::SUCCESS);
    assert_eq!(health["ok"], false);
    assert!(health["error"].as_str().unwrap().starts_with("not ready: "));
    assert!(health["result"]["checks"].is_array());
    let health = &health["result"];
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
    let home = tempfile::tempdir().unwrap();
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
                .env("REM_TEST_NO_LIVE_SCSI", "1")
                .env("HOME", home.path())
                .env("XDG_STATE_HOME", home.path())
                .env(
                    "REM_TEST_SYSTEM_CONFIG",
                    home.path().join("absent-system.toml"),
                )
                .args(&args)
                .output()
                .unwrap();
            assert!(
                result.status.success(),
                "{args:?}: {}",
                String::from_utf8_lossy(&result.stderr)
            );
            let help = String::from_utf8(result.stdout).unwrap();
            let library_option = match verb.as_slice() {
                ["put"] | ["get"] | ["tape", "finalize"] => Some("--endpoint"),
                ["tape", "init"] => Some("--library"),
                _ => None,
            };
            if let Some(option) = library_option {
                assert!(help.contains(option), "{args:?}: {help}");
                assert!(
                    help.trim_end().ends_with("For a standalone drive: rem setup, or pass --device PATH --allow-device SERIAL."),
                    "library help follows library routing"
                );
            } else {
                assert!(help.contains("--allow-device"), "{args:?}: {help}");
            }
            if verb == ["pool", "new"] {
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
            .env("REM_TEST_NO_LIVE_SCSI", "1")
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
                        check_all_writes: true,
                        writes_observed: 0,
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
    interrupt_finalize: bool,
    home_default: bool,
    terminal_input: bool,
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
            interrupt_finalize: false,
            home_default: false,
            terminal_input: false,
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
    /// Use the same initialized tape, changer, and production actor setup for both transports.
    fn daemon_state(&self, mode: remanence_state::AppendStagingMode) -> remanence_api::ApiState {
        let f = self;
        let mut config = remanence_state::load_config(f.config()).unwrap();
        // The daemon owns a real changer slot for mount and shutdown dismount.
        {
            let mut world = f.world.lock().unwrap();
            let tape = world.tapes.remove("FIRST").unwrap();
            *world = remanence_chaos::model::VirtualWorld::single_drive(
                "UNEXPOSED",
                0x100,
                "HOME-DRIVE",
                0x400,
                1,
            );
            world.put_tape_in_drive(0x100, "FIRST", Some(0x400), tape);
        }
        let library = f.world.lock().unwrap().library_snapshot();
        config.libraries = vec![remanence_state::LibraryConfig {
            serial: library.serial.clone(),
            allow_derived_drive_identity: false,
        }];
        config.standalone_drives.clear();
        config.daemon.append_staging_mode = mode;
        config.daemon.append_ring_bytes = 1024 * 1024;
        config.daemon.drive_idle_unload_seconds = 0;
        let spool = f.root.path().join("spool");
        std::fs::create_dir_all(&spool).unwrap();
        let world = f.world.clone();
        let state = remanence_api::ApiState::with_drive_pool_opener(
            f.index(),
            &config,
            remanence_library::DiscoveryReport {
                libraries: vec![library.clone()],
                unattached_drives: vec![],
                warnings: vec![],
                complete: true,
                incomplete_evidence: vec![],
            },
            remanence_library::StaticAllowlist::new([library.serial.as_str()]),
            spool,
            16 * 1024 * 1024,
            move |library, policy| {
                let world = world.clone();
                library.open_with(policy, move |path| {
                    let role = world
                        .lock()
                        .unwrap()
                        .role_for_path(path)
                        .expect("model device");
                    Ok(Box::new(remanence_chaos::model::ModelTransport::new(
                        world.clone(),
                        role,
                    )))
                })
            },
        )
        .unwrap();
        state
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
        if !self.home_default
            && !matches!(extra[0], "pool" | "list" | "setup")
            && !extra.starts_with(&["tape", "quarantine"])
        {
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
        let code = remanence_cli::run_with_standalone_test_input(
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
                    interrupt_write: self.interrupt_finalize,
                    fail_after_path: self.fail_after_path.lock().unwrap().clone(),
                    inner: RemovalTransport {
                        inner: remanence_chaos::model::ModelTransport::new(
                            std::sync::Arc::clone(&self.world),
                            remanence_chaos::model::DeviceRole::Drive { bay: 0x100 },
                        ),
                        config: self.config(),
                        fail_writes: false,
                        check_all_writes: !extra.starts_with(&["tape", "finalize"]),
                        writes_observed: 0,
                    },
                    mutation: std::sync::Arc::clone(&mutation),
                })
                    as Box<dyn remanence_library::SgTransport>)
            },
            &mut out,
            &mut errors,
            self.terminal_input,
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
    interrupt_write: bool,
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
        assert!(
            !(self.interrupt_write && cdb[0] == 0x0a),
            "injected interruption before finalization media write"
        );
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
            .env("REM_TEST_NO_LIVE_SCSI", "1")
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
    let fences = f.index().list_active_tape_io_fences().unwrap();
    assert!(
        fences.len() >= 2,
        "exercise shared and mount-level fences: {fences:?}"
    );
    let listed = f.ok(&["tape", "quarantine", "list"]);
    for fence in fences {
        assert_eq!(
            fence.endpoint,
            Some(remanence_library::DriveKey::Standalone {
                drive_serial: "HOME-DRIVE".into(),
            })
        );
        assert!(
            listed["active_fences"]
                .as_array()
                .unwrap()
                .iter()
                .any(|entry| {
                    entry["quarantine_id"] == fence.quarantine_id
                        && !entry["endpoint_key"].is_null()
                }),
            "{listed}"
        );
        f.ok(&[
            "tape",
            "quarantine",
            "release",
            &fence.quarantine_id,
            "--ack",
            "inspected",
        ]);
    }
    assert!(f.index().list_active_tape_io_fences().unwrap().is_empty());
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

/// Confirmation failures never reach a bootstrap write on a used cartridge.
#[test]
fn used_cartridge_requires_exact_clobber_confirmation() {
    use remanence_chaos::model::{Record, VirtualTape};
    for (options, succeeds, message) in [
        (vec![], false, "this cartridge holds LTFS data"),
        (
            vec!["--clobber-data"],
            false,
            "non-interactive clobber requires",
        ),
        (
            vec!["--clobber-data", "--confirm-clobber", "WRONG"],
            false,
            "did not match",
        ),
        (
            vec!["--clobber-data", "--confirm-clobber", "CLOBBER USED"],
            true,
            "",
        ),
    ] {
        let f = VersionFixture::new();
        let mut tape = VirtualTape::empty(256 * 1024 * 1024, 262144);
        tape.records = vec![
            Record::Block(b"<ltfslabel><creator>fixture</creator></ltfslabel>".to_vec()),
            Record::Filemark,
        ];
        f.world
            .lock()
            .unwrap()
            .put_tape_in_drive(0x100, "USED", None, tape);
        let before = f.writes();
        let mut args = vec!["tape", "init", "--label", "USED", "--generation", "L9"];
        args.extend(options);
        let (code, out, errors) = f.run(&args, true);
        assert_eq!(code == ExitCode::SUCCESS, succeeds, "{out}\n{errors}");
        if succeeds {
            assert!(errors.is_empty(), "{errors}");
            assert!(f.writes() > before);
            assert!(f.index().get_tape_by_voltag("USED").unwrap().is_some());
        } else {
            assert!(out.contains(message), "{out}");
            assert_eq!(f.writes(), before);
        }
    }
}

/// Encrypted newest versions require an explicit downgrade; other files continue.
#[test]
fn plaintext_downgrade_is_per_file_and_requires_override() {
    let f = VersionFixture::new();
    let source = f.root.path().join("secret.txt");
    let other = f.root.path().join("other.txt");
    std::fs::write(&source, b"secret").unwrap();
    std::fs::write(&other, b"ordinary").unwrap();
    let key = remanence_aead::RecipientPrivateKey::new([7; 16], "test", [7; 32]).unwrap();
    let recipient = f.root.path().join("test.remr");
    std::fs::write(&recipient, key.public_key(0).unwrap().serialize().unwrap()).unwrap();
    let recovery = f.root.path().join("recovery.remr");
    let recovery_key =
        remanence_aead::RecipientPrivateKey::new([8; 16], "recovery", [8; 32]).unwrap();
    std::fs::write(
        &recovery,
        recovery_key.public_key(1).unwrap().serialize().unwrap(),
    )
    .unwrap();
    let encrypted = f.ok(&[
        "put",
        source.to_str().unwrap(),
        "--recipient",
        recipient.to_str().unwrap(),
        "--recipient",
        recovery.to_str().unwrap(),
    ]);
    let (code, out, _) = f.run(
        &["put", source.to_str().unwrap(), other.to_str().unwrap()],
        true,
    );
    assert_ne!(code, ExitCode::SUCCESS);
    let result = VersionFixture::envelope(&out)["result"].clone();
    assert_eq!(result["receipts"][0]["event"], "refused");
    assert_eq!(
        result["receipts"][0]["reason"],
        "would_store_plaintext_copy_of_encrypted"
    );
    assert_eq!(
        result["receipts"][0]["newest_version"],
        encrypted["receipts"][0]["object_id"]
    );
    assert_eq!(result["receipts"][1]["event"], "written");
    f.ok(&["pool", "new", "backup"]);
    f.new_tape("BACKUP");
    f.ok(&[
        "tape",
        "init",
        "--label",
        "BACKUP",
        "--generation",
        "L9",
        "--pool",
        "backup",
    ]);
    let before = f.writes();
    assert_ne!(
        f.run(&["put", source.to_str().unwrap()], true).0,
        ExitCode::SUCCESS
    );
    assert_eq!(before, f.writes());
    assert_eq!(
        f.ok(&["put", source.to_str().unwrap(), "--allow-plaintext-copy"])["receipts"][0]["event"],
        "written"
    );
}

/// Rescue reads keep their ordinary path with a warning; release is local and audited.
#[test]
fn fence_allows_rescue_blocks_writes_and_releases_with_audit() {
    let f = VersionFixture::new();
    let source = f.root.path().join("rescue.txt");
    std::fs::write(&source, b"rescue bytes").unwrap();
    let put = f.ok(&["put", source.to_str().unwrap()]);
    let tape = f.index().get_tape_by_voltag("FIRST").unwrap().unwrap();
    let fence = f
        .index()
        .record_tape_io_fence_at_endpoint(
            remanence_state::TapeIoFenceInput {
                tape_uuid: tape.tape_uuid.try_into().unwrap(),
                barcode: Some("FIRST".into()),
                reason: "standalone_test".into(),
                evidence_json: None,
            },
            Some(&remanence_library::DriveKey::Standalone {
                drive_serial: "HOME-DRIVE".into(),
            }),
        )
        .unwrap();
    for args in [
        vec![
            "get".to_string(),
            put["receipts"][0]["object_id"].as_str().unwrap().into(),
            "--out".into(),
            f.root.path().join("get").display().to_string(),
        ],
        vec![
            "tape".into(),
            "restore".into(),
            "--scan".into(),
            "--out".into(),
            f.root.path().join("scan").display().to_string(),
        ],
    ] {
        let (code, out, errors) = f.run(&args.iter().map(String::as_str).collect::<Vec<_>>(), true);
        assert_eq!(code, ExitCode::SUCCESS, "{out}\n{errors}");
        assert!(errors.contains("warning: active tape-I/O fences"));
        assert_eq!(
            VersionFixture::envelope(&out)["active_fences"][0]["quarantine_id"],
            fence.quarantine_id
        );
    }
    let before = f.writes();
    assert_ne!(
        f.run(&["put", source.to_str().unwrap()], true).0,
        ExitCode::SUCCESS
    );
    assert_eq!(before, f.writes());
    assert_eq!(
        f.ok(&["tape", "quarantine", "list"])["active_fences"][0]["label"],
        "FIRST"
    );
    f.ok(&[
        "tape",
        "quarantine",
        "release",
        &fence.quarantine_id,
        "--ack",
        "recovered",
    ]);
    assert!(f.index().list_active_tape_io_fences().unwrap().is_empty());
    let cfg = remanence_state::load_config(f.config()).unwrap();
    let audit = remanence_state::FileAuditLog::replay(cfg.audit.dir).unwrap();
    assert!(audit.iter().any(
        |r| r.event == remanence_state::AuditEvent::TapeIoFenceReleased
            && r.subject.id.as_deref() == Some(&fence.quarantine_id)
    ));
}

/// The child runs production confirmation on a real terminal, with model media.
#[test]
#[ignore = "invoked by clobber_confirmation_accepts_a_real_pty with terminal stdin"]
fn clobber_pty_child() {
    use std::io::IsTerminal;
    assert!(
        std::io::stdin().is_terminal(),
        "child requires the parent PTY"
    );
    let mut f = VersionFixture::new();
    f.terminal_input = true;
    let mut tape = remanence_chaos::model::VirtualTape::empty(256 * 1024 * 1024, 262144);
    tape.records.push(remanence_chaos::model::Record::Block(
        b"<ltfslabel/>".to_vec(),
    ));
    f.world
        .lock()
        .unwrap()
        .put_tape_in_drive(0x100, "USED", None, tape);
    let before = f.writes();
    f.ok(&[
        "tape",
        "init",
        "--label",
        "USED",
        "--generation",
        "L9",
        "--clobber-data",
    ]);
    assert!(f.writes() > before);
}

/// PTY availability is required; failure is never reported as passing coverage.
#[test]
fn clobber_confirmation_accepts_a_real_pty() {
    use std::io::Write;
    use std::os::fd::FromRawFd;
    let mut master = -1;
    let mut slave = -1;
    // SAFETY: valid output pointers; null optional termios/winsize request defaults.
    assert_eq!(
        unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null(),
                std::ptr::null(),
            )
        },
        0,
        "openpty: {}",
        std::io::Error::last_os_error()
    );
    // SAFETY: successful openpty returned two newly owned file descriptors.
    let mut master = unsafe { std::fs::File::from_raw_fd(master) };
    let slave = unsafe { std::fs::File::from_raw_fd(slave) };
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "clobber_pty_child", "--ignored", "--nocapture"])
        .stdin(slave)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    master.write_all(b"CLOBBER USED\n").unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        if child.try_wait().unwrap().is_some() {
            break;
        }
        if std::time::Instant::now() >= deadline {
            child.kill().unwrap();
            panic!("PTY confirmation timed out");
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let result = child.wait_with_output().unwrap();
    assert!(String::from_utf8_lossy(&result.stdout).contains("1 passed"));
    assert!(
        result.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
}

/// Init must inspect the old BOT identity before planning its replacement UUID.
#[test]
fn clobber_refuses_fenced_old_bot_on_another_endpoint() {
    let f = VersionFixture::new();
    let source = f.root.path().join("old.txt");
    std::fs::write(&source, b"old bytes").unwrap();
    f.ok(&["put", source.to_str().unwrap()]);
    let tape = f.index().get_tape_by_voltag("FIRST").unwrap().unwrap();
    f.index()
        .record_tape_io_fence_at_endpoint(
            remanence_state::TapeIoFenceInput {
                tape_uuid: tape.tape_uuid.try_into().unwrap(),
                barcode: Some("FIRST".into()),
                reason: "test".into(),
                evidence_json: None,
            },
            Some(&remanence_library::DriveKey::Standalone {
                drive_serial: "OTHER".into(),
            }),
        )
        .unwrap();
    let before = f.writes();
    let (code, out, _) = f.run(
        &[
            "tape",
            "init",
            "--label",
            "FIRST",
            "--generation",
            "L9",
            "--clobber-data",
            "--confirm-clobber",
            "CLOBBER FIRST",
        ],
        true,
    );
    assert_ne!(code, ExitCode::SUCCESS);
    assert!(
        out.contains("write blocked by active tape-I/O fence"),
        "{out}"
    );
    assert_eq!(before, f.writes());
}

/// Swapping cartridges does not erase an explicit endpoint fence, and historical
/// UUID-only fences do not prevent writing an unrelated cartridge in that drive.
#[test]
fn other_cartridge_write_depends_on_recorded_endpoint() {
    for endpoint_match in [false, true] {
        let f = VersionFixture::new();
        let old = f.index().get_tape_by_voltag("FIRST").unwrap().unwrap();
        f.new_tape("SECOND");
        f.ok(&["tape", "init", "--label", "SECOND", "--generation", "L9"]);
        let endpoint = remanence_library::DriveKey::Standalone {
            drive_serial: "HOME-DRIVE".into(),
        };
        f.index()
            .record_tape_io_fence_at_endpoint(
                remanence_state::TapeIoFenceInput {
                    tape_uuid: old.tape_uuid.try_into().unwrap(),
                    barcode: Some("FIRST".into()),
                    reason: "test".into(),
                    evidence_json: None,
                },
                endpoint_match.then_some(&endpoint),
            )
            .unwrap();
        let source = f.root.path().join("new.txt");
        std::fs::write(&source, b"new cartridge").unwrap();
        let before = f.writes();
        let (code, out, errors) = f.run(&["put", source.to_str().unwrap()], true);
        assert_eq!(
            code == ExitCode::SUCCESS,
            !endpoint_match,
            "{out}\n{errors}"
        );
        if endpoint_match {
            assert_eq!(before, f.writes());
        } else {
            assert!(f.writes() > before);
        }
    }
}

/// A confirmed overwrite reuses the catalog provisioning path and removes stale copies.
#[test]
fn clobber_catalogued_tape_and_refuse_another_label_owner() {
    let f = VersionFixture::new();
    let source = f.root.path().join("old.txt");
    std::fs::write(&source, b"old data").unwrap();
    let put = f.ok(&["put", source.to_str().unwrap()]);
    let old = f
        .index()
        .get_tape_by_voltag("FIRST")
        .unwrap()
        .unwrap()
        .tape_uuid;
    f.ok(&[
        "tape",
        "init",
        "--label",
        "FIRST",
        "--generation",
        "L9",
        "--clobber-data",
        "--confirm-clobber",
        "CLOBBER FIRST",
    ]);
    let new = f
        .index()
        .get_tape_by_voltag("FIRST")
        .unwrap()
        .unwrap()
        .tape_uuid;
    assert_ne!(old, new);
    let object = f
        .index()
        .get_native_object(put["receipts"][0]["object_id"].as_str().unwrap())
        .unwrap()
        .unwrap();
    assert!(!object.copies.iter().any(|c| c.status == "committed"));
    let mut foreign = remanence_chaos::model::VirtualTape::empty(256 * 1024 * 1024, 262144);
    foreign.records.push(remanence_chaos::model::Record::Block(
        b"foreign data".to_vec(),
    ));
    f.world
        .lock()
        .unwrap()
        .put_tape_in_drive(0x100, "OTHER", None, foreign);
    let before = f.writes();
    let (code, out, _) = f.run(
        &[
            "tape",
            "init",
            "--label",
            "FIRST",
            "--generation",
            "L9",
            "--clobber-data",
        ],
        true,
    );
    assert_ne!(code, ExitCode::SUCCESS);
    assert!(out.contains("belongs to tape"), "{out}");
    assert!(out.contains("tape rebind --previous-uuid"), "{out}");
    assert_eq!(f.writes(), before);
}

/// Every init checks the partition page, including an explicitly confirmed clobber.
#[test]
fn partitioned_init_refuses_before_writes() {
    for additional in [0, 1] {
        for clobber in [false, true] {
            let f = VersionFixture::new();
            let mut tape = remanence_chaos::model::VirtualTape::empty(256 * 1024 * 1024, 262144);
            tape.additional_partitions = additional;
            f.world
                .lock()
                .unwrap()
                .put_tape_in_drive(0x100, "PART", None, tape);
            let before = f.writes();
            let mut args = vec!["tape", "init", "--label", "PART", "--generation", "L9"];
            if clobber {
                args.extend(["--clobber-data", "--confirm-clobber", "CLOBBER PART"]);
            }
            let (code, out, _) = f.run(&args, true);
            assert_eq!(code == ExitCode::SUCCESS, additional == 0, "{out}");
            if additional != 0 {
                assert!(out.contains("this cartridge has 2 partitions"), "{out}");
                assert!(out.contains("mkpartition 0"), "{out}");
                assert!(!out.contains("malformed MODE response"), "{out}");
                assert_eq!(before, f.writes());
            }
        }
    }
}

/// Discovery reads the catalog without competing with the write owner's state lock.
#[test]
fn discovery_succeeds_while_state_is_locked() {
    let f = VersionFixture::new();
    let config = remanence_state::load_config(f.config()).unwrap();
    let _owner = remanence_state::StateHandle::open_with_config(
        remanence_state::StatePaths::from_config(f.config(), &config),
        config,
    )
    .unwrap();
    let archive = f.root.path().join("diagnostics.tar");
    for args in [
        vec!["doctor"],
        vec!["drives"],
        vec!["diagnostics", "capture", "--out", archive.to_str().unwrap()],
    ] {
        let (code, out, errors) = f.run(&args, true);
        if args == ["doctor"] {
            assert_ne!(code, ExitCode::SUCCESS, "{out}\n{errors}");
            let report: serde_json::Value = serde_json::from_str(&out).unwrap();
            assert_eq!(report["result"]["catalog_fence_check"]["ok"], true);
        } else {
            assert_eq!(code, ExitCode::SUCCESS, "{out}\n{errors}");
        }
    }
}

/// Endpoint kind, not user-supplied reason, determines release authorization.
#[test]
fn standalone_cannot_release_a_library_fence() {
    for endpoint in [None, Some(remanence_library::DriveKey::new("LIB", 0x100))] {
        let f = VersionFixture::new();
        let fence = f
            .index()
            .record_tape_io_fence_at_endpoint(
                remanence_state::TapeIoFenceInput {
                    tape_uuid: [17; 16],
                    barcode: None,
                    reason: "standalone_misleading_reason".into(),
                    evidence_json: None,
                },
                endpoint.as_ref(),
            )
            .unwrap();
        let (code, out, _) = f.run(
            &[
                "tape",
                "quarantine",
                "release",
                &fence.quarantine_id,
                "--ack",
                "ok",
            ],
            true,
        );
        assert_ne!(code, ExitCode::SUCCESS);
        assert!(out.contains("--after-settled-inventory"), "{out}");
        assert!(out.contains("--config <the library config>"), "{out}");
        assert!(out.contains("host that owns it"), "{out}");
        assert_eq!(f.index().list_active_tape_io_fences().unwrap().len(), 1);
    }
}

/// Retirement cannot bypass the physical fresh-media gate or report false success.
#[test]
fn retired_nonblank_cartridge_cannot_bypass_fresh_media_gate() {
    let f = VersionFixture::new();
    let old: [u8; 16] = f
        .index()
        .get_tape_by_voltag("FIRST")
        .unwrap()
        .unwrap()
        .tape_uuid
        .try_into()
        .unwrap();
    let config = remanence_state::load_config(f.config()).unwrap();
    let mut state = remanence_state::StateHandle::open_with_config(
        remanence_state::StatePaths::from_config(f.config(), &config),
        config,
    )
    .unwrap();
    state
        .retire_tape(remanence_state::RetireTapeInput {
            tape_uuid: old,
            reason: "recycled".into(),
        })
        .unwrap();
    drop(state);
    let before = f.writes();
    let (code, out, _) = f.run(
        &["tape", "init", "--label", "FIRST", "--generation", "L9"],
        true,
    );
    assert_ne!(code, ExitCode::SUCCESS);
    assert!(out.contains("--clobber-data"), "{out}");
    assert_eq!(before, f.writes());
    assert!(f.index().get_tape_by_voltag("FIRST").unwrap().is_none());
    assert_eq!(f.index().get_tape(&old).unwrap().unwrap().state, "retired");
    f.ok(&[
        "tape",
        "init",
        "--label",
        "FIRST",
        "--generation",
        "L9",
        "--clobber-data",
        "--confirm-clobber",
        "CLOBBER FIRST",
    ]);
    assert_ne!(
        f.index()
            .get_tape_by_voltag("FIRST")
            .unwrap()
            .unwrap()
            .tape_uuid,
        old
    );
    assert_eq!(f.index().get_tape(&old).unwrap().unwrap().state, "retired");
}

/// A stale catalog remains a visible failed doctor check without blocking discovery.
#[test]
fn doctor_reports_schema_mismatch() {
    use std::io::{Seek, SeekFrom, Write};
    let f = VersionFixture::new();
    let config = remanence_state::load_config(f.config()).unwrap();
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .open(config.index.sqlite_path)
        .unwrap();
    // SQLite's header stores user_version as a big-endian integer at byte 60.
    file.seek(SeekFrom::Start(60)).unwrap();
    file.write_all(&999_u32.to_be_bytes()).unwrap();
    file.sync_all().unwrap();
    drop(file);
    let (code, out, errors) = f.run(&["doctor"], true);
    assert_ne!(code, ExitCode::SUCCESS, "{out}\n{errors}");
    let report: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(
        report["result"]["catalog_fence_check"]["ok"], false,
        "{out}"
    );
    assert!(out.contains("999"), "{out}");
    assert!(errors.contains("cannot check catalog fences"), "{errors}");
}

/// Header access observes migration without accidentally opening the writable catalog.
fn catalog_schema(f: &VersionFixture, set: Option<u32>) -> u32 {
    use std::io::{Read, Seek, SeekFrom, Write};
    let config = remanence_state::load_config(f.config()).unwrap();
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(config.index.sqlite_path)
        .unwrap();
    file.seek(SeekFrom::Start(60)).unwrap();
    if let Some(version) = set {
        file.write_all(&version.to_be_bytes()).unwrap();
        file.sync_all().unwrap();
        file.seek(SeekFrom::Start(60)).unwrap();
    }
    let mut bytes = [0; 4];
    file.read_exact(&mut bytes).unwrap();
    u32::from_be_bytes(bytes)
}

/// Ordinary home reads and writes migrate; discovery reports stale state without modifying it.
#[test]
fn home_put_and_get_migrate_old_catalog_but_doctor_does_not() {
    let f = VersionFixture::new();
    let source = f.root.path().join("migrate.txt");
    std::fs::write(&source, b"migration payload").unwrap();
    assert_eq!(catalog_schema(&f, Some(19)), 19);
    let (code, out, errors) = f.run(&["doctor"], true);
    assert_ne!(code, ExitCode::SUCCESS, "{out} {errors}");
    let doctor = VersionFixture::envelope(&out)["result"].clone();
    assert_eq!(doctor["catalog_fence_check"]["ok"], false);
    assert_eq!(catalog_schema(&f, None), 19);
    f.ok(&["put", source.to_str().unwrap()]);
    assert_eq!(catalog_schema(&f, None), remanence_state::SCHEMA_VERSION);
    assert_eq!(catalog_schema(&f, Some(19)), 19);
    let dest = f.root.path().join("restored-migration");
    f.ok(&["get", "migrate.txt", "--out", dest.to_str().unwrap()]);
    assert_eq!(catalog_schema(&f, None), remanence_state::SCHEMA_VERSION);
    assert_eq!(
        std::fs::read(dest.join("migrate.txt")).unwrap(),
        b"migration payload"
    );
}

/// Real parser, catalog, tape transport, and filesystem sinks preserve member dates.
#[test]
fn restore_dates_listing_and_member_selection() {
    let mut f = VersionFixture::with_parity(false);
    let directory = f.root.path().join("photos");
    std::fs::create_dir(&directory).unwrap();
    let source = directory.join("a.txt");
    let date = std::time::UNIX_EPOCH + std::time::Duration::new(1_234_567_890, 123_456_789);
    for bytes in [b"first".as_slice(), b"second".as_slice()] {
        std::fs::write(&source, bytes).unwrap();
        std::fs::File::open(&source)
            .unwrap()
            .set_modified(date)
            .unwrap();
        f.ok(&["put", directory.to_str().unwrap()]);
    }
    let source_date = std::fs::metadata(&source).unwrap().modified().unwrap();
    let dest = f.root.path().join("get");
    f.ok(&["get", "photos/a.txt", "--out", dest.to_str().unwrap()]);
    assert_eq!(
        std::fs::metadata(dest.join("photos/a.txt"))
            .unwrap()
            .modified()
            .unwrap(),
        source_date
    );
    assert!(!std::fs::read_dir(&dest).unwrap().any(|e| e
        .unwrap()
        .file_name()
        .to_string_lossy()
        .starts_with(".rem-staging-")));
    let before_io = f.world.lock().unwrap().command_log.len();
    let rows = f.ok(&["list"]);
    assert_eq!(
        f.world.lock().unwrap().command_log.len(),
        before_io,
        "catalog list performs no device IO"
    );
    assert_eq!(rows["rows"][0]["version_count"], 2);
    assert_eq!(rows["rows"][0]["path"], "photos/a.txt");
    assert_eq!(rows["rows"][0]["tapes"], serde_json::json!(["FIRST"]));
    assert!(f.ok(&["list", "--path-prefix", "absent"])["rows"]
        .as_array()
        .unwrap()
        .is_empty());
    assert_eq!(f.ok(&["list", "--tape", "FIRST"])["rows"], rows["rows"]);
    let (code, table, errors) = f.run(&["list"], false);
    assert_eq!(code, ExitCode::SUCCESS, "{errors}");
    println!("CATALOG TABLE\n{table}");
    let no_output = f.root.path().join("list-output");
    let writes = f.writes();
    let listing = f.ok(&[
        "tape",
        "restore",
        "--list",
        "--out",
        no_output.to_str().unwrap(),
    ]);
    assert!(!no_output.exists());
    assert_eq!(f.writes(), writes);
    assert_eq!(listing["candidates"].as_array().unwrap().len(), 2);
    assert_eq!(
        listing["candidates"][0]["manifest"]["rows"][0]["path"],
        "photos/a.txt"
    );
    assert_ne!(
        listing["candidates"][0]["manifest"]["object_id"],
        listing["candidates"][1]["manifest"]["object_id"]
    );
    let (code, transcript, errors) = f.run(&["tape", "restore", "--list"], false);
    assert_eq!(code, ExitCode::SUCCESS, "{errors}");
    println!("TAPE LIST TRANSCRIPT\n{transcript}");
    for (n, selector) in ["photos/", "photos/a.txt"].iter().enumerate() {
        let dest = f.root.path().join(format!("selected-{n}"));
        f.ok(&[
            "tape",
            "restore",
            "--out",
            dest.to_str().unwrap(),
            "--path",
            selector,
        ]);
        for candidate in ["candidate-1", "candidate-2"] {
            assert_eq!(
                std::fs::metadata(dest.join(candidate).join("photos/a.txt"))
                    .unwrap()
                    .modified()
                    .unwrap(),
                source_date
            );
        }
        assert_eq!(
            std::fs::read(dest.join("candidate-1/photos/a.txt")).unwrap(),
            b"first"
        );
        assert_eq!(
            std::fs::read(dest.join("candidate-2/photos/a.txt")).unwrap(),
            b"second"
        );
        assert_eq!(std::fs::read_dir(&dest).unwrap().count(), 2);
    }
    let (code, output, _) = f.run(
        &["tape", "restore", "--list", "--path", "missing.txt"],
        true,
    );
    assert_ne!(code, ExitCode::SUCCESS);
    assert!(VersionFixture::envelope(&output)["error"]
        .as_str()
        .unwrap()
        .contains("missing.txt"));
    let identified = f.ok(&["tape", "identify"]);
    assert_eq!(identified["status"], "not_finalized");
    assert_eq!(identified["object_count"], 2);
    assert_eq!(identified["finalized"], false);
    assert_eq!(identified["pool"], "default");

    // Terminal replicas include physical separation extents; model enough media.
    f.world
        .lock()
        .unwrap()
        .tapes
        .get_mut("FIRST")
        .unwrap()
        .capacity_bytes = 3 * 1024 * 1024 * 1024;
    // Interrupt after durable request admission, before the transport writes.
    f.interrupt_finalize = true;
    let interrupted = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        f.run(&["tape", "finalize", "--acknowledge-tape", "FIRST"], true)
    }));
    assert!(
        interrupted.is_err(),
        "fixture must reach finalization's first media write"
    );
    f.interrupt_finalize = false;
    let tape_uuid: [u8; 16] = f
        .index()
        .get_tape_by_voltag("FIRST")
        .unwrap()
        .unwrap()
        .tape_uuid
        .try_into()
        .unwrap();
    let accepted = f
        .index()
        .terminal_finalization(&tape_uuid)
        .unwrap()
        .expect("interrupted request was durable");
    let config = remanence_state::load_config(f.config()).unwrap();
    let intent = remanence_state::FileCheckpointJournal::open(
        config.journal.dir.join("checkpoints"),
        tape_uuid,
    )
    .unwrap()
    .terminal_finalization_intent()
    .unwrap()
    .expect("durable finalization intent");
    let manual = intent.manual.expect("operator close-out identity");
    assert_eq!(manual.reason, "finalized with rem");
    assert_eq!(
        manual.idempotency_key,
        *uuid::Uuid::new_v5(
            &uuid::Uuid::parse_str("c33a391e-7209-5b35-a198-5cab71b91ea4").unwrap(),
            &tape_uuid
        )
        .as_bytes()
    );
    // Opening the drive reconciles abandoned sessions; reads remain available.
    f.ok(&["tape", "identify"]);
    // Restart recovery must fence the abandoned session; use the normal audited release.
    let quarantine = f.ok(&["tape", "quarantine", "list"]);
    let fences = quarantine["active_fences"].as_array().unwrap();
    assert_eq!(fences.len(), 1);
    f.ok(&[
        "tape",
        "quarantine",
        "release",
        fences[0]["quarantine_id"].as_str().unwrap(),
        "--ack",
        "fixture interrupted before first media write",
    ]);
    let finalized = f.ok(&["tape", "finalize", "--acknowledge-tape", "FIRST"]);
    assert_eq!(
        finalized["operation_id"],
        accepted.operation_id.unwrap().to_string()
    );
    let retried = f.ok(&["tape", "finalize", "--ack-tape-uuid", "FIRST"]);
    assert_eq!(finalized["operation_id"], retried["operation_id"]);
    assert_eq!(finalized["finalized"], true);
    assert!(finalized["completed_replicas"].is_number());
    let identified = f.ok(&["tape", "identify"]);
    assert_eq!(identified["finalized"], true);
    assert_eq!(identified["object_count"], 2);
    assert!(identified["bytes"].as_u64().unwrap() > 0);
    let verified = f.ok(&["tape", "verify"]);
    assert_eq!(verified["object_count"], 2);
    assert_eq!(verified["status"], "verified");
    let terminal_listing = f.ok(&["tape", "restore", "--list"]);
    assert_eq!(terminal_listing["terminal_index"], true);
    assert_eq!(
        terminal_listing["candidates"][0]["manifest"]["rows"][0]["path"],
        "photos/a.txt"
    );
    assert_eq!(f.ok(&["list"])["rows"][0]["finalized"], true);
}

/// Keyless listing reports identity; decrypted listing validates without extraction.
#[test]
fn encrypted_listing_and_staging_cleanup() {
    let f = VersionFixture::with_parity(false);
    let source = f.root.path().join("secret.txt");
    std::fs::write(&source, b"encrypted member").unwrap();
    let key = remanence_aead::RecipientPrivateKey::new([9; 16], "listing", [9; 32]).unwrap();
    let recipient = f.root.path().join("key.remr");
    let private = f.root.path().join("key.remp");
    std::fs::write(&recipient, key.public_key(0).unwrap().serialize().unwrap()).unwrap();
    std::fs::write(&private, key.serialize()).unwrap();
    let recovery = f.root.path().join("recovery.remr");
    let recovery_key =
        remanence_aead::RecipientPrivateKey::new([10; 16], "recovery", [10; 32]).unwrap();
    std::fs::write(
        &recovery,
        recovery_key.public_key(1).unwrap().serialize().unwrap(),
    )
    .unwrap();
    f.ok(&[
        "put",
        source.to_str().unwrap(),
        "--recipient",
        recipient.to_str().unwrap(),
        "--recipient",
        recovery.to_str().unwrap(),
    ]);
    let listing = f.ok(&["tape", "restore", "--list", "--path", "secret.txt"]);
    assert_eq!(listing["candidates"][0]["manifest"]["encrypted"], true);
    assert!(listing["candidates"][0]["manifest"]["message"]
        .as_str()
        .unwrap()
        .contains("--private-key"));
    let dest = f.root.path().join("not-created");
    let listing = f.ok(&[
        "tape",
        "restore",
        "--list",
        "--private-key",
        private.to_str().unwrap(),
        "--out",
        dest.to_str().unwrap(),
    ]);
    assert!(!dest.exists());
    assert_eq!(
        listing["candidates"][0]["manifest"]["rows"][0]["path"], "secret.txt",
        "{listing}"
    );
    let dest = f.root.path().join("get-error");
    let (code, _, _) = f.run(
        &["get", "secret.txt", "--out", dest.to_str().unwrap()],
        true,
    );
    assert_ne!(code, ExitCode::SUCCESS);
    assert_eq!(std::fs::read_dir(&dest).unwrap().count(), 0);
    f.ok(&[
        "get",
        "secret.txt",
        "--out",
        dest.to_str().unwrap(),
        "--private-key",
        private.to_str().unwrap(),
    ]);
    assert_eq!(
        std::fs::metadata(dest.join("secret.txt"))
            .unwrap()
            .modified()
            .unwrap(),
        std::fs::metadata(&source).unwrap().modified().unwrap()
    );
    assert_eq!(std::fs::read_dir(&dest).unwrap().count(), 1);
}

/// A corrupt first object cannot conceal a later valid object's members.
#[test]
fn catalogless_listing_continues_after_invalid_metadata() {
    let f = VersionFixture::with_parity(false);
    let source = f.root.path().join("bad.txt");
    std::fs::write(&source, b"bad").unwrap();
    let written = f.ok(&["put", source.to_str().unwrap()]);
    let id = written["receipts"][0]["object_id"].as_str().unwrap();
    let object = f.index().get_native_object(id).unwrap().unwrap();
    let tape_file = object.copies[0].tape_file_number;
    let good = f.root.path().join("good.txt");
    std::fs::write(&good, b"good").unwrap();
    f.ok(&["put", good.to_str().unwrap()]);
    {
        let mut world = f.world.lock().unwrap();
        let tape = world.tapes.get_mut("FIRST").unwrap();
        let lba = tape.export_image().files[tape_file as usize].start_record;
        let remanence_chaos::model::Record::Block(block) = &mut tape.records[lba] else {
            panic!("object header must be a data block")
        };
        // Preserve discovery magic; break the canonical header checksum.
        block[100] ^= 1;
    }
    let listing = f.ok(&["tape", "restore", "--scan", "--list"]);
    let candidates = listing["candidates"].as_array().unwrap();
    assert!(
        candidates.iter().any(|c| c["status"] == "refused"),
        "{listing}"
    );
    assert!(
        candidates
            .iter()
            .any(|c| c["manifest"]["rows"][0]["path"] == "good.txt"),
        "{listing}"
    );
}

/// Selection never extracts unrequested objects or follows destination symlinks.
#[test]
fn selected_members_keep_destination_containment() {
    let f = VersionFixture::with_parity(false);
    for name in ["a.txt", "b.txt", "c.txt"] {
        let path = f.root.path().join(name);
        std::fs::write(&path, name).unwrap();
        f.ok(&["put", path.to_str().unwrap()]);
    }
    let dest = f.root.path().join("selection");
    f.ok(&[
        "tape",
        "restore",
        "--out",
        dest.to_str().unwrap(),
        "--path",
        "a.txt",
        "--path",
        "c.txt",
    ]);
    assert_eq!(
        std::fs::read(dest.join("candidate-1/a.txt")).unwrap(),
        b"a.txt"
    );
    assert!(!dest.join("candidate-2").exists());
    assert_eq!(
        std::fs::read(dest.join("candidate-3/c.txt")).unwrap(),
        b"c.txt"
    );
    let dest = f.root.path().join("no-match");
    let (code, output, _) = f.run(
        &[
            "tape",
            "restore",
            "--out",
            dest.to_str().unwrap(),
            "--path",
            "missing.txt",
        ],
        true,
    );
    assert_ne!(code, ExitCode::SUCCESS);
    assert!(VersionFixture::envelope(&output)["error"]
        .as_str()
        .unwrap()
        .contains("missing.txt"));
    assert!(!dest.exists());
    let dest = f.root.path().join("containment");
    let outside = f.root.path().join("outside");
    std::fs::create_dir(&dest).unwrap();
    std::fs::create_dir(&outside).unwrap();
    std::os::unix::fs::symlink(&outside, dest.join("candidate-1")).unwrap();
    let (code, _, _) = f.run(
        &[
            "tape",
            "restore",
            "--out",
            dest.to_str().unwrap(),
            "--path",
            "a.txt",
        ],
        true,
    );
    assert_ne!(code, ExitCode::SUCCESS);
    assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 0);
    assert_eq!(std::fs::read_dir(&dest).unwrap().count(), 1);
    f.ok(&["pool", "new", "offsite", "--scheme", "none"]);
    f.new_tape("OFFSITE");
    f.ok(&[
        "tape",
        "init",
        "--label",
        "OFFSITE",
        "--pool",
        "offsite",
        "--generation",
        "L9",
    ]);
    f.ok(&["put", f.root.path().join("a.txt").to_str().unwrap()]);
    let inventory = f.ok(&["list", "--path-prefix", "a."]);
    let rows = inventory["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["pool"], "default");
    assert_eq!(rows[1]["pool"], "offsite");
    assert_eq!(rows[0]["tapes"], serde_json::json!(["FIRST"]));
    assert_eq!(rows[1]["tapes"], serde_json::json!(["OFFSITE"]));
    let offsite = f.ok(&["list", "--tape", "OFFSITE"]);
    assert_eq!(offsite["rows"], serde_json::json!([rows[1]]));
    let id = f.ok(&["tape", "identify"])["uuid"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(f.ok(&["list", "--tape", &id])["rows"], offsite["rows"]);
    assert_ne!(
        f.run(&["list", "--tape", "UNKNOWN"], true).0,
        ExitCode::SUCCESS
    );
}

/// Production CLI and daemon services preserve timestamps through both append modes.
#[test]
fn daemon_put_get_preserves_mtime() {
    for mode in [
        remanence_state::AppendStagingMode::Serial,
        remanence_state::AppendStagingMode::Overlap,
    ] {
        let f = VersionFixture::with_parity(false);
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let socket = f.root.path().join("daemon.sock");
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let server = {
            let _entered = runtime.enter();
            let state = f.daemon_state(mode);
            // Fail loudly at the actual socket operation when sandbox policy forbids it.
            let probe = std::os::unix::net::UnixListener::bind(&socket)
                .expect("daemon test requires Unix sockets");
            drop(probe);
            std::fs::remove_file(&socket).unwrap();
            let socket = socket.clone();
            runtime.spawn(async move {
                remanence_daemon::serve(state, &socket, None, async {
                    stopped.await.unwrap();
                })
                .await
                .unwrap()
            })
        };
        for _ in 0..100 {
            if socket.exists() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(socket.exists(), "daemon must start");
        let input = f.root.path().join("dated.txt");
        std::fs::write(&input, b"daemon timestamp payload").unwrap();
        let date = std::time::UNIX_EPOCH + std::time::Duration::new(1_234_567_890, 123_456_789);
        std::fs::File::open(&input)
            .unwrap()
            .set_modified(date)
            .unwrap();
        let endpoint = format!("unix:{}", socket.display());
        let run = |args: &[&str]| {
            let result = std::process::Command::new(env!("CARGO_BIN_EXE_rem"))
                .env("REM_TEST_NO_LIVE_SCSI", "1")
                .current_dir(f.root.path())
                .args(args)
                .args(["--endpoint", &endpoint, "--json"])
                .output()
                .unwrap();
            assert!(
                result.status.success(),
                "stdout={} stderr={}",
                String::from_utf8_lossy(&result.stdout),
                String::from_utf8_lossy(&result.stderr)
            );
        };
        run(&["put", "dated.txt", "--pool", "default"]);
        let output = f.root.path().join("daemon-restored");
        run(&[
            "get",
            "dated.txt",
            "--caller-id",
            "--dest",
            output.to_str().unwrap(),
        ]);
        assert_eq!(
            std::fs::read(output.join("dated.txt")).unwrap(),
            b"daemon timestamp payload"
        );
        assert_eq!(
            std::fs::metadata(output.join("dated.txt"))
                .unwrap()
                .modified()
                .unwrap(),
            date
        );
        stop.send(()).unwrap();
        runtime.block_on(server).unwrap();
    }
}

/// Exact selectors and unresolved selectors must refuse before creating any output.
#[test]
fn selection_is_validated_before_output() {
    let f = VersionFixture::with_parity(false);
    let source = f.root.path().join("photos.txt");
    std::fs::write(&source, b"photo").unwrap();
    f.ok(&["put", source.to_str().unwrap()]);
    for selectors in [vec!["photos"], vec!["photos.txt", "missing.txt"]] {
        let dest = f.root.path().join("no-output");
        let mut args = vec!["tape", "restore", "--out", dest.to_str().unwrap()];
        for selector in selectors {
            args.extend(["--path", selector]);
        }
        assert_ne!(f.run(&args, true).0, ExitCode::SUCCESS);
        assert!(!dest.exists());
    }
}

/// Keep the WAL writer in a separate process, as it is for inventory commands.
struct InventoryWriter(
    std::process::Child,
    std::io::BufReader<std::process::ChildStdout>,
);

impl Drop for InventoryWriter {
    fn drop(&mut self) {
        drop(self.0.stdin.take());
        use std::io::Read;
        self.1.read_to_end(&mut Vec::new()).unwrap();
        assert!(self.0.wait().unwrap().success(), "inventory writer failed");
    }
}

fn inventory_writer(f: &VersionFixture) -> InventoryWriter {
    use std::io::BufRead;
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "catalog_inventory_writer_child", "--nocapture"])
        .env("REM_TEST_INVENTORY_WRITER", f.config())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut output = std::io::BufReader::new(child.stdout.take().unwrap());
    loop {
        let mut line = String::new();
        assert_ne!(
            output.read_line(&mut line).unwrap(),
            0,
            "writer exited before ready"
        );
        if line.trim().ends_with("writer-ready") {
            break;
        }
    }
    InventoryWriter(child, output)
}

/// Child-process entrypoint; the parent keeps stdin open until reads finish.
#[test]
fn catalog_inventory_writer_child() {
    use std::io::{Read, Write};
    let Some(config) = std::env::var_os("REM_TEST_INVENTORY_WRITER") else {
        // The ordinary test invocation exercises the child entrypoint as well.
        let f = VersionFixture::with_parity(false);
        drop(inventory_writer(&f));
        return;
    };
    let config = remanence_state::load_config(config).unwrap();
    let mut writer = remanence_state::CatalogIndex::open(&config.index.sqlite_path).unwrap();
    writer
        .upsert_tape_pool_projection(remanence_state::TapePoolProjectionInput {
            pool_id: "live-wal".into(),
            display_name: None,
            copy_class: None,
            content_class: None,
            created_at_utc: None,
        })
        .unwrap();
    println!("writer-ready");
    std::io::stdout().flush().unwrap();
    std::io::stdin().read_to_end(&mut Vec::new()).unwrap();
}

/// Catalog inventory leaves every state byte and file timestamp unchanged.
#[test]
fn catalog_list_does_not_mutate_state() {
    fn snapshot(
        root: &std::path::Path,
    ) -> std::collections::BTreeMap<std::path::PathBuf, (Vec<u8>, std::time::SystemTime)> {
        let mut result = std::collections::BTreeMap::new();
        for entry in std::fs::read_dir(root).unwrap() {
            let entry = entry.unwrap();
            if entry.file_type().unwrap().is_dir() {
                result.extend(snapshot(&entry.path()));
            } else {
                result.insert(
                    entry.path(),
                    (
                        std::fs::read(entry.path()).unwrap(),
                        entry.metadata().unwrap().modified().unwrap(),
                    ),
                );
            }
        }
        result
    }
    let f = VersionFixture::with_parity(false);
    // Keep WAL and shared memory live; listing must neither ignore the WAL
    // nor alter its read marks. The second pool exists only in the WAL.
    let _writer = inventory_writer(&f);
    let before = snapshot(&f.root.path().join("state"));
    let inventory = remanence_state::CatalogIndex::open_inventory(
        &remanence_state::load_config(f.config())
            .unwrap()
            .index
            .sqlite_path,
    )
    .unwrap();
    assert!(inventory.get_tape_pool("live-wal").unwrap().is_some());
    drop(inventory);
    assert_eq!(snapshot(&f.root.path().join("state")), before);
    f.ok(&["list"]);
    assert_eq!(snapshot(&f.root.path().join("state")), before);
}

/// A subprocess isolates TMPDIR from parallel tests and exercises a complete get.
#[test]
fn standalone_get_ignores_tmpdir() {
    const CHILD: &str = "REM_TEST_RESTORE_TMPDIR_CHILD";
    if std::env::var_os(CHILD).is_some() {
        let f = VersionFixture::with_parity(false);
        let source = f.root.path().join("input.txt");
        std::fs::write(&source, b"restore").unwrap();
        f.ok(&["put", source.to_str().unwrap()]);
        let dest = f.root.path().join("output");
        // VersionFixture needs a usable temp root; only restoration gets the bad TMPDIR.
        unsafe {
            std::env::set_var("TMPDIR", f.root.path().join("absent"));
        }
        f.ok(&["get", "input.txt", "--out", dest.to_str().unwrap()]);
        assert_eq!(std::fs::read(dest.join("input.txt")).unwrap(), b"restore");
    } else {
        let result = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "standalone_get_ignores_tmpdir",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(CHILD, "1")
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{} {}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
    }
}

/// The real daemon ingress and drive actor round trip a timestamp without a socket.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn in_process_daemon_append_preserves_mtime() {
    use remanence_api::pb::read_session_service_server::ReadSessionService;
    use remanence_api::pb::{self, write_session_service_server::WriteSessionService};
    use sha2::{Digest, Sha256};
    use tokio_stream::StreamExt;
    for mode in [
        remanence_state::AppendStagingMode::Serial,
        remanence_state::AppendStagingMode::Overlap,
    ] {
        let f = VersionFixture::with_parity(false);
        let state = f.daemon_state(mode);
        let service = state.write_session_service();
        let session = service
            .open_write_session(tonic::Request::new(pb::OpenWriteSessionRequest {
                target: Some(pb::open_write_session_request::Target::PoolTarget(
                    pb::TapePoolTarget {
                        pool_id: "default".into(),
                        mount_if_needed: true,
                        ..Default::default()
                    },
                )),
                ..Default::default()
            }))
            .await
            .unwrap()
            .into_inner();
        let payload = b"daemon timestamp payload";
        let digest = Sha256::digest(payload).to_vec();
        let mtime = "1234567890.123456789";
        let messages = vec![
            pb::append_object_message::Payload::Start(pb::AppendObjectStart {
                session_id: session.session_id.clone(),
                caller_object_id: "dated.txt".into(),
                declared_size_bytes: Some(payload.len() as u64),
                expected_content_sha256: Some(digest.clone()),
                source_replay_capability: pb::SourceReplayCapability::ReplayFromStart as i32,
                mtime: Some(mtime.into()),
                ..Default::default()
            }),
            pb::append_object_message::Payload::Chunk(pb::AppendObjectChunk {
                session_id: session.session_id.clone(),
                data: payload.to_vec(),
            }),
            pb::append_object_message::Payload::Finish(pb::AppendObjectFinish {
                session_id: session.session_id.clone(),
                expected_content_sha256: Some(digest),
                ..Default::default()
            }),
        ]
        .into_iter()
        .map(|payload| {
            Ok(pb::AppendObjectMessage {
                payload: Some(payload),
            })
        });
        let record = service
            .append_object_from_stream(tonic::Request::new(tokio_stream::iter(messages)))
            .await
            .unwrap_or_else(|e| panic!("{mode:?}: {e}"))
            .into_inner();
        service
            .close_write_session(tonic::Request::new(pb::CloseWriteSessionRequest {
                session_id: session.session_id,
                ..Default::default()
            }))
            .await
            .unwrap();
        let object_id = uuid::Uuid::from_slice(&record.object_id)
            .unwrap()
            .to_string();
        let files = f.index().list_native_object_files(&object_id).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].mtime.as_deref(), Some(mtime));
        let reader = state.read_session_service();
        let read_session = reader
            .open_read_session(tonic::Request::new(pb::OpenReadSessionRequest {
                target: Some(pb::open_read_session_request::Target::TapeTarget(
                    pb::TapeTarget {
                        tape_uuid: session.tape_uuid.clone().expect("write session tape"),
                        mount_if_needed: true,
                        required_pool_id: "default".into(),
                        allow_unpooled: false,
                    },
                )),
                ..Default::default()
            }))
            .await
            .unwrap()
            .into_inner();
        let mut stream = reader
            .read_file(tonic::Request::new(pb::ReadFileRequest {
                session_id: read_session.session_id.clone(),
                object_id: record.object_id,
                file_id: files[0].file_id.as_bytes().to_vec(),
                stream_chunk_bytes: 1024,
            }))
            .await
            .unwrap()
            .into_inner();
        let mut restored = Vec::new();
        while let Some(chunk) = stream.next().await {
            restored.extend(chunk.unwrap().data);
        }
        assert_eq!(restored, payload);
        reader
            .close_read_session(tonic::Request::new(pb::CloseReadSessionRequest {
                session_id: read_session.session_id,
                ..Default::default()
            }))
            .await
            .unwrap();
        state.shutdown_drive_pool().await.unwrap();
    }
}

/// Inventory must work with no writable temp directory, including uncheckpointed WAL rows.
#[test]
fn catalog_inventory_never_copies_to_tmpdir() {
    const CHILD: &str = "REM_TEST_INVENTORY_CHILD";
    if let Some(config) = std::env::var_os(CHILD) {
        let config = remanence_state::load_config(config).unwrap();
        let index =
            remanence_state::CatalogIndex::open_inventory(&config.index.sqlite_path).unwrap();
        assert!(index.get_tape_pool("live-wal").unwrap().is_some());
    } else {
        let f = VersionFixture::with_parity(false);
        let mut writer = f.index();
        writer
            .upsert_tape_pool_projection(remanence_state::TapePoolProjectionInput {
                pool_id: "live-wal".into(),
                display_name: None,
                copy_class: None,
                content_class: None,
                created_at_utc: None,
            })
            .unwrap();
        let directory = writer.path().parent().unwrap().to_owned();
        let snapshot = || {
            std::fs::read_dir(&directory)
                .unwrap()
                .map(|e| {
                    let e = e.unwrap();
                    (
                        e.file_name(),
                        (
                            std::fs::read(e.path()).unwrap(),
                            e.metadata().unwrap().modified().unwrap(),
                        ),
                    )
                })
                .collect::<std::collections::BTreeMap<_, _>>()
        };
        let before = snapshot();
        let result = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "catalog_inventory_never_copies_to_tmpdir",
                "--nocapture",
            ])
            .env(CHILD, f.config())
            .env("TMPDIR", f.root.path().join("absent"))
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{} {}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
        assert_eq!(snapshot(), before);
        drop(writer);
        // Also exercise the closed-catalog case, where SQLite must not create sidecars.
        let before = snapshot();
        let result = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "catalog_inventory_never_copies_to_tmpdir",
                "--nocapture",
            ])
            .env(CHILD, f.config())
            .env("TMPDIR", f.root.path().join("absent"))
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{} {}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
        assert_eq!(snapshot(), before);
    }
}

/// Resolve a caller-supplied symlink root while retaining member containment.
#[test]
fn standalone_get_accepts_symlink_destination() {
    let f = VersionFixture::with_parity(false);
    let source = f.root.path().join("input.txt");
    std::fs::write(&source, b"restore").unwrap();
    f.ok(&["put", source.to_str().unwrap()]);
    let dest = f.root.path().join("output");
    std::fs::create_dir(&dest).unwrap();
    let link = f.root.path().join("output-link");
    std::os::unix::fs::symlink(&dest, &link).unwrap();
    f.ok(&["get", "input.txt", "--out", link.to_str().unwrap()]);
    assert_eq!(std::fs::read(dest.join("input.txt")).unwrap(), b"restore");
    std::fs::remove_file(dest.join("input.txt")).unwrap();
    let outside = f.root.path().join("outside.txt");
    std::fs::write(&outside, b"untouched").unwrap();
    std::os::unix::fs::symlink(&outside, dest.join("input.txt")).unwrap();
    assert_ne!(
        f.run(&["get", "input.txt", "--out", link.to_str().unwrap()], true)
            .0,
        ExitCode::SUCCESS
    );
    assert_eq!(std::fs::read(outside).unwrap(), b"untouched");
}

/// Setup uses fixture discovery and persists only an affirmative, unique choice.
#[test]
fn setup_selects_one_drive_and_refuses_zero_or_two() {
    use remanence_chaos::model::{DeviceRole, ModelTransport, VirtualWorld};
    use std::sync::{Arc, Mutex};
    for (count, incomplete, select) in [
        (0, false, false),
        (1, false, false),
        (2, false, false),
        (1, true, false),
        (2, false, true),
    ] {
        let root = tempfile::tempdir().unwrap();
        let worlds: Vec<_> = (0..count)
            .map(|i| {
                Arc::new(Mutex::new(VirtualWorld::single_drive(
                    "HIDDEN",
                    0x100,
                    format!("DRIVE-{i}"),
                    0x400,
                    0,
                )))
            })
            .collect();
        let devices: Vec<_> = (0..count)
            .map(|i| remanence_library::DeviceAttachment {
                sg_path: format!("/dev/setup-{i}").into(),
                sysfs_path: format!("/sys/setup-{i}").into(),
            })
            .collect();
        let mut out = Vec::new();
        let mut errors = Vec::new();
        let mut args = vec!["rem", "setup", "--yes", "--json"];
        if select {
            args.extend(["--device", "/dev/setup-1"]);
        }
        let code = remanence_cli::run_with_standalone_test_enumeration(
            &args,
            root.path(),
            &root.path().join("state"),
            &root.path().join("absent"),
            &root.path().join("locks"),
            remanence_library::sysfs::DeviceEnumeration {
                devices,
                warnings: if incomplete {
                    vec![remanence_library::DiscoveryWarning::DeviceUnreachable {
                        path: "/dev/unreachable".into(),
                        source: remanence_library::IoErrorKind::from(&std::io::Error::from(
                            std::io::ErrorKind::PermissionDenied,
                        )),
                    }]
                } else {
                    vec![]
                },
            },
            |path| {
                let i: usize = path
                    .to_str()
                    .unwrap()
                    .rsplit('-')
                    .next()
                    .unwrap()
                    .parse()
                    .unwrap();
                Ok(Box::new(ModelTransport::new(
                    worlds[i].clone(),
                    DeviceRole::Drive { bay: 0x100 },
                )))
            },
            &mut out,
            &mut errors,
        );
        let value: serde_json::Value = serde_json::from_slice(&out).unwrap();
        let config_path = root.path().join("state/rem/config.toml");
        if count == 1 || select {
            assert_eq!(code, ExitCode::SUCCESS, "{value}");
            let saved: remanence_state::RemConfig =
                toml::from_str(&std::fs::read_to_string(config_path).unwrap()).unwrap();
            assert_eq!(
                saved.home.unwrap().drive_serial.as_deref(),
                Some(if select { "DRIVE-1" } else { "DRIVE-0" })
            );
            assert_eq!(saved.standalone_drives.len(), usize::from(incomplete));
            if incomplete {
                assert_eq!(saved.standalone_drives[0].serial, "DRIVE-0");
                assert!(!String::from_utf8_lossy(&errors).contains("[y/N]"));
            }
            assert!(
                saved.journal.require_trusted_volume,
                "diagnostics must not weaken saved durability policy"
            );
        } else {
            assert_ne!(code, ExitCode::SUCCESS, "{value}");
            assert!(
                !config_path.exists(),
                "ambiguous discovery must not save configuration"
            );
            assert!(value["error"].as_str().unwrap().contains(if count == 0 {
                "no standalone"
            } else {
                "exactly one"
            }));
        }
    }
}

/// Binary coverage is limited to help parsing, with an explicit route independent of host config.
#[test]
fn top_level_standalone_help_never_discovers() {
    for args in [
        vec!["tape", "--help"],
        vec!["help", "tape", "init"],
        vec!["tape", "help", "init"],
        vec!["put", "--help"],
    ] {
        let result = std::process::Command::new(env!("CARGO_BIN_EXE_rem"))
            .env("REM_TEST_NO_LIVE_SCSI", "1")
            .args(["--device", "/dev/fixture"])
            .args(&args)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert!(String::from_utf8(result.stdout)
            .unwrap()
            .contains("--allow-device"));
    }
}

/// Saved authorization resolves by discovery and cannot authorize an explicit different path.
#[test]
fn saved_drive_defaults_feed_the_existing_device_gate() {
    let mut f = VersionFixture::new();
    f.ok(&["setup", "--yes"]);
    f.home_default = true;
    let identity = f.ok(&["tape", "identify"]);
    assert_eq!(identity["label"], "FIRST");
    let (code, out, _) = f.run(
        &["tape", "identify", "--allow-device", "ANOTHER-DRIVE"],
        true,
    );
    assert_ne!(code, ExitCode::SUCCESS);
    assert!(VersionFixture::envelope(&out)["error"]
        .as_str()
        .unwrap()
        .contains("does not authorize"));
    let (code, out, _) = f.run(&["tape", "identify", "--device", "/dev/other"], true);
    assert_ne!(code, ExitCode::SUCCESS);
    assert!(VersionFixture::envelope(&out)["error"]
        .as_str()
        .unwrap()
        .contains("explicit --allow-device"));
    let (code, out, _) = f.run(
        &[
            "tape",
            "identify",
            "--device",
            "/dev/other",
            "--allow-device",
            "HOME-DRIVE",
        ],
        true,
    );
    assert_ne!(code, ExitCode::SUCCESS);
    let error = VersionFixture::envelope(&out)["error"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(
        error.contains("no SCSI device at /dev/other") && error.contains("/dev/home-model"),
        "{error}"
    );
}

/// Pin the public help output while checking every individual flag in parser tests.
#[test]
fn top_level_help_snapshot() {
    let result = std::process::Command::new(env!("CARGO_BIN_EXE_rem"))
        .env("REM_TEST_NO_LIVE_SCSI", "1")
        .arg("--help")
        .output()
        .unwrap();
    assert!(result.status.success());
    assert_eq!(
        String::from_utf8(result.stdout).unwrap(),
        include_str!("snapshots/rem-help.txt")
    );
}

/// Setup preserves unrelated bytes and supports reruns, replacement and pool creation.
#[test]
fn setup_preserves_config_and_reports_one_human_sentence() {
    let f = VersionFixture::new();
    let original = std::fs::read_to_string(f.config()).unwrap();
    let (code, out, errors) = f.run(&["setup", "--yes"], false);
    assert_eq!(code, ExitCode::SUCCESS, "{errors}");
    assert_eq!(out.lines().count(), 1, "{out}");
    assert!(
        out.starts_with("saved ") && out.ends_with("serial HOME-DRIVE as this host's drive\n"),
        "{out}"
    );
    assert!(!errors.contains("[y/N]"));
    let saved = std::fs::read_to_string(f.config()).unwrap();
    assert!(saved.starts_with(&original));
    let loaded = remanence_state::load_config(f.config()).unwrap();
    let roundtrip = remanence_state::parse_config_toml(&toml::to_string(&loaded).unwrap()).unwrap();
    assert_eq!(
        toml::to_string(&loaded).unwrap(),
        toml::to_string(&roundtrip).unwrap()
    );
    let edited = format!(
        "# operator comment\n{}",
        saved.replace("HOME-DRIVE", "OLD-DRIVE")
    );
    std::fs::write(f.config(), &edited).unwrap();
    let (code, out, errors) = f.run(&["setup", "--yes"], true);
    assert_eq!(code, ExitCode::SUCCESS, "{out} {errors}");
    assert_eq!(
        std::fs::read_to_string(f.config()).unwrap(),
        edited.replace("OLD-DRIVE", "HOME-DRIVE")
    );
    let (code, out, errors) = f.run(&["pool", "new", "offsite"], true);
    assert_eq!(code, ExitCode::SUCCESS, "{out} {errors}");
    let before = std::fs::read_to_string(f.config()).unwrap();
    let (code, out, errors) = f.run(&["setup", "--yes"], true);
    assert_eq!(code, ExitCode::SUCCESS, "{out} {errors}");
    assert_eq!(std::fs::read_to_string(f.config()).unwrap(), before);
}

/// A test-spawned production binary refuses discovery without an environment guard.
#[test]
fn cli_children_always_disable_live_scsi() {
    let home = tempfile::tempdir().unwrap();
    let result = std::process::Command::new(env!("CARGO_BIN_EXE_rem"))
        .env_remove("REM_TEST_NO_LIVE_SCSI")
        .env_remove("REM_TEST_LIVE_SCSI")
        .env("HOME", home.path())
        .env("XDG_STATE_HOME", home.path())
        .args(["drives"])
        .output()
        .unwrap();
    assert!(!result.status.success());
    assert!(
        String::from_utf8_lossy(&result.stderr).contains("live SCSI access is forbidden"),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
}

/// Pin the defaults at durable admission through the actual finalize dispatcher.
#[test]
fn finalize_stores_literal_defaults_for_fixed_tape() {
    let mut f = VersionFixture::with_parity(false);
    let tape_uuid = *uuid::Uuid::parse_str("42424242-4242-4242-8242-424242424242")
        .unwrap()
        .as_bytes();
    {
        let mut world = f.world.lock().unwrap();
        let tape = world.tapes.get_mut("FIRST").unwrap();
        let remanence_chaos::model::Record::Block(block) = &mut tape.records[0] else {
            panic!("initialized tape must start with bootstrap");
        };
        let mut bootstrap = remanence_parity::bootstrap::parse_bootstrap_block(block).unwrap();
        bootstrap.tape_uuid = tape_uuid;
        block.fill(0);
        remanence_parity::bootstrap::write_bootstrap_block(&bootstrap, block).unwrap();
        tape.capacity_bytes = 3 * 1024 * 1024 * 1024;
    }
    f.ok(&["tape", "adopt", "--label", "FIXED", "--pool", "default"]);
    f.ok(&["tape", "set-generation", "--generation", "L9"]);
    let source = f.root.path().join("fixed.txt");
    std::fs::write(&source, b"fixed identity").unwrap();
    f.ok(&["put", source.to_str().unwrap()]);
    f.interrupt_finalize = true;
    assert!(
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            f.run(&["tape", "finalize", "--acknowledge-tape", "FIXED"], true)
        }))
        .is_err(),
        "must reach the first terminal media write"
    );
    let config = remanence_state::load_config(f.config()).unwrap();
    let intent = remanence_state::FileCheckpointJournal::open(
        config.journal.dir.join("checkpoints"),
        tape_uuid,
    )
    .unwrap()
    .terminal_finalization_intent()
    .unwrap()
    .expect("durable request");
    let manual = intent.manual.unwrap();
    assert_eq!(manual.reason, "finalized with rem");
    assert_eq!(
        uuid::Uuid::from_bytes(manual.idempotency_key).to_string(),
        "ec179b67-8df5-5ebe-b713-079b6e7ef632"
    );
}

/// Explicit daemon selection must not suggest switching to standalone setup on failure.
#[test]
fn explicit_endpoint_connection_error_has_no_setup_hint() {
    let home = tempfile::tempdir().unwrap();
    let endpoint = format!("unix:{}", home.path().join("absent.sock").display());
    for endpoint_args in [
        vec!["--endpoint".to_string(), endpoint.clone()],
        vec![format!("--endpoint={endpoint}")],
    ] {
        let result = std::process::Command::new(env!("CARGO_BIN_EXE_rem"))
            .env("REM_TEST_NO_LIVE_SCSI", "1")
            .env("HOME", home.path())
            .env("XDG_STATE_HOME", home.path())
            .args(["get", "42424242-4242-4242-8242-424242424242", "--out"])
            .arg(home.path().join("restore"))
            .args(endpoint_args)
            .output()
            .unwrap();
        assert!(!result.status.success());
        let error = String::from_utf8_lossy(&result.stderr);
        assert!(error.contains("connect daemon at"), "{error}");
        assert!(!error.contains("rem setup"), "{error}");
    }
}

/// The public local rebuild command reconstructs tape-level verification from audit authority.
#[test]
fn rebuild_command_restores_tape_verification_record() {
    use ciborium::value::Value;
    use remanence_state::{
        AuditActor, AuditEvent, AuditEventRecord, AuditSubject, FileAuditLog, SourceLayer,
    };
    let f = VersionFixture::with_parity(false);
    let tape = f.index().get_tape_by_voltag("FIRST").unwrap().unwrap();
    let text = std::fs::read_to_string(f.config())
        .unwrap()
        .replace("[journal]\n", "[journal]\nrequire_trusted_volume = false\n");
    std::fs::write(f.config(), text).unwrap();
    let config = remanence_state::load_config(f.config()).unwrap();
    let record = FileAuditLog::open(&config.audit.dir, false)
        .unwrap()
        .append_and_return_record(AuditEventRecord {
            actor: AuditActor::System,
            source_layer: SourceLayer::Layer5,
            operation_id: Some(uuid::Uuid::new_v4()),
            session_id: None,
            idempotency_key: None,
            event: AuditEvent::OperationFinished,
            subject: AuditSubject {
                kind: "tape".into(),
                id: Some(uuid::Uuid::from_slice(&tape.tape_uuid).unwrap().to_string()),
            },
            detail: std::collections::BTreeMap::from([
                (
                    "operation_kind".into(),
                    Value::Text("tape_verification".into()),
                ),
                ("tape_uuid".into(), Value::Bytes(tape.tape_uuid.to_vec())),
                ("verification_state".into(), Value::Integer(2.into())),
                ("edition_digest".into(), Value::Bytes(vec![0x67; 32])),
            ]),
        })
        .unwrap();
    assert!(f
        .index()
        .get_tape(&tape.tape_uuid)
        .unwrap()
        .unwrap()
        .last_verified_at_utc
        .is_none());
    for _ in 0..2 {
        let output = std::process::Command::new(env!("CARGO_BIN_EXE_rem"))
            .env("REM_TEST_NO_LIVE_SCSI", "1")
            .args([
                "rebuild-catalog-from-journals",
                "--config",
                f.config().to_str().unwrap(),
            ])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("rebuild-catalog-from-journals ok")
        );
        let rebuilt = f.index().get_tape(&tape.tape_uuid).unwrap().unwrap();
        assert_eq!(
            rebuilt.last_verified_at_utc.as_deref(),
            Some(record.1.timestamp_utc.as_str())
        );
        assert_eq!(rebuilt.last_verification_state, Some(2));
        assert_eq!(rebuilt.last_verified_edition_digest, Some(vec![0x67; 32]));
    }
}
