fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-changed=cockatiel_engine-rs/cockatiel_lib/cockatiel_proto/cockatiel_protobuf.proto");
    prost_build::compile_protos(
        &["../cockatiel_engine-rs/cockatiel_lib/cockatiel_proto/cockatiel_protobuf.proto"],
        &["../cockatiel_engine-rs/cockatiel_lib/cockatiel_proto"],
    )?;
    Ok(())
}