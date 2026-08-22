//! Executable naming, search, image discovery, and materialization primitives.
//!
//! Callers name the *program* they want. Whether that program is a file called
//! `runpm` or `runpm.exe`, and where a sibling install lives relative to the
//! running image, is a host mechanic and is decided here.

pub use crate::{
    executable_file_name as file_name,
    executable_sibling_of_current_image as sibling_of_current_image, EXECUTABLE_EXTENSION,
};

#[cfg(test)]
mod tests {
    use super::*;

    /// The host decides the spelling; the caller never does.
    ///
    /// Asserted against `EXECUTABLE_EXTENSION` rather than a hard-coded
    /// `.exe`, so the test states the contract instead of restating one host's
    /// answer -- the shape the caller sites used to have.
    #[test]
    fn file_name_applies_the_host_executable_extension() {
        let named = file_name("running-process-daemon");
        match EXECUTABLE_EXTENSION {
            Some(extension) => {
                assert_eq!(named, format!("running-process-daemon.{extension}"));
                assert!(std::path::Path::new(&named).extension().is_some());
            }
            None => assert_eq!(named, "running-process-daemon"),
        }
    }

    /// The running image is always a sibling of itself, under whatever
    /// spelling this host uses -- which is the only claim that holds on every
    /// host without assuming what else is installed.
    #[test]
    fn the_running_image_is_found_beside_itself() {
        let current = std::env::current_exe().expect("current image");
        let bare = current
            .file_stem()
            .expect("image stem")
            .to_string_lossy()
            .into_owned();

        assert_eq!(sibling_of_current_image(&bare).as_deref(), Some(&*current));
    }

    /// A program that is not installed beside us is reported as absent rather
    /// than as a path that does not exist.
    #[test]
    fn an_absent_sibling_is_none_not_a_missing_path() {
        assert!(sibling_of_current_image("rp-no-such-sibling-program").is_none());
    }
}
