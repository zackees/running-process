use crate::debug_traces::running_process_py_debug_hang_outer;

/// The hang hook is host-neutral: it announces readiness by writing `ready`
/// to the ready path, then returns once the release path exists. With the
/// release file already present it must return immediately on every host.
#[test]
fn debug_hang_hook_writes_ready_and_returns_once_released() {
    let dir = std::env::temp_dir().join(format!(
        "rp-py-debug-hang-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let ready = dir.join("ready");
    let release = dir.join("release");
    std::fs::write(&release, b"").unwrap();

    running_process_py_debug_hang_outer(&ready, &release).unwrap();

    assert_eq!(std::fs::read(&ready).unwrap(), b"ready");
    let _ = std::fs::remove_dir_all(&dir);
}
