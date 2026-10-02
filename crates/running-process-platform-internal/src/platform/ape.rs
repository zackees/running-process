//! Actually Portable Executable (APE) launch support.
//!
//! A Cosmopolitan APE binary is a polyglot: a Windows PE image whose first
//! bytes (`MZqFpD='`) are also a POSIX shell prologue. A host that has no
//! `binfmt_misc` registration for it -- stock NixOS, most containers, macOS --
//! refuses `execve` with `ENOEXEC`. Interactive shells recover by re-running
//! the file through `/bin/sh`, which lets the prologue find or extract the
//! APE loader; a direct `posix_spawn` does not, so the same program that runs
//! from a terminal fails with "Exec format error" from Rust or Python.
//!
//! The spawn paths in this crate recover the same way a shell does, and only
//! after the kernel has already refused the image, so a native executable
//! never pays for the check:
//!
//! - A spawn whose environment is known ([`SpawnSpec`](crate::SpawnSpec))
//!   re-launches the image through a loader chosen by [`plan_launch`]: a
//!   system `ape`, else the loader embedded in the image itself, else the
//!   host shell. Running the loader directly needs no `PATH`, `dd` or `gzip`,
//!   so it works under a cleared environment.
//! - A caller-built command keeps every setting the caller gave it.
//!   [`prepare_std_retry`] first materializes the embedded loader where the
//!   image's own prologue looks for it, then routes the retry through
//!   `execvp`, whose POSIX `ENOEXEC` rule hands the file to the shell.
//!
//! Anything that is not an APE image keeps its original error.

use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

pub use crate::{
    ape_is_exec_format_error as is_exec_format_error, APE_EMBEDDED_LOADER as EMBEDDED_LOADER,
    APE_EXECVP_SHELL_FALLBACK as EXECVP_SHELL_FALLBACK, APE_NEEDS_LOADER as NEEDS_LOADER,
    APE_SHELL as SHELL, APE_SYSTEM_LOADERS as SYSTEM_LOADERS,
};

/// Leading bytes of every APE image: release, `jartsr` and debug spellings.
pub const MAGICS: [&[u8]; 3] = [b"MZqFpD='", b"jartsr='", b"APEDBG='"];

/// How much of the image the shell prologue is searched in. Cosmopolitan's
/// prologue is about 4 KiB; the limit only bounds a malformed file.
#[cfg(feature = "ape-loader")]
const PROLOGUE_LIMIT: u64 = 16 * 1024;

/// Upper bound on a compressed embedded loader. The real one is about 4 KiB.
#[cfg(feature = "ape-loader")]
const COMPRESSED_LOADER_LIMIT: u64 = 1 << 20;

/// Upper bound on a decompressed embedded loader. The real one is about 9 KiB.
#[cfg(feature = "ape-loader")]
const LOADER_LIMIT: usize = 4 << 20;

/// Whether `header` begins with an APE magic.
pub fn is_ape_header(header: &[u8]) -> bool {
    MAGICS.iter().any(|magic| header.starts_with(magic))
}

/// Whether the file at `path` is an APE image.
///
/// Unreadable and missing files are not APE images: the caller's original
/// spawn error is the more useful report for both.
pub fn is_ape_file(path: &Path) -> bool {
    let mut header = [0u8; 8];
    File::open(path)
        .and_then(|mut file| file.read_exact(&mut header))
        .is_ok_and(|()| is_ape_header(&header))
}

#[cfg(feature = "ape-loader")]
fn read_prologue(path: &Path) -> io::Result<Vec<u8>> {
    let mut prologue = Vec::new();
    File::open(path)?
        .take(PROLOGUE_LIMIT)
        .read_to_end(&mut prologue)?;
    Ok(prologue)
}

/// Where an image's Linux prologue branch keeps its gzip-compressed loader.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EmbeddedLoader {
    /// Byte offset of the gzip member within the image.
    pub offset: u64,
    /// Length of the gzip member in bytes.
    pub len: u64,
}

/// Find the loader the prologue would extract for `machine` (`uname -m`).
///
/// The prologue's last `if [ ! -d /Applications ]` block is the Linux and
/// BSD branch; within it, each machine's block extracts the loader with
/// `dd if="$o" skip=OFFSET count=LEN bs=1 | gzip -dc`. The earlier blocks
/// patch the image for `--assimilate` or build the loader on macOS and are
/// not a loader this host can run as extracted.
pub fn embedded_loader(prologue: &[u8], machine: &str) -> Option<EmbeddedLoader> {
    let text = String::from_utf8_lossy(prologue);
    let branch = &text[text.rfind("if [ ! -d /Applications ]; then")?..];
    let block = &branch[branch.find(&format!("if [ \"$m\" = {machine} ]"))?..];
    let block = &block[..block.find("\nfi\n").unwrap_or(block.len())];
    let extract = &block[block.find("dd if=\"$o\" skip=")? + "dd if=\"$o\" ".len()..];
    let field = |name: &str| -> Option<u64> {
        let value = &extract[extract.find(name)? + name.len()..];
        let end = value
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(value.len());
        value[..end].parse().ok()
    };
    let offset = field("skip=")?;
    let len = field("count=")?;
    (len > 0).then_some(EmbeddedLoader { offset, len })
}

