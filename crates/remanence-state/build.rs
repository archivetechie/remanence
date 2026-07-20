//! Stamp audit-producing builds with crate and source-control identity.

use std::path::Path;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-env-changed=REMANENCE_SOFTWARE_BUILD");
    println!("cargo:rerun-if-changed=../../.git/HEAD");
    println!("cargo:rerun-if-changed=../../.git/packed-refs");
    if let Ok(head) = std::fs::read_to_string("../../.git/HEAD") {
        if let Some(reference) = head.trim().strip_prefix("ref: ") {
            println!("cargo:rerun-if-changed=../../.git/{reference}");
        }
    }

    let package_version = std::env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "unknown".into());
    let software_build = std::env::var("REMANENCE_SOFTWARE_BUILD")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .map(|value| value.trim().to_string())
        .unwrap_or_else(|| derived_build(package_version.as_str()));
    println!("cargo:rustc-env=REMANENCE_SOFTWARE_BUILD={software_build}");
}

fn derived_build(package_version: &str) -> String {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".into());
    let repository = Path::new(manifest_dir.as_str()).join("../..");
    let description = Command::new("git")
        .arg("-C")
        .arg(repository)
        .args(["describe", "--always", "--tags"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    match description {
        Some(description) => format!("{package_version}+{description}"),
        None => package_version.to_string(),
    }
}
