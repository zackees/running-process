use std::path::PathBuf;

use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamKind {
    Stdout,
    Stderr,
}

impl StreamKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Stdout => "stdout",
            Self::Stderr => "stderr",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamEvent {
    pub stream: StreamKind,
    pub line: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadStatus<T> {
    Line(T),
    Timeout,
    Eof,
}

#[derive(Debug, Error)]
pub enum ProcessError {
    #[error("process already started")]
    AlreadyStarted,
    #[error("process is not running")]
    NotRunning,
    #[error("process stdin is not available")]
    StdinUnavailable,
    #[error("failed to spawn process: {0}")]
    Spawn(std::io::Error),
    #[error("failed to read process output: {0}")]
    Io(std::io::Error),
    #[error("process timed out")]
    Timeout,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunOutput {
    /// Raw stdout bytes captured from the child.
    pub stdout: Vec<u8>,
    /// Raw stderr bytes captured from the child.
    pub stderr: Vec<u8>,
    /// Process exit code, with Unix signal exits represented as negative signal numbers.
    pub exit_code: i32,
}

#[derive(Debug, Clone)]
pub enum CommandSpec {
    Shell(String),
    Argv(Vec<String>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StdinMode {
    Inherit,
    Piped,
    Null,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StderrMode {
    Stdout,
    Pipe,
}

#[derive(Debug, Clone)]
pub struct ProcessConfig {
    pub command: CommandSpec,
    pub cwd: Option<PathBuf>,
    pub env: Option<Vec<(String, String)>>,
    pub capture: bool,
    pub stderr_mode: StderrMode,
    pub creationflags: Option<u32>,
    pub create_process_group: bool,
    pub stdin_mode: StdinMode,
    pub nice: Option<i32>,
}
