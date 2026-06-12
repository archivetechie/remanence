//! Dependency guard for the reusable tape-platform crates.
//!
//! `remanence-scsi` and `remanence-library` are the format-free platform
//! layer. This test keeps higher Remanence layers from becoming dependencies
//! of that layer by asserting on parsed Cargo manifests.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use toml::Value;

const INTERNAL_PREFIX: &str = "remanence-";

#[test]
fn platform_crates_do_not_depend_on_higher_layers() {
    let workspace = workspace_root();
    let crates = workspace.join("crates");

    assert_internal_dependencies(
        &crates.join("remanence-aead").join("Cargo.toml"),
        BTreeSet::new(),
    );
    assert_internal_dependencies(
        &crates.join("remanence-scsi").join("Cargo.toml"),
        BTreeSet::new(),
    );
    assert_internal_dependencies(
        &crates.join("remanence-library").join("Cargo.toml"),
        BTreeSet::from(["remanence-scsi"]),
    );
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("workspace root")
        .to_path_buf()
}

fn assert_internal_dependencies(manifest_path: &Path, allowed: BTreeSet<&str>) {
    let contents = fs::read_to_string(manifest_path).expect("read Cargo.toml");
    let manifest: Value = contents.parse().expect("parse Cargo.toml");

    let internal_dependencies = collect_internal_dependencies(&manifest);
    let violations = internal_dependencies
        .iter()
        .filter(|name| !allowed.contains(name.as_str()))
        .cloned()
        .collect::<Vec<_>>();

    assert!(
        violations.is_empty(),
        "{} may only depend on allowed platform crates; forbidden internal dependencies: {}",
        manifest_path.display(),
        violations.join(", ")
    );
}

fn collect_internal_dependencies(manifest: &Value) -> BTreeSet<String> {
    let mut dependencies = BTreeSet::new();

    collect_dependency_table(manifest.get("dependencies"), &mut dependencies);
    collect_dependency_table(manifest.get("build-dependencies"), &mut dependencies);
    collect_dependency_table(manifest.get("dev-dependencies"), &mut dependencies);

    if let Some(targets) = manifest.get("target").and_then(Value::as_table) {
        for target in targets.values() {
            collect_dependency_table(target.get("dependencies"), &mut dependencies);
            collect_dependency_table(target.get("build-dependencies"), &mut dependencies);
            collect_dependency_table(target.get("dev-dependencies"), &mut dependencies);
        }
    }

    dependencies
}

fn collect_dependency_table(table: Option<&Value>, dependencies: &mut BTreeSet<String>) {
    let Some(table) = table.and_then(Value::as_table) else {
        return;
    };

    for (declared_name, dependency) in table {
        let package_name = dependency
            .as_table()
            .and_then(|inline_table| inline_table.get("package"))
            .and_then(Value::as_str)
            .unwrap_or(declared_name);

        if package_name.starts_with(INTERNAL_PREFIX) {
            dependencies.insert(package_name.to_owned());
        }
    }
}
