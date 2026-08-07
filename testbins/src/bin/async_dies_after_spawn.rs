//! Test binary: starts a long-lived child through the public async API, then
//! waits for its owner to be force-killed by the integration test.

use std::io::Write;

use running_process::AsyncProcess;

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let target = std::env::args()
        .nth(1)
        .expect("expected the long-lived child path as argv[1]");

    let mut child = AsyncProcess::new(target).kill_when_owner_dies(true);
    child.start().await.expect("start async child");
    let pid = child.pid().await.expect("read async child pid");

    println!("GRANDCHILD_PID={pid}");
    std::io::stdout().flush().expect("flush child pid");
    println!("READY");
    std::io::stdout().flush().expect("flush ready marker");

    // Keep the actor and its child alive until the test kills this process.
    // Dropping the public handle is a valid close operation, not the owner
    // death scenario this fixture is intended to exercise.
    std::mem::forget(child);
    std::thread::park();
}
