//! Spawn coordination contract for broker-managed backends.
//!
//! This module does not launch child processes yet. It owns the state that
//! Phase 4/5 launch code needs before spawning: per-backend-key budget windows,
//! single-flight protection, and retry-after hints for refused Hello replies.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use super::backend_registry::BackendKey;

/// Default backend spawn attempts allowed per budget window.
pub const DEFAULT_SPAWN_ATTEMPTS_PER_WINDOW: u32 = 3;

/// Default backend spawn budget window.
pub const DEFAULT_SPAWN_BUDGET_WINDOW: Duration = Duration::from_secs(60);

/// Spawn-budget tuning.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SpawnBudgetConfig {
    /// Maximum spawn attempts in one window.
    pub max_attempts: u32,
    /// Window duration.
    pub window: Duration,
}

impl SpawnBudgetConfig {
    /// Build a config, clamping zero values to safe non-zero defaults.
    pub fn new(max_attempts: u32, window: Duration) -> Self {
        Self {
            max_attempts: max_attempts.max(1),
            window: if window.is_zero() {
                Duration::from_millis(1)
            } else {
                window
            },
        }
    }
}

impl Default for SpawnBudgetConfig {
    fn default() -> Self {
        Self {
            max_attempts: DEFAULT_SPAWN_ATTEMPTS_PER_WINDOW,
            window: DEFAULT_SPAWN_BUDGET_WINDOW,
        }
    }
}

/// Coordinates bounded spawn attempts for backend keys.
#[derive(Debug)]
pub struct SpawnCoordinator {
    config: SpawnBudgetConfig,
    states: HashMap<BackendKey, SpawnBudgetState>,
}

impl SpawnCoordinator {
    /// Create an empty coordinator with default budget settings.
    pub fn new() -> Self {
        Self::with_config(SpawnBudgetConfig::default())
    }

    /// Create an empty coordinator with explicit budget settings.
    pub fn with_config(config: SpawnBudgetConfig) -> Self {
        Self {
            config,
            states: HashMap::new(),
        }
    }

    /// Begin one spawn attempt for `key`.
    ///
    /// The returned permit is a contract token for the caller that will perform
    /// the actual child-process launch in later slices. Call [`Self::finish`]
    /// when that launch path succeeds or fails.
    pub fn try_begin(
        &mut self,
        key: BackendKey,
        now: Instant,
    ) -> Result<SpawnPermit, SpawnBeginError> {
        let state = self
            .states
            .entry(key.clone())
            .or_insert_with(|| SpawnBudgetState::new(now));
        state.refresh(now, self.config.window);

        if state.in_flight {
            return Err(SpawnBeginError::AlreadyInProgress);
        }

        if state.attempts_used >= self.config.max_attempts {
            return Err(SpawnBeginError::BudgetExhausted {
                retry_after: retry_after(state.window_started_at, now, self.config.window),
                remaining: 0,
            });
        }

        state.attempts_used += 1;
        state.in_flight = true;
        Ok(SpawnPermit {
            key,
            attempt_number: state.attempts_used,
            remaining_after_begin: self.config.max_attempts - state.attempts_used,
        })
    }

    /// Finish an in-flight spawn attempt.
    pub fn finish(&mut self, key: &BackendKey, outcome: SpawnOutcome, now: Instant) {
        let Some(state) = self.states.get_mut(key) else {
            return;
        };
        state.refresh(now, self.config.window);
        state.in_flight = false;
        if outcome == SpawnOutcome::Success {
            state.window_started_at = now;
            state.attempts_used = 0;
        }
    }

    /// Return the current budget snapshot for one backend key.
    pub fn snapshot(&mut self, key: BackendKey, now: Instant) -> SpawnBudgetSnapshot {
        let state = self
            .states
            .entry(key.clone())
            .or_insert_with(|| SpawnBudgetState::new(now));
        state.refresh(now, self.config.window);
        snapshot_for(key, state, self.config, now)
    }
}

impl Default for SpawnCoordinator {
    fn default() -> Self {
        Self::new()
    }
}

/// Token returned for a spawn attempt that may proceed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpawnPermit {
    /// Backend key this permit covers.
    pub key: BackendKey,
    /// 1-based attempt number inside the current window.
    pub attempt_number: u32,
    /// Budget remaining after this attempt starts.
    pub remaining_after_begin: u32,
}

/// Result of a spawn attempt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpawnOutcome {
    /// The backend process was launched and verified.
    Success,
    /// The backend process failed to launch or verify.
    Failed,
}

/// Errors returned when a spawn attempt cannot begin.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SpawnBeginError {
    /// Another worker is already launching this backend key.
    #[error("backend spawn already in progress")]
    AlreadyInProgress,
    /// The per-key spawn budget is exhausted.
    #[error("backend spawn budget exhausted; retry after {retry_after:?}")]
    BudgetExhausted {
        /// Time until the budget window resets.
        retry_after: Duration,
        /// Remaining attempts, always zero for this variant.
        remaining: u32,
    },
}

/// Current budget state for metrics/admin snapshots.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpawnBudgetSnapshot {
    /// Backend key this snapshot describes.
    pub key: BackendKey,
    /// Attempts used in the active window.
    pub attempts_used: u32,
    /// Attempts still available in the active window.
    pub remaining: u32,
    /// Whether a spawn is currently in flight.
    pub in_flight: bool,
    /// Retry-after hint when no attempts remain.
    pub retry_after: Option<Duration>,
}

#[derive(Clone, Debug)]
struct SpawnBudgetState {
    window_started_at: Instant,
    attempts_used: u32,
    in_flight: bool,
}

impl SpawnBudgetState {
    fn new(now: Instant) -> Self {
        Self {
            window_started_at: now,
            attempts_used: 0,
            in_flight: false,
        }
    }

    fn refresh(&mut self, now: Instant, window: Duration) {
        if elapsed_since(self.window_started_at, now) >= window {
            self.window_started_at = now;
            self.attempts_used = 0;
            self.in_flight = false;
        }
    }
}

fn snapshot_for(
    key: BackendKey,
    state: &SpawnBudgetState,
    config: SpawnBudgetConfig,
    now: Instant,
) -> SpawnBudgetSnapshot {
    let remaining = config.max_attempts.saturating_sub(state.attempts_used);
    SpawnBudgetSnapshot {
        key,
        attempts_used: state.attempts_used,
        remaining,
        in_flight: state.in_flight,
        retry_after: (remaining == 0)
            .then(|| retry_after(state.window_started_at, now, config.window)),
    }
}

fn retry_after(window_started_at: Instant, now: Instant, window: Duration) -> Duration {
    window.saturating_sub(elapsed_since(window_started_at, now))
}

fn elapsed_since(started_at: Instant, now: Instant) -> Duration {
    now.checked_duration_since(started_at)
        .unwrap_or(Duration::ZERO)
}
