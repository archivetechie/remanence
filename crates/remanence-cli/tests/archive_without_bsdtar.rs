//! `rem archive` needs bsdtar only to create or unpack a `.remwrap.tar`
//! wrapper. These tests run the binary with a PATH that holds no bsdtar: work
//! without a wrapper succeeds, and work that needs one fails before it writes.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use remanence_aead::RecipientPrivateKey;
use serde_json::Value;

/// Run `rem` with `path_dir` as its whole PATH, or with the inherited PATH.
fn rem(path_dir: Option<&Path>, args: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_rem"));
    if let Some(path_dir) = path_dir {
        command.env("PATH", path_dir);
    }
    command.args(args).output().expect("run rem")
}

fn succeeded(output: &Output) -> Value {
    assert!(
        output.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("json report")
}

fn failed_for_bsdtar(output: &Output) {
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("bsdtar"), "{stderr}");
}

/// Write two recipients' public keys and the first one's private key, the way
/// the CLI's own encrypted tests do, and return (public, public, private).
fn write_recipients(root: &Path) -> (PathBuf, PathBuf, PathBuf) {
    let primary =
        RecipientPrivateKey::new([0x31; 16], "bsdtar-primary".to_string(), [0x51; 32]).unwrap();
    let recovery =
        RecipientPrivateKey::new([0x32; 16], "bsdtar-recovery".to_string(), [0x52; 32]).unwrap();
    let primary_public = root.join("primary.remr");
    let recovery_public = root.join("recovery.remr");
    let primary_private = root.join("primary.remp");
    fs::write(
        &primary_public,
        primary.public_key(0).unwrap().serialize().unwrap(),
    )
    .unwrap();
    fs::write(
        &recovery_public,
        recovery.public_key(1).unwrap().serialize().unwrap(),
    )
    .unwrap();
    fs::write(&primary_private, primary.serialize()).unwrap();
    (primary_public, recovery_public, primary_private)
}

const BUILD_IDS: [&str; 10] = [
    "--chunk-size",
    "4KiB",
    "--object-id",
    "object-bsdtar",
    "--caller-object-id",
    "caller-bsdtar",
    "--manifest-file-id",
    "manifest-bsdtar",
    "--timestamp",
    "2026-01-01T00:00:00Z",
];

#[test]
fn archive_build_scan_and_extract_without_wrappers_do_not_need_bsdtar() {
    let temp = tempfile::tempdir().unwrap();
    let no_tools = temp.path().join("empty-path");
    fs::create_dir_all(&no_tools).unwrap();
    let inputs = temp.path().join("inputs");
    fs::create_dir_all(&inputs).unwrap();
    fs::write(inputs.join("keep.txt"), b"native payload").unwrap();
    let object = temp.path().join("plain.rem-object");
    let dest = temp.path().join("restore");
    let inputs_arg = inputs.to_str().unwrap();

    let scan = succeeded(&rem(
        Some(&no_tools),
        &["archive", "build", "--inputs", inputs_arg, "--scan-only"],
    ));
    assert!(scan["tar_engine"].is_null(), "{scan}");

    let mut build_args = vec![
        "archive",
        "build",
        "--inputs",
        inputs_arg,
        "--out",
        object.to_str().unwrap(),
    ];
    build_args.extend(BUILD_IDS);
    let build = succeeded(&rem(Some(&no_tools), &build_args));
    assert!(build["ingest"]["tar_engine"].is_null(), "{build}");

    let extract = succeeded(&rem(
        Some(&no_tools),
        &[
            "archive",
            "extract",
            "--object",
            object.to_str().unwrap(),
            "--dest",
            dest.to_str().unwrap(),
            "--chunk-size",
            "4KiB",
        ],
    ));
    assert_eq!(extract["unwrap"]["wrappers_unwrapped"], 0);
    assert!(extract["unwrap"]["tar_engine"].is_null(), "{extract}");
    assert_eq!(fs::read(dest.join("keep.txt")).unwrap(), b"native payload");
}

