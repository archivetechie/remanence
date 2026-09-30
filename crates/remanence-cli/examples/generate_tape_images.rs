//! Generate/check review-only full-tape digests and damage descriptors.
//! Expected outcomes are copied from the frozen specification-authored source.
use remanence_cli::negative_vectors as negatives;
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
        if path.exists()
            && (!relative.starts_with("negatives/")
                || relative.ends_with("expected.json")
                || relative.ends_with("negative-cases.json")
                || relative.ends_with("negative-cases-supplement.json")
                || relative.ends_with("adjudications.json")
                || relative.ends_with("isolation-exceptions.json"))
        {
            return Err(format!("existing fixture differs; refusing to change {relative}").into());
        }
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
    let mut args: Vec<_> = std::env::args().skip(1).collect();
    let supplement_path = if let Some(i) = args.iter().position(|s| s == "--negative-supplement") {
        if i + 1 >= args.len() {
            return Err("--negative-supplement requires a path".into());
        }
        let path = PathBuf::from(args.remove(i + 1));
        args.remove(i);
        Some(path)
    } else {
        None
    };
    if args.first().is_some_and(|arg| arg == "--export-objects") {
        if args.len() != 2 || args[1].starts_with('-') {
            return Err("usage: generate_tape_images --export-objects <directory>".into());
        }
        return export_objects(Path::new(&args[1]));
    }
    if args.first().is_some_and(|arg| arg == "--export-images") {
        if args.len() != 2 || args[1].starts_with('-') {
            return Err("usage: generate_tape_images --export-images <directory>".into());
        }
        let root = Path::new(&args[1]);
        for name in IMAGE_NAMES {
            let vector = generate(name)?;
            let directory = root.join(name);
            fs::create_dir_all(&directory)?;
            for (index, file) in vector.image.files.iter().enumerate() {
                let suffix = if file.filemark_record.is_some() {
                    ""
                } else {
                    "-torn"
                };
                fs::write(
                    directory.join(format!("tape-file-{index:03}{suffix}.bin")),
                    &file.bytes,
                )?;
            }
        }
        println!("exported {} tape images", IMAGE_NAMES.len());
        return Ok(());
    }
    let (check, source_path) = match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        [] => (false, None),
        ["--check"] => (true, None),
        ["--negative-cases", path] => (false, Some(PathBuf::from(path))),
        ["--check", "--negative-cases", path] => (true, Some(PathBuf::from(path))),
        _ => {
            return Err("usage: generate_tape_images [--check] [--negative-cases <source>] [--negative-supplement <source>]".into())
        }
    };
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/rem-parity-terminal-index-draft/tape-images");
    // Erratum set E4's overlay for existing damage cases is a source, like the
    // expected cases; it is read by the executor and never generated.
    let mut emitted = BTreeSet::from([
        PathBuf::from("expected-cases.json"),
        PathBuf::from("expected-e4.json"),
    ]);
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
    artifact(&root, "README.md", b"# Full-tape review candidates\n\nReview-only REM-PARITY generation-2 fixtures. These are not publication artifacts. Image bytes are pinned by size and SHA-256 per tape file and for the concatenation of all data records in MANIFEST.tsv. Filemarks and EOD are structural expectations, not bytes in those streams; an unterminated tail has no filemark. Image streams are regenerated, never checked in. REM-PARITY's companion archive will carry the bytes at freeze.\n\nRun `cargo run -p remanence-cli --example generate_tape_images` to regenerate metadata, or append `-- --check` to compare every digest and descriptor. Inputs record the complete byte-deciding recipe, including repeat-byte payloads, REM-OBJECT options, diagnostics, checkpoints and stop points. The second edition uses the same recipe except its explicit edition id and sequence, and replaces only replica B.\n\nexpected-cases.json is the frozen specification-authored source. Per-case expected.json preserves its case verbatim, including pinned and sections. Never derive expectations from executor results. The executor runs as damage_vectors in the workspace suite; `cargo test -p remanence-cli --lib damage_vectors -- --nocapture` reports every outcome. Copy-health annotations and note/informative fields are informative. Unpinned cases execute but do not decide a pass. Disagreements remain failing pending specification review. Fault maps include physical and file-relative addresses; failed data addresses also produce real medium errors. Record faults replace one record by a record of a stated length (the original's leading bytes, or the original followed by zero bytes) and edit bytes within it, rebuilding a bootstrap's payload and recomputing or leaving stale its two CRCs as the case states; the fault map records every resolved edit with its old and new bytes. A sidecar copy's canonical metadata hash and CRCs can be recomputed after an edit (`sidecar_hash`, `sidecar_crcs`), and a ParityMap directory entry can be changed with the ParityMap re-encoded through the production codec (`parity_map_edits`), so every hash and CRC it carries is recomputed. A tape file can gain an extra record after its last record (`extra_records`, resolved to `record_insertions`), a filemark can be removed (`removed_filemark_after_tape_file`), and tape files can be appended after the image's last filemark (`appended_files`, each a list of foreign records, zeros with a stated first byte, or byte copies of a stated record, with or without a trailing filemark), or a replica A of a second edition planned as a later tape file (`second_edition_replica`); every such key is explicit, and an unknown key fails a run. A case with observations runs the reader once per observation, each with its own supplied values or none. Such a case's expected outcome gives each observation's outcome, written from the specification text alone; one that reads pending, or that the executor does not know, fails the case.\n\nS6 cases s6-01 through s6-14 pin the independent section 12.3 expectations verbatim. Cases s6-13 and s6-14 cover an unreadable ParityMap head with an intact or CRC-invalid footer, respectively; their expected entries preserve the independent S6b author verbatim. Unreadable-head damage reports remain unconstrained, as the author notes. Each `checks` entry transcribes their observable requirements; arrays pin the permitted sets (s6-04 artifact present or absent, s6-08 either directory error name, s6-09 footer rejection or validated directory). Unreadable LBAs refer to the modified tape after filemark removal; their resolved file/record coordinates describe that tape. Record edits retain original-image file/record coordinates to identify source bytes, with their resolved LBA rebased after removal. Both walk and recovery observations use supplied values when declared.\n", check, &mut emitted)?;
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
    let negative_source =
        fs::read(source_path.unwrap_or_else(|| root.join("negatives/negative-cases.json")))?;
    let parsed_negatives = negatives::parse_source(&negative_source)?;
    artifact(
        &root,
        "negatives/negative-cases.json",
        &negative_source,
        check,
        &mut emitted,
    )?;
    let adjudications = fs::read(root.join("negatives/adjudications.json"))?;
    negatives::parse_adjudications(&adjudications)?;
    artifact(
        &root,
        "negatives/adjudications.json",
        &adjudications,
        check,
        &mut emitted,
    )?;
    let isolation_exceptions = fs::read(root.join("negatives/isolation-exceptions.json"))?;
    negatives::parse_isolation_exceptions(&isolation_exceptions)?;
    artifact(
        &root,
        "negatives/isolation-exceptions.json",
        &isolation_exceptions,
        check,
        &mut emitted,
    )?;
    // Erratum set E2 overrides pinned expectations without editing the frozen
    // sources; it also makes the cases it unblocks executable.
    // Erratum set E4 replaces E2's expectation for the vectors that the owner's
    // Q9 and Q10 rulings decide; each set is its own file.
    let mut errata = negatives::erratum::Errata::default();
    for path in negatives::erratum::paths() {
        let bytes = fs::read(&path)?;
        let set = negatives::erratum::parse(&bytes)?;
        errata.entries.extend(set.entries);
        artifact(
            &root,
            &format!("negatives/{}", path.file_name().unwrap().to_string_lossy()),
            &bytes,
            check,
            &mut emitted,
        )?;
    }
    let negative_source = parsed_negatives;
    let mut negative_manifest =
        String::from("case\tartifact\ttape_file\tblock_within_file\tbytes\tsha256\n");
    let mut unresolved = Vec::new();
    for (case, variant) in negatives::cases(&negative_source) {
        let path = negatives::case_path(case, variant);
        artifact(
            &root,
            &format!("negatives/{path}/expected.json"),
            &serde_json::to_vec_pretty(variant.unwrap_or(case))?,
            check,
            &mut emitted,
        )?;
        if let Some(reason) = errata.not_executable(case) {
            println!("NOT-EXECUTABLE {path}: {reason}");
            continue;
        }
        match negatives::resolve(case, variant) {
            Ok(vector) => {
                artifact(
                    &root,
                    &format!("negatives/{path}/mutation.json"),
                    &serde_json::to_vec_pretty(&vector.descriptor)?,
                    check,
                    &mut emitted,
                )?;
                negative_manifest.push_str(&vector.manifest);
            }
            Err(error) => {
                eprintln!("UNRESOLVED {path}: {error}");
                unresolved.push(path);
            }
        }
    }
    let supplement_bytes = fs::read(
        supplement_path.unwrap_or_else(|| root.join("negatives/negative-cases-supplement.json")),
    )?;
    let supplement = negatives::supplement::parse_source(&supplement_bytes)?;
    errata.check_against_sources(&negative_source, &supplement)?;
    artifact(
        &root,
        "negatives/negative-cases-supplement.json",
        &supplement_bytes,
        check,
        &mut emitted,
    )?;
    for variant in supplement["variants"]
        .as_array()
        .ok_or("supplement variants missing")?
    {
        let path = variant["id"].as_str().ok_or("supplement id missing")?;
        artifact(
            &root,
            &format!("negatives/{path}/expected.json"),
            &serde_json::to_vec_pretty(variant)?,
            check,
            &mut emitted,
        )?;
        match negatives::supplement::resolve(variant) {
            Ok(vector) => {
                artifact(
                    &root,
                    &format!("negatives/{path}/mutation.json"),
                    &serde_json::to_vec_pretty(&vector.descriptor)?,
                    check,
                    &mut emitted,
                )?;
                negative_manifest.push_str(&vector.manifest);
            }
            Err(e) => {
                eprintln!("UNRESOLVED {path}: {e}");
                unresolved.push(path.to_string());
            }
        }
    }
    // Erratum set E1's negative constructions; their expectations are pending.
    let erratum_bytes = fs::read(root.join("negatives/negative-cases-e1.json"))?;
    let erratum = negatives::parse_erratum_source(&erratum_bytes)?;
    artifact(
        &root,
        "negatives/negative-cases-e1.json",
        &erratum_bytes,
        check,
        &mut emitted,
    )?;
    for case in erratum["cases"].as_array().ok_or("erratum cases missing")? {
        let path = negatives::case_path(case, None);
        artifact(
            &root,
            &format!("negatives/{path}/expected.json"),
            &serde_json::to_vec_pretty(case)?,
            check,
            &mut emitted,
        )?;
        match negatives::resolve(case, None) {
            Ok(vector) => {
                artifact(
                    &root,
                    &format!("negatives/{path}/mutation.json"),
                    &serde_json::to_vec_pretty(&vector.descriptor)?,
                    check,
                    &mut emitted,
                )?;
                negative_manifest.push_str(&vector.manifest);
            }
            Err(e) => {
                eprintln!("UNRESOLVED {path}: {e}");
                unresolved.push(path);
            }
        }
    }
    artifact(
        &root,
        "negatives/MANIFEST.tsv",
        negative_manifest.as_bytes(),
        check,
        &mut emitted,
    )?;
    if !unresolved.is_empty() {
        return Err(format!("unresolved negative cases: {unresolved:?}").into());
    }
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
        "{}: all six image digests, {} damage descriptors and {} resume cases; all 7a/7b/7c negative descriptors, {} supplement variants, {} erratum E1 negative cases and erratum E2's {} overrides",
        if check { "CHECK PASS" } else { "GENERATED" },
        source["cases"].as_array().unwrap().len(),
        resume_source["cases"].as_array().unwrap().len(),
        supplement["variants"].as_array().unwrap().len(),
        erratum["cases"].as_array().unwrap().len(),
        errata
            .entries
            .iter()
            .map(|e| e.observations.len())
            .sum::<usize>()
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
