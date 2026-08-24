use std::path::PathBuf;

/// One broker instance discovered from the local pipe namespace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrokerInstance {
    /// Filesystem path or named pipe string.
    pub path: String,
}

/// Enumerate broker v1 instances visible to the current user.
pub fn list() -> Vec<BrokerInstance> {
    // Enumeration here means reading a directory, so it only means anything
    // where an endpoint has a name in the filesystem. Asking the transport
    // rather than the host also keeps this honest about *why* the Windows
    // answer is empty: not because it is Windows, but because there is no
    // directory to read. Enumerating the named-pipe namespace directly lands
    // with the broker binary in Phase 4.
    if !crate::platform::ipc::endpoint_is_filesystem_backed() {
        return Vec::new();
    }
    {
        instance_dirs()
            .into_iter()
            .flat_map(|dir| {
                std::fs::read_dir(dir)
                    .into_iter()
                    .flat_map(|rd| rd.flatten())
                    .filter_map(|entry| {
                        let path = entry.path();
                        let name = path.file_name()?.to_string_lossy();
                        if name.starts_with("rpb-v1-") && name.ends_with(".sock") {
                            Some(BrokerInstance {
                                path: path.to_string_lossy().into_owned(),
                            })
                        } else {
                            None
                        }
                    })
                    .collect::<Vec<_>>()
            })
            .collect()
    }
}

/// Render `running-process-cleanup instances --json`.
pub fn render_json(instances: &[BrokerInstance]) -> String {
    let body = instances
        .iter()
        .map(|instance| {
            format!(
                "{{\"path\":\"{}\"}}",
                crate::cleanup::json_escape(&instance.path)
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    format!("{{\"schema_version\":1,\"instances\":[{body}]}}")
}

/// Directories a filesystem-backed broker endpoint may live in.
fn instance_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(runtime) = crate::env_vars::XDG_RUNTIME_DIR.path() {
        dirs.push(runtime.join("running-process").join("broker"));
    }
    dirs.push(std::env::temp_dir());
    dirs
}