#[test]
fn archive_work_that_needs_a_wrapper_fails_without_bsdtar_before_writing() {
    let temp = tempfile::tempdir().unwrap();
    let no_tools = temp.path().join("empty-path");
    fs::create_dir_all(&no_tools).unwrap();
    let inputs = temp.path().join("inputs");
    fs::create_dir_all(inputs.join("Cache")).unwrap();
    fs::write(inputs.join("keep.txt"), b"native payload").unwrap();
    fs::write(inputs.join("Cache/blob.bin"), b"blob payload").unwrap();
    let rules = temp.path().join("blob.rules");
    fs::write(&rules, "blob Cache/\n").unwrap();
    let object = temp.path().join("wrapped.rem-object");
    let inputs_arg = inputs.to_str().unwrap();
    let rules_arg = rules.to_str().unwrap();

    // The wrapped object itself is built on a host that has bsdtar.
    let mut build_args = vec![
        "archive",
        "build",
        "--inputs",
        inputs_arg,
        "--rules",
        rules_arg,
        "--out",
        object.to_str().unwrap(),
    ];
    build_args.extend(BUILD_IDS);
    let build = succeeded(&rem(None, &build_args));
    assert_eq!(build["ingest"]["tar_engine"]["program"], "bsdtar");

    failed_for_bsdtar(&rem(
        Some(&no_tools),
        &[
            "archive",
            "build",
            "--inputs",
            inputs_arg,
            "--rules",
            rules_arg,
            "--scan-only",
        ],
    ));

    let refused_object = temp.path().join("refused.rem-object");
    let mut refused_args = vec![
        "archive",
        "build",
        "--inputs",
        inputs_arg,
        "--rules",
        rules_arg,
        "--out",
        refused_object.to_str().unwrap(),
    ];
    refused_args.extend(BUILD_IDS);
    failed_for_bsdtar(&rem(Some(&no_tools), &refused_args));
    assert!(!refused_object.exists());

    let dest = temp.path().join("restore");
    failed_for_bsdtar(&rem(
        Some(&no_tools),
        &[
            "archive",
            "extract",
            "--object",
            object.to_str().unwrap(),
            "--dest",
            dest.to_str().unwrap(),
            "--chunk-size",
            "4KiB",
        ],
    ));
    assert!(
        !dest.exists(),
        "extract must fail before it changes the destination"
    );

    // Keeping the wrapper literal never needs bsdtar.
    let literal = temp.path().join("literal");
    let extract = succeeded(&rem(
        Some(&no_tools),
        &[
            "archive",
            "extract",
            "--object",
            object.to_str().unwrap(),
            "--dest",
            literal.to_str().unwrap(),
            "--chunk-size",
            "4KiB",
            "--no-unwrap",
        ],
    ));
    assert_eq!(extract["unwrap"]["enabled"], false);
    assert!(literal.join("Cache.remwrap.tar").exists());
}

#[test]
fn archive_extract_leaves_a_wrapper_it_did_not_restore_and_needs_no_bsdtar() {
    let temp = tempfile::tempdir().unwrap();
    let no_tools = temp.path().join("empty-path");
    fs::create_dir_all(&no_tools).unwrap();
    let inputs = temp.path().join("inputs");
    fs::create_dir_all(&inputs).unwrap();
    fs::write(inputs.join("keep.txt"), b"native payload").unwrap();
    let object = temp.path().join("plain.rem-object");
    let mut build_args = vec![
        "archive",
        "build",
        "--inputs",
        inputs.to_str().unwrap(),
        "--out",
        object.to_str().unwrap(),
    ];
    build_args.extend(BUILD_IDS);
    succeeded(&rem(Some(&no_tools), &build_args));

    // A wrapper an earlier `--no-unwrap` extract left in the destination.
    let dest = temp.path().join("restore");
    fs::create_dir_all(&dest).unwrap();
    fs::write(dest.join("Old.remwrap.tar"), b"left by an earlier extract").unwrap();

    let extract = succeeded(&rem(
        Some(&no_tools),
        &[
            "archive",
            "extract",
            "--object",
            object.to_str().unwrap(),
            "--dest",
            dest.to_str().unwrap(),
            "--chunk-size",
            "4KiB",
        ],
    ));
    assert_eq!(extract["unwrap"]["wrappers_unwrapped"], 0);
    assert!(extract["unwrap"]["tar_engine"].is_null(), "{extract}");
    assert_eq!(fs::read(dest.join("keep.txt")).unwrap(), b"native payload");
    assert_eq!(
        fs::read(dest.join("Old.remwrap.tar")).unwrap(),
        b"left by an earlier extract"
    );
}

