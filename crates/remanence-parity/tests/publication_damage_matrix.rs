//! Execute the published damage-matrix vectors against the real Recoverer.
//!
//! The publication audit of 2026-07-31 found that nothing in the tree ever ran
//! a damage-matrix cell through the implementation. `verify_publication_test_vectors.py`
//! checks the *shape* of each manifest — fault model, non-empty indices, burst
//! geometry — and `rem_parity_rederive.py` verifies parity arithmetic from the
//! *undamaged* source artifact. Neither applies a fault and asks the Recoverer
//! what it does. As a result the `sidecar-footer-and-primary` cell asserted an
//! outcome ("directory-assisted tail rescue succeeds") that the implementation
//! could not produce, and no gate noticed.
//!
//! This test closes that hole for the cells whose source artifact is a tape file
//! of the pinned `minimal-image`. It resolves each cell's `source-artifact.bin`
//! to its tape file by SHA-256 match — the artifact is a single tape file, not a
//! whole image, so the fault indices are block offsets *within* that file —
//! then marks those blocks unreadable on a source opened over the whole image
//! directory, and drives the real scan and recovery path.
//!
//! The remaining cells use other images (burst geometries, the multi-parity-map
//! selection fixture) and are not covered here; extending the resolver to them
//! is follow-up work, and the assertion below states how many cells ran so a
//! silent reduction in coverage cannot pass unnoticed.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use remanence_parity::{
    acquire_filemark_map_with_report, bootstrap::parse_bootstrap_block,
    recover_ordinal_from_sidecar, ImageDirectoryRawSource, ParityError,
};
use sha2::{Digest, Sha256};

const BLOCK_SIZE: u32 = 4096;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crate sits two levels below the repository root")
        .to_path_buf()
}