/// File name the prologue caches its extracted loader under, e.g. `.ape-1.10`.
pub fn loader_cache_name(prologue: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(prologue);
    let marker = "t=\"${TMPDIR:-${HOME:-.}}/";
    let name = &text[text.find(marker)? + marker.len()..];
    let name = &name[..name.find('"')?];
    let valid = name.starts_with(".ape-")
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'.' || byte == b'-');
    valid.then(|| name.to_owned())
}

/// Machine name in the spelling the prologue's `uname -m` tests use.
#[cfg(feature = "ape-loader")]
fn host_machine() -> &'static str {
    std::env::consts::ARCH
}

/// Decompress the loader embedded in the image at `path` for `machine`.
///
/// `Ok(None)` means the image carries no loader for that machine.
#[cfg(feature = "ape-loader")]
pub fn extract_embedded_loader(path: &Path, machine: &str) -> io::Result<Option<Vec<u8>>> {
    use std::io::{Seek, SeekFrom};

    let Some(location) = embedded_loader(&read_prologue(path)?, machine) else {
        return Ok(None);
    };
    if location.len > COMPRESSED_LOADER_LIMIT {
        return Err(invalid("embedded APE loader is implausibly large"));
    }
    let mut file = File::open(path)?;
    file.seek(SeekFrom::Start(location.offset))?;
    let mut compressed = Vec::new();
    file.take(location.len).read_to_end(&mut compressed)?;
    if compressed.len() as u64 != location.len {
        return Err(invalid("embedded APE loader is truncated"));
    }
    gunzip(&compressed, LOADER_LIMIT).map(Some)
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

/// The variables an APE launch consults, as the child will see them.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ChildEnvironment {
    pub path: Option<OsString>,
    pub tmpdir: Option<OsString>,
    pub home: Option<OsString>,
}

impl ChildEnvironment {
    /// The values a child inherits from this process.
    pub fn inherited() -> Self {
        Self {
            path: crate::env_vars::PATH.os(),
            tmpdir: crate::env_vars::TMPDIR.os(),
            home: crate::env_vars::HOME.os(),
        }
    }

    /// Apply a command's environment edits: `None` removes a variable.
    ///
    /// `clear` starts from an empty environment, as `env_clear` does.
    pub fn with_overrides<'a>(
        clear: bool,
        overrides: impl IntoIterator<Item = (&'a OsStr, Option<&'a OsStr>)>,
    ) -> Self {
        let mut environment = if clear {
            Self::default()
        } else {
            Self::inherited()
        };
        for (key, value) in overrides {
            let slot = match key.to_str() {
                Some("PATH") => &mut environment.path,
                Some("TMPDIR") => &mut environment.tmpdir,
                Some("HOME") => &mut environment.home,
                _ => continue,
            };
            *slot = value.map(OsStr::to_os_string);
        }
        environment
    }

    /// Directory the prologue caches its loader in: `${TMPDIR:-${HOME}}`.
    ///
    /// The prologue falls back to the working directory after both; that is
    /// not somewhere this crate writes on a child's behalf, so it is `None`.
    fn prologue_cache_dir(&self) -> Option<PathBuf> {
        [&self.tmpdir, &self.home]
            .into_iter()
            .flatten()
            .find(|value| !value.is_empty())
            .map(PathBuf::from)
    }
}

/// Resolve `program` the way the child's `execvp` will.
///
/// A name with a slash is a path, relative to `current_dir` when one is set.
/// A bare name is searched in the child's `PATH`; the first APE image found
/// is the one the kernel refused, since earlier native hits would have run.
pub fn resolve_program(
    program: &OsStr,
    current_dir: Option<&Path>,
    environment: &ChildEnvironment,
) -> Option<PathBuf> {
    let in_dir = |path: PathBuf| match current_dir {
        Some(dir) if path.is_relative() => dir.join(path),
        _ => path,
    };
    if program.as_encoded_bytes().contains(&b'/') {
        return Some(in_dir(PathBuf::from(program)));
    }
    std::env::split_paths(environment.path.as_deref()?)
        .map(|dir| {
            let dir = if dir.as_os_str().is_empty() {
                PathBuf::from(".")
            } else {
                dir
            };
            in_dir(dir.join(program))
        })
        .find(|candidate| is_ape_file(candidate))
}

/// Which loader a planned launch runs the image through.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LoaderKind {
    /// An `ape` loader installed on the host.
    System,
    /// The loader embedded in the image, extracted beside the prologue's cache.
    Embedded,
    /// The host shell, which runs the image's own prologue.
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
}

