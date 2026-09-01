//! Shared validation for frozen v1 registration records.

/// Errors that prevent using a valid registration name or broker path.
///
/// The legacy broker's pipe helpers re-export this exact type when `client`
/// is selected. Registration itself only needs the `InvalidName` form.
#[derive(Debug, thiserror::Error)]
pub enum PipePathError {
    /// A name argument failed validation.
    #[error("invalid name {name:?}: {reason}")]
    InvalidName {
        /// The offending input.
        name: String,
        /// Why it was rejected.
        reason: &'static str,
    },

    /// The derived path exceeds a platform-specific bound.
    #[error("derived path exceeds {limit_label} ({len} > {max})")]
    PathTooLong {
        /// Length we tried to produce.
        len: usize,
        /// Platform-specific cap.
        max: usize,
        /// "Windows MAX_PATH" / "macOS sun_path" / etc.
        limit_label: &'static str,
    },

    /// Failure to compute the per-user SID hash for a legacy broker pipe.
    #[cfg(feature = "client")]
    #[error(transparent)]
    Sid(#[from] crate::broker::lifecycle::sid::SidError),
}

/// Validate a service name against `[a-z0-9-]{1,64}`.
pub fn validate_service_name(name: &str) -> Result<(), PipePathError> {
    if name.is_empty() {
        return Err(PipePathError::InvalidName {
            name: name.into(),
            reason: "service name must be at least 1 character",
        });
    }
    if name.len() > 64 {
        return Err(PipePathError::InvalidName {
            name: name.into(),
            reason: "service name must be 64 characters or fewer",
        });
    }
    for character in name.chars() {
        match character {
            'a'..='z' | '0'..='9' | '-' => {}
            'A'..='Z' => {
                return Err(PipePathError::InvalidName {
                    name: name.into(),
                    reason: "uppercase letters are forbidden (case-only \
                             collisions with lowercase names would silently \
                             merge under Windows named-pipe semantics)",
                });
            }
            _ => {
                return Err(PipePathError::InvalidName {
                    name: name.into(),
                    reason: "only lowercase ASCII letters, digits, and '-' allowed",
                });
            }
        }
    }
    Ok(())
}

/// Validate a semver-like version string against
/// `^[0-9]+\.[0-9]+\.[0-9]+(-[a-z0-9.]+)?$`.
#[cfg(feature = "daemon-registration")]
pub fn validate_version(version: &str) -> Result<(), PipePathError> {
    if version.is_empty() {
        return Err(PipePathError::InvalidName {
            name: version.into(),
            reason: "version must not be empty",
        });
    }
    let (core, prerelease) = match version.split_once('-') {
        Some((core, tail)) => (core, Some(tail)),
        None => (version, None),
    };
    let parts: Vec<&str> = core.split('.').collect();
    if parts.len() != 3 {
        return Err(PipePathError::InvalidName {
            name: version.into(),
            reason: "version core must be MAJOR.MINOR.PATCH",
        });
    }
    for part in &parts {
        if part.is_empty() || !part.chars().all(|character| character.is_ascii_digit()) {
            return Err(PipePathError::InvalidName {
                name: version.into(),
                reason: "MAJOR/MINOR/PATCH must be non-empty digits",
            });
        }
    }
    if let Some(tail) = prerelease {
        if tail.is_empty() {
            return Err(PipePathError::InvalidName {
                name: version.into(),
                reason: "pre-release suffix after '-' must not be empty",
            });
        }
        for character in tail.chars() {
            match character {
                'a'..='z' | '0'..='9' | '.' => {}
                _ => {
                    return Err(PipePathError::InvalidName {
                        name: version.into(),
                        reason: "pre-release tail allows only [a-z0-9.]",
                    });
                }
            }
        }
    }
    Ok(())
}
