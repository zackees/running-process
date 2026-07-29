//! Durable SQLite crash history, owner-private artifacts, and bounded GC.
//!
//! The crashing process performs one fixed-size write and exits. `rpprobed`
//! parses that spool outside compromised context, writes a durable JSON
//! artifact, and commits its redacted query metadata to SQLite.

use std::collections::{HashMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use running_process::broker::secure_dir::ensure_private_dir;
use running_process_probe::crash::spool::{parse, RawCrashReport, RECORD_SIZE, REPORT_DIR_ENV};
use rusqlite::{params, Connection, OpenFlags, OptionalExtension as _, TransactionBehavior};
use serde::Serialize;
use sysinfo::{Pid, ProcessRefreshKind, System};

const SCHEMA_VERSION: i64 = 2;
const DEFAULT_MAX_AGE: Duration = Duration::from_secs(30 * 24 * 60 * 60);
const DEFAULT_KEEP_LAST_N: usize = 100;
const DEFAULT_MAX_TOTAL_BYTES: u64 = 1024 * 1024 * 1024;
const DEFAULT_MAX_ROWS: usize = 10_000;
const DEFAULT_MAX_SINGLE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_INLINE_REPORT_BYTES: usize = 64 * 1024;

/// Retention and admission bounds for durable crash data.
#[derive(Clone, Debug)]
pub struct CleanupPolicy {
    /// Delete unpinned records older than this age. `Duration::ZERO` disables.
    pub max_age: Duration,
    /// Keep at most this many newest rows per `(app_class, app_name)`.
    /// Zero disables the per-app bound.
    pub keep_last_n_per_app: usize,
    /// Maximum combined artifact bytes. Zero disables the byte bound.
    pub max_total_artifact_bytes: u64,
    /// Maximum total rows. Zero disables the row bound.
    pub max_rows: usize,
    /// Reject a single artifact above this size.
    pub max_single_artifact_bytes: u64,
}

impl Default for CleanupPolicy {
    fn default() -> Self {
        Self {
            max_age: DEFAULT_MAX_AGE,
            keep_last_n_per_app: DEFAULT_KEEP_LAST_N,
            max_total_artifact_bytes: DEFAULT_MAX_TOTAL_BYTES,
            max_rows: DEFAULT_MAX_ROWS,
            max_single_artifact_bytes: DEFAULT_MAX_SINGLE_BYTES,
        }
    }
}

/// One durable crash row returned by query operations.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CrashRecord {
    /// Stable database identifier.
    pub id: i64,
    /// Coarse application class.
    pub app_class: String,
    /// Human-readable application name.
    pub app_name: String,
    /// Application version.
    pub app_version: String,
    /// Optional application instance.
    pub instance_name: String,
    /// Crashed process id.
    pub pid: u32,
    /// Process creation/install time paired with `pid`.
    pub creation_time_ms: u64,
    /// Working directory captured before the crash.
    pub cwd: String,
    /// Stable crash-bucket digest.
    pub signature: String,
    /// Crash wall-clock time.
    pub crashed_at_ms: u64,
    /// Signal name or exception code.
    pub fault_kind: String,
    /// Owner-private JSON artifact path, empty only after reconciliation.
    pub artifact_path: PathBuf,
    /// Artifact size at insertion.
    pub artifact_bytes: u64,
}

