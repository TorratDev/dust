fn main() -> Result<(), Box<dyn std::error::Error>> {
    const PROTO: &str = "../../proto/dust_agent.proto";
    println!("cargo:rerun-if-changed={PROTO}");
    println!("cargo:rerun-if-env-changed=PROTOC");
    if std::env::var_os("PROTOC").is_none() {
        std::env::set_var("PROTOC", protoc_bin_vendored::protoc_bin_path()?);
    }
    tonic_prost_build::configure()
        .build_server(false)
        .compile_protos(&[PROTO], &["../../proto"])?;
    Ok(())
}
