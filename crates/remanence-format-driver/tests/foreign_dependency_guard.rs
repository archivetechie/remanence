//! Dependency guard for the published format-driver boundary.
//!
//! Core RAO and streaming crates must not depend on concrete legacy or foreign
//! archive implementations. Optional plugin edges are allowed only at API/CLI
//! dispatch layers.

use std::fs;
use std::path::{Path, PathBuf};

use toml::Value;

const FOREIGN_FORMAT_CRATES: &[&str] = &["remanence-bru"];

#[test]
fn core_format_crates_do_not_depend_on_foreign_formats() {
    let crates = workspace_root().join("crates");
    for crate_name in [
        "remanence-format-driver",
        "remanence-format",
        "remanence-stream",
    ] {
        let manifest_path = crates.join(crate_name).join("Cargo.toml");
        let manifest = read_manifest(&manifest_path);
        for dependency in manifest_dependencies(&manifest) {
            assert!(
                !FOREIGN_FORMAT_CRATES.contains(&dependency.as_str()),
                "{} must not depend on foreign-format crate {dependency}",
                manifest_path.display()
            );
        }
    }
}

#[test]
fn api_foreign_format_dependency_stays_optional() {
    let manifest_path = workspace_root()
        .join("crates")
        .join("remanence-api")
        .join("Cargo.toml");
    let manifest = read_manifest(&manifest_path);
    let dependencies = manifest
        .get("dependencies")
        .and_then(Value::as_table)
        .expect("dependencies table");
    let bru = dependencies
        .get("remanence-bru")
        .and_then(Value::as_table)
        .expect("remanence-api declares remanence-bru as an inline dependency");
    assert_eq!(
        bru.get("optional").and_then(Value::as_bool),
        Some(true),
        "remanence-api may only depend on remanence-bru as an optional plugin"
    );

    let default_features = manifest
        .get("features")
        .and_then(Value::as_table)
        .and_then(|features| features.get("default"))
        .and_then(Value::as_array)
        .expect("default feature list");
    assert!(
        default_features.is_empty(),
        "remanence-api default features must not enable foreign-format plugins"
    );
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("workspace root")
        .to_path_buf()
}

fn read_manifest(path: &Path) -> Value {
    fs::read_to_string(path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
        .parse()
        .unwrap_or_else(|error| panic!("parse {}: {error}", path.display()))
}

fn manifest_dependencies(manifest: &Value) -> Vec<String> {
    let mut dependencies = Vec::new();
    collect_dependency_table(manifest.get("dependencies"), &mut dependencies);
    collect_dependency_table(manifest.get("build-dependencies"), &mut dependencies);

    if let Some(targets) = manifest.get("target").and_then(Value::as_table) {
        for target in targets.values() {
            collect_dependency_table(target.get("dependencies"), &mut dependencies);
            collect_dependency_table(target.get("build-dependencies"), &mut dependencies);
        }
    }

    dependencies
}

fn collect_dependency_table(table: Option<&Value>, dependencies: &mut Vec<String>) {
    let Some(table) = table.and_then(Value::as_table) else {
        return;
    };

    for (declared_name, dependency) in table {
        let package_name = dependency
            .as_table()
            .and_then(|inline_table| inline_table.get("package"))
            .and_then(Value::as_str)
            .unwrap_or(declared_name);
        dependencies.push(package_name.to_owned());
    }
}
