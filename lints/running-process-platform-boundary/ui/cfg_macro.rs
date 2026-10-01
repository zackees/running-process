fn main() {
    // `cfg!` selects a host at a call site; it is a boundary violation just
    // like the attribute form.
    let _is_windows = cfg!(windows);
}
