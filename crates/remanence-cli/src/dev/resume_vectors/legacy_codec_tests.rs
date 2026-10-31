//! Frozen v4 reference encoder from base 5c43f0bd, isolated from semantic tests.
//! Only opening uses the fixture filesystem-policy bypass. Record encoding is unchanged.
use super::*;
use ciborium::value::Value as Cbor;
use remanence_parity::crc64_xz;
use std::{fs::OpenOptions, io::Write};

#[test]
fn resume_vector_journal_bytes_match_base_including_sidecar_code_and_null_count() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/rem-parity-terminal-index-draft/tape-images/resume");
    let mut cases = std::fs::read_dir(root)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.join("inputs.json").is_file())
        .collect::<Vec<_>>();
    cases.sort();
    assert!(!cases.is_empty());
    for case in cases {
        let input: Value =
            serde_json::from_slice(&std::fs::read(case.join("inputs.json")).unwrap()).unwrap();
        let Some(image) = input["image"].as_str() else {
            continue;
        };
        let vector = crate::tape_image_vectors::generate(image).unwrap();
        for absent in [false, true] {
            let mut input = input.clone();
            if absent {
                input["committed_prefix"][0]["block_count"] = Value::Null;
                input["committed_prefix"][0]["physical_start_override"] = json!(0);
            }
            let directory = tempfile::tempdir().unwrap();
            let expected_path = directory.path().join("base");
            let actual_dir = directory.path().join("current");
            let expected = base_adapt(&input, &vector, &expected_path);
            let actual = adapt(&input, &vector, &actual_dir);
            assert_eq!(actual.is_ok(), expected.is_ok(), "{}", case.display());
            let actual_path =
                TapeRowAuthority::location_in(&actual_dir, vector.written.inputs.tape_uuid);
            assert_eq!(
                std::fs::read(actual_path).unwrap(),
                std::fs::read(expected_path).unwrap(),
                "{}; absent block count: {absent}",
                case.display()
            );
        }
    }
}

fn base_adapt(
    input: &Value,
    vector: &VectorImage,
    path: &Path,
) -> Result<TapeRowAuthority, ParityError> {
    let journal = remanence_parity::authority::fixtures::LegacyCodecFixture::at_path(
        path,
        vector.written.inputs.tape_uuid,
        BLOCK,
        vector.written.inputs.scheme.clone(),
    )
    .build()?;
    let mut file = OpenOptions::new()
        .append(true)
        .open(path)
        .map_err(JournalError::from)?;
    let uint = |n: u64| Cbor::Integer(n.into());
    let optional = |v: &Value| v.as_u64().map_or(Cbor::Null, &uint);
    let mut w = 0;
    let mut t = 0;
    let mut lba = 0;
    for row in input["committed_prefix"].as_array().unwrap() {
        let f = row["tape_file_number"].as_u64().unwrap();
        if let Some(physical) = row["physical_start_override"].as_u64() {
            lba = physical;
        }
        let (kind, bundle) = match row["kind"].as_str().unwrap() {
            "Bootstrap" => (2, 1),
            "Object" => (0, 0),
            "ParitySidecar" => (1, 3),
            // The final ParityMap commits in the reference's TerminalPrefix
            // bundle, its hash the payload SHA-256 of its header.
            "ParityMap" => (3, 5),
            other => panic!("unsupported resume row {other}"),
        };
        if let Some(first) = row["first_parity_data_ordinal"].as_u64() {
            // A hostile claim whose end does not fit the journal's u64
            // watermark leaves the watermark at the last claim that fits; the
            // row's own block count is recorded unchanged. Honest claims
            // always fit.
            if let Some(end) = first.checked_add(row["block_count"].as_u64().unwrap()) {
                t = t.max(end);
            }
        }
        if let Some(end) = row["protected_ordinal_end_exclusive"].as_u64() {
            w = w.max(end);
        }
        let hash = if kind == 3 {
            Cbor::Bytes(vector.image.files[f as usize].bytes[0x38..0x58].to_vec())
        } else {
            vector
                .written
                .sidecars
                .iter()
                .find(|s| s.tape_file_number == f)
                .map_or(Cbor::Null, |s| {
                    Cbor::Bytes(s.canonical_metadata_hash.to_vec())
                })
        };
        let entry = Cbor::Map(vec![
            (uint(1), uint(f)),
            (uint(2), uint(kind)),
            (uint(3), optional(&row["block_count"])),
            (uint(4), uint(lba)),
            (uint(5), Cbor::Null),
            (uint(6), optional(&row["first_parity_data_ordinal"])),
            (uint(7), optional(&row["epoch_id"])),
            (uint(8), optional(&row["protected_ordinal_start"])),
            (uint(9), optional(&row["protected_ordinal_end_exclusive"])),
            (uint(10), hash),
            (uint(11), Cbor::Null),
        ]);
        frame(&mut file, bundle, vec![entry], w, t)?;
        // Supplemental hostile commit claims retain measured starts, avoiding
        // arithmetic on their block counts inside the reference adapter.
        if row["physical_start_override"].is_null() {
            lba += row["block_count"].as_u64().unwrap() + 1;
        }
    }
    frame(
        &mut file,
        4,
        vec![],
        input["W"].as_u64().unwrap(),
        input["T"].as_u64().unwrap(),
    )?;
    file.sync_all().map_err(JournalError::from)?;
    drop(file);
    drop(journal);
    resume_record_result(
        remanence_parity::authority::fixtures::LegacyCodecFixture::at_path(
            path,
            vector.written.inputs.tape_uuid,
            BLOCK,
            vector.written.inputs.scheme.clone(),
        )
        .build(),
    )
}

fn frame(
    file: &mut std::fs::File,
    kind: u64,
    entries: Vec<Cbor>,
    w: u64,
    t: u64,
) -> Result<(), ParityError> {
    let uint = |n: u64| Cbor::Integer(n.into());
    let value = Cbor::Map(vec![
        (uint(1), uint(kind)),
        (uint(2), Cbor::Array(entries)),
        (uint(3), uint(w)),
        (uint(4), uint(t)),
    ]);
    let mut bytes = Vec::new();
    ciborium::into_writer(&value, &mut bytes).map_err(|e| JournalError::Codec(e.to_string()))?;
    file.write_all(&u32::try_from(bytes.len()).unwrap().to_le_bytes())
        .map_err(JournalError::from)?;
    file.write_all(&bytes).map_err(JournalError::from)?;
    file.write_all(&crc64_xz(&bytes).to_le_bytes())
        .map_err(JournalError::from)?;
    Ok(())
}
