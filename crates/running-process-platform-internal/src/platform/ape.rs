//! Actually Portable Executable (APE / cosmocc) launch support.
//!
//! An APE image is simultaneously a Windows PE, a Bourne-shell script, and
//! (through its embedded loader) an ELF/Mach-O program. Windows runs it
//! natively. Unix kernels do not: `execve` returns `ENOEXEC` unless the host
//! registered an APE `binfmt_misc` handler, and `posix_spawn` -- what Rust's
//! `Command` uses -- never retries through `/bin/sh` the way an interactive
//! shell does. NixOS ships no APE handler, so a bare spawn fails with
//! "Exec format error".
//!
//! An APE image is launched through a loader instead: `<loader> <image>
//! <args...>`. Both loader kinds accept that argv shape:
//!
//! * the Cosmopolitan `ape` loader, which maps the image directly; or
//! * a POSIX `sh`, which runs the image's shell prologue; the prologue
//!   extracts its embedded loader to `$TMPDIR/.ape-*` and `exec`s it.
//!
//! On Linux this crate first provides the loader itself: every cosmocc image
//! embeds a gzip'd static ELF loader per CPU, located by `dd skip=N count=M`
//! lines in its prologue. The one for the host CPU is inflated, validated,
//! and installed content-addressed into an owner-only, exec-capable cache
//! directory, falling back to a sealed `memfd` when no such directory
//! exists. That path needs no `sh`, coreutils, `gzip`, `PATH`, or writable
//! `$TMPDIR` in the child environment.
//!
//! Loader precedence ([`plan_launch`]): an explicit loader
//! ([`LOADER_ENV`] or [`ApeOptions::loader`]) -> the loader embedded in the
//! image (Linux) -> `ape` on `PATH` -> well-known `ape` install locations ->
//! `/bin/sh` -> `sh` on `PATH`.
//!
//! Two ways in:
//!
//! * **Before a spawn**, wherever the child's program and environment are
//!   known: [`SpawnSpec`](crate::SpawnSpec) plans every launch, and
//!   [`command`] / [`tokio_command`] build a command that already runs
//!   through the planned loader; [`plan_launch`] exposes the plan to callers
//!   that build their own. Planning first matters: once a `pre_exec` hook
//!   routes std through `execvp`, glibc hands a refused image to `/bin/sh`
//!   silently, and the prologue then needs `PATH`, `dd` and `gzip`.
//! * **After a refusal**, for a command the caller built: it keeps every
//!   caller setting and is retried once through `execvp`, whose POSIX
//!   `ENOEXEC` rule runs the image's prologue ([`spawn_std`]).
//!
//! Writing a loader and executing it races with other threads' forks: a
//! child forked while the writable descriptor is open holds it until its own
//! `exec`, and executing the loader fails with `ETXTBSY` meanwhile. The
//! process-wide [`fork_guard`] / [`exclusive_fork_guard`] pair (Go's
//! `syscall.ForkLock`) closes that window for every spawn that goes through
//! this crate, and [`retry_while_busy`] covers spawns that do not.

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::time::SystemTime;

pub use crate::{
    ape_is_exec_format_error as is_exec_format_error,
    APE_EXECVP_SHELL_FALLBACK as EXECVP_SHELL_FALLBACK, APE_LOADER_HOST as LOADER_HOST,
    APE_NEEDS_LOADER as NEEDS_LOADER, APE_SHELL as SHELL, APE_SYSTEM_LOADERS as SYSTEM_LOADERS,
};

/// Environment variable naming an explicit APE loader (an `ape` binary or a
/// POSIX shell). Read from the child environment first, then this process's.
pub const LOADER_ENV: &str = "RUNNING_PROCESS_APE_LOADER";

/// Environment variable naming the preferred directory for loaders extracted
/// from APE images. Tried before the host defaults; like them it is used only
/// when owned by the current user, not group/world-writable, and on an
/// exec-capable mount.
pub const CACHE_DIR_ENV: &str = "RUNNING_PROCESS_APE_CACHE_DIR";

/// Leading bytes of every APE image: `MZqFpD='` (the standard header, also a
/// valid DOS/PE `MZ` stub), `jartsr='` (non-Windows), `APEDBG='` (debug).
pub const MAGICS: [&[u8]; 3] = [b"MZqFpD='", b"jartsr='", b"APEDBG='"];

/// Bytes of an image scanned for the shell prologue. Real prologues end
/// within the first ~8 KiB; the cap bounds work on hostile input.
#[cfg(feature = "ape-loader")]
const PROLOGUE_SCAN_BYTES: u64 = 64 * 1024;

/// Upper bound on a compressed or inflated embedded loader (real ones are
/// ~4-5 KiB compressed, ~10 KiB inflated).
#[cfg(feature = "ape-loader")]
const MAX_LOADER_BYTES: u64 = 4 * 1024 * 1024;

