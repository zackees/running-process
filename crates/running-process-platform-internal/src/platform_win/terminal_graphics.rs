//! Windows has no active terminal-graphics probe implementation yet.

pub fn active_graphics_probe(
    _timeout: std::time::Duration,
) -> crate::platform::terminal_graphics::TerminalGraphicsProbe {
    crate::platform::terminal_graphics::TerminalGraphicsProbe::default()
}
