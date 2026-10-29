//! Process-level routing checks keep development commands out of installed CLIs.
use std::process::Command;

#[test]
fn shipping_binaries_redirect_development_commands_before_io() {
    for binary in [env!("CARGO_BIN_EXE_rem"), env!("CARGO_BIN_EXE_rem-debug")] {
        for (path, replacement) in [
            (["tape", "freeze-drill"], "rem-dev tape freeze-drill"),
            (
                ["tape", "terminal-index-drill"],
                "rem-dev tape terminal-index-drill",
            ),
            (
                ["dev", "write-dump-to-tape"],
                "rem-dev dev write-dump-to-tape",
            ),
        ] {
            for globals in [vec![], vec!["--allow", "unused"], vec!["--allow=unused"]] {
                let output = Command::new(binary)
                    .args(globals)
                    .args(path)
                    .args(["--device", "/nonexistent/rem-dev-device"])
                    .output()
                    .unwrap();
                assert_eq!(output.status.code(), Some(1));
                assert!(output.stdout.is_empty());
                assert_eq!(
                    String::from_utf8(output.stderr).unwrap(),
                    format!("error: use `{replacement}`\n")
                );
            }
        }
        let help = Command::new(binary)
            .args(["tape", "--help"])
            .output()
            .unwrap();
        assert!(help.status.success());
        let help = String::from_utf8(help.stdout).unwrap();
        assert!(!help.contains("freeze-drill"));
        assert!(!help.contains("terminal-index-drill"));
        let help = Command::new(binary).arg("--help").output().unwrap();
        assert!(help.status.success());
        assert!(!String::from_utf8(help.stdout)
            .unwrap()
            .contains("write-dump-to-tape"));
    }
}

#[test]
fn development_help_and_required_arguments_are_available() {
    for args in [
        vec!["--help"],
        vec!["tape", "freeze-drill", "--help"],
        vec!["tape", "terminal-index-drill", "--help"],
        vec!["dev", "write-dump-to-tape", "--help"],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_rem-dev"))
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let output = Command::new(env!("CARGO_BIN_EXE_rem-dev"))
        .args(["generate-tape-images", "--invalid-generator-argument"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("usage: rem-dev generate-tape-images"));
}

#[test]
fn operator_argument_values_do_not_trigger_development_redirects() {
    let output = Command::new(env!("CARGO_BIN_EXE_rem"))
        .args(["archive", "inspect", "--help", "tape", "freeze-drill"])
        .output()
        .unwrap();
    assert!(!String::from_utf8_lossy(&output.stderr).contains("error: use `rem-dev"));
}
