//! Two-phase memory charging fixture for independent-spawn acceptance tests.
//! The harness supplies a fresh private directory, waits for `started`, reads
//! its cgroup baseline, then creates `allocate`. `resident` acknowledges that
//! all 64 MiB have been touched. `release` ends the fixture; deadlines ensure
//! an abandoned harness never leaves an indefinitely running memory holder.

use std::io::{self, Write};
use std::path::Path;
use std::time::{Duration, Instant};

const RESIDENT_BYTES: usize = 64 * 1024 * 1024;

fn marker(directory: &Path, name: &str, text: &str) -> io::Result<()> {
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(directory.join(name))?;
    file.write_all(text.as_bytes())?;
    file.sync_all()
}

fn wait_for(directory: &Path, name: &str) -> io::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(60);
    while !directory.join(name).try_exists()? {
        if name == "allocate" && directory.join("release").try_exists()? {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "fixture cancelled before allocation",
            ));
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(io::ErrorKind::TimedOut, name.to_owned()));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    Ok(())
}

fn main() -> io::Result<()> {
    let directory = std::env::args_os().nth(1).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "private fixture directory required",
        )
    })?;
    let directory = Path::new(&directory);
    marker(directory, "started", &format!("{}\n", std::process::id()))?;
    wait_for(directory, "allocate")?;
    let mut resident = Vec::new();
    resident
        .try_reserve_exact(RESIDENT_BYTES)
        .map_err(|error| io::Error::other(error.to_string()))?;
    // Nonzero initialization faults in the allocation instead of leaving
    // zero pages lazily mapped. black_box keeps it live through the hold.
    resident.resize(RESIDENT_BYTES, 0xa5_u8);
    std::hint::black_box(&resident);
    marker(directory, "resident", &format!("{RESIDENT_BYTES}\n"))?;
    let result = wait_for(directory, "release");
    std::hint::black_box(&resident);
    drop(resident);
    result?;
    marker(directory, "released", &format!("{}\n", std::process::id()))
}
