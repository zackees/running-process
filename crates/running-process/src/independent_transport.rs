//! Owner-private, byte-preserving request artifacts for independent launchers.
//!
//! A scheduler or broker receives only this opaque path/token on its command
//! line. Target argv and environment are never rendered into manager argv.

use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
const MAX_REQUEST_BYTES: usize = 16 * 1024 * 1024;

#[cfg(unix)]
use std::os::unix::ffi::{OsStrExt, OsStringExt};
#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, MetadataExt};

#[cfg(unix)]
const MAGIC: &[u8] = b"RPINDEP1\0";
#[cfg(windows)]
const MAGIC: &[u8] = b"RPINDEW1\0";

/// Serialized launch request kept in an owner-private file until the helper
/// has acknowledged consumption. Strings retain Unix bytes or Windows UTF-16
/// code units (including isolated surrogates), never lossy display text.
#[derive(Eq, PartialEq)]
pub(crate) struct LaunchRequest {
    pub program: Vec<u8>,
    pub args: Vec<Vec<u8>>,
    pub cwd: Option<Vec<u8>>,
    pub environment: Vec<(Vec<u8>, Option<Vec<u8>>)>,
}

impl std::fmt::Debug for LaunchRequest {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LaunchRequest")
            .field("argument_count", &self.args.len())
            .field("environment_count", &self.environment.len())
            .finish_non_exhaustive()
    }
}

impl LaunchRequest {
    /// Reject unrepresentable native strings before creating artifacts or
    /// submitting a scheduler task. Diagnostics never include request values.
    fn validate(&self) -> io::Result<()> {
        self.encoded_len()?;
        if self.program.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "empty independent program",
            ));
        }
        validate_launch_string(&self.program)?;
        for argument in &self.args {
            validate_launch_string(argument)?;
        }
        if let Some(cwd) = &self.cwd {
            validate_launch_string(cwd)?;
        }
        for (key, value) in &self.environment {
            validate_launch_string(key)?;
            if key.is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "empty independent environment key",
                ));
            }
            #[cfg(unix)]
            if key.contains(&b'=') {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "invalid independent environment key",
                ));
            }
            if let Some(value) = value {
                validate_launch_string(value)?;
            }
        }
        Ok(())
    }

    fn encoded_len(&self) -> io::Result<usize> {
        let oversized = || {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "independent request exceeds 16 MiB",
            )
        };
        let mut length = MAGIC.len() + 4 + 4 + 1 + 4;
        let mut add = |bytes: usize| -> io::Result<()> {
            length = length
                .checked_add(bytes)
                .filter(|length| *length <= MAX_REQUEST_BYTES)
                .ok_or_else(oversized)?;
            Ok(())
        };
        add(self.program.len())?;
        for arg in &self.args {
            add(4)?;
            add(arg.len())?;
        }
        if let Some(cwd) = &self.cwd {
            add(4)?;
            add(cwd.len())?;
        }
        for (key, value) in &self.environment {
            add(5)?;
            add(key.len())?;
            if let Some(value) = value {
                add(4)?;
                add(value.len())?;
            }
        }
        Ok(length)
    }

    #[cfg(unix)]
    pub(crate) fn from_command(
        command: &std::process::Command,
        inherit_environment: bool,
    ) -> io::Result<Self> {
        let mut environment: std::collections::BTreeMap<OsString, OsString> = if inherit_environment
        {
            std::env::vars_os().collect()
        } else {
            std::collections::BTreeMap::new()
        };
        for (key, value) in command.get_envs() {
            match value {
                Some(value) => {
                    environment.insert(key.to_owned(), value.to_owned());
                }
                None => {
                    environment.remove(key);
                }
            }
        }
        let caller_directory = std::env::current_dir()?;
        let cwd = match command.get_current_dir() {
            Some(path) if path.is_absolute() => path.to_path_buf(),
            Some(path) => caller_directory.join(path),
            None => caller_directory,
        };
        let request = Self {
            program: command.get_program().as_bytes().to_vec(),
            args: command
                .get_args()
                .map(|arg| arg.as_bytes().to_vec())
                .collect(),
            cwd: Some(cwd.as_os_str().as_bytes().to_vec()),
            environment: environment
                .into_iter()
                .map(|(key, value)| (key.as_bytes().to_vec(), Some(value.as_bytes().to_vec())))
                .collect(),
        };
        request.validate()?;
        Ok(request)
    }
}

