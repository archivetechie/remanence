//! Pin the compatibility boundary between the published generation-1 vectors
//! and the generation-2 Recoverer shipped on `main`.
//!
//! The published damage matrix cannot currently exercise the Recoverer: its
//! authority bootstraps use `schema_major = 1`, while the implementation reads
//! only generation 2.  This test therefore asserts that incompatibility
//! explicitly.  It must be replaced by an executing damage-matrix gate when
//! generation-2 vectors become the publication baseline.

use std::path::{Path, PathBuf};

use remanence_parity::{
    bootstrap::{parse_bootstrap_block, BOOTSTRAP_SCHEMA_MAJOR},
    ParityError,
};

const BLOCK_SIZE: usize = 4096;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crate sits two levels below the repository root")
        .to_path_buf()
}

/// Unpack the pinned archive into a temporary directory.
fn extract_archive() -> tempfile::TempDir {
    let tar = repo_root().join("specs/publication/remanence-test-vectors.tar");
    assert!(tar.is_file(), "pinned vector archive is missing: {tar:?}");
    let out = tempfile::tempdir().expect("create extraction directory");
    let status = std::process::Command::new("tar")
        .arg("xf")
        .arg(&tar)
        .arg("-C")
        .arg(out.path())
        .status()
        .expect("run tar");
    assert!(status.success(), "extracting the pinned archive failed");
    out
}

#[test]
fn published_generation_1_authority_is_explicitly_rejected_by_generation_2_reader() {
    assert_eq!(
        BOOTSTRAP_SCHEMA_MAJOR, 2,
        "this compatibility assertion must be revisited when the reader changes generation"
    );

    let root = extract_archive();
    let image = root.path().join("rem-parity-1/positive/minimal-image");
    assert!(image.is_dir(), "published minimal image is missing");

    for name in [
        "tape-file-000-bootstrap.bin",
        "tape-file-003-final-bootstrap.bin",
    ] {
        let bytes = std::fs::read(image.join(name)).expect("read published bootstrap");
        let major = u16::from_be_bytes(
            bytes[8..10]
                .try_into()
                .expect("published bootstrap has schema-major bytes"),
        );
        assert_eq!(major, 1, "{name} is no longer a generation-1 vector");

        let error = parse_bootstrap_block(
            bytes
                .get(..BLOCK_SIZE)
                .expect("published bootstrap is at least one block"),
        )
        .expect_err("the generation-2 reader must reject generation-1 authority");
        assert!(
            matches!(&error, ParityError::BootstrapParse(_)),
            "{name}: expected typed bootstrap rejection, got {error}"
        );
        assert!(
            !error.to_string().is_empty(),
            "{name}: typed rejection must retain diagnostic context"
        );
    }
}
