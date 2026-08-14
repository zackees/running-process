fn main() {
    #[cfg(any(windows, not(target_os = "linux")))]
    use std::os::windows::process::CommandExt as _;
}