#[cfg(windows)]
impl LaunchRequest {
    pub(crate) fn from_command(
        command: &std::process::Command,
        inherit_environment: bool,
    ) -> io::Result<Self> {
        // Keep overrides after the snapshot. The Windows command builder
        // applies case-insensitive key replacement/removal; a Rust BTreeMap
        // keyed by OsString would incorrectly distinguish PATH from Path.
        let mut environment = Vec::new();
        if inherit_environment {
            environment.extend(std::env::vars_os().map(|(key, value)| {
                (
                    windows_string_bytes(&key),
                    Some(windows_string_bytes(&value)),
                )
            }));
        }
        environment.extend(
            command
                .get_envs()
                .map(|(key, value)| (windows_string_bytes(key), value.map(windows_string_bytes))),
        );
        let caller_directory = std::env::current_dir()?;
        let cwd = match command.get_current_dir() {
            Some(path) if path.is_absolute() => path.to_path_buf(),
            Some(path) => caller_directory.join(path),
            None => caller_directory,
        };
        let request = Self {
            program: windows_string_bytes(command.get_program()),
            args: command.get_args().map(windows_string_bytes).collect(),
            cwd: Some(windows_string_bytes(cwd.as_os_str())),
            environment,
        };
        request.validate()?;
        Ok(request)
    }
}

#[cfg(unix)]
fn validate_launch_string(value: &[u8]) -> io::Result<()> {
    if value.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "NUL in independent launch string",
        ));
    }
    Ok(())
}

#[cfg(windows)]
fn validate_launch_string(value: &[u8]) -> io::Result<()> {
    if value.len() % 2 != 0 || value.chunks_exact(2).any(|unit| unit == [0, 0]) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid independent Windows launch string",
        ));
    }
    // Unpaired surrogates remain valid OsString code units and are preserved.
    Ok(())
}

#[cfg(windows)]
fn windows_string_bytes(value: &OsStr) -> Vec<u8> {
    use std::os::windows::ffi::OsStrExt;
    value.encode_wide().flat_map(u16::to_le_bytes).collect()
}

#[cfg(all(test, windows))]
mod windows_codec_tests {
    use super::*;
    use std::os::windows::ffi::OsStringExt;

    #[test]
    fn isolated_surrogates_and_argument_boundaries_roundtrip() {
        let value = OsString::from_wide(&[0xd800, 0x20, 0x22, 0x5c]);
        assert_eq!(
            windows_string_from_bytes(&windows_string_bytes(&value)).unwrap(),
            value
        );
        let mut command = std::process::Command::new("helper.exe");
        command.arg(&value).arg("").env("RP_VALUE", &value);
        let request = LaunchRequest::from_command(&command, false).unwrap();
        assert_eq!(request.args.len(), 2);
        assert_eq!(windows_string_from_bytes(&request.args[0]).unwrap(), value);
        assert_eq!(LaunchRequest::decode(&request.encode()).unwrap(), request);
    }

    #[test]
    fn malformed_windows_strings_are_rejected() {
        assert!(windows_string_from_bytes(&[1]).is_err());
        assert!(windows_string_from_bytes(&[0, 0]).is_err());
        assert!(windows_string_from_bytes(&[]).is_ok());
        assert!(validate_launch_string(&[1]).is_err());
        assert!(validate_launch_string(&[0, 0]).is_err());
        assert!(validate_launch_string(&[0, 0xd8]).is_ok());
    }
}

#[cfg(windows)]
pub(crate) fn windows_string_from_bytes(value: &[u8]) -> io::Result<OsString> {
    use std::os::windows::ffi::OsStringExt;
    if value.len() % 2 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "odd Windows string byte length",
        ));
    }
    let units: Vec<u16> = value
        .chunks_exact(2)
        .map(|bytes| u16::from_le_bytes([bytes[0], bytes[1]]))
        .collect();
    if units.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "NUL in Windows launch string",
        ));
    }
    Ok(OsString::from_wide(&units))
}

impl LaunchRequest {
    pub(crate) fn encode(&self) -> Vec<u8> {
        let mut out = MAGIC.to_vec();
        write_bytes(&mut out, &self.program);
        write_many(&mut out, &self.args);
        match &self.cwd {
            Some(cwd) => {
                out.push(1);
                write_bytes(&mut out, cwd);
            }
            None => out.push(0),
        }
        out.extend_from_slice(&(self.environment.len() as u32).to_le_bytes());
        for (key, value) in &self.environment {
            write_bytes(&mut out, key);
            match value {
                Some(value) => {
                    out.push(1);
                    write_bytes(&mut out, value);
                }
                None => out.push(0),
            }
        }
        out
    }