/// Whether `header` begins with an APE magic.
pub fn is_ape_header(header: &[u8]) -> bool {
    MAGICS.iter().any(|magic| header.starts_with(magic))
}

/// Whether the file at `path` is an APE image. Unreadable files are not.
pub fn is_ape_file(path: &Path) -> bool {
    let mut header = [0u8; 8];
    File::open(path)
        .and_then(|mut file| file.read_exact(&mut header))
        .is_ok_and(|()| is_ape_header(&header))
}

// ---------------------------------------------------------------------------
// Fork lock
// ---------------------------------------------------------------------------

/// Process-wide fork lock, as Go's `syscall.ForkLock`.
///
/// Spawns hold it shared across fork->exec (`spawn` returns only after the
/// child has exec'd); writers of to-be-executed files hold it exclusively
/// while their descriptor is open, so no child can inherit it.
static FORK_LOCK: RwLock<()> = RwLock::new(());

/// Hold across a spawn so no executable is being written meanwhile.
///
/// Never hold it while planning a launch: planning may take the exclusive
/// guard to install a loader.
pub fn fork_guard() -> RwLockReadGuard<'static, ()> {
    FORK_LOCK.read().unwrap_or_else(|error| error.into_inner())
}

/// Hold while a file that will be executed is open for writing.
pub fn exclusive_fork_guard() -> RwLockWriteGuard<'static, ()> {
    FORK_LOCK.write().unwrap_or_else(|error| error.into_inner())
}

/// Run `spawn` again while the host reports the program busy (`ETXTBSY`).
///
/// For spawns outside this crate's fork lock: a file another thread has just
/// written can still be open for writing in a child forked before the write
/// finished, until that child execs. A short backoff closes the window.
pub fn retry_while_busy<T>(mut spawn: impl FnMut() -> io::Result<T>) -> io::Result<T> {
    for delay_ms in [1, 4, 16, 64, 256] {
        match spawn() {
            Err(error) if error.kind() == io::ErrorKind::ExecutableFileBusy => {
                std::thread::sleep(std::time::Duration::from_millis(delay_ms));
            }
            result => return result,
        }
    }
    spawn()
}

// ---------------------------------------------------------------------------
// Planning
// ---------------------------------------------------------------------------

/// What a launch plan consults, as the child will see it.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ApeOptions {
    /// The child's `PATH`: locates a bare program name, `ape` and `sh`.
    pub path: Option<OsString>,
    /// An explicit loader, path or bare name; wins over every other choice.
    pub loader: Option<OsString>,
    /// Directories to install an extracted loader in, in order. The first
    /// usable one wins; with none usable a sealed `memfd` is used (Linux).
    pub cache_dirs: Vec<PathBuf>,
}

impl ApeOptions {
    /// The values a child inherits from this process: `PATH`,
    /// [`LOADER_ENV`], then [`CACHE_DIR_ENV`] ahead of the host default
    /// cache directories.
    pub fn inherited() -> Self {
        Self::with_overrides(false, [])
    }

    /// Apply a command's environment edits over the inherited values; `None`
    /// removes a variable and `clear` starts from an empty environment, as
    /// `env_clear` does. The host default cache directories always follow,
    /// since they do not depend on the child's environment.
    pub fn with_overrides<'a>(
        clear: bool,
        overrides: impl IntoIterator<Item = (&'a OsStr, Option<&'a OsStr>)>,
    ) -> Self {
        let inherited = |name: &str| {
            if clear {
                return None;
            }
            match name {
                "PATH" => crate::env_vars::PATH.os(),
                LOADER_ENV => crate::env_vars::APE_LOADER.os(),
                _ => crate::env_vars::APE_CACHE_DIR.os(),
            }
        };
        let mut path = inherited("PATH");
        let mut loader = inherited(LOADER_ENV);
        let mut cache_dir = inherited(CACHE_DIR_ENV);
        for (key, value) in overrides {
            let slot = match key.to_str() {
                Some("PATH") => &mut path,
                Some(LOADER_ENV) => &mut loader,
                Some(CACHE_DIR_ENV) => &mut cache_dir,
                _ => continue,
            };
            *slot = value.map(OsStr::to_os_string);
        }
        let set = |value: Option<OsString>| value.filter(|value| !value.is_empty());
        let mut cache_dirs: Vec<PathBuf> = set(cache_dir).map(PathBuf::from).into_iter().collect();
        cache_dirs.extend(crate::ape_default_loader_dirs());
        Self {
            path: set(path),
            loader: set(loader),
            cache_dirs,
        }
    }
}

/// Which loader a planned launch runs the image through.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LoaderKind {
    /// The loader named by [`LOADER_ENV`] or [`ApeOptions::loader`].
    Explicit,
    /// The loader embedded in the image, extracted by this crate.
    Embedded,
    /// An `ape` loader installed on the host.
    System,
    /// A POSIX shell, which runs the image's own prologue.
    Shell,
}