/// Errors from durable crash storage.
#[derive(Debug, thiserror::Error)]
pub enum CrashStoreError {
    /// SQLite operation failed.
    #[error("crash database operation failed: {0}")]
    Sql(#[from] rusqlite::Error),
    /// Filesystem operation failed.
    #[error("crash artifact operation failed: {0}")]
    Io(#[from] io::Error),
    /// A value cannot be represented safely in SQLite.
    #[error("crash field is outside the supported integer range")]
    IntegerRange,
    /// Admission rejected an oversized artifact.
    #[error("crash artifact is {actual} bytes; maximum is {maximum}")]
    ArtifactTooLarge {
        /// Attempted artifact size.
        actual: u64,
        /// Configured single-artifact cap.
        maximum: u64,
    },
    /// Database was created by a newer incompatible daemon.
    #[error("crash database schema {found} is newer than supported schema {supported}")]
    FutureSchema {
        /// Version found on disk.
        found: i64,
        /// Highest supported version.
        supported: i64,
    },
}

/// Thread-safe durable crash database.
pub struct CrashStore {
    conn: Mutex<Connection>,
    artifacts_dir: PathBuf,
    policy: CleanupPolicy,
    session: StoreSession,
}

impl CrashStore {
    /// Open or create a crash database using the default bounded policy.
    pub fn open(db_path: &Path, artifacts_dir: &Path) -> Result<Self, CrashStoreError> {
        Self::open_with_policy(db_path, artifacts_dir, CleanupPolicy::default())
    }

    /// Open or create a crash database using `policy`.
    pub fn open_with_policy(
        db_path: &Path,
        artifacts_dir: &Path,
        policy: CleanupPolicy,
    ) -> Result<Self, CrashStoreError> {
        if let Some(parent) = db_path.parent() {
            ensure_private_root(parent)?;
        }
        reject_symlink(db_path)?;
        ensure_private_root(artifacts_dir)?;
        let conn = Connection::open_with_flags(
            db_path,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_CREATE
                | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "FULL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        migrate(&conn)?;
        let session = process_session()?.clone();
        let store = Self {
            conn: Mutex::new(conn),
            artifacts_dir: artifacts_dir.to_path_buf(),
            policy,
            session,
        };
        store.reconcile_on_open()?;
        Ok(store)
    }

    /// Persist one parsed crash report and its owner-private JSON artifact.
    pub fn record(&self, report: &RawCrashReport) -> Result<CrashRecord, CrashStoreError> {
        self.record_with_symbol_report(report, None)
    }

    fn record_with_symbol_report(
        &self,
        report: &RawCrashReport,
        symbol_report: Option<&str>,
    ) -> Result<CrashRecord, CrashStoreError> {
        self.record_with_source(report, symbol_report, None)
    }

    fn record_with_source(
        &self,
        report: &RawCrashReport,
        symbol_report: Option<&str>,
        source_id: Option<&str>,
    ) -> Result<CrashRecord, CrashStoreError> {
        let signature = signature(report);
        let fault_kind = fault_kind(report.fault_code);
        let durable = DurableReport::new(report, &signature, &fault_kind, symbol_report);
        let bytes = serde_json::to_vec_pretty(&durable).map_err(io::Error::other)?;
        let artifact_bytes =
            u64::try_from(bytes.len()).map_err(|_| CrashStoreError::IntegerRange)?;
        if self.policy.max_single_artifact_bytes != 0
            && artifact_bytes > self.policy.max_single_artifact_bytes
        {
            return Err(CrashStoreError::ArtifactTooLarge {
                actual: artifact_bytes,
                maximum: self.policy.max_single_artifact_bytes,
            });
        }

        let report_json = if bytes.len() <= MAX_INLINE_REPORT_BYTES {
            std::str::from_utf8(&bytes)
                .map_err(io::Error::other)?
                .to_owned()
        } else {
            String::new()
        };
        let mut conn = self
            .conn
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let transaction = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(source_id) = source_id {
            if let Some(existing) = record_by_source(&transaction, &self.artifacts_dir, source_id)?
            {
                transaction.commit()?;
                return Ok(existing);
            }
        }
        // Hold SQLite's writer lease across artifact publication. A
        // concurrent open/reconciliation must wait until the row and file are
        // either both committed or both absent.
        let artifact_path = self.write_artifact(report.crash_unix_ms, &bytes)?;
        let inserted = insert_row(
            &transaction,
            report,
            &signature,
            &fault_kind,
            &report_json,
            (&artifact_path, artifact_bytes),
            source_id,
        );
        let id = match inserted {
            Ok(id) => id,
            Err(error) => {
                let _ = fs::remove_file(&artifact_path);
                return Err(error);
            }
        };
        let record = record_by_id(&transaction, &self.artifacts_dir, id)?
            .ok_or(rusqlite::Error::QueryReturnedNoRows)?;
        transaction.commit()?;
        // Retention is best-effort after the durable insert. Returning an
        // error now would make a surviving source spool retry and duplicate a
        // crash that is already committed.
        let _ = gc_locked(&mut conn, &self.artifacts_dir, &self.policy, unix_millis());
        Ok(record)
    }

    /// Query crashes by application class, newest first.
    pub fn query_by_class(
        &self,
        app_class: &str,
        limit: usize,
    ) -> Result<Vec<CrashRecord>, CrashStoreError> {
        let conn = self
            .conn
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let mut statement = conn.prepare(
            "SELECT id, app_class, app_name, app_version, instance_name, pid,
                    creation_time_ms, cwd, signature, crashed_at_ms, exit_signal,
                    artifact_path, artifact_bytes
             FROM crashes
             WHERE app_class = ?1
             ORDER BY crashed_at_ms DESC, id DESC
             LIMIT ?2",
        )?;
        let rows = statement.query_map(params![app_class, limit], |row| {
            row_to_record(row, &self.artifacts_dir)
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// Fetch one crash row by id.
    pub fn get(&self, id: i64) -> Result<Option<CrashRecord>, CrashStoreError> {
        let conn = self
            .conn
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        record_by_id(&conn, &self.artifacts_dir, id).map_err(Into::into)
    }

    /// Pin an artifact for a streaming fetch.
    ///
    /// The returned guard decrements the persisted reference count on drop.
    /// GC serializes with this increment and never removes a pinned row.
    pub fn begin_fetch(&self, id: i64) -> Result<Option<FetchGuard<'_>>, CrashStoreError> {
        let mut conn = self
            .conn
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let transaction = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let artifact: Option<(String, i64)> = transaction
            .query_row(
                "SELECT artifact_path, artifact_bytes FROM crashes WHERE id = ?1",
                [id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((path, expected_bytes)) = artifact.filter(|(path, _)| !path.is_empty()) else {
            return Ok(None);
        };
        let path = resolve_artifact_path(&self.artifacts_dir, &path);
        let expected_bytes =
            u64::try_from(expected_bytes).map_err(|_| CrashStoreError::IntegerRange)?;
        let Some(file) = open_artifact_for_fetch(&self.artifacts_dir, &path, expected_bytes)?
        else {
            return Ok(None);
        };
        transaction.execute(
            "INSERT INTO crash_fetch_pins (crash_id, session_id, pin_count)
             VALUES (?1, ?2, 1)
             ON CONFLICT(crash_id, session_id)
             DO UPDATE SET pin_count = pin_count + 1",
            params![id, self.session.id],
        )?;
        let changed = transaction.execute(
            "UPDATE crashes SET refcount = refcount + 1
             WHERE id = ?1 AND artifact_path <> ''",
            [id],
        )?;
        if changed == 0 {
            return Ok(None);
        }
        transaction.commit()?;
        Ok(Some(FetchGuard {
            store: self,
            id,
            path,
            file,
        }))
    }

    /// Apply age, per-app, byte, and row retention bounds.
    pub fn gc(&self) -> Result<usize, CrashStoreError> {
        let mut conn = self
            .conn
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        gc_locked(&mut conn, &self.artifacts_dir, &self.policy, unix_millis())
    }

    fn write_artifact(&self, crashed_at_ms: u64, bytes: &[u8]) -> Result<PathBuf, CrashStoreError> {
        let suffix = random_hex()?;
        let final_path = self
            .artifacts_dir
            .join(format!("crash-{crashed_at_ms}-{suffix}.json"));
        let temporary = self.artifacts_dir.join(format!(".{suffix}.tmp"));
        let mut options = OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let result = (|| {
            let mut file = options.open(&temporary)?;
            file.write_all(bytes)?;
            file.sync_all()?;
            drop(file);
            fs::rename(&temporary, &final_path)?;
            sync_directory(&self.artifacts_dir)
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result?;
        Ok(final_path)
    }

    fn end_fetch(&self, id: i64) {
        let mut conn = self
            .conn
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Ok(transaction) = conn.transaction_with_behavior(TransactionBehavior::Immediate) {
            let _ = transaction.execute(
                "UPDATE crash_fetch_pins
                 SET pin_count = pin_count - 1
                 WHERE crash_id = ?1 AND session_id = ?2 AND pin_count > 0",
                params![id, self.session.id],
            );
            let _ = transaction.execute(
                "DELETE FROM crash_fetch_pins
                 WHERE crash_id = ?1 AND session_id = ?2 AND pin_count <= 0",
                params![id, self.session.id],
            );
            let _ = transaction.execute(
                "UPDATE crashes
                 SET refcount = CASE WHEN refcount > 0 THEN refcount - 1 ELSE 0 END
                 WHERE id = ?1",
                [id],
            );
            let _ = transaction.commit();
        };
    }

    fn reconcile_on_open(&self) -> Result<(), CrashStoreError> {
        let mut conn = self
            .conn
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let transaction = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let sessions = {
            let mut statement = transaction.prepare(
                "SELECT session_id, pid, process_start_ms, boot_id
                 FROM crash_store_sessions",
            )?;
            let sessions = statement
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, String>(3)?,
                    ))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            sessions
        };
        for (session_id, pid, start_ms, boot_id) in sessions {
            let alive = u32::try_from(pid)
                .ok()
                .zip(u64::try_from(start_ms).ok())
                .is_some_and(|(pid, start_ms)| session_is_alive(pid, start_ms, &boot_id));
            if session_id != self.session.id && !alive {
                transaction.execute(
                    "DELETE FROM crash_store_sessions WHERE session_id = ?1",
                    [session_id],
                )?;
            }
        }
        transaction.execute(
            "INSERT INTO crash_store_sessions (
                 session_id, pid, process_start_ms, boot_id
             ) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(session_id) DO UPDATE SET
                 pid = excluded.pid,
                 process_start_ms = excluded.process_start_ms,
                 boot_id = excluded.boot_id",
            params![
                self.session.id,
                i64::from(self.session.pid),
                sqlite_u64(self.session.process_start_ms)?,
                self.session.boot_id,
            ],
        )?;
        transaction.execute(
            "UPDATE crashes
             SET refcount = COALESCE((
                 SELECT SUM(pin_count)
                 FROM crash_fetch_pins
                 WHERE crash_id = crashes.id
             ), 0)",
            [],
        )?;
        reconcile_artifacts(&transaction, &self.artifacts_dir)?;
        transaction.commit()?;
        Ok(())
    }
}

/// RAII lease protecting one artifact from GC while it is read.
pub struct FetchGuard<'a> {
    store: &'a CrashStore,
    id: i64,
    path: PathBuf,
    file: fs::File,
}

impl FetchGuard<'_> {
    /// Owner-private artifact to stream.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Already-opened, no-follow artifact handle for streaming.
    pub fn file(&self) -> &fs::File {
        &self.file
    }
}

impl Drop for FetchGuard<'_> {
    fn drop(&mut self) {
        self.store.end_fetch(self.id);
    }
}

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

/// Resolve the durable owner-local artifact directory.
///
/// An explicit S7 environment override wins; otherwise crash history lives
/// under the platform state directory rather than the transient spool root.
pub fn default_artifacts_dir() -> PathBuf {
    std::env::var_os(REPORT_DIR_ENV)
        .map(|root| PathBuf::from(root).join("crashes-v2"))
        .unwrap_or_else(|| {
            running_process::client::paths::data_dir()
                .join("probe")
                .join("crashes")
        })
}

/// Start an owner-local watcher backed by a durable SQLite store.
pub fn spawn_watcher(spool_dir: PathBuf, report_dir: PathBuf) -> io::Result<CrashWatcher> {
    ensure_private_root(&spool_dir)?;
    ensure_private_root(&report_dir)?;
    let db_path = report_dir
        .parent()
        .unwrap_or(&report_dir)
        .join("crashes.sqlite3");
    let store = Arc::new(CrashStore::open(&db_path, &report_dir).map_err(io::Error::other)?);
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = Arc::clone(&stop);
    let handle = std::thread::Builder::new()
        .name("rpprobed-crash-ingest".into())
        .spawn(move || {
            while !thread_stop.load(Ordering::Acquire) {
                if let Err(error) = ingest_pending_with_store(&spool_dir, &store) {
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

/// Ingest complete pending records into a persistent store rooted at
/// `report_dir`.
pub fn ingest_pending(spool_dir: &Path, report_dir: &Path) -> io::Result<Vec<PathBuf>> {
    ensure_private_root(spool_dir)?;
    ensure_private_root(report_dir)?;
    let db_path = report_dir
        .parent()
        .unwrap_or(report_dir)
        .join("crashes.sqlite3");
    let store = CrashStore::open(&db_path, report_dir).map_err(io::Error::other)?;
    ingest_pending_with_store(spool_dir, &store)
}

fn ingest_pending_with_store(spool_dir: &Path, store: &CrashStore) -> io::Result<Vec<PathBuf>> {
    let worker = crate::symbolication::worker_path();
    ingest_pending_with_store_and_worker(spool_dir, store, worker.as_deref())
}

fn ingest_pending_with_store_and_worker(
    spool_dir: &Path,
    store: &CrashStore,
    worker: Option<&Path>,
) -> io::Result<Vec<PathBuf>> {
    ensure_private_root(spool_dir)?;
    let mut written = Vec::new();
    for entry in fs::read_dir(spool_dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) != Some("rpcrash") {
            continue;
        }
        let metadata = entry.metadata()?;
        if metadata.len() < RECORD_SIZE as u64 {
            continue;
        }
        if metadata.len() != RECORD_SIZE as u64 {
            quarantine(&path)?;
            continue;
        }
        let bytes = fs::read(&path)?;
        let source_id = spool_source_id(&path, &bytes)?;
        let report = match parse(&bytes) {
            Ok(report) => report,
            Err(_) => {
                quarantine(&path)?;
                continue;
            }
        };
        // The S8 worker remains a disposable process boundary. Any discovery,
        // parser, timeout, or worker-crash failure degrades to the raw S7
        // frames; it must never make the durable crash itself disappear.
        let symbol_report = worker.and_then(|worker| symbolize_crash_with_worker(&report, worker));
        let record = store
            .record_with_source(&report, symbol_report.as_deref(), Some(&source_id))
            .map_err(io::Error::other)?;
        fs::remove_file(&path)?;
        sync_directory(spool_dir)?;
        written.push(record.artifact_path);
    }
    Ok(written)
}

fn migrate(conn: &Connection) -> Result<(), CrashStoreError> {
    let found: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if found > SCHEMA_VERSION {
        return Err(CrashStoreError::FutureSchema {
            found,
            supported: SCHEMA_VERSION,
        });
    }
    conn.execute_batch(
        "BEGIN IMMEDIATE;
         CREATE TABLE IF NOT EXISTS crashes (
             id                 INTEGER PRIMARY KEY AUTOINCREMENT,
             app_class          TEXT    NOT NULL DEFAULT '',
             app_name           TEXT    NOT NULL DEFAULT '',
             app_version        TEXT    NOT NULL DEFAULT '',
             instance_name      TEXT    NOT NULL DEFAULT '',
             pid                INTEGER NOT NULL,
             creation_time_ms   INTEGER NOT NULL,
             cwd                TEXT    NOT NULL DEFAULT '',
             signature          TEXT    NOT NULL DEFAULT '',
             crashed_at_ms      INTEGER NOT NULL,
             exit_signal        TEXT    NOT NULL DEFAULT '',
             report_json        TEXT    NOT NULL DEFAULT '',
             artifact_path      TEXT    NOT NULL DEFAULT '',
             artifact_bytes     INTEGER NOT NULL DEFAULT 0,
             refcount           INTEGER NOT NULL DEFAULT 0,
             source_id          TEXT
         );",
    )?;
    let migration = (|| {
        // Additive migration also repairs databases produced by development
        // snapshots of S7, before this schema was versioned.
        ensure_column(
            conn,
            "app_class",
            "ALTER TABLE crashes ADD COLUMN app_class TEXT NOT NULL DEFAULT ''",
        )?;
        ensure_column(
            conn,
            "app_name",
            "ALTER TABLE crashes ADD COLUMN app_name TEXT NOT NULL DEFAULT ''",
        )?;
        ensure_column(
            conn,
            "app_version",
            "ALTER TABLE crashes ADD COLUMN app_version TEXT NOT NULL DEFAULT ''",
        )?;
        ensure_column(
            conn,
            "instance_name",
            "ALTER TABLE crashes ADD COLUMN instance_name TEXT NOT NULL DEFAULT ''",
        )?;
        ensure_column(
            conn,
            "pid",
            "ALTER TABLE crashes ADD COLUMN pid INTEGER NOT NULL DEFAULT 0",
        )?;
        ensure_column(
            conn,
            "creation_time_ms",
            "ALTER TABLE crashes ADD COLUMN creation_time_ms INTEGER NOT NULL DEFAULT 0",
        )?;
        ensure_column(
            conn,
            "cwd",
            "ALTER TABLE crashes ADD COLUMN cwd TEXT NOT NULL DEFAULT ''",
        )?;
        ensure_column(
            conn,
            "signature",
            "ALTER TABLE crashes ADD COLUMN signature TEXT NOT NULL DEFAULT ''",
        )?;
        ensure_column(
            conn,
            "crashed_at_ms",
            "ALTER TABLE crashes ADD COLUMN crashed_at_ms INTEGER NOT NULL DEFAULT 0",
        )?;
        ensure_column(
            conn,
            "exit_signal",
            "ALTER TABLE crashes ADD COLUMN exit_signal TEXT NOT NULL DEFAULT ''",
        )?;
        ensure_column(
            conn,
            "report_json",
            "ALTER TABLE crashes ADD COLUMN report_json TEXT NOT NULL DEFAULT ''",
        )?;
        ensure_column(
            conn,
            "artifact_path",
            "ALTER TABLE crashes ADD COLUMN artifact_path TEXT NOT NULL DEFAULT ''",
        )?;
        ensure_column(
            conn,
            "artifact_bytes",
            "ALTER TABLE crashes ADD COLUMN artifact_bytes INTEGER NOT NULL DEFAULT 0",
        )?;
        ensure_column(
            conn,
            "refcount",
            "ALTER TABLE crashes ADD COLUMN refcount INTEGER NOT NULL DEFAULT 0",
        )?;
        ensure_column(
            conn,
            "source_id",
            "ALTER TABLE crashes ADD COLUMN source_id TEXT",
        )?;
        conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS idx_crashes_class_time
                 ON crashes(app_class, crashed_at_ms);
             CREATE INDEX IF NOT EXISTS idx_crashes_signature
                 ON crashes(signature);
             CREATE UNIQUE INDEX IF NOT EXISTS idx_crashes_source
                 ON crashes(source_id) WHERE source_id IS NOT NULL;
             CREATE TABLE IF NOT EXISTS crash_store_sessions (
                 session_id TEXT PRIMARY KEY,
                 pid INTEGER NOT NULL,
                 process_start_ms INTEGER NOT NULL,
                 boot_id TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS crash_fetch_pins (
                 crash_id INTEGER NOT NULL,
                 session_id TEXT NOT NULL,
                 pin_count INTEGER NOT NULL,
                 PRIMARY KEY (crash_id, session_id),
                 FOREIGN KEY (crash_id) REFERENCES crashes(id) ON DELETE CASCADE,
                 FOREIGN KEY (session_id) REFERENCES crash_store_sessions(session_id)
                     ON DELETE CASCADE
             );
             PRAGMA user_version = 2;",
        )?;
        Ok::<_, rusqlite::Error>(())
    })();
    match migration {
        Ok(()) => {
            conn.execute_batch("COMMIT")?;
            Ok(())
        }
        Err(error) => {
            let _ = conn.execute_batch("ROLLBACK");
            Err(error.into())
        }
    }
}

fn ensure_column(conn: &Connection, name: &str, alter_statement: &str) -> rusqlite::Result<()> {
    let mut statement = conn.prepare("PRAGMA table_info(crashes)")?;
    let present = statement
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<Result<Vec<_>, _>>()?
        .iter()
        .any(|column| column == name);
    drop(statement);
    if !present {
        conn.execute_batch(alter_statement)?;
    }
    Ok(())
}

fn insert_row(
    conn: &Connection,
    report: &RawCrashReport,
    signature: &str,
    fault_kind: &str,
    report_json: &str,
    artifact: (&Path, u64),
    source_id: Option<&str>,
) -> Result<i64, CrashStoreError> {
    let (artifact_path, artifact_bytes) = artifact;
    let artifact_name = artifact_path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "generated crash artifact name is not valid UTF-8",
            )
        })?;
    conn.execute(
        "INSERT INTO crashes (
             app_class, app_name, app_version, instance_name, pid,
             creation_time_ms, cwd, signature, crashed_at_ms, exit_signal,
             report_json, artifact_path, artifact_bytes, refcount, source_id
         ) VALUES (
             ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, 0, ?14
         )",
        params![
            report.metadata.app_class,
            report.metadata.app_name,
            report.metadata.app_version,
            report.metadata.instance_name,
            i64::from(report.pid),
            sqlite_u64(report.metadata.creation_time_ms)?,
            report.metadata.cwd,
            signature,
            sqlite_u64(report.crash_unix_ms)?,
            fault_kind,
            report_json,
            artifact_name,
            sqlite_u64(artifact_bytes)?,
            source_id,
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

fn record_by_source(
    conn: &Connection,
    artifacts_dir: &Path,
    source_id: &str,
) -> rusqlite::Result<Option<CrashRecord>> {
    conn.query_row(
        "SELECT id, app_class, app_name, app_version, instance_name, pid,
                creation_time_ms, cwd, signature, crashed_at_ms, exit_signal,
                artifact_path, artifact_bytes
         FROM crashes WHERE source_id = ?1",
        [source_id],
        |row| row_to_record(row, artifacts_dir),
    )
    .optional()
}

fn record_by_id(
    conn: &Connection,
    artifacts_dir: &Path,
    id: i64,
) -> rusqlite::Result<Option<CrashRecord>> {
    conn.query_row(
        "SELECT id, app_class, app_name, app_version, instance_name, pid,
                creation_time_ms, cwd, signature, crashed_at_ms, exit_signal,
                artifact_path, artifact_bytes
         FROM crashes WHERE id = ?1",
        [id],
        |row| row_to_record(row, artifacts_dir),
    )
    .optional()
}

fn row_to_record(row: &rusqlite::Row<'_>, artifacts_dir: &Path) -> rusqlite::Result<CrashRecord> {
    let pid = u32::try_from(row.get::<_, i64>(5)?).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            5,
            rusqlite::types::Type::Integer,
            Box::new(error),
        )
    })?;
    let creation_time_ms = db_u64(row, 6)?;
    let crashed_at_ms = db_u64(row, 9)?;
    let artifact_bytes = db_u64(row, 12)?;
    Ok(CrashRecord {
        id: row.get(0)?,
        app_class: row.get(1)?,
        app_name: row.get(2)?,
        app_version: row.get(3)?,
        instance_name: row.get(4)?,
        pid,
        creation_time_ms,
        cwd: row.get(7)?,
        signature: row.get(8)?,
        crashed_at_ms,
        fault_kind: row.get(10)?,
        artifact_path: resolve_artifact_path(artifacts_dir, &row.get::<_, String>(11)?),
        artifact_bytes,
    })
}

fn reconcile_artifacts(conn: &Connection, artifacts_dir: &Path) -> Result<(), CrashStoreError> {
    let mut statement = conn.prepare("SELECT id, artifact_path FROM crashes")?;
    let rows = statement
        .query_map([], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    drop(statement);
    let mut referenced = HashSet::new();
    for (id, path) in rows {
        if path.is_empty() {
            continue;
        }
        let path = resolve_artifact_path(artifacts_dir, &path);
        if is_safe_regular_artifact(artifacts_dir, &path) {
            referenced.insert(path);
        } else {
            conn.execute(
                "UPDATE crashes
                 SET artifact_path = '', artifact_bytes = 0
                 WHERE id = ?1",
                [id],
            )?;
        }
    }
    let mut first_error = None;
    for entry in fs::read_dir(artifacts_dir)? {
        let path = entry?.path();
        if referenced.contains(&path) || !is_owned_cleanup_path(&path) {
            continue;
        }
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                if first_error.is_none() {
                    first_error = Some(error);
                }
            }
        }
    }
    first_error.map_or(Ok(()), |error| Err(error.into()))
}

fn db_u64(row: &rusqlite::Row<'_>, index: usize) -> rusqlite::Result<u64> {
    u64::try_from(row.get::<_, i64>(index)?).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            index,
            rusqlite::types::Type::Integer,
            Box::new(error),
        )
    })
}