    pub(crate) fn decode(input: &[u8]) -> io::Result<Self> {
        if input.len() > MAX_REQUEST_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "independent request exceeds 16 MiB",
            ));
        }
        if !input.starts_with(MAGIC) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid independent request magic",
            ));
        }
        let mut input = &input[MAGIC.len()..];
        let program = read_bytes(&mut input)?;
        let args = read_many(&mut input)?;
        let cwd = match take(&mut input)? {
            0 => None,
            1 => Some(read_bytes(&mut input)?),
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "invalid cwd flag",
                ))
            }
        };
        // Every entry needs at least a four-byte key length and a value flag.
        // Validate before reserving so tiny hostile requests cannot request a
        // multi-gigabyte allocation.
        let count = read_u32(&mut input)? as usize;
        if count > input.len() / 5 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "impossible environment count",
            ));
        }
        let mut environment = Vec::with_capacity(count);
        for _ in 0..count {
            let key = read_bytes(&mut input)?;
            let value = match take(&mut input)? {
                0 => None,
                1 => Some(read_bytes(&mut input)?),
                _ => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "invalid env flag",
                    ))
                }
            };
            environment.push((key, value));
        }
        if !input.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "trailing independent request bytes",
            ));
        }
        let request = Self {
            program,
            args,
            cwd,
            environment,
        };
        request.validate().map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid independent launch request",
            )
        })?;
        Ok(request)
    }
}
fn take(input: &mut &[u8]) -> io::Result<u8> {
    let Some((&value, rest)) = input.split_first() else {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "truncated independent request",
        ));
    };
    *input = rest;
    Ok(value)
}
fn read_u32(input: &mut &[u8]) -> io::Result<u32> {
    if input.len() < 4 {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "truncated independent request",
        ));
    }
    let (raw, rest) = input.split_at(4);
    *input = rest;
    Ok(u32::from_le_bytes(raw.try_into().unwrap()))
}
fn read_bytes(input: &mut &[u8]) -> io::Result<Vec<u8>> {
    let length = read_u32(input)? as usize;
    if length > MAX_REQUEST_BYTES || input.len() < length {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "truncated or oversized independent request",
        ));
    }
    let (raw, rest) = input.split_at(length);
    *input = rest;
    Ok(raw.to_vec())
}
fn read_many(input: &mut &[u8]) -> io::Result<Vec<Vec<u8>>> {
    let count = read_u32(input)? as usize;
    if count > input.len() / 4 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "impossible argument count",
        ));
    }
    (0..count).map(|_| read_bytes(input)).collect()
}

#[cfg(unix)]
pub(crate) fn spawn_request(request: LaunchRequest) -> io::Result<std::process::Child> {
    let mut command = std::process::Command::new(OsString::from_vec(request.program));
    command.env_clear();
    command.args(request.args.into_iter().map(OsString::from_vec));
    if let Some(cwd) = request.cwd {
        command.current_dir(OsString::from_vec(cwd));
    }
    for (key, value) in request.environment {
        match value {
            Some(value) => {
                command.env(OsString::from_vec(key), OsString::from_vec(value));
            }
            None => {
                command.env_remove(OsString::from_vec(key));
            }
        }
    }
    running_process_platform_internal::spawn_independent_helper_child(&mut command)
}

#[cfg(windows)]
pub(crate) fn spawn_request(request: LaunchRequest) -> io::Result<std::process::Child> {
    let mut command = std::process::Command::new(windows_string_from_bytes(&request.program)?);
    command.env_clear();
    for argument in request.args {
        command.arg(windows_string_from_bytes(&argument)?);
    }
    if let Some(cwd) = request.cwd {
        command.current_dir(windows_string_from_bytes(&cwd)?);
    }
    for (key, value) in request.environment {
        let key = windows_string_from_bytes(&key)?;
        match value {
            Some(value) => {
                command.env(key, windows_string_from_bytes(&value)?);
            }
            None => {
                command.env_remove(key);
            }
        }
    }
    running_process_platform_internal::spawn_independent_helper_child(&mut command)
}
fn write_many(out: &mut Vec<u8>, values: &[Vec<u8>]) {
    out.extend_from_slice(&(values.len() as u32).to_le_bytes());
    for value in values {
        write_bytes(out, value);
    }
}
fn write_bytes(out: &mut Vec<u8>, value: &[u8]) {
    out.extend_from_slice(&(value.len() as u32).to_le_bytes());
    out.extend_from_slice(value);
}