impl ApeLaunch {
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
/// `None` means `program` is not an APE image, the host runs APE images
/// natively, or no loader is available; in each case the original spawn
/// error stands.
pub fn plan_launch(
    program: &OsStr,
    current_dir: Option<&Path>,
    environment: &ChildEnvironment,
) -> Option<ApeLaunch> {
    if !NEEDS_LOADER {
        return None;
    }
    let image = resolve_program(program, current_dir, environment)?;
    if !is_ape_file(&image) {
        return None;
    }
    let image = std::path::absolute(&image).ok()?;
    let launch = |kind, loader| ApeLaunch {
        kind,
        loader,
        image: image.clone(),
    };
    if let Some(loader) = system_loader(environment) {
        return Some(launch(LoaderKind::System, loader));
    }
    if let Some(loader) = materialize_embedded_loader(&image, environment, true) {
        return Some(launch(LoaderKind::Embedded, loader));
    }
    let shell = Path::new(SHELL);
    shell
        .is_file()
        .then(|| launch(LoaderKind::Shell, shell.to_path_buf()))
}

/// An installed `ape` loader: the child's `PATH` first, as the prologue's
/// `type ape` checks, then the host's conventional install locations.
fn system_loader(environment: &ChildEnvironment) -> Option<PathBuf> {
    let on_path = environment
        .path
        .as_deref()
        .into_iter()
        .flat_map(std::env::split_paths)
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join("ape"));
    on_path
        .chain(SYSTEM_LOADERS.iter().map(PathBuf::from))
        .find(|candidate| candidate.is_file())
}

/// Write the image's embedded loader where its prologue caches it.
///
/// The prologue looks for `${TMPDIR:-${HOME}}/.ape-<version>` before
/// extracting with `dd` and `gzip`, so a loader placed there serves both a
/// direct launch and the shell prologue. With `temp_fallback`, a child with
/// neither variable gets the host temporary directory instead; only a direct
/// launch can use that, since the prologue would not look there.
///
/// An existing file is reused only when its bytes are the loader's own.
pub fn materialize_embedded_loader(
    image: &Path,
    environment: &ChildEnvironment,
    temp_fallback: bool,
) -> Option<PathBuf> {
    if !EMBEDDED_LOADER {
        return None;
    }
    let dir = environment
        .prologue_cache_dir()
        .or_else(|| temp_fallback.then(std::env::temp_dir))?;
    materialize_into(image, &dir)
}

#[cfg(feature = "ape-loader")]
fn materialize_into(image: &Path, dir: &Path) -> Option<PathBuf> {
    let name = loader_cache_name(&read_prologue(image).ok()?)?;
    let loader = extract_embedded_loader(image, host_machine()).ok()??;
    if !loader.starts_with(b"\x7fELF") {
        return None;
    }
    let target = dir.join(name);
    if std::fs::read(&target).is_ok_and(|existing| existing == loader) {
        return Some(target);
    }
    std::fs::create_dir_all(dir).ok()?;
    let staging = dir.join(format!(
        "{}.{}.tmp",
        target.file_name()?.to_string_lossy(),
        std::process::id()
    ));
    let written = std::fs::write(&staging, &loader)
        .and_then(|()| crate::ape_mark_executable(&staging))
        .and_then(|()| std::fs::rename(&staging, &target));
    if written.is_err() {
        let _ = std::fs::remove_file(&staging);
        return None;
    }
    Some(target)
}

#[cfg(not(feature = "ape-loader"))]
fn materialize_into(_image: &Path, _dir: &Path) -> Option<PathBuf> {
    None
}

/// Prepare a caller-built command to be spawned again as an APE image.
///
/// Returns `true` when `error` is the kernel refusing an APE image and the
/// command now routes through `execvp`, whose `ENOEXEC` rule runs the
/// image's prologue with the host shell; spawn it once more. Every setting
/// the caller made is kept. The child's environment is read from the
/// command's own edits over this process's; a command that cleared its
/// environment can still run, but its prologue may then have to extract the
/// loader itself.
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
    let environment = ChildEnvironment::with_overrides(false, command.get_envs());
    let Some(image) = resolve_program(
        command.get_program(),
        command.get_current_dir(),
        &environment,
    ) else {
        return false;
    };
    if !is_ape_file(&image) {
        return false;
    }
    // Best effort: without it the prologue extracts the loader itself.
    if system_loader(&environment).is_none() {
        if let Ok(image) = std::path::absolute(&image) {
            let _ = materialize_embedded_loader(&image, &environment, false);
        }
    }
    true
}

/// Spawn a caller-built command, retrying once if it is a refused APE image.
pub fn spawn_std<T>(
    command: &mut std::process::Command,
    mut spawn: impl FnMut(&mut std::process::Command) -> io::Result<T>,
) -> io::Result<T> {
    match spawn(command) {
        Err(error) if prepare_std_retry(command, &error) => spawn(command),
        result => result,
    }
}

/// Spawn a Tokio command, retrying once if it is a refused APE image.
#[cfg(feature = "async-process")]
pub fn spawn_tokio<T>(
    command: &mut tokio::process::Command,
    mut spawn: impl FnMut(&mut tokio::process::Command) -> io::Result<T>,
) -> io::Result<T> {
    match spawn(command) {
        Err(error) if prepare_tokio_retry(command, &error) => spawn(command),
        result => result,
    }
}

#[cfg(test)]
#[path = "ape_tests.rs"]
mod tests;