/// How to run one APE image: `loader image args...`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ApeLaunch {
    pub kind: LoaderKind,
    /// Program to execute in place of the image.
    pub loader: PathBuf,
    /// Absolute path of the image, passed as the loader's first argument.
    pub image: PathBuf,
    /// A private directory holding this loader as `ape`. Spawners put it
    /// first on the child's `PATH` ([`Self::child_path`]) so APE programs the
    /// child spawns in turn (gcc -> `cc1`) find a loader through their
    /// prologue's `type ape`; without it those nested spawns need `sh`,
    /// coreutils and a writable temporary directory.
    pub ape_path_dir: Option<PathBuf>,
}

impl ApeLaunch {
    /// The child's `PATH` with [`Self::ape_path_dir`] first, given the `PATH`
    /// it would otherwise get. `None` when there is nothing to add.
    pub fn child_path(&self, inherited: Option<&OsStr>) -> Option<OsString> {
        let dir = self.ape_path_dir.as_ref()?;
        let rest = inherited.filter(|path| !path.is_empty());
        std::env::join_paths(
            std::iter::once(dir.clone()).chain(rest.into_iter().flat_map(std::env::split_paths)),
        )
        .ok()
    }

    /// Loader arguments: the image, then the caller's arguments unchanged.
    pub fn args<I, S>(&self, args: I) -> Vec<OsString>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        std::iter::once(self.image.clone().into_os_string())
            .chain(args.into_iter().map(|arg| arg.as_ref().to_os_string()))
            .collect()
    }
}

/// Plan how to run `program` if it resolves to an APE image.
///
/// `current_dir` is the child's working directory, used to resolve a
/// relative program path. `None` means `program` is not an APE image, the
/// host runs APE images natively, or no loader is available; in each case
/// the program should be spawned as it is.
pub fn plan_launch(
    program: &OsStr,
    current_dir: Option<&Path>,
    options: &ApeOptions,
) -> Option<ApeLaunch> {
    if !NEEDS_LOADER {
        return None;
    }
    let image = resolve_program(program, current_dir, options.path.as_deref())?;
    if !is_ape_file(&image) {
        return None;
    }
    let launch = |kind, loader: PathBuf| ApeLaunch {
        kind,
        ape_path_dir: ape_path_dir(&loader, &options.cache_dirs),
        loader,
        image: image.clone(),
    };
    if let Some(explicit) = options.loader.as_deref() {
        return resolve_explicit_loader(explicit, options.path.as_deref())
            .map(|loader| launch(LoaderKind::Explicit, loader));
    }
    if let Some(loader) = embedded_loader(&image, &options.cache_dirs) {
        return Some(launch(LoaderKind::Embedded, loader));
    }
    if let Some(loader) = system_loader(options.path.as_deref()) {
        return Some(launch(LoaderKind::System, loader));
    }
    shell(options.path.as_deref()).map(|loader| launch(LoaderKind::Shell, loader))
}

/// Resolve `program` to the file the child's `execvp` would execute: a path
/// with a separator is taken as-is (relative to `current_dir` when given), a
/// bare name is searched on `path`. The result is absolute.
pub fn resolve_program(
    program: &OsStr,
    current_dir: Option<&Path>,
    path: Option<&OsStr>,
) -> Option<PathBuf> {
    let program_path = Path::new(program);
    if program_path.components().count() > 1 || program_path.is_absolute() {
        let resolved = match current_dir {
            Some(dir) if program_path.is_relative() => dir.join(program_path),
            _ => program_path.to_path_buf(),
        };
        return std::path::absolute(resolved).ok();
    }
    find_executable_on_path(program, path).and_then(|found| std::path::absolute(found).ok())
}

fn resolve_explicit_loader(explicit: &OsStr, path: Option<&OsStr>) -> Option<PathBuf> {
    let explicit_path = Path::new(explicit);
    if explicit_path.components().count() > 1 || explicit_path.is_absolute() {
        return Some(explicit_path.to_path_buf());
    }
    find_executable_on_path(explicit, path)
}

/// An installed `ape`: the child's `PATH` first, then the host's
/// conventional install locations.
fn system_loader(path: Option<&OsStr>) -> Option<PathBuf> {
    find_executable_on_path(OsStr::new("ape"), path)
        .or_else(|| first_executable(SYSTEM_LOADERS.iter().map(PathBuf::from)))
}

fn shell(path: Option<&OsStr>) -> Option<PathBuf> {
    first_executable([PathBuf::from(SHELL)])
        .or_else(|| find_executable_on_path(OsStr::new("sh"), path))
}

fn find_executable_on_path(name: &OsStr, path: Option<&OsStr>) -> Option<PathBuf> {
    let dirs = std::env::split_paths(path?).filter(|dir| !dir.as_os_str().is_empty());
    first_executable(dirs.map(|dir| dir.join(name)))
}

