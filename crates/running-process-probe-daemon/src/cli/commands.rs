//! Subcommand implementations (S14 / #643).

use running_process_probe::probe_diag::v1 as wire;
use running_process_probe::probe_diag::v1::probe_envelope::Body;

use crate::cli::render;
use crate::cli::transport::{http_get, http_post, load_discovery, CliError, Client};
use crate::cli::{Cli, Command, DEFAULT_LIMIT};
use crate::discovery::DiscoveryInfo;

/// Run one subcommand and return what should be printed.
pub fn dispatch(cli: &Cli) -> Result<String, CliError> {
    let (path, info) = load_discovery(cli.discovery.as_deref())?;

    match &cli.command {
        Command::Ps {
            name,
            include_unregistered,
            env,
            limit,
        } => {
            let query = wire::ProcessQuery {
                name_glob: name.clone().unwrap_or_default(),
                include_unregistered: *include_unregistered,
                include_env: *env,
                limit: limit.unwrap_or(DEFAULT_LIMIT),
                ..Default::default()
            };
            let processes = query_processes(&info, query)?;
            Ok(render::processes(&processes, cli.json))
        }

        Command::Dump {
            pid,
            name,
            instance,
            all,
            max_depth,
        } => dump(
            cli,
            &info,
            *pid,
            name.as_deref(),
            instance.as_deref(),
            *all,
            *max_depth,
        ),

        Command::Snapshot { pid, max_depth } => {
            let target = resolve_single_pid(&info, *pid)?;
            let status = capture(&info, &target, *max_depth)?;
            Ok(render::capture(&[status], cli.json))
        }

        Command::Crashes {
            class,
            class_like,
            signature,
            stats,
            limit,
        } => {
            let filter = wire::CrashQuery {
                app_class: class.clone().unwrap_or_default(),
                app_class_like: class_like.clone().unwrap_or_default(),
                signature: signature.clone().unwrap_or_default(),
                limit: limit.unwrap_or(DEFAULT_LIMIT),
                ..Default::default()
            };
            if *stats {
                let reply = call(
                    &info,
                    Body::CrashStatsQuery(wire::CrashStatsQuery {
                        filter: Some(filter),
                    }),
                )?;
                match reply {
                    Body::CrashStatsReply(stats) => {
                        refuse_if_error(stats.error, &stats.detail)?;
                        Ok(render::crash_stats(&stats, cli.json))
                    }
                    other => Err(unexpected(&other)),
                }
            } else {
                match call(&info, Body::CrashQuery(filter))? {
                    Body::CrashQueryReply(reply) => {
                        refuse_if_error(reply.error, &reply.detail)?;
                        Ok(render::crashes(&reply.records, cli.json))
                    }
                    other => Err(unexpected(&other)),
                }
            }
        }

        Command::Fetch { id, out } => {
            // Always HTTP: this is exactly the payload the socket's frame cap
            // exists to keep off it.
            let bytes = http_get(&info, &format!("/v1/artifacts/{id}"))?;
            let destination = out
                .clone()
                .unwrap_or_else(|| std::path::PathBuf::from(format!("probe-artifact-{id}.bin")));
            std::fs::write(&destination, &bytes)?;
            Ok(format!(
                "wrote {} bytes to {}\n",
                bytes.len(),
                destination.display()
            ))
        }

        Command::Profile {
            seconds,
            hz,
            format,
            out,
        } => {
            let mut url = format!("/v1/profile?seconds={seconds}");
            if let Some(hz) = hz {
                url.push_str(&format!("&hz={hz}"));
            }
            // The daemon runs the session, so this request takes as long as
            // the profile does.
            let response = http_post(&info, &url)?;
            let parsed: serde_json::Value = serde_json::from_slice(&response)
                .map_err(|error| CliError::UnexpectedReply(error.to_string()))?;
            let id = parsed["id"]
                .as_u64()
                .ok_or_else(|| CliError::UnexpectedReply("profile reply carried no id".into()))?;

            let bytes = http_get(&info, &format!("/v1/profiles/{id}/export/{format}"))?;
            let destination = out
                .clone()
                .unwrap_or_else(|| std::path::PathBuf::from(format!("profile-{id}.{format}")));
            std::fs::write(&destination, &bytes)?;

            if cli.json {
                Ok(format!(
                    "{parsed}
"
                ))
            } else {
                Ok(format!(
                    "captured {} sample(s) across {} thread(s), {:.1}% overhead
                     wrote {} bytes to {}
                     flame graph: http://127.0.0.1:{}/v1/profiles/{id}/flamegraph
",
                    parsed["samples_captured"].as_u64().unwrap_or(0),
                    parsed["threads_seen"].as_u64().unwrap_or(0),
                    parsed["overhead_ratio"].as_f64().unwrap_or(0.0) * 100.0,
                    bytes.len(),
                    destination.display(),
                    info.http_port,
                ))
            }
        }

        Command::Doctor => doctor(cli, &path, &info),
    }
}

