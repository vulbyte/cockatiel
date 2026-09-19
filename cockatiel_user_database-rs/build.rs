fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-changed=cockatiel_proto/user_database.proto");
    prost_build::compile_protos(
        &["cockatiel_proto/user_database.proto"],
        &["cockatiel_proto"],
    )?;
    Ok(())
}