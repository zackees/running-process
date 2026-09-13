//! Explicit unavailable-platform contract; never silently create a child.
use crate::platform::independent_spawn::LaunchSpec;
use std::{io, path::Path, sync::atomic::AtomicBool, time::Duration};

pub enum IndependentChild {}
impl IndependentChild {
    pub fn id(&self) -> u32 {
        match *self {}
    }
    pub fn is_alive(&self) -> bool {
        match *self {}
    }
    pub fn stop(&mut self, _timeout: Duration) -> io::Result<()> {
        match *self {}
    }
    pub fn wait(&self, _timeout: Duration, _cancelled: &AtomicBool) -> io::Result<()> {
        match *self {}
    }
}
pub fn spawn(
    _spec: &LaunchSpec,
    _helper: &Path,
    _timeout: Duration,
    _cancelled: &AtomicBool,
) -> io::Result<IndependentChild> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "no verified independent scheduler backend for this platform",
    ))
}
