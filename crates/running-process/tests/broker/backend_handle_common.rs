#![allow(dead_code)]

use running_process::broker::backend_lifecycle::identity::DaemonProcess;
use running_process::broker::protocol::Endpoint;

pub fn test_endpoint() -> Endpoint {
    Endpoint {
        namespace_id: "test-namespace".to_string(),
        path: test_endpoint_path(),
    }
}

pub fn current_daemon() -> DaemonProcess {
    DaemonProcess::current_process(test_endpoint(), Some(30)).unwrap()
}

pub fn impossible_pid() -> u32 {
    u32::MAX
}

fn test_endpoint_path() -> String {
    #[cfg(windows)]
    {
        format!(
            r"\\.\pipe\running-process-backend-handle-test-{}",
            std::process::id()
        )
    }

    #[cfg(unix)]
    {
        std::env::temp_dir()
            .join(format!(
                "running-process-backend-handle-test-{}.sock",
                std::process::id()
            ))
            .to_string_lossy()
            .into_owned()
    }
}