fn first_executable(candidates: impl IntoIterator<Item = PathBuf>) -> Option<PathBuf> {
    candidates.into_iter().find(|candidate| {
        std::fs::metadata(candidate)
            .is_ok_and(|meta| meta.is_file() && crate::ape_is_executable(&meta))
    })
}

// ---------------------------------------------------------------------------
// Embedded loader
// ---------------------------------------------------------------------------

/// Which loader embedded in an APE image a host can run.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LoaderHost {
    /// No embedded loader is used (Windows runs APE natively).
    None,
    /// The static ELF loader from the prologue's Linux branch.
    Linux,
    /// The x86_64 Mach-O loader, or the Apple Silicon loader compiled from
    /// the image's `ape-m1.c` with the host `cc`.
    Macos,
}

/// Byte range `(skip, count)` within an image.
pub type Range = (u64, u64);

/// Where one image's prologue keeps its loaders.
///
/// cosmocc emits the same line shapes for every image; only the numbers
/// change. Recognized lines, keyed by the enclosing `if [ "$m" = <cpu> ]`:
///
/// * `dd if="$o" skip=N count=M ... | gzip -dc >"$t.$$"` -- a gzip'd loader.
///   When the next line patches it (`dd if="$t.$$" of="$t.$$" skip=5 count=8
///   bs=64`) it is the macOS x86_64 loader (the ELF blob turned Mach-O),
///   otherwise the Linux loader for the CPU.
/// * `dd if="$o" skip=N count=M ... | gzip -dc >"$t.c.$$"` -- the gzip'd C
///   source of the Apple Silicon loader (`ape-m1.c`), compiled with `cc`.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Prologue {
    pub linux_loader_x86_64: Option<Range>,
    pub linux_loader_aarch64: Option<Range>,
    pub macos_loader_x86_64: Option<Range>,
    pub macos_loader_source_aarch64: Option<Range>,
}

impl Prologue {
    /// Parse the prologue text at the start of an APE image.
    pub fn parse(prologue: &[u8]) -> Self {
        let text = String::from_utf8_lossy(prologue);
        let mut parsed = Self::default();
        let mut cpu = None;
        let mut lines = text.lines().map(str::trim).peekable();
        while let Some(line) = lines.next() {
            if line.contains("\"$m\" = x86_64") {
                cpu = Some("x86_64");
            } else if line.contains("\"$m\" = aarch64") {
                cpu = Some("aarch64");
            }
            let Some(cpu) = cpu else { continue };
            if !line.starts_with("dd if=\"$o\" ") {
                continue;
            }
            if line.contains("| gzip -dc >\"$t.c.$$\"") {
                if cpu == "aarch64" {
                    parsed.macos_loader_source_aarch64 = range(line);
                }
            } else if line.contains("| gzip -dc >\"$t.$$\"") {
                let patched = lines
                    .peek()
                    .is_some_and(|next| next.starts_with("dd if=\"$t.$$\" of=\"$t.$$\""));
                match (cpu, patched) {
                    ("x86_64", true) => parsed.macos_loader_x86_64 = range(line),
                    ("x86_64", false) => parsed.linux_loader_x86_64 = range(line),
                    ("aarch64", false) => parsed.linux_loader_aarch64 = range(line),
                    _ => {}
                }
            }
        }
        parsed
    }

    /// The Linux loader for `machine` (`uname -m` spelling).
    pub fn linux_loader(&self, machine: &str) -> Option<Range> {
        match machine {
            "x86_64" => self.linux_loader_x86_64,
            "aarch64" => self.linux_loader_aarch64,
            _ => None,
        }
    }
}

fn range(line: &str) -> Option<Range> {
    let field = |key: &str| {
        line.split_whitespace()
            .find_map(|token| token.strip_prefix(key))
            .and_then(|value| value.parse::<u64>().ok())
    };
    Some((field("skip=")?, field("count=")?))
}

/// `(skip, count)` of the gzip'd Linux loader for `machine`.
pub fn loader_blob_range(prologue: &[u8], machine: &str) -> Option<Range> {
    Prologue::parse(prologue).linux_loader(machine)
}

/// Identity of an image (and where its loader may live) for the memos: a
/// rebuilt image at the same path changes length or mtime and is
/// re-extracted.
type ImageKey = (PathBuf, u64, Option<SystemTime>, Vec<PathBuf>);

struct Memo(Mutex<Option<HashMap<ImageKey, PathBuf>>>);

impl Memo {
    const fn new() -> Self {
        Self(Mutex::new(None))
    }

    fn get(&self, key: &ImageKey) -> Option<PathBuf> {
        let guard = self.0.lock().unwrap_or_else(|error| error.into_inner());
        let hit = guard.as_ref()?.get(key)?.clone();
        // A cache cleaner may have removed it since; re-materialize then.
        hit.exists().then_some(hit)
    }

