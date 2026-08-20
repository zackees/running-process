//! Public PTY facade contract.
//!
//! The 4.0.1 public neutral traits remain reachable and usable through
//! `NativePtyHandles`. The platform-boundary migration keeps the historical
//! concrete `portable-pty` and native fd/HANDLE escape hatches deprecated until
//! the next major release, while new mechanics use only facade-owned operations.
//!
//! Uses `python -c sleep` as the child, matching the rest of the
//! integration test suite. If the test runner doesn't have a
//! `python` on PATH, the test is skipped via early return.

use std::time::{Duration, Instant};

use running_process::pty::backend::{PtyChild, PtyMaster, PtySize};
use running_process::pty::NativePtyProcess;

struct NeutralMaster;

#[allow(deprecated)]
impl PtyMaster for NeutralMaster {
    fn try_clone_reader(&mut self) -> std::io::Result<Box<dyn std::io::Read + Send>> {
        Ok(Box::new(std::io::empty()))
    }

    fn take_writer(&mut self) -> std::io::Result<Box<dyn std::io::Write + Send>> {
        Ok(Box::new(std::io::sink()))
    }

    fn resize(&self, _size: PtySize) -> std::io::Result<()> {
        Ok(())
    }

    fn get_size(&self) -> std::io::Result<PtySize> {
        Ok(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
    }

    fn process_group_leader(&self) -> Option<i32> {
        Some(17)
    }

    fn as_raw_fd(&self) -> Option<i32> {
        Some(23)
    }
}

struct NeutralChild;

#[allow(deprecated)]
impl PtyChild for NeutralChild {
    fn pid(&self) -> u32 {
        1
    }

    fn try_wait(&mut self) -> std::io::Result<Option<u32>> {
        Ok(None)
    }

    fn wait(&mut self) -> std::io::Result<u32> {
        Ok(0)
    }

    fn kill(&mut self) -> std::io::Result<()> {
        Ok(())
    }

    fn as_raw_handle(&self) -> Option<*mut std::ffi::c_void> {
        Some(std::ptr::without_provenance_mut(29))
    }
}

#[test]
fn downstream_neutral_backend_traits_require_no_host_specific_methods() {
    fn assert_master<T: PtyMaster>() {}
    fn assert_child<T: PtyChild>() {}

    assert_master::<NeutralMaster>();
    assert_child::<NeutralChild>();
}

#[test]
#[allow(deprecated)]
fn downstream_legacy_pty_trait_methods_and_helpers_remain_source_compatible() {
    let master = NeutralMaster;
    assert_eq!(master.process_group_leader(), Some(17));
    assert_eq!(master.as_raw_fd(), Some(23));
    assert_eq!(
        NeutralChild.as_raw_handle().map(|handle| handle.addr()),
        Some(29)
    );

    let _command =
        running_process::pty::command_builder_from_argv(&["echo".to_owned(), "compat".to_owned()]);
    let status =
        running_process::pty::reexports::portable_pty::ExitStatus::with_signal("Interrupt");
    assert_eq!(running_process::pty::portable_exit_code(status), -2);
}

fn python_available() -> bool {
    std::process::Command::new("python")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[test]
fn pty_master_resize_and_get_size_through_handles() {
    if !python_available() {
        eprintln!("[skip] python not on PATH");
        return;
    }

    let process = NativePtyProcess::new(
        vec![
            "python".into(),
            "-c".into(),
            "import time; time.sleep(5)".into(),
        ],
        None,
        None,
        24,
        80,
        None,
    )
    .expect("construct pty");
    process.start_impl().expect("start pty");

    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if process
            .handles
            .lock()
            .expect("pty handles mutex poisoned")
            .is_some()
        {
            break;
        }
        if Instant::now() >= deadline {
            panic!("handles never populated after start");
        }
        std::thread::sleep(Duration::from_millis(25));
    }

    {
        let guard = process.handles.lock().expect("pty handles mutex poisoned");
        let handles = guard.as_ref().expect("handles populated");

        // Initial size should match openpty.
        let initial = handles.master.get_size().expect("get_size after openpty");
        assert_eq!(
            initial,
            PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0
            }
        );

        // Resize → get_size returns the new value.
        let new_size = PtySize {
            rows: 40,
            cols: 132,
            pixel_width: 0,
            pixel_height: 0,
        };
        handles.master.resize(new_size).expect("resize");
        let observed = handles.master.get_size().expect("get_size after resize");
        assert_eq!(observed, new_size);
    }

    let _ = process.kill_impl();
}