/// Ask the daemon for matching processes.
fn query_processes(
    info: &DiscoveryInfo,
    query: wire::ProcessQuery,
) -> Result<Vec<wire::ProcessInfo>, CliError> {
    match call(info, Body::ProcessQuery(query))? {
        Body::ProcessQueryReply(reply) => {
            refuse_if_error(reply.error, &reply.detail)?;
            Ok(reply.processes)
        }
        other => Err(unexpected(&other)),
    }
}

/// `dump` — select targets, then capture each.
#[allow(clippy::too_many_arguments)]
fn dump(
    cli: &Cli,
    info: &DiscoveryInfo,
    pid: Option<u32>,
    name: Option<&str>,
    instance: Option<&str>,
    all: bool,
    max_depth: u32,
) -> Result<String, CliError> {
    let targets = if let Some(pid) = pid {
        vec![resolve_single_pid(info, pid)?]
    } else {
        let query = wire::ProcessQuery {
            name_glob: name.unwrap_or("*").to_string(),
            limit: DEFAULT_LIMIT,
            ..Default::default()
        };
        let mut matches = query_processes(info, query)?;
        if let Some(instance) = instance {
            matches.retain(|process| process.instance_name == instance);
        }

        if matches.is_empty() {
            return Err(CliError::Refused(format!(
                "no registered process matches {}",
                name.unwrap_or("*")
            )));
        }
        // Refusing an ambiguous selection is the point of `--all`. Capturing
        // "the first match" would be a coin flip the operator did not know
        // they were tossing, and a stack from the wrong worker looks exactly
        // like a stack from the right one.
        if matches.len() > 1 && !all {
            let names: Vec<String> = matches
                .iter()
                .map(|p| {
                    format!(
                        "{} (pid {})",
                        p.name,
                        p.key.as_ref().map(|k| k.pid).unwrap_or(0)
                    )
                })
                .collect();
            return Err(CliError::Refused(format!(
                "{} processes match; pass --all to capture every one, or narrow the \
                 selection with --instance. Matches: {}",
                matches.len(),
                names.join(", ")
            )));
        }
        matches
            .into_iter()
            .filter_map(|process| process.key)
            .collect()
    };

    let mut statuses = Vec::new();
    for target in &targets {
        statuses.push(capture(info, target, max_depth)?);
    }
    Ok(render::capture(&statuses, cli.json))
}

/// Turn a bare pid into a full process key by asking the daemon.
///
/// A pid alone is not an identity — the OS recycles them — so the capture verb
/// wants the start time too. Looking it up here means the operator can still
/// type a pid, and the daemon still gets an identity that survives reuse.
fn resolve_single_pid(info: &DiscoveryInfo, pid: u32) -> Result<wire::ProcessKey, CliError> {
    let query = wire::ProcessQuery {
        pid: Some(u64::from(pid)),
        limit: 2,
        ..Default::default()
    };
    let mut matches = query_processes(info, query)?;
    match matches.len() {
        0 => Err(CliError::Refused(format!(
            "pid {pid} is not registered with this daemon"
        ))),
        _ => matches
            .remove(0)
            .key
            .ok_or_else(|| CliError::UnexpectedReply("match had no process key".into())),
    }
}