/// Unpack the pinned archive into a temporary directory once per test run.
fn extract_archive() -> PathBuf {
    let tar = repo_root().join("specs/publication/remanence-test-vectors.tar");
    assert!(tar.is_file(), "pinned vector archive is missing: {tar:?}");
    let out = std::env::temp_dir().join(format!("rem-damage-matrix-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&out);
    std::fs::create_dir_all(&out).expect("create extraction directory");
    let status = std::process::Command::new("tar")
        .arg("xf")
        .arg(&tar)
        .arg("-C")
        .arg(&out)
        .status()
        .expect("run tar");
    assert!(status.success(), "extracting the pinned archive failed");
    out
}

fn sha256(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Map each tape file of an image directory to its SHA-256, so a cell's
/// single-tape-file `source-artifact.bin` can be resolved to a tape file number.
fn image_tape_file_digests(image: &Path) -> BTreeMap<String, u32> {
    let mut out = BTreeMap::new();
    for entry in std::fs::read_dir(image).expect("read image directory") {
        let path = entry.expect("directory entry").path();
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();
        if !name.starts_with("tape-file-") || !name.ends_with(".bin") {
            continue;
        }
        let digits: String = name["tape-file-".len()..]
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .collect();
        let number: u32 = digits
            .parse()
            .unwrap_or_else(|_| panic!("tape file number in {name}"));
        let bytes = std::fs::read(&path).expect("read tape file");
        out.insert(sha256(&bytes), number);
    }
    out
}

fn json_field<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    let needle = format!("\"{key}\"");
    let start = text.find(&needle)? + needle.len();
    let rest = &text[start..];
    let colon = rest.find(':')? + 1;
    let rest = &rest[colon..];
    let quote = rest.find('"')?;
    let rest2 = &rest[quote + 1..];
    let end = rest2.find('"')?;
    Some(&rest2[..end])
}

fn json_u64_list(text: &str, key: &str) -> Vec<u64> {
    let needle = format!("\"{key}\"");
    let Some(start) = text.find(&needle) else {
        return Vec::new();
    };
    let rest = &text[start..];
    let Some(open) = rest.find('[') else {
        return Vec::new();
    };
    let Some(close) = rest.find(']') else {
        return Vec::new();
    };
    rest[open + 1..close]
        .split(',')
        .filter_map(|piece| piece.trim().parse::<u64>().ok())
        .collect()
}

#[test]
fn published_damage_matrix_cells_execute_against_the_recoverer() {
    let root = extract_archive();
    let matrix = root.join("rem-parity-1/damage-matrix");
    let image = root.join("rem-parity-1/positive/minimal-image");
    assert!(matrix.is_dir(), "damage-matrix directory missing");
    assert!(image.is_dir(), "minimal-image directory missing");

    let digests = image_tape_file_digests(&image);
    // The final bootstrap is the authority: the BOT copy carries an empty
    // validated prefix, so recovery of any ordinal would be out of scope.
    let bootstrap = std::fs::read(image.join("tape-file-003-final-bootstrap.bin"))
        .expect("read the final bootstrap");
    let payload = parse_bootstrap_block(&bootstrap[..BLOCK_SIZE as usize])
        .expect("the pinned final bootstrap parses");

    // Recover under the scheme the tape actually records, not the build default.
    let record = payload
        .scheme
        .as_ref()
        .expect("the pinned image is parity-protected");
    let scheme = remanence_parity::ParityScheme {
        id: remanence_parity::SchemeId::new_owned(record.id.clone()),
        data_blocks_per_stripe: record.data_blocks_per_stripe,
        parity_blocks_per_stripe: record.parity_blocks_per_stripe,
        stripes_per_neighborhood: record.stripes_per_neighborhood,
    };

    let mut executed = Vec::new();
    let mut cells: Vec<_> = std::fs::read_dir(&matrix)
        .expect("read damage matrix")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_dir())
        .collect();
    cells.sort();

    for cell in cells {
        let name = cell
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default()
            .to_string();
        let source_artifact = cell.join("source-artifact.bin");
        let Ok(artifact) = std::fs::read(&source_artifact) else {
            continue;
        };
        // Only cells whose artifact is a tape file of the minimal image are
        // resolvable here; the rest use other pinned images.
        let Some(&tape_file_number) = digests.get(&sha256(&artifact)) else {
            continue;
        };

        let fault = std::fs::read_to_string(cell.join("fault-map.json")).expect("fault map");
        let expected = std::fs::read_to_string(cell.join("expected.json")).expect("expected");
        let indices = json_u64_list(&fault, "unreadable_block_indices");
        assert!(
            !indices.is_empty(),
            "{name}: fault map declares no unreadable blocks"
        );

        let mut source = ImageDirectoryRawSource::open(&image).expect("open the pinned image");
        for index in &indices {
            source
                .mark_unreadable(tape_file_number, *index)
                .unwrap_or_else(|e| panic!("{name}: mark block {index} unreadable: {e}"));
        }

        let report = acquire_filemark_map_with_report(&mut source, &payload, None);
        let outcome = json_field(&expected, "expected_outcome").unwrap_or_default();

        match report {
            Ok(report) => {
                // Every cell in this set declares whole_tape_failure false, so a
                // map must be obtainable under the declared damage.
                assert!(
                    expected.contains("\"whole_tape_failure\": false"),
                    "{name}: recovered a map for a cell that declares whole-tape failure"
                );

                if let Some(ordinal) = json_u64_list(&expected, "recovery_target_ordinal")
                    .first()
                    .copied()
                    .or_else(|| {
                        expected
                            .find("\"recovery_target_ordinal\"")
                            .and_then(|i| expected[i..].split(':').nth(1))
                            .and_then(|s| {
                                s.trim()
                                    .trim_end_matches(',')
                                    .lines()
                                    .next()
                                    .and_then(|v| v.trim().trim_end_matches(',').parse().ok())
                            })
                    })
                {
                    let recovered = recover_ordinal_from_sidecar(
                        &mut source,
                        &report.scoped_map,
                        &scheme,
                        payload.tape_uuid,
                        BLOCK_SIZE,
                        ordinal,
                    );
                    match recovered {
                        Ok(result) => {
                            if let Some(pinned) = json_field(&expected, "recovered_block_sha256") {
                                assert_eq!(
                                    sha256(&result.recovered_block),
                                    pinned,
                                    "{name}: recovered block does not match the pinned digest"
                                );
                            }
                        }
                        Err(err) => panic!(
                            "{name}: expected outcome {outcome:?} but recovery failed: {err}"
                        ),
                    }
                }
            }
            Err(ParityError::DriveCompressionEnabled) => {
                panic!("{name}: unexpected compression refusal")
            }
            Err(err) => panic!("{name}: expected outcome {outcome:?} but the scan failed: {err}"),
        }
        executed.push(name);
    }

    // The audit resolved five cells to minimal-image tape files. If that number
    // falls, coverage has silently shrunk and this gate must be re-examined
    // rather than quietly passing on fewer cells.
    assert!(
        executed.len() >= 5,
        "expected at least 5 executable damage-matrix cells, ran {}: {executed:?}",
        executed.len()
    );
    assert!(
        executed.iter().any(|n| n == "sidecar-footer-and-primary"),
        "the directory-assisted tail-rescue cell must be among those executed: {executed:?}"
    );
}
