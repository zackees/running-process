//! Terminal, PTY, console, input, and terminal-I/O primitives.

use std::ffi::OsString;
use std::io::{self, Read, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};

/// Caller-facing PTY dimensions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PtySize {
    pub rows: u16,
    pub cols: u16,
    pub pixel_width: u16,
    pub pixel_height: u16,
}

/// One chunk read from the host terminal input source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PtyInputChunk {
    pub data: Vec<u8>,
    pub submit: bool,
}

/// Independently lockable writer used by the PTY session policy layer.
pub type SharedPtyWriter = Arc<Mutex<Box<dyn Write + Send>>>;

/// Opaque host control data for a PTY master.
///
/// Shared callers can carry this token without observing a native descriptor
/// or process-group value.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, Default)]
pub struct PtyMasterControlToken {
    pub(crate) process_group_leader: Option<i32>,
    pub(crate) raw_fd: Option<i32>,
}

/// Opaque host control data for a spawned PTY child.
///
/// Native process handles never appear in the facade signature.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, Default)]
pub struct PtyChildControlToken {
    pub(crate) raw_handle: Option<usize>,
}

/// Platform-neutral handle for the master side of a pseudo-terminal.
pub trait PtyMaster: Send + 'static {
    fn try_clone_reader(&mut self) -> io::Result<Box<dyn Read + Send>>;
    fn take_writer(&mut self) -> io::Result<Box<dyn Write + Send>>;
    fn resize(&self, size: PtySize) -> io::Result<()>;
    fn get_size(&self) -> io::Result<PtySize>;

    /// Return opaque host control data without exposing native descriptors.
    fn control_token(&self) -> PtyMasterControlToken {
        PtyMasterControlToken::default()
    }
}

/// Platform-neutral handle for a child process running inside a PTY.
pub trait PtyChild: Send + 'static {
    fn pid(&self) -> u32;
    fn try_wait(&mut self) -> io::Result<Option<u32>>;
    fn wait(&mut self) -> io::Result<u32>;
    fn kill(&mut self) -> io::Result<()>;

    /// Return opaque host control data without exposing a native process handle.
    fn control_token(&self) -> PtyChildControlToken {
        PtyChildControlToken::default()
    }
}

/// Platform-neutral handle for the slave side of a pseudo-terminal.
pub trait PtySlave: Send + 'static {
    type Child: PtyChild;

    fn spawn(
        self,
        argv: &[OsString],
        cwd: Option<&Path>,
        env: Option<&[(OsString, OsString)]>,
    ) -> io::Result<Self::Child>;
}

/// Factory trait implemented by the selected host PTY backend.
pub trait PtyBackend {
    type Master: PtyMaster;
    type Slave: PtySlave;

    fn openpty(size: PtySize) -> io::Result<(Self::Master, Self::Slave)>;
}

/// Raw replies returned by a bounded active terminal-graphics probe.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TerminalGraphicsProbe {
    pub sixel_xtsmgraphics: Option<String>,
    pub sixel_da1: Option<String>,
    pub kitty_graphics: Option<String>,
    pub iterm2_capabilities: Option<String>,
}

/// Probe the controlling terminal without exposing terminal descriptors.
pub fn active_graphics_probe(timeout: std::time::Duration) -> TerminalGraphicsProbe {
    crate::active_graphics_probe(timeout)
}

pub mod input {
    pub use crate::terminal_input::*;
}

#[cfg(feature = "pty")]
pub use crate::{
    Backend, ChildProcessInfo, ConPtyBackendKind, OrphanConhostInfo, PtyProcessGuard,
    PtySpawnContext, TerminalInputSession,
};

#[cfg(feature = "pty")]
pub use crate::current_backend_kind;

#[cfg(feature = "pty")]
pub fn before_pty_spawn() -> PtySpawnContext {
    crate::before_pty_spawn()
}

#[cfg(feature = "pty")]
pub fn prepare_pty_child(
    context: PtySpawnContext,
    child: PtyChildControlToken,
    nice: Option<i32>,
) -> io::Result<PtyProcessGuard> {
    crate::prepare_pty_child(context, child, nice)
}

#[cfg(feature = "pty")]
pub fn input_payload(data: &[u8]) -> Vec<u8> {
    crate::input_payload(data)
}

#[cfg(feature = "pty")]
pub fn query_responses(data: &[u8]) -> Vec<Vec<u8>> {
    crate::query_responses(data)
}

#[cfg(feature = "pty")]
pub fn shell_argv(command: &str) -> Vec<String> {
    crate::shell_argv(command)
}

#[cfg(feature = "pty")]
pub fn wait_before_close_supported() -> bool {
    crate::wait_before_pty_close_supported()
}

#[cfg(feature = "pty")]
pub fn is_ignorable_process_control_error(error: &io::Error) -> bool {
    crate::is_ignorable_process_control_error(error)
}

#[cfg(feature = "pty")]
pub fn send_pty_interrupt(
    target: PtyMasterControlToken,
    writer: &SharedPtyWriter,
) -> io::Result<bool> {
    crate::send_pty_interrupt(target, writer)
}

#[cfg(feature = "pty")]
pub fn kill_pty_process_group(target: PtyMasterControlToken) -> io::Result<()> {
    crate::kill_pty_process_group(target)
}

#[cfg(feature = "pty")]
pub fn terminate_pty_child(pid: u32) -> io::Result<bool> {
    crate::terminate_pty_child(pid)
}

#[cfg(feature = "pty")]
pub fn signal_pty_tree(pid: u32, force: bool) -> io::Result<bool> {
    crate::signal_pty_tree(pid, force)
}

#[cfg(feature = "pty")]
pub fn resize_pty(master: &dyn PtyMaster, size: PtySize) -> io::Result<()> {
    crate::resize_pty(master, size)
}

#[cfg(feature = "pty")]
pub fn preferred_pty_pid(master: &dyn PtyMaster, child: &dyn PtyChild) -> Option<u32> {
    crate::preferred_pty_pid(master, child)
}

#[cfg(feature = "pty")]
pub fn find_child_processes(parent_pid: u32) -> Vec<ChildProcessInfo> {
    crate::find_child_processes(parent_pid)
}

#[cfg(feature = "pty")]
pub fn find_orphan_conhosts() -> Vec<OrphanConhostInfo> {
    crate::find_orphan_conhosts()
}