#[test]
fn encrypted_archive_needs_bsdtar_only_for_a_wrapper() {
    let temp = tempfile::tempdir().unwrap();
    let no_tools = temp.path().join("empty-path");
    fs::create_dir_all(&no_tools).unwrap();
    let (primary_public, recovery_public, primary_private) = write_recipients(temp.path());
    let recipients = [
        "--recipient",
        primary_public.to_str().unwrap(),
        "--recipient",
        recovery_public.to_str().unwrap(),
    ];
    let private_key = primary_private.to_str().unwrap();

    // Without a wrapper: build and extract both run with no bsdtar.
    let plain_inputs = temp.path().join("plain-inputs");
    fs::create_dir_all(&plain_inputs).unwrap();
    fs::write(plain_inputs.join("keep.txt"), b"sealed native payload").unwrap();
    let plain_object = temp.path().join("plain.rem-object");
    let mut build_args = vec![
        "archive",
        "build",
        "--inputs",
        plain_inputs.to_str().unwrap(),
        "--out",
        plain_object.to_str().unwrap(),
    ];
    build_args.extend(recipients);
    build_args.extend(BUILD_IDS);
    let build = succeeded(&rem(Some(&no_tools), &build_args));
    assert_eq!(build["representation"], "encrypted");
    assert!(build["ingest"]["tar_engine"].is_null(), "{build}");
    let plain_dest = temp.path().join("plain-restore");
    let extract = succeeded(&rem(
        Some(&no_tools),
        &[
            "archive",
            "extract",
            "--object",
            plain_object.to_str().unwrap(),
            "--dest",
            plain_dest.to_str().unwrap(),
            "--private-key",
            private_key,
        ],
    ));
    assert_eq!(extract["representation"], "encrypted");
    assert!(extract["unwrap"]["tar_engine"].is_null(), "{extract}");
    assert_eq!(
        fs::read(plain_dest.join("keep.txt")).unwrap(),
        b"sealed native payload"
    );

    // With a wrapper, built where bsdtar exists: extract fails before it
    // writes, and `--no-unwrap` still succeeds.
    let wrapped_inputs = temp.path().join("wrapped-inputs");
    fs::create_dir_all(wrapped_inputs.join("Cache")).unwrap();
    fs::write(
        wrapped_inputs.join("Cache/blob.bin"),
        b"sealed blob payload",
    )
    .unwrap();
    let rules = temp.path().join("blob.rules");
    fs::write(&rules, "blob Cache/\n").unwrap();
    let wrapped_object = temp.path().join("wrapped.rem-object");
    let mut build_args = vec![
        "archive",
        "build",
        "--inputs",
        wrapped_inputs.to_str().unwrap(),
        "--rules",
        rules.to_str().unwrap(),
        "--out",
        wrapped_object.to_str().unwrap(),
    ];
    build_args.extend(recipients);
    build_args.extend(BUILD_IDS);
    let build = succeeded(&rem(None, &build_args));
    assert_eq!(build["ingest"]["tar_engine"]["program"], "bsdtar");

    let wrapped_dest = temp.path().join("wrapped-restore");
    failed_for_bsdtar(&rem(
        Some(&no_tools),
        &[
            "archive",
            "extract",
            "--object",
            wrapped_object.to_str().unwrap(),
            "--dest",
            wrapped_dest.to_str().unwrap(),
            "--private-key",
            private_key,
        ],
    ));
    assert!(
        !wrapped_dest.exists(),
        "extract must fail before it changes the destination"
    );

    let literal = temp.path().join("wrapped-literal");
    let extract = succeeded(&rem(
        Some(&no_tools),
        &[
            "archive",
            "extract",
            "--object",
            wrapped_object.to_str().unwrap(),
            "--dest",
            literal.to_str().unwrap(),
            "--private-key",
            private_key,
            "--no-unwrap",
        ],
    ));
    assert_eq!(extract["unwrap"]["enabled"], false);
    assert!(literal.join("Cache.remwrap.tar").exists());
}