#[derive(Debug)]
struct GcRow {
    id: i64,
    app_class: String,
    app_name: String,
    crashed_at_ms: u64,
    artifact_bytes: u64,
    artifact_path: PathBuf,
    refcount: u64,
}

fn gc_locked(
    conn: &mut Connection,
    artifacts_dir: &Path,
    policy: &CleanupPolicy,
    now_ms: u64,
) -> Result<usize, CrashStoreError> {
    let transaction = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let rows = {
        let mut statement = transaction.prepare(
            "SELECT id, app_class, app_name, crashed_at_ms, artifact_bytes,
                    artifact_path, refcount
             FROM crashes
             ORDER BY crashed_at_ms DESC, id DESC",
        )?;
        let collected = statement
            .query_map([], |row| {
                Ok(GcRow {
                    id: row.get(0)?,
                    app_class: row.get(1)?,
                    app_name: row.get(2)?,
                    crashed_at_ms: db_u64(row, 3)?,
                    artifact_bytes: db_u64(row, 4)?,
                    artifact_path: resolve_artifact_path(artifacts_dir, &row.get::<_, String>(5)?),
                    refcount: db_u64(row, 6)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        collected
    };

    let age_cutoff =
        now_ms.saturating_sub(u64::try_from(policy.max_age.as_millis()).unwrap_or(u64::MAX));
    let mut selected = HashSet::new();
    let mut per_app = HashMap::<(String, String), usize>::new();
    for row in &rows {
        let count = per_app
            .entry((row.app_class.clone(), row.app_name.clone()))
            .or_default();
        *count += 1;
        let expired = !policy.max_age.is_zero() && row.crashed_at_ms < age_cutoff;
        let beyond_per_app = policy.keep_last_n_per_app != 0 && *count > policy.keep_last_n_per_app;
        if row.refcount == 0 && (expired || beyond_per_app) {
            selected.insert(row.id);
        }
    }

    let mut remaining_rows = rows.len().saturating_sub(selected.len());
    let mut remaining_bytes = rows
        .iter()
        .filter(|row| !selected.contains(&row.id))
        .fold(0_u64, |total, row| total.saturating_add(row.artifact_bytes));
    for row in rows.iter().rev() {
        let rows_over = policy.max_rows != 0 && remaining_rows > policy.max_rows;
        let bytes_over = policy.max_total_artifact_bytes != 0
            && remaining_bytes > policy.max_total_artifact_bytes;
        if !rows_over && !bytes_over {
            break;
        }
        if row.refcount != 0 || selected.contains(&row.id) {
            continue;
        }
        selected.insert(row.id);
        remaining_rows = remaining_rows.saturating_sub(1);
        remaining_bytes = remaining_bytes.saturating_sub(row.artifact_bytes);
    }

    let mut deleted_paths = Vec::new();
    let mut deleted = 0;
    for row in &rows {
        if !selected.contains(&row.id) {
            continue;
        }
        let changed = transaction.execute(
            "DELETE FROM crashes WHERE id = ?1 AND refcount = 0",
            [row.id],
        )?;
        if changed != 0 {
            deleted += changed;
            if is_safe_artifact_path(artifacts_dir, &row.artifact_path) {
                deleted_paths.push(row.artifact_path.clone());
            }
        }
    }
    transaction.commit()?;

    // Row first, artifact second. A crash can leave an orphan (reconciled on
    // open), but never a live row pointing at a file that GC already removed.
    for path in deleted_paths {
        match fs::remove_file(path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(_) => {}
        }
    }
    // Retry every safe orphan on every GC pass, including passes that had no
    // rows left to select after a prior unlink failure.
    let retry = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    reconcile_artifacts(&retry, artifacts_dir)?;
    retry.commit()?;
    sync_directory(artifacts_dir)?;
    Ok(deleted)
}

#[derive(Serialize)]
struct DurableReport<'a> {
    schema: &'static str,
    pid: u32,
    creation_time_ms: u64,
    cwd: &'a str,
    faulting_tid: u64,
    fault_kind: &'a str,
    fault_address: String,
    crash_unix_ms: u64,
    signature: &'a str,
    app_class: &'a str,
    app_name: &'a str,
    app_version: &'a str,
    instance_name: &'a str,
    modules: Vec<&'a str>,
    all_threads: Vec<DurableThread>,
    raw_context_hex: String,
    truncated: bool,
    symbolized: Option<serde_json::Value>,
}

impl<'a> DurableReport<'a> {
    fn new(
        report: &'a RawCrashReport,
        signature: &'a str,
        fault_kind: &'a str,
        symbol_report: Option<&str>,
    ) -> Self {
        Self {
            schema: "running-process.crash.v2",
            pid: report.pid,
            creation_time_ms: report.metadata.creation_time_ms,
            cwd: &report.metadata.cwd,
            faulting_tid: report.tid,
            fault_kind,
            fault_address: format!("0x{:x}", report.fault_address),
            crash_unix_ms: report.crash_unix_ms,
            signature,
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
            symbolized: symbol_report.and_then(|json| serde_json::from_str(json).ok()),
        }
    }
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

fn signature(report: &RawCrashReport) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"running-process.crash-signature.v1\0");
    hasher.update(&report.fault_code.to_le_bytes());
    if let Some(thread) = report
        .threads
        .iter()
        .find(|thread| thread.os_tid == report.tid)
        .or_else(|| report.threads.first())
    {
        for frame in thread.frames.iter().take(16) {
            if let Some(module) = frame
                .module_index
                .and_then(|index| report.modules.get(index as usize))
            {
                let stable_name = Path::new(&module.identity)
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or(&module.identity);
                hasher.update(stable_name.as_bytes());
            }
            hasher.update(&[0]);
            hasher.update(&frame.relative_address.to_le_bytes());
        }
    }
    hasher.finalize().to_hex().to_string()
}

#[derive(Serialize)]
struct WorkerCapture<'a> {
    format: &'static str,
    modules: Vec<WorkerModule<'a>>,
    threads: Vec<WorkerThread>,
}

#[derive(Serialize)]
struct WorkerModule<'a> {
    name: &'a str,
    path_hint: &'a str,
}