/// Open only an owner-private regular request and cap reads even if the file
/// grows after metadata inspection. Never follow a request-file symlink.
#[cfg(windows)]
pub(crate) fn read_private_request(path: &Path) -> io::Result<LaunchRequest> {
    use std::io::Read as _;
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "request has no parent"))?;
    let _directory = running_process_platform_internal::open_private_launch_directory(parent)?;
    let file = running_process_platform_internal::open_private_launch_file(path)?;
    if file.metadata()?.len() > MAX_REQUEST_BYTES as u64 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "independent request exceeds 16 MiB",
        ));
    }
    let mut bytes = Vec::new();
    file.take(MAX_REQUEST_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    LaunchRequest::decode(&bytes)
}

#[cfg(unix)]
pub(crate) fn read_private_request(path: &Path) -> io::Result<LaunchRequest> {
    use std::io::Read as _;
    use std::os::unix::fs::OpenOptionsExt;

    let parent = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "request has no parent directory",
        )
    })?;
    let directory = std::fs::symlink_metadata(parent)?;
    let owner = unsafe { libc::geteuid() };
    if !directory.is_dir() || directory.uid() != owner || directory.mode() & 0o077 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "request directory is not owner-private",
        ));
    }
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.uid() != owner
        || metadata.mode() & 0o077 != 0
        || metadata.nlink() != 1
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "request is not an owner-private regular file",
        ));
    }
    if metadata.len() > MAX_REQUEST_BYTES as u64 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "independent request exceeds 16 MiB",
        ));
    }
    let mut bytes = Vec::new();
    file.take(MAX_REQUEST_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    LaunchRequest::decode(&bytes)
}

/// Create a 0700 request directory and an exclusive 0600 opaque request.
#[cfg(windows)]
pub(crate) fn write_private_request(request: &LaunchRequest) -> io::Result<PathBuf> {
    use running_process_platform_internal::{
        create_private_launch_directory, create_private_launch_file,
    };
    use std::io::Write as _;
    request.validate()?;
    let runtime = std::env::temp_dir().canonicalize()?;
    let root = runtime.join(format!(
        "running-process-independent-{}-{}",
        std::process::id(),
        nonce()
    ));
    create_private_launch_directory(&root)?;
    let directory = running_process_platform_internal::open_private_launch_directory(&root)?;
    let path = root.join("request");
    let mut file = match create_private_launch_file(&path) {
        Ok(file) => file,
        Err(error) => {
            // Do not delete a path we could not successfully adopt. An ACL
            // verification failure may leave a harmless empty artifact.
            drop(directory);
            let _ = std::fs::remove_dir(&root);
            return Err(error);
        }
    };
    let result = file
        .write_all(&request.encode())
        .and_then(|()| file.sync_all());
    // The writer is exclusive. Close it before either cleanup or helper read.
    drop(file);
    drop(directory);
    if let Err(error) = result {
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&root);
        return Err(error);
    }
    Ok(path)
}

#[cfg(unix)]
pub(crate) fn write_private_request(request: &LaunchRequest) -> io::Result<PathBuf> {
    request.validate()?;
    use std::fs::OpenOptions;
    use std::os::unix::fs::OpenOptionsExt;
    // Minimal containers often have no XDG user session. Atomic 0700 directory
    // creation supplies the same per-request privacy beneath the temp root.
    let runtime = crate::env_vars::XDG_RUNTIME_DIR
        .path()
        .unwrap_or_else(std::env::temp_dir)
        .canonicalize()?;
    // Never chmod or traverse a predictable shared directory. The unique
    // directory is atomically created 0700 and rejected unless it is a real,
    // owner-owned directory before the request file is opened.
    let root = runtime.join(format!(
        "running-process-independent-{}-{}",
        std::process::id(),
        nonce()
    ));
    std::fs::DirBuilder::new().mode(0o700).create(&root)?;
    let metadata = std::fs::symlink_metadata(&root)?;
    if !metadata.file_type().is_dir()
        || metadata.file_type().is_symlink()
        || metadata.uid() != unsafe { libc::geteuid() }
    {
        let _ = std::fs::remove_dir(&root);
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "independent request directory is not owner-private",
        ));
    }
    let path = root.join("request");
    use std::io::Write as _;
    let mut file = match OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&path)
    {
        Ok(file) => file,
        Err(error) => {
            let _ = std::fs::remove_dir(&root);
            return Err(error);
        }
    };
    if let Err(error) = file
        .write_all(&request.encode())
        .and_then(|_| file.sync_all())
    {
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&root);
        return Err(error);
    }
    Ok(path)
}
fn nonce() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|time| time.as_nanos())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn roundtrips_non_utf8_cwd_and_environment_removal() {
        let request = LaunchRequest {
            program: b"x".to_vec(),
            args: vec![vec![0xff]],
            cwd: Some(vec![0xfe]),
            environment: vec![(b"A".to_vec(), None)],
        };
        assert_eq!(LaunchRequest::decode(&request.encode()).unwrap(), request);
    }
    #[test]
    fn rejects_truncated_request() {
        assert!(LaunchRequest::decode(MAGIC).is_err());
    }
}

