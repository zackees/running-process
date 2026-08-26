fn main() -> Result<(), Box<dyn std::error::Error>> {
    // cargo-llvm-cov sets this cfg while linking LLVM's profiling runtime.
    println!("cargo:rustc-check-cfg=cfg(coverage)");

    // Without these, edits to the .proto don't reliably retrigger
    // codegen on incremental builds.
    println!("cargo:rerun-if-changed=proto/daemon.proto");
    println!("cargo:rerun-if-changed=proto/broker_v1/broker_v1_envelope.proto");
    println!("cargo:rerun-if-changed=proto/broker_v1/broker_v1_admin.proto");
    println!("cargo:rerun-if-changed=proto/broker_v1/broker_v1_manifest.proto");
    println!("cargo:rerun-if-changed=proto/broker_v1/broker_v1_service_def.proto");
    println!("cargo:rerun-if-changed=proto/broker_v2/broker_v2_service_def.proto");
    println!("cargo:rerun-if-changed=proto/broker_v2/broker_v2_control.proto");
    println!("cargo:rerun-if-changed=proto/broker_v2/broker_v2_session.proto");
    println!("cargo:rerun-if-changed=proto/broker_v2/broker_v2_manifest.proto");
    println!("cargo:rerun-if-changed=build.rs");
    let file_descriptors = protox::compile(
        [
            "proto/daemon.proto",
            "proto/broker_v1/broker_v1_envelope.proto",
            "proto/broker_v1/broker_v1_admin.proto",
            "proto/broker_v1/broker_v1_manifest.proto",
            "proto/broker_v1/broker_v1_service_def.proto",
            "proto/broker_v2/broker_v2_service_def.proto",
            "proto/broker_v2/broker_v2_control.proto",
            "proto/broker_v2/broker_v2_session.proto",
            "proto/broker_v2/broker_v2_manifest.proto",
        ],
        ["proto/"],
    )?;
    prost_build::compile_fds(file_descriptors)?;

    Ok(())
}
