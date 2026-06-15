//! Transparent Win10 ConPTY sidecar self-acquisition (#445).
//!
//! On Windows 10 the system `kernel32!CreatePseudoConsole` silently
//! ignores `PSEUDOCONSOLE_PASSTHROUGH_MODE`. #443 added a
//! "sidecar `conpty.dll` next to the host exe" lookup, but that put
//! the bundling burden on every consumer. This module makes the
//! library self-acquiring instead: on first ConPTY use, the
//! Microsoft redistributable is fetched from a pinned GitHub
//! release asset, decompressed, and cached under the platform
//! cache directory. Subsequent runs find it in the cache and skip
//! the network entirely.
//!
//! Trust root: HTTPS to github.com plus GitHub's content-locked
//! release assets. No SHA pin in this revision — release assets
//! cannot be mutated after upload and an attacker who can replace
//! the asset can also replace the crate. We rely on HTTPS + the
//! maintainer's account integrity, the same trust root the crate's
//! own publication depends on.
//!
//! Failure mode: any error path (no network, cache write failure,
//! decompression error, asset missing for this `CARGO_PKG_VERSION`)
//! returns `io::Error`. Caller (`conpty_api::get`) logs and falls
//! back to `kernel32`. No crash. The error path is also exercised
//! by `RUNNING_PROCESS_CONPTY_OFFLINE=1` for air-gapped hosts.

#![cfg(windows)]

use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

/// Asset arch directory string. Compile-time constant per target.
pub(super) const fn arch_dir() -> &'static str {
    #[cfg(target_arch = "x86_64")]
    {
        "x64"
    }
    #[cfg(target_arch = "aarch64")]
    {
        "arm64"
    }
    #[cfg(target_arch = "x86")]
    {
        "x86"
    }
    #[cfg(target_arch = "arm")]
    {
        "arm"
    }
    #[cfg(not(any(
        target_arch = "x86_64",
        target_arch = "aarch64",
        target_arch = "x86",
        target_arch = "arm"
    )))]
    {
        compile_error!("Windows builds must target x86_64, aarch64, x86, or arm");
    }
}

static CACHED_SIDECAR_DIR: OnceLock<io::Result<PathBuf>> = OnceLock::new();

/// Returns the path to a directory containing a verified `conpty.dll`
/// (and `OpenConsole.exe` alongside it). Resolves via:
///
/// 1. Cache hit at `<cache_root>/<rp-version>/<arch>/`.
/// 2. HTTP fetch of the GitHub release asset for this
///    `CARGO_PKG_VERSION`, decompressed and atomically renamed into
///    the cache directory.
/// 3. `Err(io::Error)` on any failure — caller falls back to
///    `kernel32`.
///
/// The first-call result (success or failure) is cached for the
/// process lifetime via `OnceLock`. Repeated calls do not re-attempt
/// the fetch — preventing a hostile network from generating one
/// request per ConPTY open.
pub(super) fn ensure_cached_sidecar() -> io::Result<PathBuf> {
    let cached = CACHED_SIDECAR_DIR.get_or_init(|| {
        let cache_root = resolve_cache_root(std::env::var_os("RUNNING_PROCESS_CONPTY_CACHE"))?;
        let dir = cache_root
            .join("running-process")
            .join("conpty")
            .join(env!("CARGO_PKG_VERSION"))
            .join(arch_dir());
        ensure_in_dir(&dir)?;
        Ok(dir)
    });
    match cached {
        Ok(p) => Ok(p.clone()),
        Err(e) => Err(io::Error::new(e.kind(), e.to_string())),
    }
}

/// Resolve the cache root, honoring the env-var override.
/// Returns `dirs::cache_dir()` by default.
fn resolve_cache_root(override_dir: Option<std::ffi::OsString>) -> io::Result<PathBuf> {
    if let Some(p) = override_dir {
        if !p.is_empty() {
            return Ok(PathBuf::from(p));
        }
    }
    dirs::cache_dir().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "no platform cache directory available",
        )
    })
}

