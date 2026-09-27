//! Generate/check review-only full-tape digests and damage descriptors.
//! Expected outcomes are copied from the frozen specification-authored source.
use remanence_cli::resume_vectors as resume;
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
    } else if fs::read(&path).ok().as_deref() != Some(bytes) {
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
    if args.first().is_some_and(|arg| arg == "--export-objects") {
        if args.len() != 2 || args[1].starts_with('-') {
            return Err("usage: generate_tape_images --export-objects <directory>".into());
        }
        return export_objects(Path::new(&args[1]));
    }
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
    artifact(
        &root,
        "resume/expected-cases.json",
        resume::EXPECTATIONS.as_bytes(),
        check,
        &mut emitted,
    )?;
    let resume_source: Value = serde_json::from_str(resume::EXPECTATIONS)?;
    let literals = resume::literal_cases();
    assert_eq!(
        literals.len(),
        resume_source["cases"].as_array().unwrap().len()
    );
    let mut resume_manifest = String::from("image\ttape_file\tstart_record\tdata_records\tbytes\tsha256\tfilemark_record\teod_record\n");
    for literal in literals {
        let case: Value = serde_json::from_str(literal)?;
        let id = case["id"].as_str().unwrap();
        artifact(
            &root,
            &format!("resume/{id}/expected.json"),
            literal.as_bytes(),
            check,
            &mut emitted,
        )?;
        if case["portable"] == false {
            continue;
        }
        let vector = generate(case["image"].as_str().unwrap())?;
        let input = resume::portable_input(id, &vector);
        assert_eq!(input["image"], case["image"]);
        assert_eq!(input["W"], case["W"]);
        assert_eq!(input["T"], case["T"]);
        artifact(
            &root,
            &format!("resume/{id}/inputs.json"),
            &serde_json::to_vec_pretty(&input)?,
            check,
            &mut emitted,
        )?;
        if case["expected"]["accepted"] == true {
            let result = resume::execute_positive(&input, &vector)?;
            resume::assert_expected(id, &case["expected"], &result.actual);
            append_manifest(&mut resume_manifest, id, &result.image);
            if id == "resume-open" {
                append_manifest(
                    &mut resume_manifest,
                    "resume-open-uninterrupted",
                    &resume::uninterrupted()?,
                );
            }
        }
    }
    artifact(
        &root,
        "resume/MANIFEST.tsv",
        resume_manifest.as_bytes(),
        check,
        &mut emitted,
    )?;
    artifact(
        &root,
        "resume/README.md",
        resume::README.as_bytes(),
        check,
        &mut emitted,
    )?;
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
        "{}: all six image digests, 25 damage descriptors and nine resume cases",
        if check { "CHECK PASS" } else { "GENERATED" }
    );
    Ok(())
}

/// Export only tape files classified as Objects by the production writer's map.
fn export_objects(directory: &Path) -> Result<(), Box<dyn std::error::Error>> {
    // Reject fixture destinations, including paths redirected through symlinks.
    let fixtures =
        fs::canonicalize(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures"))?;
    let mut destination = PathBuf::new();
    for component in std::env::current_dir()?.join(directory).components() {
        match component {
            std::path::Component::ParentDir => {
                destination.pop();
            }
            std::path::Component::CurDir => {}
            _ => destination.push(component),
        }
        if destination.exists() {
            destination = fs::canonicalize(&destination)?;
        }
        if destination.starts_with(&fixtures) {
            return Err("Object exports must be outside the fixture directory".into());
        }
    }
    fs::create_dir_all(&destination)?;
    for name in IMAGE_NAMES {
        let image_directory = destination.join(name);
        if fs::symlink_metadata(&image_directory).is_ok_and(|m| m.file_type().is_symlink()) {
            return Err("Object export image directory must not be a symlink".into());
        }
        let vector = generate(name)?;
        for entry in vector.written.map.entries() {
            if entry.kind != remanence_parity::TapeFileKind::Object {
                continue;
            }
            let file = vector
                .image
                .files
                .get(usize::try_from(entry.tape_file_number)?)
                .ok_or("writer Object tape file is absent from image")?;
            fs::create_dir_all(&image_directory)?;
            let path = image_directory.join(format!("tape-file-{}.bin", entry.tape_file_number));
            let mut output = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(path)?;
            std::io::Write::write_all(&mut output, &file.bytes)?;
        }
    }
    println!("EXPORTED: Object tape files for all six images");
    Ok(())
}

/// The resume manifest uses exactly the original image manifest's byte stream
/// convention: structural filemarks and EOD, concatenated data bytes for ALL.
fn append_manifest(
    manifest: &mut String,
    name: &str,
    image: &remanence_chaos::model::ExportedTapeImage,
) {
    let mut whole = Sha256::new();
    let mut size = 0;
    let mut records = 0;
    for (index, file) in image.files.iter().enumerate() {
        whole.update(&file.bytes);
        size += file.bytes.len();
        records += file.record_offsets.len();
        manifest.push_str(&format!(
            "{name}\t{index}\t{}\t{}\t{}\t{}\t{}\t{}\n",
            file.start_record,
            file.record_offsets.len(),
            file.bytes.len(),
            hex(&Sha256::digest(&file.bytes)),
            file.filemark_record
                .map_or("none".into(), |n| n.to_string()),
            image.eod_record
        ));
    }
    manifest.push_str(&format!(
        "{name}\tALL\t0\t{records}\t{size}\t{}\tstructural\t{}\n",
        hex(&whole.finalize()),
        image.eod_record
    ));
}
