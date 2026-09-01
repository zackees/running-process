//! Bounded terminal-graphics probing without pseudo-terminal ownership.

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