    fn put(&self, key: ImageKey, path: PathBuf) {
        let mut guard = self.0.lock().unwrap_or_else(|error| error.into_inner());
        guard.get_or_insert_with(HashMap::new).insert(key, path);
    }
}

static LOADER_MEMO: Memo = Memo::new();

fn image_key(image: &Path, cache_dirs: &[PathBuf]) -> Option<ImageKey> {
    let meta = std::fs::metadata(image).ok()?;
    Some((
        image.to_path_buf(),
        meta.len(),
        meta.modified().ok(),
        cache_dirs.to_vec(),
    ))
}

/// The host-runnable loader embedded in `image`, installed into the first
/// usable `cache_dirs` entry (or, on Linux, a sealed memfd). `None` when the
/// host runs APE natively, the image carries no valid loader for this host,
/// or nothing could be materialized.
pub fn embedded_loader(image: &Path, cache_dirs: &[PathBuf]) -> Option<PathBuf> {
    if LOADER_HOST == LoaderHost::None {
        return None;
    }
    let key = image_key(image, cache_dirs)?;
    if let Some(hit) = LOADER_MEMO.get(&key) {
        return Some(hit);
    }
    let loader = install_embedded_loader(image, cache_dirs)?;
    LOADER_MEMO.put(key, loader.clone());
    Some(loader)
}

#[cfg(feature = "ape-loader")]
fn install_embedded_loader(image: &Path, cache_dirs: &[PathBuf]) -> Option<PathBuf> {
    let machine = std::env::consts::ARCH;
    match (LOADER_HOST, machine) {
        (LoaderHost::Linux, _) => {
            let bytes = extract_loader(image, machine)?;
            let name = format!("ape-loader-linux-{machine}-{}", short_hash(&bytes));
            materialize(&bytes, &name, cache_dirs)
        }
        (LoaderHost::Macos, "x86_64") => {
            let bytes = extract_macos_x86_64_loader(image)?;
            let name = format!("ape-loader-macos-{machine}-{}", short_hash(&bytes));
            materialize(&bytes, &name, cache_dirs)
        }
        (LoaderHost::Macos, "aarch64") => compile_macos_aarch64_loader(image, cache_dirs),
        _ => None,
    }
}

#[cfg(not(feature = "ape-loader"))]
fn install_embedded_loader(_image: &Path, _cache_dirs: &[PathBuf]) -> Option<PathBuf> {
    None
}

/// Install `bytes` as an executable named `name` in the first usable cache
/// directory, else as a host anonymous executable (a Linux memfd).
#[cfg(feature = "ape-loader")]
fn materialize(bytes: &[u8], name: &str, dirs: &[PathBuf]) -> Option<PathBuf> {
    install(dirs, None, name, bytes).or_else(|| crate::ape_anonymous_executable(bytes, name))
}

/// Content address for an installed file. Collisions are harmless: an
/// installed file is reused only when its bytes match exactly.
fn short_hash(bytes: &[u8]) -> String {
    let hash = bytes.iter().fold(0xcbf2_9ce4_8422_2325_u64, |hash, &byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3)
    });
    format!("{hash:016x}")
}

/// Install `bytes` as executable `<dir>/<subdir>/<name>` in the first usable
/// candidate `dir`, reusing an identical existing file. `None` when no
/// candidate is usable.
pub fn install(
    dirs: &[PathBuf],
    subdir: Option<&str>,
    name: &str,
    bytes: &[u8],
) -> Option<PathBuf> {
    dirs.iter().find_map(|dir| match subdir {
        // The parent must pass the same checks before anything nests in it.
        Some(sub) => crate::ape_private_exec_dir(dir)
            .then(|| install_in(&dir.join(sub), bytes, name))
            .flatten(),
        None => install_in(dir, bytes, name),
    })
}

/// Install `bytes` as executable `<dir>/<name>` when `dir` is private to this
/// user and exec-capable. Installs are atomic (staging file + `rename`), a
/// tampered, truncated or non-executable copy is replaced, and the staging
/// file is written under the exclusive fork guard.
pub fn install_in(dir: &Path, bytes: &[u8], name: &str) -> Option<PathBuf> {
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};

    if !crate::ape_private_exec_dir(dir) {
        return None;
    }
    let target = dir.join(name);
    if is_installed(&target, bytes) {
        return Some(target);
    }
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let staging = dir.join(format!(
        ".{name}.{}.{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    // No child may inherit the writable descriptor, or executing the file
    // would hit ETXTBSY until that child execs.
    let written = {
        let _fork = exclusive_fork_guard();
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&staging)
            .and_then(|mut file| {
                file.write_all(bytes)?;
                file.sync_all()
            })
    }
    .and_then(|()| crate::ape_mark_executable(&staging))
    // rename(2) is atomic: concurrent installers race benignly to identical
    // content, and readers never see a partial file.
    .and_then(|()| std::fs::rename(&staging, &target));
    if written.is_err() {
        let _ = std::fs::remove_file(&staging);
        return None;
    }
    is_installed(&target, bytes).then_some(target)
}

