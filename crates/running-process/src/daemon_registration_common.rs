//! Shared internals for the independently selectable registration writers.
//!
//! This is deliberately crate-private. The v1 and v2 public modules each
//! expose their established public paths, while both retain one canonical
//! validation, error, default-directory, and owner-private-directory
//! implementation.

#[path = "daemon_registration/validation.rs"]
pub(crate) mod validation;

#[path = "broker/secure_dir.rs"]
pub(crate) mod secure_dir;

pub(crate) mod service_definition {
    use std::io;
    use std::path::{Path, PathBuf};

    use super::{secure_dir, validation::PipePathError};

    /// Environment override for tests and development.
    #[cfg(feature = "daemon-registration")]
    pub const SERVICE_DEF_DIR_ENV: &str = "RUNNING_PROCESS_SERVICE_DEF_DIR";

    /// Return the platform service-definition directory.
    #[must_use]
    pub fn service_definition_dir() -> PathBuf {
        // An empty value is not a directory. It used to yield `PathBuf::from("")`,
        // which resolves relative to the working directory -- so `…SERVICE_DEF_DIR=`
        // silently moved service-definition lookup to wherever the broker happened
        // to be started from, while `config --effective` still reported an override.
        if let Some(path) = crate::env_vars::SERVICE_DEF_DIR.path() {
            return path;
        }

        // Where a host keeps a product's configuration is a role `platform::fs`
        // names; this used to spell out all three answers. Config is deliberately
        // not the data root: Windows separates roaming settings from local data,
        // and XDG gives configuration its own base directory.
        crate::platform::fs::user_config_dir("running-process").join("services")
    }

    /// Ensure a service-definition directory exists with private permissions.
    pub fn ensure_service_definition_dir(path: &Path) -> Result<(), ServiceDefinitionError> {
        secure_dir::ensure_private_dir(path)?;
        ensure_loadable_service_definition_dir(path)
    }

    /// Errors returned while loading or writing service-definition files.
    #[derive(Debug, thiserror::Error)]
    pub enum ServiceDefinitionError {
        /// Filesystem operation failed.
        #[error("service-definition I/O failed: {0}")]
        Io(#[from] io::Error),
        /// Protobuf decode failed.
        #[error("service-definition protobuf decode failed: {0}")]
        Decode(#[from] prost::DecodeError),
        /// Name or version validation failed.
        #[error(transparent)]
        InvalidName(#[from] PipePathError),
        /// Directory permissions are too broad.
        #[error("service-definition directory has insecure permissions: {0}")]
        InsecureDirectory(PathBuf),
        /// File content did not match the requested service.
        #[error("service-definition requested {requested:?} but file declares {actual:?}")]
        ServiceNameMismatch {
            /// Service name requested by the Hello path.
            requested: String,
            /// Service name decoded from disk.
            actual: String,
        },
        /// A path field was empty or relative.
        #[error("service-definition {field} is invalid: {path:?} ({reason})")]
        InvalidPath {
            /// Field name.
            field: &'static str,
            /// Field value.
            path: String,
            /// Why it failed validation.
            reason: &'static str,
        },
        /// Isolation fields were inconsistent.
        #[error("service-definition isolation is invalid: {reason}")]
        InvalidIsolation {
            /// Why it failed validation.
            reason: &'static str,
        },
    }

    fn ensure_loadable_service_definition_dir(path: &Path) -> Result<(), ServiceDefinitionError> {
        if !secure_dir::private_dir_permissions_are_private(path)? {
            return Err(ServiceDefinitionError::InsecureDirectory(
                path.to_path_buf(),
            ));
        }
        Ok(())
    }
}