/// Stage the sidecar binaries into `cache_dir` (no-op if already
/// present). Public helper for unit tests; production code reaches it
/// through `ensure_cached_sidecar`.
pub(super) fn ensure_in_dir(cache_dir: &Path) -> io::Result<()> {
    let dll = cache_dir.join("conpty.dll");
    let exe = cache_dir.join("OpenConsole.exe");
    if dll.is_file() && exe.is_file() {
        diag(|| format!("ConPTY sidecar cache hit at {}", cache_dir.display()));
        return Ok(());
    }

    if std::env::var_os("RUNNING_PROCESS_CONPTY_OFFLINE").is_some() {
        diag(|| "ConPTY sidecar fetch suppressed (RUNNING_PROCESS_CONPTY_OFFLINE)".to_string());
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "offline mode: no cached sidecar and fetch disabled",
        ));
    }

    fetch_and_extract(cache_dir)
}

fn fetch_and_extract(cache_dir: &Path) -> io::Result<()> {
    let url = asset_url();
    diag(|| {
        format!(
            "ConPTY sidecar cache miss; fetching {} → {}",
            url,
            cache_dir.display()
        )
    });

    let bytes = http_get(&url)
        .map_err(|e| io::Error::other(format!("conpty sidecar fetch from {url} failed: {e}")))?;

    let parent = cache_dir
        .parent()
        .ok_or_else(|| io::Error::other("cache dir has no parent"))?;
    fs::create_dir_all(parent)?;

    let tmp_dir = parent.join(format!(
        ".tmp-conpty-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let _ = fs::remove_dir_all(&tmp_dir);
    fs::create_dir_all(&tmp_dir)?;

    extract_tar_zst(&bytes, &tmp_dir).map_err(|e| {
        let _ = fs::remove_dir_all(&tmp_dir);
        io::Error::other(format!("conpty sidecar archive extraction failed: {e}"))
    })?;

    if !tmp_dir.join("conpty.dll").is_file() || !tmp_dir.join("OpenConsole.exe").is_file() {
        let _ = fs::remove_dir_all(&tmp_dir);
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "conpty sidecar archive missing conpty.dll or OpenConsole.exe",
        ));
    }

    match fs::rename(&tmp_dir, cache_dir) {
        Ok(()) => Ok(()),
        Err(rename_err) => {
            // Another process may have won the race and the final
            // dir now exists. Accept that outcome silently.
            if cache_dir.join("conpty.dll").is_file() && cache_dir.join("OpenConsole.exe").is_file()
            {
                let _ = fs::remove_dir_all(&tmp_dir);
                Ok(())
            } else {
                let _ = fs::remove_dir_all(&tmp_dir);
                Err(rename_err)
            }
        }
    }
}

fn asset_url() -> String {
    format!(
        "https://github.com/zackees/running-process/releases/download/v{ver}/conpty-sidecar-{arch}.tar.zst",
        ver = env!("CARGO_PKG_VERSION"),
        arch = arch_dir(),
    )
}

fn http_get(url: &str) -> Result<Vec<u8>, String> {
    let resp = ureq::get(url)
        .timeout(std::time::Duration::from_secs(30))
        .call()
        .map_err(|e| e.to_string())?;
    let mut out = Vec::with_capacity(8 * 1024 * 1024);
    resp.into_reader()
        .take(64 * 1024 * 1024) // hard cap: 64 MB ceiling, ~10x typical asset
        .read_to_end(&mut out)
        .map_err(|e| e.to_string())?;
    Ok(out)
}

