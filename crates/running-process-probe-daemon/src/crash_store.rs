//! Ingest fixed crash spools into durable, human-readable reports.
//!
//! The crashing process performs one bounded write and exits. `rpprobed`
//! watches the owner-private spool, validates complete records, and does all
//! allocation/JSON/filesystem work here, outside the compromised process.

use std::fs::{self, OpenOptions};
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use running_process_probe::crash::spool::{create_private_dir, parse, RawCrashReport, RECORD_SIZE};
use serde::Serialize;

/// Handle for the daemon's background spool watcher.
pub struct CrashWatcher {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl Drop for CrashWatcher {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// Start an owner-local watcher. Incomplete files remain pending for a later
/// pass; malformed full-size files are quarantined with a `.invalid` suffix.
pub fn spawn_watcher(spool_dir: PathBuf, report_dir: PathBuf) -> io::Result<CrashWatcher> {
    create_private_dir(&spool_dir)?;
    create_private_dir(&report_dir)?;
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = Arc::clone(&stop);
    let handle = std::thread::Builder::new()
        .name("rpprobed-crash-ingest".into())
        .spawn(move || {
            while !thread_stop.load(Ordering::Acquire) {
                if let Err(error) = ingest_pending(&spool_dir, &report_dir) {
                    eprintln!("rpprobed: crash spool ingest failed: {error}");
                }
                std::thread::sleep(Duration::from_millis(25));
            }
        })?;
    Ok(CrashWatcher {
        stop,
        handle: Some(handle),
    })
}

/// Ingest every complete pending record once.
pub fn ingest_pending(spool_dir: &Path, report_dir: &Path) -> io::Result<Vec<PathBuf>> {
    create_private_dir(spool_dir)?;
    create_private_dir(report_dir)?;
    let mut written = Vec::new();
    for entry in fs::read_dir(spool_dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) != Some("rpcrash") {
            continue;
        }
        let metadata = entry.metadata()?;
        if metadata.len() < RECORD_SIZE as u64 {
            // The crashing process pre-creates an empty file. Seeing it before
            // the one write completes is normal, not corruption.
            continue;
        }
        if metadata.len() != RECORD_SIZE as u64 {
            quarantine(&path)?;
            continue;
        }
        let bytes = fs::read(&path)?;
        let report = match parse(&bytes) {
            Ok(report) => report,
            Err(_) => {
                quarantine(&path)?;
                continue;
            }
        };
        let destination = write_report(report_dir, &path, &report)?;
        fs::remove_file(&path)?;
        sync_directory(spool_dir)?;
        written.push(destination);
    }
    Ok(written)
}

fn quarantine(path: &Path) -> io::Result<()> {
    let mut invalid = path.as_os_str().to_os_string();
    invalid.push(".invalid");
    fs::rename(path, PathBuf::from(invalid))
}

#[derive(Serialize)]
struct DurableReport<'a> {
    schema: &'static str,
    pid: u32,
    faulting_tid: u64,
    fault_kind: String,
    fault_address: String,
    crash_unix_ms: u64,
    app_class: &'a str,
    app_name: &'a str,
    app_version: &'a str,
    instance_name: &'a str,
    modules: Vec<&'a str>,
    all_threads: Vec<DurableThread>,
    raw_context_hex: String,
    truncated: bool,
}

#[derive(Serialize)]
struct DurableThread {
    os_tid: u64,
    frames: Vec<DurableFrame>,
}

#[derive(Serialize)]
struct DurableFrame {
    module_index: Option<u32>,
    relative_address: String,
}

static REPORT_SEQUENCE: AtomicU64 = AtomicU64::new(0);

