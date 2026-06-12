//! Hosted database dependency guard for the Layer 4 workspace surface.

use std::fs;
use std::path::{Path, PathBuf};

const FORBIDDEN_DATABASE_PACKAGES: &[&str] = &[
    "bb8-postgres",
    "deadpool-postgres",
    "diesel",
    "mongodb",
    "mysql",
    "mysql_async",
    "mysql_common",
    "postgres",
    "postgres-protocol",
    "postgres-types",
    "redis",
    "sqlx",
    "sqlx-core",
    "sqlx-mysql",
    "sqlx-postgres",
    "tiberius",
    "tokio-postgres",
];

const FORBIDDEN_RUNTIME_TOKENS: &[&str] = &[
    "DATABASE_URL",
    "MYSQL_HOST",
    "MYSQL_PASSWORD",
    "MYSQL_TCP_PORT",
    "MYSQL_USER",
    "PGHOST",
    "PGPASSWORD",
    "PGPORT",
    "PGUSER",
    "postgres://",
    "mysql://",
];

#[test]
fn workspace_has_no_hosted_database_dependency_or_runtime_hook() {
    let workspace = workspace_root();

    assert_no_forbidden_lockfile_packages(&workspace.join("Cargo.lock"));
    assert_no_forbidden_manifest_dependencies(&workspace);
    assert_no_forbidden_runtime_tokens(&workspace);
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("workspace root")
        .to_path_buf()
}

fn assert_no_forbidden_lockfile_packages(path: &Path) {
    let contents = fs::read_to_string(path).expect("read Cargo.lock");
    let mut violations = Vec::new();

    for line in contents.lines() {
        let line = line.trim();
        let Some(name) = line
            .strip_prefix("name = \"")
            .and_then(|value| value.strip_suffix('"'))
        else {
            continue;
        };

        if FORBIDDEN_DATABASE_PACKAGES.contains(&name) {
            violations.push(name.to_owned());
        }
    }

    assert!(
        violations.is_empty(),
        "hosted database packages are not allowed in Cargo.lock: {}",
        violations.join(", ")
    );
}

fn assert_no_forbidden_manifest_dependencies(workspace: &Path) {
    let mut manifest_paths = vec![workspace.join("Cargo.toml")];
    collect_files(&workspace.join("crates"), "Cargo.toml", &mut manifest_paths);

    let mut violations = Vec::new();
    for path in manifest_paths {
        let contents = fs::read_to_string(&path).expect("read Cargo.toml");
        for (line_number, raw_line) in contents.lines().enumerate() {
            let line = raw_line.split('#').next().unwrap_or("").trim();
            let Some((key, _value)) = line.split_once('=') else {
                continue;
            };
            let key = key.trim().trim_matches('"');

            if FORBIDDEN_DATABASE_PACKAGES.contains(&key) {
                violations.push(format!("{}:{}", path.display(), line_number + 1));
            }
        }
    }

    assert!(
        violations.is_empty(),
        "hosted database dependencies are not allowed in manifests: {}",
        violations.join(", ")
    );
}

fn assert_no_forbidden_runtime_tokens(workspace: &Path) {
    let mut source_paths = Vec::new();
    collect_files(&workspace.join("crates"), "rs", &mut source_paths);

    let this_test = Path::new(file!());
    let mut violations = Vec::new();
    for path in source_paths {
        if path.ends_with(this_test) {
            continue;
        }

        let contents = fs::read_to_string(&path).expect("read Rust source");
        for token in FORBIDDEN_RUNTIME_TOKENS {
            if contents.contains(token) {
                violations.push(format!("{} contains {token}", path.display()));
            }
        }
    }

    assert!(
        violations.is_empty(),
        "hosted database runtime hooks are not allowed in crate source: {}",
        violations.join(", ")
    );
}

fn collect_files(root: &Path, suffix: &str, out: &mut Vec<PathBuf>) {
    if !root.exists() {
        return;
    }

    for entry in fs::read_dir(root).expect("read directory") {
        let entry = entry.expect("read directory entry");
        let path = entry.path();
        if path.is_dir() {
            collect_files(&path, suffix, out);
        } else if path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name == suffix || name.ends_with(&format!(".{suffix}")))
        {
            out.push(path);
        }
    }
}
