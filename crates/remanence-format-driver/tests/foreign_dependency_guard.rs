//! Dependency guard for the published format-driver boundary.
//!
//! The core workspace must not contain or depend on concrete legacy or foreign
//! archive implementations. Distributions assemble those implementations
//! outside this repository through `remanence-format-driver`.

use std::fs;
use std::path::{Path, PathBuf};

use toml::Value;

const FOREIGN_FORMAT_CRATES: &[&str] = &["remanence-bru"];

#[test]
fn core_workspace_does_not_contain_or_depend_on_foreign_formats() {
    let root_manifest_path = workspace_root().join("Cargo.toml");
    let root_manifest = read_manifest(&root_manifest_path);
    let members = root_manifest
        .get("workspace")
        .and_then(Value::as_table)
        .and_then(|workspace| workspace.get("members"))
        .and_then(Value::as_array)
        .expect("workspace members");
    for member in members.iter().filter_map(Value::as_str) {
        for foreign in FOREIGN_FORMAT_CRATES {
            assert!(
                !member.ends_with(foreign),
                "core workspace must not contain foreign-format crate {foreign}"
            );
        }
    }

    let crates = workspace_root().join("crates");
    for entry in fs::read_dir(&crates).expect("read core crates directory") {
        let manifest_path = entry
            .expect("crate directory entry")
            .path()
            .join("Cargo.toml");
        if !manifest_path.is_file() {
            continue;
        }
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
fn dispatch_crates_depend_on_the_generic_registry_contract() {
    for crate_name in ["remanence-api", "remanence-cli", "remanence-daemon"] {
        let manifest_path = workspace_root()
            .join("crates")
            .join(crate_name)
            .join("Cargo.toml");
        let dependencies = manifest_dependencies(&read_manifest(&manifest_path));
        assert!(
            dependencies
                .iter()
                .any(|name| name == "remanence-format-driver"),
            "{crate_name} must dispatch foreign formats through remanence-format-driver"
        );
        for foreign in FOREIGN_FORMAT_CRATES {
            assert!(
                !dependencies.iter().any(|name| name == foreign),
                "{crate_name} must not depend on concrete adapter {foreign}"
            );
        }
    }
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