#[derive(Serialize)]
struct WorkerThread {
    os_tid: u64,
    frames: Vec<WorkerFrame>,
}

#[derive(Serialize)]
struct WorkerFrame {
    module_index: u32,
    relative_address: u64,
}

fn symbolize_crash_with_worker(report: &RawCrashReport, worker: &Path) -> Option<String> {
    let capture = WorkerCapture {
        format: "cooperative_frames",
        modules: report
            .modules
            .iter()
            .map(|module| WorkerModule {
                name: Path::new(&module.identity)
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or(&module.identity),
                path_hint: &module.identity,
            })
            .collect(),
        threads: report
            .threads
            .iter()
            .map(|thread| WorkerThread {
                os_tid: thread.os_tid,
                frames: thread
                    .frames
                    .iter()
                    .map(|frame| WorkerFrame {
                        module_index: frame.module_index.unwrap_or(u32::MAX),
                        relative_address: frame.relative_address,
                    })
                    .collect(),
            })
            .collect(),
    };
    let capture = serde_json::to_vec(&capture).ok()?;
    crate::symbolication::symbolize_with_worker_at(
        worker,
        &capture,
        crate::symbolication::DEFAULT_WORKER_TIMEOUT,
    )
    .ok()
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

fn random_hex() -> io::Result<String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|error| io::Error::other(error.to_string()))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

