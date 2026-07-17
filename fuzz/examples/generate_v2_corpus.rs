use std::fs;
use std::io::Cursor;
use std::path::Path;

use remanence_aead::{
    seal_deterministic_for_test_vectors, DataEncryptionKey, EnvelopeSealOptions, KeyFrame,
    RecipientPrivateKey, RecipientSlot, SealOptions,
};
use sha2::{Digest, Sha256};

fn main() {
    write_whole_object_seed();
    write_key_frame_seeds();
}

fn write_whole_object_seed() {
    let plaintext = vec![0x5a; 512];
    let digest: [u8; 32] = Sha256::digest(&plaintext).into();
    let primary = RecipientPrivateKey::new([0x31; 16], "primary-2026", [0x41; 32]).unwrap();
    let recovery = RecipientPrivateKey::new([0x32; 16], "recovery-2026", [0x42; 32]).unwrap();
    let options = EnvelopeSealOptions {
        common: SealOptions {
            chunk_size: 512,
            object_id: "fuzz-v2-object".to_string(),
            plaintext_size: plaintext.len() as u64,
            plaintext_digest: digest,
        },
        recipients: vec![
            primary.public_key(0).unwrap(),
            recovery.public_key(1).unwrap(),
        ],
    };
    let mut sealed = Vec::new();
    seal_deterministic_for_test_vectors(
        Cursor::new(&plaintext),
        &mut sealed,
        &options,
        DataEncryptionKey::from_bytes([0x5d; 32]),
        [0xa7; 32],
    )
    .unwrap();
    write_seed(
        "corpus/rao_whole_object_open_verify/valid-v2-recipient-object",
        &sealed,
    );
    write_seed("corpus/rao_envelope_header/valid-v2-header", &sealed[..128]);
}

fn write_key_frame_seeds() {
    let one = KeyFrame::new(vec![slot(0)]).unwrap().serialize().unwrap();
    let two = KeyFrame::new((0..2).map(slot).collect())
        .unwrap()
        .serialize()
        .unwrap();
    let eight = KeyFrame::new((0..8).map(slot).collect())
        .unwrap()
        .serialize()
        .unwrap();
    write_seed("corpus/rao_key_frame/valid-1-slot", &one);
    write_seed("corpus/rao_key_frame/valid-2-slot", &two);
    write_seed("corpus/rao_key_frame/valid-8-slot", &eight);

    let mut truncated = two.clone();
    truncated.pop();
    write_seed("corpus/rao_key_frame/truncated-2-slot", &truncated);

    let mut duplicate = two.clone();
    duplicate[one.len()] = 0;
    write_seed("corpus/rao_key_frame/duplicate-slot-index", &duplicate);

    let mut trailing = two;
    trailing.push(0);
    write_seed("corpus/rao_key_frame/trailing-byte", &trailing);
}

fn slot(index: u8) -> RecipientSlot {
    RecipientSlot {
        slot_index: index,
        recipient_epoch_id: [index.wrapping_add(1); 16],
        epoch_label: format!("slot-{index}"),
        enc: [index.wrapping_add(2); 32],
        ciphertext: [index.wrapping_add(3); 48],
    }
}

fn write_seed(path: &str, bytes: &[u8]) {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(path);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, bytes).unwrap();
}