/// A regular (not symlinked), executable file whose content is exactly
/// `bytes`.
fn is_installed(path: &Path, bytes: &[u8]) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|meta| {
        meta.file_type().is_file()
            && crate::ape_is_executable(&meta)
            && meta.len() == bytes.len() as u64
    }) && std::fs::read(path).is_ok_and(|content| content == bytes)
}

/// A private directory containing `loader` under the name `ape`, for
/// [`ApeLaunch::ape_path_dir`]: with it first on the child's `PATH`, APE
/// programs the child spawns in turn (gcc -> `cc1`) find a loader through
/// their prologue's `type ape`. A shell is never exposed as `ape`, and a
/// memfd loader cannot serve a grandchild, so neither yields a directory.
fn ape_path_dir(loader: &Path, cache_dirs: &[PathBuf]) -> Option<PathBuf> {
    if loader.file_name().is_some_and(|name| name == "sh") || loader.starts_with("/proc/self") {
        return None;
    }
    if loader.file_name().is_some_and(|name| name == "ape") {
        return loader.parent().map(Path::to_path_buf);
    }
    let bytes = std::fs::read(loader).ok()?;
    if bytes.len() as u64 > MAX_PATH_LOADER_BYTES {
        return None;
    }
    let sub = format!("bin-{}", short_hash(&bytes));
    install(cache_dirs, Some(&sub), "ape", &bytes)?
        .parent()
        .map(Path::to_path_buf)
}

/// Largest loader copied into an `ape` `PATH` directory.
const MAX_PATH_LOADER_BYTES: u64 = 4 * 1024 * 1024;

/// An opened image with its parsed prologue.
#[cfg(feature = "ape-loader")]
struct Image {
    file: File,
    len: u64,
    prologue: Prologue,
}

#[cfg(feature = "ape-loader")]
impl Image {
    fn open(image: &Path) -> Option<Self> {
        let mut file = File::open(image).ok()?;
        let len = file.metadata().ok()?.len();
        let mut head = Vec::new();
        (&mut file)
            .take(PROLOGUE_SCAN_BYTES)
            .read_to_end(&mut head)
            .ok()?;
        if !is_ape_header(&head) {
            return None;
        }
        Some(Self {
            file,
            len,
            prologue: Prologue::parse(&head),
        })
    }

    /// Read `range`, bounded by the file length and [`MAX_LOADER_BYTES`].
    fn read(&mut self, (skip, count): Range) -> Option<Vec<u8>> {
        use std::io::{Seek, SeekFrom};
        let end = skip.checked_add(count)?;
        if count == 0 || count > MAX_LOADER_BYTES || end > self.len {
            return None;
        }
        self.file.seek(SeekFrom::Start(skip)).ok()?;
        let mut out = Vec::with_capacity(count as usize);
        (&mut self.file).take(count).read_to_end(&mut out).ok()?;
        (out.len() as u64 == count).then_some(out)
    }

    fn inflate(&mut self, range: Range) -> Option<Vec<u8>> {
        gunzip(&self.read(range)?, MAX_LOADER_BYTES as usize).ok()
    }
}

/// ELF `e_machine` of the static loader cosmocc embeds for `machine`
/// (`uname -m` spelling).
#[cfg(feature = "ape-loader")]
fn elf_machine(machine: &str) -> Option<u16> {
    match machine {
        "x86_64" => Some(62),   // EM_X86_64
        "aarch64" => Some(183), // EM_AARCH64
        _ => None,
    }
}

/// Inflate and validate the Linux ELF loader `image` embeds for `machine`
/// (`"x86_64"` or `"aarch64"`).
#[cfg(feature = "ape-loader")]
pub fn extract_loader(image: &Path, machine: &str) -> Option<Vec<u8>> {
    let elf_machine = elf_machine(machine)?;
    let mut image = Image::open(image)?;
    let range = image.prologue.linux_loader(machine)?;
    let elf = image.inflate(range)?;
    is_static_elf_for(&elf, elf_machine).then_some(elf)
}

/// The macOS x86_64 loader: the same gzip'd blob as Linux, with the Mach-O
/// header it carries at offset 320 (8 x 64 bytes) moved to offset 0 -- what
/// the prologue's `dd if="$t.$$" of="$t.$$" skip=5 count=8 bs=64` does.
#[cfg(feature = "ape-loader")]
pub fn extract_macos_x86_64_loader(image: &Path) -> Option<Vec<u8>> {
    let mut image = Image::open(image)?;
    let range = image.prologue.macos_loader_x86_64?;
    let mut bin = image.inflate(range)?;
    if bin.len() < 320 + 512 {
        return None;
    }
    bin.copy_within(320..320 + 512, 0);
    is_macho_x86_64(&bin).then_some(bin)
}

