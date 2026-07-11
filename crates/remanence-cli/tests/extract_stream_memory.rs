//! Process-level memory regression test for the streaming AEAD decrypt helper.
//!
//! The test seals a 64 MiB object without materializing it, runs the real
//! `rem archive extract-stream` binary, and samples Linux peak RSS while the
//! plaintext is drained to `/dev/null`.

#![cfg(target_os = "linux")]

use std::fs::{self, File};
use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

use remanence_aead::{seal, RootKey, SealOptions};
use sha2::{Digest, Sha256};

const PLAINTEXT_SIZE: u64 = 64 * 1024 * 1024;
const CHUNK_SIZE: u32 = 256 * 1024;
const MAX_PEAK_RSS_KIB: u64 = 48 * 1024;

#[test]
fn extract_stream_peak_rss_is_bounded_independently_of_object_size() {
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

    let stdin = File::open(&encrypted_path).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_rem"))
        .args([
            "archive",
            "extract-stream",
            "--key-file",
            key_path.to_str().unwrap(),
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
        "extract-stream peak RSS {peak_rss_kib} KiB exceeded {MAX_PEAK_RSS_KIB} KiB for a {PLAINTEXT_SIZE}-byte object"
    );
}

fn parse_status_kib(status: &str, field: &str) -> Option<u64> {
    status.lines().find_map(|line| {
        line.strip_prefix(field)
            .and_then(|value| value.split_whitespace().next())
            .and_then(|value| value.parse().ok())
    })
}
