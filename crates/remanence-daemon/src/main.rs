//! rem-daemon — Layer 5 local daemon entrypoint.

use std::process::ExitCode;

fn main() -> ExitCode {
    remanence_daemon::main_entry_with_registry(Default::default())
}