/// The Apple Silicon loader source (`ape-m1.c`) embedded in `image`.
#[cfg(feature = "ape-loader")]
pub fn extract_macos_aarch64_loader_source(image: &Path) -> Option<Vec<u8>> {
    let mut image = Image::open(image)?;
    let range = image.prologue.macos_loader_source_aarch64?;
    let source = image.inflate(range)?;
    (std::str::from_utf8(&source).is_ok() && source.windows(4).any(|window| window == b"main"))
        .then_some(source)
}

/// Compile the image's `ape-m1.c` with the host C compiler (Xcode Command
/// Line Tools) into a content-addressed cache entry, as the prologue would,
/// without depending on `sh`, `dd`, `gzip` or `$TMPDIR`.
#[cfg(feature = "ape-loader")]
fn compile_macos_aarch64_loader(image: &Path, dirs: &[PathBuf]) -> Option<PathBuf> {
    let source = extract_macos_aarch64_loader_source(image)?;
    let name = format!("ape-loader-macos-aarch64-{}", short_hash(&source));
    let dir = dirs.iter().find(|dir| crate::ape_private_exec_dir(dir))?;
    let target = dir.join(&name);
    if first_executable([target.clone()]).is_some() {
        return Some(target);
    }
    let tag = format!(
        "{}.{}",
        std::process::id(),
        short_hash(target.as_os_str().as_encoded_bytes())
    );
    let source_path = dir.join(format!(".{name}.{tag}.c"));
    let out_path = dir.join(format!(".{name}.{tag}"));
    std::fs::write(&source_path, &source).ok()?;
    let cc =
        first_executable([PathBuf::from("/usr/bin/cc")]).unwrap_or_else(|| PathBuf::from("cc"));
    let mut command = std::process::Command::new(&cc);
    command
        .args(["-w", "-O", "-o"])
        .arg(&out_path)
        .arg(&source_path)
        .stdin(std::process::Stdio::null());
    let compiled = retry_while_busy(|| {
        let _fork = fork_guard();
        command.output()
    });
    let _ = std::fs::remove_file(&source_path);
    let built = compiled.is_ok_and(|output| output.status.success())
        && std::fs::rename(&out_path, &target).is_ok();
    if !built {
        let _ = std::fs::remove_file(&out_path);
        return None;
    }
    Some(target)
}

/// A 64-bit little-endian ELF executable for `machine`.
#[cfg(feature = "ape-loader")]
fn is_static_elf_for(elf: &[u8], machine: u16) -> bool {
    elf.len() >= 64
        && elf.len() as u64 <= MAX_LOADER_BYTES
        && elf.starts_with(b"\x7fELF")
        && elf[4] == 2 // ELFCLASS64
        && elf[5] == 1 // ELFDATA2LSB
        // ET_EXEC (x86_64 loader) or ET_DYN (position-independent aarch64 loader)
        && matches!(u16::from_le_bytes([elf[16], elf[17]]), 2 | 3)
        && u16::from_le_bytes([elf[18], elf[19]]) == machine
}

/// A 64-bit Mach-O for x86_64 (`MH_MAGIC_64`, `CPU_TYPE_X86_64`).
#[cfg(feature = "ape-loader")]
fn is_macho_x86_64(bin: &[u8]) -> bool {
    bin.len() >= 32 && bin.starts_with(&[0xcf, 0xfa, 0xed, 0xfe]) && bin[4..8] == [7, 0, 0, 1]
}

/// Decode one gzip member (RFC 1952), refusing output over `limit` bytes.
#[cfg(feature = "ape-loader")]
pub fn gunzip(member: &[u8], limit: usize) -> io::Result<Vec<u8>> {
    const FHCRC: u8 = 0x02;
    const FEXTRA: u8 = 0x04;
    const FNAME: u8 = 0x08;
    const FCOMMENT: u8 = 0x10;

    if member.len() < 18 || member[..3] != [0x1f, 0x8b, 8] {
        return Err(invalid("not a deflate gzip member"));
    }
    let flags = member[3];
    let mut at = 10;
    if flags & FEXTRA != 0 {
        let extra = member
            .get(at..at + 2)
            .ok_or_else(|| invalid("truncated gzip"))?;
        at += 2 + usize::from(u16::from_le_bytes([extra[0], extra[1]]));
    }
    for flag in [FNAME, FCOMMENT] {
        if flags & flag != 0 {
            let terminator = member
                .get(at..)
                .and_then(|rest| rest.iter().position(|&byte| byte == 0))
                .ok_or_else(|| invalid("truncated gzip"))?;
            at += terminator + 1;
        }
    }
    if flags & FHCRC != 0 {
        at += 2;
    }
    let trailer = member.len() - 8;
    let deflate = member
        .get(at..trailer)
        .ok_or_else(|| invalid("truncated gzip"))?;
    let output = miniz_oxide::inflate::decompress_to_vec_with_limit(deflate, limit)
        .map_err(|error| invalid(&format!("corrupt gzip member: {error}")))?;
    let size = u32::from_le_bytes(member[trailer + 4..].try_into().expect("four bytes"));
    if output.len() as u32 != size {
        return Err(invalid("gzip member size does not match its trailer"));
    }
    Ok(output)
}

