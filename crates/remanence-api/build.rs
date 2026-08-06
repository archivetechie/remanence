fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-changed=../../proto/layer5.proto");
    println!("cargo:rerun-if-changed=../../proto");

    // Emit the compiled descriptor set so the contract can be ENUMERATED rather
    // than scanned for. Counting occurrences of a pattern in the code
    // undercounts silently — the field list is finite and authoritative, so
    // presence checks walk it instead. See tools/wire_presence_census.py.
    let descriptor_path =
        std::path::PathBuf::from(std::env::var("OUT_DIR")?).join("layer5_descriptor.bin");
    println!(
        "cargo:rustc-env=LAYER5_DESCRIPTOR_PATH={}",
        descriptor_path.display()
    );

    tonic_prost_build::configure()
        .file_descriptor_set_path(&descriptor_path)
        .build_client(true)
        .build_server(true)
        .compile_protos(
            &[
                "../../proto/layer5.proto",
                "../../proto/google/rpc/status.proto",
                "../../proto/google/rpc/error_details.proto",
            ],
            &["../../proto"],
        )?;

    Ok(())
}