#[derive(Clone, Debug)]
struct StoreSession {
    id: String,
    pid: u32,
    process_start_ms: u64,
    boot_id: String,
}

static PROCESS_SESSION: OnceLock<StoreSession> = OnceLock::new();

fn process_session() -> Result<&'static StoreSession, CrashStoreError> {
    if let Some(session) = PROCESS_SESSION.get() {
        return Ok(session);
    }
    let pid = std::process::id();
    let mut system = System::new();
    let sys_pid = Pid::from_u32(pid);
    system.refresh_process_specifics(sys_pid, ProcessRefreshKind::new());
    let process_start_ms = system
        .process(sys_pid)
        .map(|process| process.start_time().saturating_mul(1000))
        .unwrap_or_else(unix_millis);
    let session = StoreSession {
        id: random_hex()?,
        pid,
        process_start_ms,
        boot_id: running_process::broker::host_identity::current().boot_id,
    };
    // Another thread may win initialization with an equally valid identity.
    let _ = PROCESS_SESSION.set(session);
    PROCESS_SESSION.get().ok_or_else(|| {
        CrashStoreError::Io(io::Error::other("failed to initialize crash store session"))
    })
}

fn session_is_alive(pid: u32, process_start_ms: u64, boot_id: &str) -> bool {
    if running_process::broker::host_identity::current().boot_id != boot_id {
        return false;
    }
    let mut system = System::new();
    let pid = Pid::from_u32(pid);
    system.refresh_process_specifics(pid, ProcessRefreshKind::new());
    system
        .process(pid)
        .is_some_and(|process| process.start_time().saturating_mul(1000) == process_start_ms)
}

