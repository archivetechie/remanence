use prost::Message;

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

    // protox is a Rust implementation of protoc, so the build needs no system
    // protobuf compiler. It resolves imports, well-known types included, and
    // keeps source info so the generated code carries the proto comments.
    let descriptors = protox::compile(
        [
            "layer5.proto",
            "google/rpc/status.proto",
            "google/rpc/error_details.proto",
        ],
        ["../../proto"],
    )?;
    std::fs::write(&descriptor_path, descriptors.encode_to_vec())?;

    tonic_prost_build::configure()
        // The terminal summary is far larger than the per-row stream items
        // that share its oneof; boxing it keeps every row small.
        .boxed(".remanence.api.v1.TapeInventoryStreamItem.item.summary")
        .build_client(true)
        .build_server(true)
        .compile_fds(descriptors)?;

    Ok(())
}
