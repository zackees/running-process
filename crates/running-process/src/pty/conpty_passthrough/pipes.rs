//! Anonymous-pipe pair plumbing for ConPTY passthrough (#150 W2).
//!
//! ConPTY needs two anonymous pipes:
//! * input pipe — host writes child's stdin; child reads it
//! * output pipe — child writes stdout/stderr; host reads it
//!
//! Both pipes are inherited by the child via the ConPTY handle. We
//! must mark only the ConPTY-side ends as inheritable (the master-side
//! ends stay private to the host process via
//! `SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0)`).
//!
//! Wave 1 stub. Actual `CreatePipe` + `SetHandleInformation` wiring
//! lands in W2.

#![cfg(windows)]
