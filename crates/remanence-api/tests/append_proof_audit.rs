//! Source audit for the append-admission proof.
//!
//! `AppendBoundaryProof` is an `unsafe trait`: implementing it lets the holder
//! assert that a tape boundary was proved, which permits overwriting data past
//! it. Rust cannot seal a trait across crates, so this test keeps the set of
//! implementations fixed instead: exactly one production implementation (the
//! admission funnel), and every other implementation compiled only for tests.

use std::fs;
use std::path::{Path, PathBuf};

const FUNNEL: &str = "crates/remanence-api/src/append_admission.rs";

fn rust_sources(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).expect("read source directory") {
        let path = entry.expect("directory entry").path();
        if path.is_dir() {
            if path.file_name().is_some_and(|name| name == "target") {
                continue;
            }
            rust_sources(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
}

#[test]
fn append_boundary_proof_has_one_production_implementation() {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut sources = Vec::new();
    rust_sources(&workspace.join("crates"), &mut sources);

    // Built from parts so this file's own search line is not a match.
    let unsafe_impl = ["unsafe", " impl"].concat();
    let proof_trait = ["AppendBoundary", "Proof"].concat();
    let mut production = Vec::new();
    for path in sources {
        let text = fs::read_to_string(&path).expect("read source file");
        let lines: Vec<&str> = text.lines().collect();
        for (index, line) in lines.iter().enumerate() {
            if !(line.contains(&unsafe_impl) && line.contains(&proof_trait)) {
                continue;
            }
            let relative = path
                .strip_prefix(&workspace)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            let test_only = index > 0 && lines[index - 1].trim() == "#[cfg(test)]";
            if !test_only {
                production.push(format!("{relative}:{}", index + 1));
            }
        }
    }

    assert_eq!(
        production.len(),
        1,
        "AppendBoundaryProof must have exactly one production implementation, \
         in {FUNNEL}; found {production:?}"
    );
    assert!(
        production[0].starts_with(FUNNEL),
        "the production AppendBoundaryProof implementation must be the admission \
         funnel in {FUNNEL}; found {}",
        production[0]
    );
}