fn sqlite_u64(value: u64) -> Result<i64, CrashStoreError> {
    i64::try_from(value).map_err(|_| CrashStoreError::IntegerRange)
}

fn quarantine(path: &Path) -> io::Result<()> {
    let mut invalid = path.as_os_str().to_os_string();
    invalid.push(".invalid");
    fs::rename(path, PathBuf::from(invalid))
}

fn spool_source_id(path: &Path, bytes: &[u8]) -> io::Result<String> {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid crash spool name"))?;
    Ok(format!("{name}:{}", blake3::hash(bytes).to_hex()))
}

fn ensure_private_root(path: &Path) -> io::Result<()> {
    reject_symlink(path)?;
    ensure_private_dir(path)?;
    reject_symlink(path)
}

fn reject_symlink(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("private crash path is a symlink: {}", path.display()),
        )),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn resolve_artifact_path(root: &Path, stored: &str) -> PathBuf {
    if stored.is_empty() {
        return PathBuf::new();
    }
    let stored = Path::new(stored);
    if stored.is_absolute() {
        // Schema v1 stored an absolute UTF-8 path. Keep those rows readable
        // while all new rows use an ASCII basename that is lossless even
        // when the private root itself is not Unicode.
        stored.to_path_buf()
    } else {
        root.join(stored)
    }
}

fn is_safe_artifact_path(root: &Path, path: &Path) -> bool {
    path.parent() == Some(root)
        && path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(is_owned_artifact_name)
}

fn is_owned_artifact_name(name: &str) -> bool {
    let Some(stem) = name
        .strip_prefix("crash-")
        .and_then(|name| name.strip_suffix(".json"))
    else {
        return false;
    };
    let Some((timestamp, suffix)) = stem.rsplit_once('-') else {
        return false;
    };
    !timestamp.is_empty()
        && timestamp.bytes().all(|byte| byte.is_ascii_digit())
        && suffix.len() == 32
        && suffix.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn is_owned_cleanup_path(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    if is_owned_artifact_name(name) {
        return true;
    }
    name.strip_prefix('.')
        .and_then(|name| name.strip_suffix(".tmp"))
        .is_some_and(|suffix| {
            suffix.len() == 32 && suffix.bytes().all(|byte| byte.is_ascii_hexdigit())
        })
}

fn is_safe_regular_artifact(root: &Path, path: &Path) -> bool {
    is_safe_artifact_path(root, path)
        && fs::symlink_metadata(path)
            .is_ok_and(|metadata| metadata.is_file() && !metadata.file_type().is_symlink())
}

fn open_artifact_for_fetch(
    root: &Path,
    path: &Path,
    expected_bytes: u64,
) -> io::Result<Option<fs::File>> {
    if !is_safe_artifact_path(root, path) {
        return Ok(None);
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt as _;
        // FILE_FLAG_OPEN_REPARSE_POINT: validate the named object, not its
        // target, and keep this exact handle for the whole fetch.
        options.custom_flags(0x0020_0000);
    }
    let file = match options.open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let metadata = file.metadata()?;
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt as _;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Ok(None);
        }
    }
    if !metadata.is_file() || metadata.len() != expected_bytes {
        return Ok(None);
    }
    Ok(Some(file))
}

