//! Generate/check review-only full-tape digests and damage descriptors.
//! Expected outcomes are copied from the frozen specification-authored source.
use remanence_cli::tape_image_vectors::{fault_map, generate, hex, EXPECTATIONS, IMAGE_NAMES};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
};

fn artifact(
    root: &Path,
    relative: &str,
    bytes: &[u8],
    check: bool,
    emitted: &mut BTreeSet<PathBuf>,
) -> Result<(), Box<dyn std::error::Error>> {
    emitted.insert(PathBuf::from(relative));
    let path = root.join(relative);
    if check {
        if fs::read(&path).map_err(|error| format!("fixture {relative}: {error}"))? != bytes {
            return Err(format!("fixture differs: {relative}").into());
        }
    } else {
        fs::create_dir_all(path.parent().unwrap())?;
        fs::write(path, bytes)?;
    }
    Ok(())
}

/// Collect relative file paths so check mode rejects stale artifacts as well.
fn files_under(
    root: &Path,
    directory: &Path,
    files: &mut BTreeSet<PathBuf>,
) -> std::io::Result<()> {
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            files_under(root, &entry.path(), files)?;
        } else {
            files.insert(entry.path().strip_prefix(root).unwrap().to_path_buf());
        }
    }
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a != "--check") {
        return Err("usage: generate_tape_images [--check]".into());
    }
    let check = args.iter().any(|a| a == "--check");
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/rem-parity-terminal-index-draft/tape-images");
    let mut emitted = BTreeSet::from([PathBuf::from("expected-cases.json")]);
    let source: Value = serde_json::from_str(EXPECTATIONS)?;
    // Preserve each source case's literal JSON rather than synthesizing outcomes.
    let literal_cases: BTreeMap<String, &str> = EXPECTATIONS
        .lines()
        .filter_map(|line| {
            let text = line.trim().trim_end_matches(',');
            let value: Value = serde_json::from_str(text).ok()?;
            Some((value.get("id")?.as_str()?.to_string(), text))
        })
        .collect();
    assert_eq!(
        literal_cases.len(),
        source["cases"].as_array().unwrap().len()
    );
    let mut manifest = String::from("image\ttape_file\tstart_record\tdata_records\tbytes\tsha256\tfilemark_record\teod_record\n");
    for name in IMAGE_NAMES {
        let vector = generate(name)?;
        let mut whole = Sha256::new();
        let mut size = 0;
        for (index, file) in vector.image.files.iter().enumerate() {
            whole.update(&file.bytes);
            size += file.bytes.len();
            manifest.push_str(&format!(
                "{name}\t{index}\t{}\t{}\t{}\t{}\t{}\t{}\n",
                file.start_record,
                file.record_offsets.len(),
                file.bytes.len(),
                hex(&Sha256::digest(&file.bytes)),
                file.filemark_record
                    .map_or("none".into(), |n| n.to_string()),
                vector.image.eod_record
            ));
        }
        manifest.push_str(&format!(
            "{name}\tALL\t0\t{}\t{size}\t{}\tstructural\t{}\n",
            vector
                .image
                .files
                .iter()
                .map(|f| f.record_offsets.len())
                .sum::<usize>(),
            hex(&whole.finalize()),
            vector.image.eod_record
        ));
        artifact(
            &root,
            &format!("inputs/{name}.json"),
            &serde_json::to_vec_pretty(&vector.recipe)?,
            check,
            &mut emitted,
        )?;
        // The source's image table is the image expectation, never writer output.
        artifact(
            &root,
            &format!("images/{name}/expected.json"),
            &serde_json::to_vec_pretty(&source["images"][name])?,
            check,
            &mut emitted,
        )?;
        for case in source["cases"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|c| c["image"] == name)
        {
            let id = case["id"].as_str().unwrap();
            artifact(
                &root,
                &format!("cases/{id}/expected.json"),
                literal_cases[id].as_bytes(),
                check,
                &mut emitted,
            )?;
            artifact(
                &root,
                &format!("cases/{id}/fault-map.json"),
                &serde_json::to_vec_pretty(&fault_map(case, &vector.image))?,
                check,
                &mut emitted,
            )?;
        }
    }
    artifact(
        &root,
        "MANIFEST.tsv",
        manifest.as_bytes(),
        check,
        &mut emitted,
    )?;
    artifact(&root, "README.md", b"# Full-tape review candidates\n\nReview-only REM-PARITY generation-2 fixtures. These are not publication artifacts. Image bytes are pinned by size and SHA-256 per tape file and for the concatenation of all data records in MANIFEST.tsv. Filemarks and EOD are structural expectations, not bytes in those streams; an unterminated tail has no filemark. Image streams are regenerated, never checked in. REM-PARITY's companion archive will carry the bytes at freeze.\n\nRun `cargo run -p remanence-cli --example generate_tape_images` to regenerate metadata, or append `-- --check` to compare every digest and descriptor. Inputs record the complete byte-deciding recipe, including repeat-byte payloads, REM-OBJECT options, diagnostics, checkpoints and stop points. The second edition uses the same recipe except its explicit edition id and sequence, and replaces only replica B.\n\nexpected-cases.json is the frozen specification-authored source. Per-case expected.json preserves its case verbatim, including pinned and sections. Never derive expectations from executor results. The executor runs as damage_vectors in the workspace suite; `cargo test -p remanence-cli --lib damage_vectors -- --nocapture` reports every outcome. Copy-health annotations and note/informative fields are informative. Unpinned cases execute but do not decide a pass. Disagreements remain failing pending specification review. Fault maps include physical and file-relative addresses; failed data addresses also produce real medium errors.\n", check, &mut emitted)?;
    if check {
        let mut actual = BTreeSet::new();
        files_under(&root, &root, &mut actual)?;
        let extra: Vec<_> = actual.difference(&emitted).collect();
        let missing: Vec<_> = emitted.difference(&actual).collect();
        if !extra.is_empty() || !missing.is_empty() {
            return Err(
                format!("fixture file set differs: extra={extra:?}, missing={missing:?}").into(),
            );
        }
    }
    println!(
        "{}: all six image digests and 25 case descriptors",
        if check { "CHECK PASS" } else { "GENERATED" }
    );
    Ok(())
}
