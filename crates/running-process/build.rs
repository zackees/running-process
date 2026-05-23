fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Without these, edits to the .proto don't reliably retrigger
    // codegen on incremental builds.
    println!("cargo:rerun-if-changed=proto/daemon.proto");
    println!("cargo:rerun-if-changed=build.rs");
    let file_descriptors = protox::compile(["proto/daemon.proto"], ["proto/"])?;
    prost_build::compile_fds(file_descriptors)?;
    Ok(())
}
