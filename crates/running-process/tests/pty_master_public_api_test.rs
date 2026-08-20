//! Public PTY facade contract.
//!
//! The 4.0.1 public neutral traits remain reachable and usable through
//! `NativePtyHandles`. The 5.0 platform-boundary migration deliberately removes
//! concrete `portable-pty` and native fd/HANDLE escape hatches, while keeping
//! custom host-neutral trait implementations source-compatible.
//!
//! Uses `python -c sleep` as the child, matching the rest of the
//! integration test suite. If the test runner doesn't have a
//! `python` on PATH, the test is skipped via early return.

use std::time::{Duration, Instant};

use running_process::pty::backend::{PtyChild, PtyMaster, PtySize};
use running_process::pty::NativePtyProcess;

struct NeutralMaster;

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
}

struct NeutralChild;

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
}

#[test]
fn downstream_neutral_backend_traits_require_no_host_specific_methods() {
    fn assert_master<T: PtyMaster>() {}
    fn assert_child<T: PtyChild>() {}

    assert_master::<NeutralMaster>();
    assert_child::<NeutralChild>();
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
