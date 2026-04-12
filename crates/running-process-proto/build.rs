fn main() -> Result<(), Box<dyn std::error::Error>> {
    let file_descriptors = protox::compile(["proto/daemon.proto"], ["proto/"])?;
    prost_build::compile_fds(file_descriptors)?;
    Ok(())
}
