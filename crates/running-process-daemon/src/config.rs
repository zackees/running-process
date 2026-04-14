use serde::Deserialize;
use std::path::PathBuf;

#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct DaemonConfig {
    pub idle_timeout_secs: u64,
    pub reaper_interval_secs: u64,
    pub runtime_gc_interval_secs: u64,
    pub runtime_gc_stale_after_secs: u64,
    pub connection_idle_timeout_secs: u64,
    pub max_connections: usize,
    pub dev: DevConfig,
}

#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct DevConfig {
    pub idle_timeout_secs: u64,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            idle_timeout_secs: 600, // 10 minutes
            reaper_interval_secs: 30,
            runtime_gc_interval_secs: 300,
            runtime_gc_stale_after_secs: 6 * 60 * 60,
            connection_idle_timeout_secs: 60,
            max_connections: 64,
            dev: DevConfig::default(),
        }
    }
}

impl Default for DevConfig {
    fn default() -> Self {
        Self {
            idle_timeout_secs: 120, // 2 minutes
        }
    }
}

impl DaemonConfig {
    /// Load config from the platform config directory.
    /// Falls back to defaults if file doesn't exist or is malformed.
    pub fn load() -> Self {
        let path = Self::config_path();
        match std::fs::read_to_string(&path) {
            Ok(contents) => toml::from_str(&contents).unwrap_or_else(|e| {
                tracing::warn!(
                    "failed to parse config at {}: {e}, using defaults",
                    path.display()
                );
                Self::default()
            }),
            Err(_) => Self::default(),
        }
    }

    /// Get the effective idle timeout based on scope
    pub fn effective_idle_timeout(&self, is_dev: bool) -> u64 {
        if is_dev {
            self.dev.idle_timeout_secs
        } else {
            self.idle_timeout_secs
        }
    }

    /// Platform-specific config file path
    pub fn config_path() -> PathBuf {
        let mut path = dirs::config_dir().unwrap_or_else(|| PathBuf::from("."));
        path.push("running-process");
        path.push("daemon.toml");
        path
    }
}

/// Check if we're in dev scope based on env var
pub fn is_dev_scope() -> bool {
    std::env::var("RUNNING_PROCESS_DAEMON_SCOPE")
        .map(|v| v.eq_ignore_ascii_case("dev"))
        .unwrap_or(false)
}

/// Check if tracking is disabled
pub fn is_tracking_disabled() -> bool {
    std::env::var("RUNNING_PROCESS_NO_TRACKING")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_values() {
        let cfg = DaemonConfig::default();
        assert_eq!(cfg.idle_timeout_secs, 600);
        assert_eq!(cfg.reaper_interval_secs, 30);
        assert_eq!(cfg.runtime_gc_interval_secs, 300);
        assert_eq!(cfg.runtime_gc_stale_after_secs, 21_600);
        assert_eq!(cfg.connection_idle_timeout_secs, 60);
        assert_eq!(cfg.max_connections, 64);
        assert_eq!(cfg.dev.idle_timeout_secs, 120);
    }

    #[test]
    fn effective_idle_timeout_prod() {
        let cfg = DaemonConfig::default();
        assert_eq!(cfg.effective_idle_timeout(false), 600);
    }

    #[test]
    fn effective_idle_timeout_dev() {
        let cfg = DaemonConfig::default();
        assert_eq!(cfg.effective_idle_timeout(true), 120);
    }

    #[test]
    fn load_falls_back_to_defaults() {
        // Config file almost certainly doesn't exist in test env.
        let cfg = DaemonConfig::load();
        assert_eq!(cfg.idle_timeout_secs, 600);
    }

    #[test]
    fn parse_partial_toml() {
        let toml_str = r#"
idle_timeout_secs = 300
"#;
        let cfg: DaemonConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(cfg.idle_timeout_secs, 300);
        // Other fields should get defaults via #[serde(default)]
        assert_eq!(cfg.reaper_interval_secs, 30);
        assert_eq!(cfg.runtime_gc_interval_secs, 300);
        assert_eq!(cfg.runtime_gc_stale_after_secs, 21_600);
        assert_eq!(cfg.dev.idle_timeout_secs, 120);
    }

    #[test]
    fn parse_full_toml() {
        let toml_str = r#"
idle_timeout_secs = 900
reaper_interval_secs = 15
runtime_gc_interval_secs = 120
runtime_gc_stale_after_secs = 7200
connection_idle_timeout_secs = 120
max_connections = 32

[dev]
idle_timeout_secs = 60
"#;
        let cfg: DaemonConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(cfg.idle_timeout_secs, 900);
        assert_eq!(cfg.reaper_interval_secs, 15);
        assert_eq!(cfg.runtime_gc_interval_secs, 120);
        assert_eq!(cfg.runtime_gc_stale_after_secs, 7200);
        assert_eq!(cfg.connection_idle_timeout_secs, 120);
        assert_eq!(cfg.max_connections, 32);
        assert_eq!(cfg.dev.idle_timeout_secs, 60);
    }

    #[test]
    fn config_path_is_not_empty() {
        let path = DaemonConfig::config_path();
        assert!(!path.as_os_str().is_empty());
        assert!(path.ends_with("daemon.toml"));
    }

    #[test]
    fn is_dev_scope_default() {
        // Without the env var set, should return false.
        // (We can't guarantee the env is clean, but in most test envs it will be.)
        // This is a smoke test — the real behaviour depends on env state.
        let _ = is_dev_scope();
    }

    #[test]
    fn is_tracking_disabled_default() {
        let _ = is_tracking_disabled();
    }
}
