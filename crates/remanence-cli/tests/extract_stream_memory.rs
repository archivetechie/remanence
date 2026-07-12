//! Process-level memory regression test for ranged-ciphertext AEAD extraction.
//!
//! The test seals a 64 MiB object without materializing it, runs the real
//! `rem archive extract-stream` binary on one small mid-object member, and
//! samples Linux peak RSS while asserting that only covering frames are input.

#![cfg(target_os = "linux")]

use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom, Write};
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

use remanence_aead::{
    covering_stored_range, seal, RaoHeader, RootKey, SealOptions, RAO_HEADER_LEN,
};
use sha2::{Digest, Sha256};

const PLAINTEXT_SIZE: u64 = 64 * 1024 * 1024;
const CHUNK_SIZE: u32 = 256 * 1024;
const MAX_PEAK_RSS_KIB: u64 = 48 * 1024;
const MEMBER_START: u64 = 31 * 1024 * 1024 + 17;
const MEMBER_LEN: u64 = 4096;

#[test]
fn ranged_extract_stream_peak_rss_is_bounded_by_covering_chunks() {
    let temp = tempfile::tempdir().unwrap();
    let key_path = temp.path().join("root.key");
    fs::write(&key_path, [0x71; 32]).unwrap();
    let encrypted_path = temp.path().join("large.rao");

    let pattern = vec![0xA5; CHUNK_SIZE as usize];
    let mut hasher = Sha256::new();
    for _ in 0..PLAINTEXT_SIZE / u64::from(CHUNK_SIZE) {
        hasher.update(&pattern);
    }
    let plaintext_digest: [u8; 32] = hasher.finalize().into();
    let root_key = RootKey::new([0x71; 32]).unwrap();
    let options = SealOptions {
        chunk_size: CHUNK_SIZE,
        key_id: [0x72; 16],
        object_id: "extract-stream-memory-test".to_string(),
        plaintext_size: PLAINTEXT_SIZE,
        plaintext_digest,
    };
    let mut encrypted = File::create(&encrypted_path).unwrap();
    seal(
        std::io::repeat(0xA5).take(PLAINTEXT_SIZE),
        &mut encrypted,
        &root_key,
        &options,
    )
    .unwrap();
    encrypted.flush().unwrap();
    drop(encrypted);

    let mut encrypted = File::open(&encrypted_path).unwrap();
    let mut header_bytes = [0u8; RAO_HEADER_LEN];
    encrypted.read_exact(&mut header_bytes).unwrap();
    let header = RaoHeader::parse(&header_bytes).unwrap();
    let prefix_len = RAO_HEADER_LEN + header.metadata_frame_len as usize;
    let mut prefix = vec![0u8; prefix_len];
    prefix[..RAO_HEADER_LEN].copy_from_slice(&header_bytes);
    encrypted.read_exact(&mut prefix[RAO_HEADER_LEN..]).unwrap();
    let prefix_path = temp.path().join("authenticated-prefix.rao");
    fs::write(&prefix_path, &prefix).unwrap();
    let plan = covering_stored_range(&prefix, &root_key, MEMBER_START, MEMBER_LEN).unwrap();
    let stored_start = plan.stored_range_start.unwrap();
    assert!(plan.stored_range_len < 2 * u64::from(CHUNK_SIZE));
    assert!(plan.stored_range_len < PLAINTEXT_SIZE / 100);
    encrypted.seek(SeekFrom::Start(stored_start)).unwrap();
    let ranged_path = temp.path().join("covering-frames.rao");
    let mut ranged = File::create(&ranged_path).unwrap();
    let copied = std::io::copy(&mut encrypted.take(plan.stored_range_len), &mut ranged).unwrap();
    assert_eq!(copied, plan.stored_range_len);
    ranged.flush().unwrap();
    drop(ranged);

    let stdin = File::open(&ranged_path).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_rem"))
        .args([
            "archive",
            "extract-stream",
            "--key-file",
            key_path.to_str().unwrap(),
            "--range",
            &format!("{MEMBER_START}:{MEMBER_LEN}"),
            "--authenticated-prefix",
            prefix_path.to_str().unwrap(),
            "--stored-range-start",
            &stored_start.to_string(),
        ])
        .stdin(Stdio::from(stdin))
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let pid = child.id();
    let mut peak_rss_kib = 0u64;
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            let mut stderr = String::new();
            child
                .stderr
                .take()
                .unwrap()
                .read_to_string(&mut stderr)
                .unwrap();
            assert!(status.success(), "extract-stream failed: {stderr}");
            assert!(stderr.contains("\"status\":\"ok\""), "{stderr}");
            assert!(
                stderr.contains("\"mode\":\"ranged-ciphertext\""),
                "{stderr}"
            );
            assert!(
                stderr.contains(&format!("\"bytes_written\":{MEMBER_LEN}")),
                "{stderr}"
            );
            break;
        }
        let status = fs::read_to_string(format!("/proc/{pid}/status"))
            .unwrap_or_else(|error| panic!("read child RSS from /proc/{pid}/status: {error}"));
        if let Some(rss) = parse_status_kib(&status, "VmHWM:") {
            peak_rss_kib = peak_rss_kib.max(rss);
        }
        thread::sleep(Duration::from_millis(2));
    }

    assert!(peak_rss_kib > 0, "failed to observe child VmHWM");
    assert!(
        peak_rss_kib <= MAX_PEAK_RSS_KIB,
        "ranged extract-stream peak RSS {peak_rss_kib} KiB exceeded {MAX_PEAK_RSS_KIB} KiB for a {MEMBER_LEN}-byte member of a {PLAINTEXT_SIZE}-byte object"
    );
}

fn parse_status_kib(status: &str, field: &str) -> Option<u64> {
    status.lines().find_map(|line| {
        line.strip_prefix(field)
            .and_then(|value| value.split_whitespace().next())
            .and_then(|value| value.parse().ok())
    })
}
