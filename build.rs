//! Generates the gRPC bindings for the sync protocol.
//!
//! `protox` compiles the `.proto` files in pure Rust, so no system `protoc` is required.

use std::path::PathBuf;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let proto_root = PathBuf::from("proto");
    let files = [proto_root.join("anchor/sync/v1/sync.proto")];
    for file in &files {
        println!("cargo:rerun-if-changed={}", file.display());
    }
    println!("cargo:rerun-if-changed=build.rs");

    let descriptors = protox::compile(&files, [&proto_root])?;
    tonic_prost_build::configure()
        .build_client(true)
        .build_server(true)
        .compile_fds(descriptors)?;
    Ok(())
}