fn write_report(dir: &Path, source: &Path, report: &RawCrashReport) -> io::Result<PathBuf> {
    let sequence = REPORT_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let source_name = source
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid crash spool name"))?;
    let path = dir.join(format!("crash-{source_name}.json"));
    let temporary = dir.join(format!(".crash-{}-{sequence}.tmp", std::process::id()));
    let durable = DurableReport {
        schema: "running-process.crash.v1",
        pid: report.pid,
        faulting_tid: report.tid,
        fault_kind: fault_kind(report.fault_code),
        fault_address: format!("0x{:x}", report.fault_address),
        crash_unix_ms: report.crash_unix_ms,
        app_class: &report.metadata.app_class,
        app_name: &report.metadata.app_name,
        app_version: &report.metadata.app_version,
        instance_name: &report.metadata.instance_name,
        modules: report
            .modules
            .iter()
            .map(|module| module.identity.as_str())
            .collect(),
        all_threads: report
            .threads
            .iter()
            .map(|thread| DurableThread {
                os_tid: thread.os_tid,
                frames: thread
                    .frames
                    .iter()
                    .map(|frame| DurableFrame {
                        module_index: frame.module_index,
                        relative_address: format!("0x{:x}", frame.relative_address),
                    })
                    .collect(),
            })
            .collect(),
        raw_context_hex: hex(&report.raw_context),
        truncated: report.truncated,
    };
    let bytes = serde_json::to_vec_pretty(&durable).map_err(io::Error::other)?;
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let result = (|| {
        let mut file = options.open(&temporary)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temporary, &path)?;
        sync_directory(dir)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result?;
    Ok(path)
}

fn fault_kind(code: i64) -> String {
    #[cfg(unix)]
    {
        match code as i32 {
            libc::SIGSEGV => "SIGSEGV".into(),
            libc::SIGBUS => "SIGBUS".into(),
            libc::SIGILL => "SIGILL".into(),
            libc::SIGFPE => "SIGFPE".into(),
            libc::SIGABRT => "SIGABRT".into(),
            libc::SIGTRAP => "SIGTRAP".into(),
            _ => format!("signal-{code}"),
        }
    }
    #[cfg(windows)]
    {
        format!("0x{:08X}", code as u32)
    }
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(DIGITS[(byte >> 4) as usize] as char);
        output.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    output
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> io::Result<()> {
    fs::File::open(path)?.sync_all()
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use running_process_probe::crash::spool::{
        encode, CrashFrame, CrashMetadata, CrashModule, CrashThread, RawCrashReport,
    };

    fn report() -> RawCrashReport {
        RawCrashReport {
            pid: 123,
            tid: 456,
            fault_code: test_fault_code(),
            fault_address: 0xdead,
            crash_unix_ms: 999,
            metadata: CrashMetadata {
                app_class: "compiler".into(),
                app_name: "frontend".into(),
                app_version: "1.2.3".into(),
                instance_name: "ci".into(),
            },
            modules: vec![CrashModule {
                identity: "fixture.exe".into(),
            }],
            threads: vec![
                CrashThread {
                    os_tid: 456,
                    frames: vec![
                        CrashFrame {
                            module_index: Some(0),
                            relative_address: 0x1000,
                        },
                        CrashFrame {
                            module_index: Some(0),
                            relative_address: 0x2000,
                        },
                    ],
                },
                CrashThread {
                    os_tid: 789,
                    frames: vec![CrashFrame {
                        module_index: Some(0),
                        relative_address: 0x3000,
                    }],
                },
            ],
            raw_context: vec![0xaa, 0xbb],
            truncated: false,
        }
    }

    #[cfg(windows)]
    fn test_fault_code() -> i64 {
        0xC000_0005u32 as i32 as i64
    }

    #[cfg(unix)]
    fn test_fault_code() -> i64 {
        libc::SIGSEGV as i64
    }

    #[test]
    fn pre_registration_record_is_ingested_when_daemon_appears() {
        let root = tempfile::tempdir().unwrap();
        let spool = root.path().join("spool");
        let reports = root.path().join("reports");
        create_private_dir(&spool).unwrap();
        let pending = spool.join("before-daemon.rpcrash");
        fs::write(&pending, encode(&report())).unwrap();

        let paths = ingest_pending(&spool, &reports).unwrap();
        assert_eq!(paths.len(), 1);
        assert!(!pending.exists());
        let json: serde_json::Value =
            serde_json::from_slice(&fs::read(&paths[0]).unwrap()).unwrap();
        assert_eq!(json["app_class"], "compiler");
        assert_eq!(json["all_threads"].as_array().unwrap().len(), 2);
        assert_eq!(json["modules"][0], "fixture.exe");
        assert_eq!(json["all_threads"][0]["frames"][0]["module_index"], 0);
        assert_eq!(json["fault_address"], "0xdead");
        assert_eq!(json["raw_context_hex"], "aabb");
    }

    #[test]
    fn incomplete_record_remains_pending() {
        let root = tempfile::tempdir().unwrap();
        let spool = root.path().join("spool");
        let reports = root.path().join("reports");
        create_private_dir(&spool).unwrap();
        let pending = spool.join("writing.rpcrash");
        fs::write(&pending, [1, 2, 3]).unwrap();
        assert!(ingest_pending(&spool, &reports).unwrap().is_empty());
        assert!(pending.exists());
    }
}