/// Request one capture.
fn capture(
    info: &DiscoveryInfo,
    key: &wire::ProcessKey,
    max_depth: u32,
) -> Result<CaptureStatus, CliError> {
    let request = wire::CaptureStackRequest {
        key: Some(key.clone()),
        max_depth,
        ..Default::default()
    };
    match call(info, Body::CaptureStack(request))? {
        Body::CaptureReply(reply) => Ok(CaptureStatus {
            pid: key.pid,
            job_id: reply.job_id,
            detail: String::new(),
        }),
        Body::JobStatus(status) => Ok(CaptureStatus {
            pid: key.pid,
            job_id: status.job_id,
            detail: status.detail,
        }),
        Body::RegistrationStatus(status) => Err(CliError::Refused(if status.detail.is_empty() {
            format!("capture of pid {} was refused", key.pid)
        } else {
            status.detail
        })),
        other => Err(unexpected(&other)),
    }
}

/// One capture outcome, for rendering.
#[derive(Debug)]
pub struct CaptureStatus {
    /// Target pid.
    pub pid: u64,
    /// Job to poll, when the capture is asynchronous.
    pub job_id: String,
    /// Daemon detail, when there is any.
    pub detail: String,
}

/// `doctor` — check each link in the chain and report which one is broken.
///
/// Deliberately does not stop at the first problem. An operator running
/// `doctor` wants the whole picture; reporting one fault, being fixed, and
/// then reporting the next is three round trips to learn what one run could
/// have said.
fn doctor(cli: &Cli, path: &std::path::Path, info: &DiscoveryInfo) -> Result<String, CliError> {
    let mut checks: Vec<(String, bool, String)> = Vec::new();

    checks.push((
        "discovery file".into(),
        true,
        format!("{} (daemon pid {})", path.display(), info.daemon_pid),
    ));

    let socket = Client::connect(info);
    checks.push(match &socket {
        Ok(_) => ("control socket".into(), true, info.control_socket.clone()),
        Err(error) => (
            "control socket".into(),
            false,
            format!(
                "{error} — the daemon may have exited without removing {}",
                path.display()
            ),
        ),
    });

    let registered = query_processes(
        info,
        wire::ProcessQuery {
            limit: DEFAULT_LIMIT,
            ..Default::default()
        },
    );
    checks.push(match &registered {
        Ok(processes) if processes.is_empty() => (
            "registrations".into(),
            false,
            "no processes are registered: nothing can be captured. An app must call \
             probe::install() and reach ARMED."
                .into(),
        ),
        Ok(processes) => (
            "registrations".into(),
            true,
            format!("{} process(es) ARMED", processes.len()),
        ),
        Err(error) => ("registrations".into(), false, error.to_string()),
    });

    let http = http_get(info, "/v1/ps?limit=1");
    checks.push(match &http {
        Ok(_) => (
            "http surface".into(),
            true,
            format!("127.0.0.1:{}", info.http_port),
        ),
        Err(error) => (
            "http surface".into(),
            false,
            format!("{error} — artifact download needs this; the socket cannot carry one"),
        ),
    });

    let symbolizer = crate::symbolication::worker_path();
    checks.push(match &symbolizer {
        Some(path) => ("symbolizer".into(), true, path.display().to_string()),
        None => (
            "symbolizer".into(),
            false,
            "running-process-probe-worker not found next to the daemon: captures will \
             come back with addresses instead of function names"
                .into(),
        ),
    });

    let healthy = checks.iter().all(|(_, ok, _)| *ok);
    let report = render::doctor(&checks, cli.json);
    if healthy {
        Ok(report)
    } else {
        // Printed, then failed: the operator gets the full report AND a
        // non-zero exit a script can branch on.
        print!("{report}");
        Err(CliError::Refused(
            "one or more probe checks failed (see the report above)".into(),
        ))
    }
}

/// Send one body, over the socket unless HTTP was forced.
fn call(info: &DiscoveryInfo, body: Body) -> Result<Body, CliError> {
    Client::connect(info)?.call(body)
}

/// Turn a daemon-side error code into a CLI failure.
fn refuse_if_error(error: i32, detail: &str) -> Result<(), CliError> {
    if error == 0 {
        return Ok(());
    }
    Err(CliError::Refused(if detail.is_empty() {
        format!("error code {error}")
    } else {
        detail.to_string()
    }))
}

/// A reply this build did not ask for.
fn unexpected(body: &Body) -> CliError {
    CliError::UnexpectedReply(format!("{body:?}"))
}
