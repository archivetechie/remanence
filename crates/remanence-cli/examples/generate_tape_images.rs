//! Compatibility diagnostic for the retired review-vector example.
fn main() -> std::process::ExitCode {
    eprintln!("error: use `rem-dev generate-tape-images`");
    std::process::ExitCode::from(1)
}