#[cfg(feature = "ape-loader")]
fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.to_owned())
}

// ---------------------------------------------------------------------------
// Launching
// ---------------------------------------------------------------------------

/// A [`std::process::Command`] for `program` that runs an APE image through
/// its planned loader, with this process's environment as the child's.
///
/// Use in place of `Command::new` for an external tool. A relative `program`
/// path is resolved against this process's working directory; pass an
/// absolute path when the child's `current_dir` differs.
pub fn command(program: impl AsRef<OsStr>) -> std::process::Command {
    let program = program.as_ref();
    let options = ApeOptions::inherited();
    match plan_launch(program, None, &options) {
        Some(launch) => {
            let mut command = std::process::Command::new(&launch.loader);
            command.arg(&launch.image);
            if let Some(path) = launch.child_path(options.path.as_deref()) {
                command.env("PATH", path);
            }
            command
        }
        None => std::process::Command::new(program),
    }
}

/// Tokio counterpart of [`command`].
#[cfg(feature = "async-process")]
pub fn tokio_command(program: impl AsRef<OsStr>) -> tokio::process::Command {
    let program = program.as_ref();
    let options = ApeOptions::inherited();
    match plan_launch(program, None, &options) {
        Some(launch) => {
            let mut command = tokio::process::Command::new(&launch.loader);
            command.arg(&launch.image);
            if let Some(path) = launch.child_path(options.path.as_deref()) {
                command.env("PATH", path);
            }
            command
        }
        None => tokio::process::Command::new(program),
    }
}

/// Prepare a caller-built command to be spawned again as an APE image.
///
/// Returns `true` when `error` is the kernel refusing an APE image and the
/// command now routes through `execvp`, whose `ENOEXEC` rule runs the
/// image's prologue with the host shell; spawn it once more. Every setting
/// the caller made is kept. For the direct-loader route, build the command
/// with [`command`] instead.
pub fn prepare_std_retry(command: &mut std::process::Command, error: &io::Error) -> bool {
    if !retryable(command, error) {
        return false;
    }
    crate::ape_route_through_execvp(command);
    true
}

/// [`prepare_std_retry`] for a Tokio command.
#[cfg(feature = "async-process")]
pub fn prepare_tokio_retry(command: &mut tokio::process::Command, error: &io::Error) -> bool {
    if !retryable(command.as_std(), error) {
        return false;
    }
    crate::ape_route_tokio_through_execvp(command);
    true
}

fn retryable(command: &std::process::Command, error: &io::Error) -> bool {
    if !EXECVP_SHELL_FALLBACK || !is_exec_format_error(error) {
        return false;
    }
    let options = ApeOptions::with_overrides(false, command.get_envs());
    resolve_program(
        command.get_program(),
        command.get_current_dir(),
        options.path.as_deref(),
    )
    .is_some_and(|image| is_ape_file(&image))
}

/// Spawn a caller-built command under the fork lock, retrying while the
/// program is transiently busy (`ETXTBSY`) and once if the host refused it as
/// an APE image.
pub fn spawn_std<T>(
    command: &mut std::process::Command,
    mut spawn: impl FnMut(&mut std::process::Command) -> io::Result<T>,
) -> io::Result<T> {
    let first = retry_while_busy(|| {
        let _fork = fork_guard();
        spawn(command)
    });
    match first {
        Err(error) if prepare_std_retry(command, &error) => {
            let _fork = fork_guard();
            spawn(command)
        }
        result => result,
    }
}

/// Spawn a Tokio command under the fork lock, retrying once if the host
/// refused it as an APE image.
#[cfg(feature = "async-process")]
pub fn spawn_tokio<T>(
    command: &mut tokio::process::Command,
    mut spawn: impl FnMut(&mut tokio::process::Command) -> io::Result<T>,
) -> io::Result<T> {
    let first = retry_while_busy(|| {
        let _fork = fork_guard();
        spawn(command)
    });
    match first {
        Err(error) if prepare_tokio_retry(command, &error) => {
            let _fork = fork_guard();
            spawn(command)
        }
        result => result,
    }
}

#[cfg(test)]
#[path = "ape_tests.rs"]
mod tests;