fn extract_tar_zst(bytes: &[u8], dest: &Path) -> Result<(), String> {
    let decoder = zstd::Decoder::new(io::Cursor::new(bytes)).map_err(|e| e.to_string())?;
    let mut archive = tar::Archive::new(decoder);
    archive.set_overwrite(true);
    archive.set_preserve_permissions(false);
    for entry in archive.entries().map_err(|e| e.to_string())? {
        let mut entry = entry.map_err(|e| e.to_string())?;
        let path = entry.path().map_err(|e| e.to_string())?.into_owned();
        // Defend against path traversal in the tarball.
        if path.components().any(|c| {
            matches!(
                c,
                std::path::Component::ParentDir | std::path::Component::RootDir
            )
        }) {
            return Err(format!(
                "rejecting unsafe path in archive: {}",
                path.display()
            ));
        }
        let out_path = dest.join(&path);
        if let Some(parent) = out_path.parent() {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        entry.unpack(&out_path).map_err(|e| e.to_string())?;
    }
    Ok(())
}

fn diag(f: impl FnOnce() -> String) {
    if std::env::var_os("RUNNING_PROCESS_CONPTY_DIAGNOSTICS").is_some() {
        eprintln!("running-process: {}", f());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn stage_fake_sidecar(dir: &Path) {
        fs::create_dir_all(dir).expect("mkdir");
        for name in ["conpty.dll", "OpenConsole.exe"] {
            let mut f = fs::File::create(dir.join(name)).expect("create");
            f.write_all(b"fake content for test").expect("write");
        }
    }

    /// `ensure_in_dir` returns Ok without touching the network when
    /// the cache dir is already populated.
    #[test]
    fn cache_hit_returns_ok_without_network() {
        let tmp = tempfile::tempdir().unwrap();
        stage_fake_sidecar(tmp.path());
        ensure_in_dir(tmp.path()).expect("cache hit must succeed");
        // Files unchanged.
        assert!(tmp.path().join("conpty.dll").is_file());
        assert!(tmp.path().join("OpenConsole.exe").is_file());
    }

    /// In offline mode with no cache, `ensure_in_dir` returns an
    /// io::Error of kind NotFound — the caller (conpty_api) will use
    /// the kernel32 fallback path.
    #[test]
    fn offline_mode_returns_not_found_without_network() {
        let tmp = tempfile::tempdir().unwrap();
        // SAFETY: env var mutation is process-global, but this test
        // sets+removes synchronously and we don't depend on parallel
        // ordering with other tests for this env var.
        std::env::set_var("RUNNING_PROCESS_CONPTY_OFFLINE", "1");
        let result = ensure_in_dir(tmp.path());
        std::env::remove_var("RUNNING_PROCESS_CONPTY_OFFLINE");
        let err = result.expect_err("offline + empty cache must error");
        assert_eq!(err.kind(), io::ErrorKind::NotFound, "got {err}");
    }

    /// `resolve_cache_root` honors the env-var override when non-empty.
    #[test]
    fn cache_root_override_used_when_present() {
        let tmp = tempfile::tempdir().unwrap();
        let resolved = resolve_cache_root(Some(tmp.path().as_os_str().to_owned())).unwrap();
        assert_eq!(resolved, tmp.path());
    }

    /// Empty override falls through to the platform default.
    #[test]
    fn empty_override_falls_through_to_platform() {
        let resolved = resolve_cache_root(Some(std::ffi::OsString::new())).unwrap();
        assert_eq!(resolved, dirs::cache_dir().expect("platform cache dir"));
    }

    /// `arch_dir` is a compile-time constant matching the host arch.
    #[test]
    fn arch_dir_matches_target() {
        let s = arch_dir();
        #[cfg(target_arch = "x86_64")]
        assert_eq!(s, "x64");
        #[cfg(target_arch = "aarch64")]
        assert_eq!(s, "arm64");
        #[cfg(target_arch = "x86")]
        assert_eq!(s, "x86");
        #[cfg(target_arch = "arm")]
        assert_eq!(s, "arm");
    }

    /// `asset_url` includes the running-process version and arch.
    #[test]
    fn asset_url_contains_version_and_arch() {
        let url = asset_url();
        assert!(url.contains(env!("CARGO_PKG_VERSION")), "got {url}");
        assert!(url.contains(arch_dir()), "got {url}");
        assert!(url.starts_with("https://github.com/zackees/running-process/releases/download/"));
        assert!(url.ends_with(".tar.zst"));
    }
}
