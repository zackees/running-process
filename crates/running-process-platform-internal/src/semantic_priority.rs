//! Portable scheduling intent mapped once by the native substrate.

/// Scheduling intent for a newly created child.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ProcessPriority {
    /// Leave the native default/inherited priority unchanged.
    #[default]
    Normal,
    /// Below-normal scheduling: Unix nice 10 or Windows BelowNormal class.
    Low,
    /// Idle scheduling: Unix nice 19 or Windows Idle class.
    Idle,
    /// Elevated scheduling: Unix nice -5 or Windows High class.
    /// Native permissions still apply; this does not grant elevation.
    High,
}

impl ProcessPriority {
    /// Return the native launch niceness that represents this intent.
    ///
    /// On Windows, this value is consumed by the substrate's established
    /// nice-to-priority-class mapping; it is not presented as a portable
    /// numeric priority API.
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

#[cfg(test)]
#[test]
fn native_priority_preserves_product_scheduling_intent() {
    assert_eq!(ProcessPriority::Normal.nice_value(), None);
    assert_eq!(ProcessPriority::Idle.nice_value(), Some(19));
    assert_eq!(
        ProcessPriority::Low.nice_value(),
        Some(if cfg!(windows) { 1 } else { 10 })
    );
    assert_eq!(
        ProcessPriority::High.nice_value(),
        Some(if cfg!(windows) { -15 } else { -5 })
    );
}