fn unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
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
    use running_process::broker::secure_dir::private_dir_permissions_are_private;
    use running_process_probe::crash::spool::{
        encode, CrashFrame, CrashMetadata, CrashModule, CrashThread, RawCrashReport,
    };

    fn report(index: u64) -> RawCrashReport {
        RawCrashReport {
            pid: 123,
            tid: 456,
            fault_code: test_fault_code(),
            fault_address: 0xdead,
            crash_unix_ms: unix_millis().saturating_add(index),
            metadata: CrashMetadata {
                app_class: "compiler".into(),
                app_name: "frontend".into(),
                app_version: "1.2.3".into(),
                instance_name: "ci".into(),
                creation_time_ms: 888,
                cwd: "/workspace".into(),
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

    fn open_store(root: &Path, policy: CleanupPolicy) -> CrashStore {
        CrashStore::open_with_policy(
            &root.join("crashes.sqlite3"),
            &root.join("artifacts"),
            policy,
        )
        .unwrap()
    }

    fn unbounded_policy() -> CleanupPolicy {
        CleanupPolicy {
            max_age: Duration::ZERO,
            keep_last_n_per_app: 0,
            max_total_artifact_bytes: 0,
            max_rows: 0,
            max_single_artifact_bytes: DEFAULT_MAX_SINGLE_BYTES,
        }
    }

    fn worker_binary() -> Option<PathBuf> {
        let mut path = std::env::current_exe().ok()?;
        path.pop();
        if path.ends_with("deps") {
            path.pop();
        }
        let candidate = path.join(format!(
            "running-process-probe-worker{}",
            std::env::consts::EXE_SUFFIX
        ));
        if candidate.is_file() {
            return Some(candidate);
        }
        assert!(
            std::env::var_os("GITHUB_ACTIONS").is_none(),
            "worker binary missing at {} in CI",
            candidate.display()
        );
        None
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
    fn persists_across_reopen_with_all_tags() {
        let root = tempfile::tempdir().unwrap();
        let first = open_store(root.path(), unbounded_policy());
        let inserted = first.record(&report(1)).unwrap();
        drop(first);

        let reopened = open_store(root.path(), unbounded_policy());
        let rows = reopened.query_by_class("compiler", 10).unwrap();
        assert_eq!(rows, vec![inserted.clone()]);
        assert_eq!(inserted.app_name, "frontend");
        assert_eq!(inserted.pid, 123);
        assert_eq!(inserted.creation_time_ms, 888);
        assert_eq!(inserted.cwd, "/workspace");
        assert_eq!(inserted.signature.len(), 64);
        assert!(inserted.artifact_path.exists());
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn non_unicode_artifact_root_round_trips_losslessly() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt as _;

        let parent = tempfile::tempdir().unwrap();
        let artifacts = parent
            .path()
            .join(OsString::from_vec(b"crashes-\xff".to_vec()));
        let database = parent.path().join("crashes.sqlite3");
        let first =
            CrashStore::open_with_policy(&database, &artifacts, unbounded_policy()).unwrap();
        let inserted = first.record(&report(1)).unwrap();
        assert!(inserted.artifact_path.exists());
        drop(first);

        let reopened =
            CrashStore::open_with_policy(&database, &artifacts, unbounded_policy()).unwrap();
        let row = reopened.get(inserted.id).unwrap().unwrap();
        assert_eq!(row.artifact_path, inserted.artifact_path);
        assert!(row.artifact_path.exists());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_unicode_edge_artifact_root_round_trips() {
        let parent = tempfile::tempdir().unwrap();
        let artifacts = parent.path().join("crashes-e\u{301}-雪");
        let database = parent.path().join("crashes.sqlite3");
        let first =
            CrashStore::open_with_policy(&database, &artifacts, unbounded_policy()).unwrap();
        let inserted = first.record(&report(1)).unwrap();
        assert!(inserted.artifact_path.exists());
        drop(first);

        let reopened =
            CrashStore::open_with_policy(&database, &artifacts, unbounded_policy()).unwrap();
        let row = reopened.get(inserted.id).unwrap().unwrap();
        assert_eq!(row.artifact_path, inserted.artifact_path);
        assert!(row.artifact_path.exists());
    }

    #[test]
    fn same_pid_with_different_creation_time_is_distinct() {
        let root = tempfile::tempdir().unwrap();
        let store = open_store(root.path(), unbounded_policy());
        let first = store.record(&report(1)).unwrap();
        let mut reused = report(2);
        reused.metadata.creation_time_ms += 1;
        let second = store.record(&reused).unwrap();
        assert_ne!(first.id, second.id);
        assert_ne!(first.creation_time_ms, second.creation_time_ms);
        assert_eq!(store.query_by_class("compiler", 10).unwrap().len(), 2);
    }

    #[test]
    fn worker_availability_does_not_change_the_stable_signature() {
        let root = tempfile::tempdir().unwrap();
        let store = open_store(root.path(), unbounded_policy());
        let symbols = r#"{"threads":[{"os_tid":456,"frames":[
            {"function":"crash_here"},{"function":"main"}]}]}"#;
        let symbolized = store
            .record_with_symbol_report(&report(1), Some(symbols))
            .unwrap();
        let degraded = store.record(&report(2)).unwrap();
        assert_eq!(symbolized.signature, degraded.signature);
    }

    #[test]
    fn gc_prunes_oldest_but_never_an_in_use_artifact() {
        let root = tempfile::tempdir().unwrap();
        let policy = CleanupPolicy {
            keep_last_n_per_app: 2,
            ..unbounded_policy()
        };
        let store = open_store(root.path(), policy);
        let oldest = store.record(&report(1)).unwrap();
        let lease = store.begin_fetch(oldest.id).unwrap().unwrap();
        let second = store.record(&report(2)).unwrap();
        let third = store.record(&report(3)).unwrap();
        let fourth = store.record(&report(4)).unwrap();

        store.gc().unwrap();
        assert!(lease.path().exists());
        assert!(store.get(oldest.id).unwrap().is_some());
        assert!(store.get(second.id).unwrap().is_none());
        assert!(store.get(third.id).unwrap().is_some());
        assert!(store.get(fourth.id).unwrap().is_some());
        drop(lease);
        store.gc().unwrap();
        assert!(store.get(oldest.id).unwrap().is_none());
        assert!(!oldest.artifact_path.exists());
    }

    #[test]
    fn artifacts_and_directory_are_owner_only() {
        let root = tempfile::tempdir().unwrap();
        let store = open_store(root.path(), unbounded_policy());
        let _artifact_path = store.record(&report(1)).unwrap().artifact_path;
        assert!(private_dir_permissions_are_private(&store.artifacts_dir).unwrap());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(
                fs::metadata(_artifact_path).unwrap().permissions().mode() & 0o077,
                0
            );
        }
    }

    #[test]
    fn reopen_reconciles_missing_and_orphan_artifacts() {
        let root = tempfile::tempdir().unwrap();
        let store = open_store(root.path(), unbounded_policy());
        let row = store.record(&report(1)).unwrap();
        fs::remove_file(&row.artifact_path).unwrap();
        let orphan = store
            .artifacts_dir
            .join("crash-1-00000000000000000000000000000000.json");
        fs::write(&orphan, b"orphan").unwrap();
        let unknown = store.artifacts_dir.join("user-notes.txt");
        fs::write(&unknown, b"preserve").unwrap();
        drop(store);

        let reopened = open_store(root.path(), unbounded_policy());
        let row = reopened.get(row.id).unwrap().unwrap();
        assert!(row.artifact_path.as_os_str().is_empty());
        assert_eq!(row.artifact_bytes, 0);
        assert!(!orphan.exists());
        assert_eq!(fs::read(unknown).unwrap(), b"preserve");
    }

    #[test]
    fn reconciliation_never_unlinks_a_tampered_external_path() {
        let root = tempfile::tempdir().unwrap();
        let store = open_store(root.path(), unbounded_policy());
        let row = store.record(&report(1)).unwrap();
        let external = root.path().join("must-survive.txt");
        fs::write(&external, b"owner data").unwrap();
        store
            .conn
            .lock()
            .unwrap()
            .execute(
                "UPDATE crashes SET artifact_path = ?1 WHERE id = ?2",
                params![external.to_string_lossy(), row.id],
            )
            .unwrap();
        drop(store);

        let reopened = open_store(root.path(), unbounded_policy());
        assert_eq!(
            fs::read(&external).unwrap(),
            b"owner data",
            "a corrupted DB path must never extend cleanup outside artifacts"
        );
        assert!(reopened
            .get(row.id)
            .unwrap()
            .unwrap()
            .artifact_path
            .as_os_str()
            .is_empty());
    }

    #[test]
    fn oversized_artifact_is_rejected_without_filesystem_debris() {
        let root = tempfile::tempdir().unwrap();
        let policy = CleanupPolicy {
            max_single_artifact_bytes: 1,
            ..unbounded_policy()
        };
        let store = open_store(root.path(), policy);
        assert!(matches!(
            store.record(&report(1)),
            Err(CrashStoreError::ArtifactTooLarge { .. })
        ));
        assert!(fs::read_dir(&store.artifacts_dir).unwrap().next().is_none());
        assert!(store.query_by_class("compiler", 10).unwrap().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn artifact_root_symlink_is_rejected() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("target");
        fs::create_dir(&target).unwrap();
        let link = root.path().join("artifacts");
        symlink(&target, &link).unwrap();
        let error = CrashStore::open(&root.path().join("crashes.sqlite3"), &link)
            .err()
            .expect("symlink must be rejected");
        assert!(matches!(error, CrashStoreError::Io(_)));
    }

    #[test]
    fn concurrent_writers_are_serialized_by_sqlite() {
        let root = tempfile::tempdir().unwrap();
        let first = open_store(root.path(), unbounded_policy());
        let second = open_store(root.path(), unbounded_policy());
        std::thread::scope(|scope| {
            scope.spawn(|| {
                first.record(&report(1)).unwrap();
            });
            scope.spawn(|| {
                second.record(&report(2)).unwrap();
            });
        });
        assert_eq!(first.query_by_class("compiler", 10).unwrap().len(), 2);
    }

    #[test]
    fn concurrent_open_preserves_a_live_session_pin() {
        let root = tempfile::tempdir().unwrap();
        let first = open_store(root.path(), unbounded_policy());
        let row = first.record(&report(1)).unwrap();
        let lease = first.begin_fetch(row.id).unwrap().unwrap();
        let policy = CleanupPolicy {
            keep_last_n_per_app: 1,
            ..unbounded_policy()
        };
        let second = open_store(root.path(), policy);
        second.record(&report(2)).unwrap();
        assert!(second.get(row.id).unwrap().is_some());
        assert_eq!(lease.file().metadata().unwrap().len(), row.artifact_bytes);
        drop(lease);
        second.gc().unwrap();
        assert!(second.get(row.id).unwrap().is_none());
    }

    #[cfg(unix)]
    #[test]
    fn fetch_guard_reads_the_validated_handle_after_path_replacement() {
        use std::io::Read as _;

        let root = tempfile::tempdir().unwrap();
        let store = open_store(root.path(), unbounded_policy());
        let row = store.record(&report(1)).unwrap();
        let lease = store.begin_fetch(row.id).unwrap().unwrap();
        let moved = store.artifacts_dir.join("original-held-open.json");
        fs::rename(&row.artifact_path, &moved).unwrap();
        fs::write(&row.artifact_path, b"replacement").unwrap();

        let mut opened = lease.file().try_clone().unwrap();
        let mut contents = String::new();
        opened.read_to_string(&mut contents).unwrap();
        assert!(contents.contains("\"running-process.crash.v2\""));
        assert_ne!(contents, "replacement");
    }

    #[test]
    fn concurrent_open_cannot_remove_an_inflight_publication() {
        let root = tempfile::tempdir().unwrap();
        let first = open_store(root.path(), unbounded_policy());
        let db = root.path().join("crashes.sqlite3");
        let artifacts = root.path().join("artifacts");
        std::thread::scope(|scope| {
            let writer = scope.spawn(|| {
                for index in 0..20 {
                    first.record(&report(index)).unwrap();
                }
            });
            let opener = scope.spawn(|| {
                for _ in 0..20 {
                    let opened =
                        CrashStore::open_with_policy(&db, &artifacts, unbounded_policy()).unwrap();
                    assert!(opened
                        .query_by_class("compiler", 100)
                        .unwrap()
                        .iter()
                        .all(|row| row.artifact_path.exists()));
                }
            });
            writer.join().unwrap();
            opener.join().unwrap();
        });
        let rows = first.query_by_class("compiler", 100).unwrap();
        assert_eq!(rows.len(), 20);
        assert!(rows.iter().all(|row| row.artifact_path.exists()));
    }

    #[test]
    fn stale_session_pins_are_recovered_on_open() {
        let root = tempfile::tempdir().unwrap();
        let store = open_store(root.path(), unbounded_policy());
        let row = store.record(&report(1)).unwrap();
        {
            let conn = store.conn.lock().unwrap();
            conn.execute(
                "INSERT INTO crash_store_sessions (
                     session_id, pid, process_start_ms, boot_id
                 ) VALUES ('stale', ?1, 1, ?2)",
                params![i64::from(u32::MAX), store.session.boot_id],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO crash_fetch_pins (crash_id, session_id, pin_count)
                 VALUES (?1, 'stale', 1)",
                [row.id],
            )
            .unwrap();
            conn.execute("UPDATE crashes SET refcount = 1 WHERE id = ?1", [row.id])
                .unwrap();
        }
        drop(store);

        let reopened = open_store(root.path(), unbounded_policy());
        let refcount: i64 = reopened
            .conn
            .lock()
            .unwrap()
            .query_row(
                "SELECT refcount FROM crashes WHERE id = ?1",
                [row.id],
                |sql_row| sql_row.get(0),
            )
            .unwrap();
        assert_eq!(refcount, 0);
    }

    #[test]
    fn additive_migration_preserves_a_development_snapshot() {
        let root = tempfile::tempdir().unwrap();
        let db_path = root.path().join("crashes.sqlite3");
        let conn = Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE crashes (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 app_class TEXT NOT NULL DEFAULT ''
             );
             INSERT INTO crashes (app_class) VALUES ('legacy');",
        )
        .unwrap();
        drop(conn);

        let store = open_store(root.path(), unbounded_policy());
        let legacy = store.query_by_class("legacy", 10).unwrap();
        assert_eq!(legacy.len(), 1);
        assert_eq!(legacy[0].pid, 0);
        let inserted = store.record(&report(1)).unwrap();
        assert_eq!(store.get(inserted.id).unwrap(), Some(inserted));
    }

    #[test]
    fn pre_registration_record_is_ingested_when_daemon_appears() {
        let root = tempfile::tempdir().unwrap();
        let spool = root.path().join("spool");
        let reports = root.path().join("reports");
        ensure_private_dir(&spool).unwrap();
        let pending = spool.join("before-daemon.rpcrash");
        fs::write(&pending, encode(&report(1))).unwrap();

        let paths = ingest_pending(&spool, &reports).unwrap();
        assert_eq!(paths.len(), 1);
        assert!(!pending.exists());
        let json: serde_json::Value =
            serde_json::from_slice(&fs::read(&paths[0]).unwrap()).unwrap();
        assert_eq!(json["app_class"], "compiler");
        assert_eq!(json["creation_time_ms"], 888);
        assert_eq!(json["cwd"], "/workspace");
        assert_eq!(json["all_threads"].as_array().unwrap().len(), 2);
        assert_eq!(json["fault_address"], "0xdead");
        assert_eq!(json["raw_context_hex"], "aabb");
    }

    #[test]
    fn spool_retry_is_idempotent_but_a_distinct_source_is_not_deduplicated() {
        let root = tempfile::tempdir().unwrap();
        let spool = root.path().join("spool");
        let reports = root.path().join("reports");
        ensure_private_dir(&spool).unwrap();
        let bytes = encode(&report(1));
        let pending = spool.join("same-source.rpcrash");
        fs::write(&pending, bytes).unwrap();
        ingest_pending(&spool, &reports).unwrap();

        // Simulate a crash after DB commit but before source unlink: the same
        // stable source filename and content must return the original row.
        fs::write(&pending, bytes).unwrap();
        ingest_pending(&spool, &reports).unwrap();
        let store = CrashStore::open(&root.path().join("crashes.sqlite3"), &reports).unwrap();
        assert_eq!(store.query_by_class("compiler", 10).unwrap().len(), 1);

        // Two legitimate crashes can have identical bounded content. Their
        // independently pre-created spool identities keep them distinct.
        fs::write(spool.join("another-source.rpcrash"), bytes).unwrap();
        ingest_pending(&spool, &reports).unwrap();
        assert_eq!(store.query_by_class("compiler", 10).unwrap().len(), 2);
    }

    #[test]
    fn s7_spool_is_routed_through_the_real_s8_worker() {
        let Some(worker) = worker_binary() else {
            eprintln!("skipping: worker binary not built");
            return;
        };
        let root = tempfile::tempdir().unwrap();
        let spool = root.path().join("spool");
        ensure_private_dir(&spool).unwrap();
        let store = open_store(root.path(), unbounded_policy());
        let pending = spool.join("real-worker.rpcrash");
        fs::write(&pending, encode(&report(1))).unwrap();

        let paths = ingest_pending_with_store_and_worker(&spool, &store, Some(&worker)).unwrap();
        assert_eq!(paths.len(), 1);
        let json: serde_json::Value =
            serde_json::from_slice(&fs::read(&paths[0]).unwrap()).unwrap();
        assert!(
            json["symbolized"]["threads"].is_array(),
            "worker report missing from durable artifact: {json}"
        );
        assert_eq!(json["symbolized"]["threads"][0]["os_tid"], 456);
        assert_eq!(
            json["symbolized"]["threads"][0]["frames"][0]["module"],
            "fixture.exe"
        );
    }

    #[test]
    fn incomplete_record_remains_pending() {
        let root = tempfile::tempdir().unwrap();
        let spool = root.path().join("spool");
        let reports = root.path().join("reports");
        ensure_private_dir(&spool).unwrap();
        let pending = spool.join("writing.rpcrash");
        fs::write(&pending, [1, 2, 3]).unwrap();
        assert!(ingest_pending(&spool, &reports).unwrap().is_empty());
        assert!(pending.exists());
    }
}
