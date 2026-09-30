// The strict lint, opted into as a workspace crate would.
#![deny(running_process_env_direct)]

const HOME: &str = "HOME";

mod env_vars {
    // Inside a declaration module a direct read is the mechanism: clean.
    pub fn home() -> Option<std::ffi::OsString> {
        std::env::var_os(super::HOME)
    }
}

fn main() {
    // Any literal, not only RUNNING_PROCESS_*: rejected.
    let _ = std::env::var_os("PATH");
    // A constant, but read directly outside a declaration module: rejected.
    let _ = std::env::var(HOME);
    let _ = env_vars::home();
}
