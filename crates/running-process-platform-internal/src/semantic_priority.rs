//! Portable scheduling intent mapped by the native substrate.

/// Scheduling intent for a newly created child.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ProcessPriority {
    /// Leave the host default/inherited priority unchanged.
    #[default]
    Normal,
    /// Below-normal scheduling.
    Low,
    /// Idle scheduling.
    Idle,
    /// Elevated scheduling. Host permissions still apply.
    High,
}

impl ProcessPriority {
    /// Return the native niceness value representing this portable intent.
    #[must_use]
    pub fn nice_value(self) -> Option<i32> {
        match self {
            Self::Normal => None,
            Self::Idle => Some(19),
            Self::Low => Some(if cfg!(windows) { 1 } else { 10 }),
            Self::High => Some(if cfg!(windows) { -15 } else { -5 }),
        }
    }
}