#[cfg(test)]
mod bounds_tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn malformed_native_fields_fail_before_artifact_creation_and_after_decode() {
        let make = || LaunchRequest {
            program: b"program".to_vec(),
            args: vec![],
            cwd: None,
            environment: vec![],
        };
        let mut cases = Vec::new();
        let mut request = make();
        request.program.clear();
        cases.push(request);
        let mut request = make();
        request.program.push(0);
        cases.push(request);
        let mut request = make();
        request.args.push(b"private\0argument".to_vec());
        cases.push(request);
        let mut request = make();
        request.cwd = Some(b"private\0directory".to_vec());
        cases.push(request);
        let mut request = make();
        request.environment.push((b"BAD=KEY".to_vec(), None));
        cases.push(request);
        let mut request = make();
        request.environment.push((vec![], None));
        cases.push(request);
        let mut request = make();
        request
            .environment
            .push((b"KEY".to_vec(), Some(b"private\0value".to_vec())));
        cases.push(request);
        for request in cases {
            let error = write_private_request(&request).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
            assert!(!error.to_string().contains("private"));
            assert_eq!(
                LaunchRequest::decode(&request.encode()).unwrap_err().kind(),
                io::ErrorKind::InvalidData
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn arbitrary_unix_bytes_and_empty_arguments_remain_supported() {
        let request = LaunchRequest {
            program: vec![b'p', 0xff],
            args: vec![vec![], vec![0xff]],
            cwd: None,
            environment: vec![(vec![b'K', 0xff], Some(vec![]))],
        };
        request.validate().unwrap();
        assert_eq!(LaunchRequest::decode(&request.encode()).unwrap(), request);
    }

    #[test]
    fn encoder_budget_and_debug_redaction() {
        let mut request = LaunchRequest {
            program: b"secret-program".to_vec(),
            args: vec![],
            cwd: None,
            environment: vec![(b"secret-key".to_vec(), Some(b"secret-value".to_vec()))],
        };
        assert_eq!(request.encoded_len().unwrap(), request.encode().len());
        assert!(!format!("{request:?}").contains("secret"));
        request.args.push(vec![0; MAX_REQUEST_BYTES]);
        assert!(request.encoded_len().is_err());
    }

    #[cfg(unix)]
    #[test]
    fn empty_environment_base_keeps_only_explicit_overrides() {
        let mut command = std::process::Command::new("unused");
        command
            .env("KEPT", "value")
            .env("REMOVED", "value")
            .env_remove("REMOVED");
        let request = LaunchRequest::from_command(&command, false).unwrap();
        assert_eq!(
            request.environment,
            vec![(b"KEPT".to_vec(), Some(b"value".to_vec()))]
        );
    }

    #[cfg(unix)]
    #[test]
    fn captures_caller_cwd_for_default_and_relative_directory() {
        let caller = std::env::current_dir().unwrap();
        let mut command = std::process::Command::new("unused");
        let request = LaunchRequest::from_command(&command, false).unwrap();
        assert_eq!(request.cwd, Some(caller.as_os_str().as_bytes().to_vec()));
        command.current_dir("relative-directory");
        let request = LaunchRequest::from_command(&command, false).unwrap();
        assert_eq!(
            request.cwd,
            Some(
                caller
                    .join("relative-directory")
                    .as_os_str()
                    .as_bytes()
                    .to_vec()
            )
        );
    }

    #[test]
    fn rejects_oversized_request_before_parsing() {
        let request = vec![0; MAX_REQUEST_BYTES + 1];
        assert_eq!(
            LaunchRequest::decode(&request).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn rejects_impossible_argument_count_before_allocating() {
        let mut request = MAGIC.to_vec();
        write_bytes(&mut request, b"program");
        request.extend_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(
            LaunchRequest::decode(&request).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn rejects_impossible_environment_count_before_allocating() {
        let mut request = MAGIC.to_vec();
        write_bytes(&mut request, b"program");
        write_many(&mut request, &[]);
        request.push(0);
        request.extend_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(
            LaunchRequest::decode(&request).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }
}
