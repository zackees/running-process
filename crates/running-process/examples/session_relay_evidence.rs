//! Reproducible SESSION-relay copy-avoidance evidence for running-process#949.
//!
//! The harness deliberately leaves the successful Soldr daemon/broker protocol
//! alone. It measures the existing full proxy (`copy_bidirectional`) against a
//! direct one-hop ceiling, a 64 KiB buffered control, and a Linux-only `splice`
//! prototype. Every non-direct run uses two real local sockets.
//!
//! ```text
//! soldr cargo run -p running-process --example session_relay_evidence \
//!   --features daemon --release -- --quick
//! ```

use std::io;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use interprocess::local_socket::tokio::prelude::*;
use interprocess::local_socket::{ListenerOptions, Name};
use tokio::io::{
    AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader, Lines,
};
use tokio::process::{Child, ChildStdout, Command};
use tokio::task::{JoinHandle, JoinSet};

const TUNED_BUFFER_BYTES: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Topology {
    Direct,
    Current,
    Tuned,
    #[cfg(target_os = "linux")]
    Splice,
}

impl Topology {
    fn label(self) -> &'static str {
        match self {
            Self::Direct => "direct",
            Self::Current => "current",
            Self::Tuned => "tuned-64k",
            #[cfg(target_os = "linux")]
            Self::Splice => "splice",
        }
    }

    fn validates_directional_eof(self) -> bool {
        #[cfg(target_os = "linux")]
        {
            matches!(self, Self::Splice)
        }
        #[cfg(not(target_os = "linux"))]
        {
            false
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum Workload {
    Stdout,
    StalledStdout,
    Duplex,
    PingPong,
    Disconnect,
}

impl Workload {
    fn label(self) -> &'static str {
        match self {
            Self::Stdout => "stdout",
            Self::StalledStdout => "stalled-stdout",
            Self::Duplex => "full-duplex",
            Self::PingPong => "ping-pong",
            Self::Disconnect => "disconnect",
        }
    }
}

#[derive(Clone, Copy)]
struct Case {
    topology: Topology,
    workload: Workload,
    sessions: usize,
    chunk_bytes: usize,
    bytes_per_direction: usize,
    ping_count: usize,
}

#[derive(Clone, Copy)]
struct Usage {
    cpu_us: u64,
    context_switches: Option<u64>,
}

struct Outcome {
    transferred_bytes: u64,
    latencies_us: Vec<u64>,
}

type ParsedArgs = (
    bool,
    bool,
    Vec<Topology>,
    Option<usize>,
    Option<Workload>,
    Option<usize>,
    Option<usize>,
);

fn parse_usize_arg(args: &mut impl Iterator<Item = String>, option: &str) -> io::Result<usize> {
    let value = args.next().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{option} needs a value"),
        )
    })?;
    let parsed = value.parse::<usize>().map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{option} must be a positive integer"),
        )
    })?;
    if parsed == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{option} must be a positive integer"),
        ));
    }
    Ok(parsed)
}

fn parse_topologies() -> io::Result<ParsedArgs> {
    let mut quick = false;
    let mut smoke = false;
    let mut selected = None;
    let mut selected_sessions = None;
    let mut selected_workload = None;
    let mut selected_bytes_mib = None;
    let mut selected_chunk_kib = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--quick" => quick = true,
            "--smoke" => {
                quick = true;
                smoke = true;
            }
            "--topology" => {
                selected = Some(args.next().ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidInput, "--topology needs a value")
                })?);
            }
            "--sessions" => {
                selected_sessions = Some(parse_usize_arg(&mut args, "--sessions")?);
            }
            "--bytes-mib" => {
                selected_bytes_mib = Some(parse_usize_arg(&mut args, "--bytes-mib")?);
            }
            "--chunk-kib" => {
                selected_chunk_kib = Some(parse_usize_arg(&mut args, "--chunk-kib")?);
            }
            "--workload" => {
                let value = args.next().ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidInput, "--workload needs a value")
                })?;
                selected_workload = Some(match value.as_str() {
                    "stdout" => Workload::Stdout,
                    "stalled-stdout" => Workload::StalledStdout,
                    "full-duplex" | "duplex" => Workload::Duplex,
                    "ping-pong" | "pingpong" => Workload::PingPong,
                    "disconnect" => Workload::Disconnect,
                    _ => {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidInput,
                            format!("unknown workload: {value}"),
                        ));
                    }
                });
            }
            "--help" | "-h" => {
                println!("usage: session_relay_evidence [--quick|--smoke] [--topology direct|current|tuned|splice|all] [--sessions N] [--workload stdout|stalled-stdout|full-duplex|ping-pong|disconnect] [--bytes-mib N] [--chunk-kib N]");
                std::process::exit(0);
            }
            other => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("unknown argument: {other}"),
                ));
            }
        }
    }

    let all = || {
        #[cfg(target_os = "linux")]
        {
            vec![
                Topology::Direct,
                Topology::Current,
                Topology::Tuned,
                Topology::Splice,
            ]
        }
        #[cfg(not(target_os = "linux"))]
        {
            vec![Topology::Direct, Topology::Current, Topology::Tuned]
        }
    };
    let topologies = match selected.as_deref().unwrap_or("all") {
        "all" => all(),
        "direct" => vec![Topology::Direct],
        "current" => vec![Topology::Current],
        "tuned" | "tuned-64k" => vec![Topology::Tuned],
        "splice" => {
            #[cfg(target_os = "linux")]
            {
                vec![Topology::Splice]
            }
            #[cfg(not(target_os = "linux"))]
            {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "splice is Linux-only; buffered current remains the fallback",
                ));
            }
        }
        value => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("unknown topology: {value}"),
            ));
        }
    };
    Ok((
        quick,
        smoke,
        topologies,
        selected_sessions,
        selected_workload,
        selected_bytes_mib,
        selected_chunk_kib,
    ))
}

fn endpoint(tag: &str, case_id: u64) -> String {
    let leaf = format!("rp-relay-evidence-{}-{tag}-{case_id}", std::process::id());
    #[cfg(windows)]
    {
        format!(r"\\.\pipe\{leaf}")
    }
    #[cfg(unix)]
    {
        std::env::temp_dir()
            .join(format!("{leaf}.sock"))
            .to_string_lossy()
            .into_owned()
    }
}

fn name(path: &str) -> io::Result<Name<'_>> {
    running_process::broker::server::singleton_bind::wrap_socket_name(path)
        .map_err(io::Error::other)
}

fn remove_endpoint(path: &str) {
    #[cfg(unix)]
    {
        let _ = std::fs::remove_file(path);
    }
    #[cfg(windows)]
    let _ = path;
}

fn expected_byte(session: usize, direction: u8, offset: usize) -> u8 {
    (session as u8)
        .wrapping_mul(17)
        .wrapping_add(direction.wrapping_mul(73))
        .wrapping_add(offset as u8)
}

async fn write_pattern<W: AsyncWrite + Unpin>(
    writer: &mut W,
    session: usize,
    direction: u8,
    total: usize,
    chunk_bytes: usize,
) -> io::Result<()> {
    let mut offset = 0;
    let mut buffer = vec![0; chunk_bytes];
    while offset < total {
        let count = (total - offset).min(buffer.len());
        for (index, byte) in buffer[..count].iter_mut().enumerate() {
            *byte = expected_byte(session, direction, offset + index);
        }
        writer.write_all(&buffer[..count]).await?;
        offset += count;
    }
    writer.shutdown().await
}

async fn read_pattern<R: AsyncRead + Unpin>(
    reader: &mut R,
    session: usize,
    direction: u8,
    total: usize,
    chunk_bytes: usize,
    expect_eof: bool,
) -> io::Result<()> {
    let mut offset = 0;
    let mut buffer = vec![0; chunk_bytes];
    while offset < total {
        let count = (total - offset).min(buffer.len());
        reader.read_exact(&mut buffer[..count]).await?;
        for (index, actual) in buffer[..count].iter().copied().enumerate() {
            let expected = expected_byte(session, direction, offset + index);
            if actual != expected {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "session {session} direction {direction} byte {}: expected {expected:#04x}, got {actual:#04x}",
                        offset + index
                    ),
                ));
            }
        }
        offset += count;
    }
    if expect_eof {
        let mut trailing = [0_u8; 1];
        if reader.read(&mut trailing).await? != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "unexpected bytes after declared payload",
            ));
        }
    }
    Ok(())
}

async fn run_server<S>(mut stream: S, case: Case, _accept_index: usize) -> io::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    // The session identity travels on the wire. Concurrent proxy connects can
    // reach the daemon in a different order than clients reached the broker.
    let mut opening = [0_u8; 9];
    stream.read_exact(&mut opening).await?;
    if opening[0] != 0x53 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid synthetic SessionStart marker",
        ));
    }
    let session = u64::from_le_bytes(opening[1..].try_into().expect("eight bytes")) as usize;
    match case.workload {
        Workload::Stdout | Workload::StalledStdout => {
            write_pattern(
                &mut stream,
                session,
                1,
                case.bytes_per_direction,
                case.chunk_bytes,
            )
            .await
        }
        Workload::Duplex => {
            let (mut reader, mut writer) = tokio::io::split(stream);
            let send = write_pattern(
                &mut writer,
                session,
                1,
                case.bytes_per_direction,
                case.chunk_bytes,
            );
            let receive = read_pattern(
                &mut reader,
                session,
                2,
                case.bytes_per_direction,
                case.chunk_bytes,
                // The splice candidate explicitly propagates directional EOF.
                // interprocess's portable AsyncWrite shutdown is a no-op, so
                // buffered controls validate whole-connection disconnect only.
                case.topology.validates_directional_eof(),
            );
            tokio::try_join!(send, receive)?;
            Ok(())
        }
        Workload::PingPong => {
            let mut buffer = vec![0; case.chunk_bytes];
            for _ in 0..case.ping_count {
                stream.read_exact(&mut buffer).await?;
                stream.write_all(&buffer).await?;
                stream.flush().await?;
            }
            stream.shutdown().await
        }
        Workload::Disconnect => {
            let mut trailing = [0_u8; 1];
            if stream.read(&mut trailing).await? != 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "disconnect workload received unexpected payload",
                ));
            }
            Ok(())
        }
    }
}

async fn run_client<S>(mut stream: S, case: Case, session: usize) -> io::Result<Outcome>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut opening = [0_u8; 9];
    opening[0] = 0x53;
    opening[1..].copy_from_slice(&(session as u64).to_le_bytes());
    stream.write_all(&opening).await?;
    stream.flush().await?;
    match case.workload {
        Workload::Stdout | Workload::StalledStdout => {
            if matches!(case.workload, Workload::StalledStdout) {
                // Let kernel and relay buffers saturate before consuming. The
                // broker RSS sampler verifies that backpressure stays bounded.
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
            read_pattern(
                &mut stream,
                session,
                1,
                case.bytes_per_direction,
                case.chunk_bytes,
                case.topology.validates_directional_eof(),
            )
            .await?;
            Ok(Outcome {
                transferred_bytes: case.bytes_per_direction as u64,
                latencies_us: Vec::new(),
            })
        }
        Workload::Duplex => {
            let (mut reader, mut writer) = tokio::io::split(stream);
            let send = write_pattern(
                &mut writer,
                session,
                2,
                case.bytes_per_direction,
                case.chunk_bytes,
            );
            let receive = read_pattern(
                &mut reader,
                session,
                1,
                case.bytes_per_direction,
                case.chunk_bytes,
                false,
            );
            tokio::try_join!(send, receive)?;
            Ok(Outcome {
                transferred_bytes: (case.bytes_per_direction * 2) as u64,
                latencies_us: Vec::new(),
            })
        }
        Workload::PingPong => {
            let mut buffer = vec![0; case.chunk_bytes];
            let mut latencies_us = Vec::with_capacity(case.ping_count);
            for round in 0..case.ping_count {
                for (index, byte) in buffer.iter_mut().enumerate() {
                    *byte = expected_byte(session, round as u8, index);
                }
                let started = Instant::now();
                stream.write_all(&buffer).await?;
                stream.flush().await?;
                stream.read_exact(&mut buffer).await?;
                latencies_us.push(started.elapsed().as_micros() as u64);
                for (index, actual) in buffer.iter().copied().enumerate() {
                    let expected = expected_byte(session, round as u8, index);
                    if actual != expected {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "ping-pong echo was not byte exact",
                        ));
                    }
                }
            }
            stream.shutdown().await?;
            Ok(Outcome {
                transferred_bytes: (case.chunk_bytes * case.ping_count * 2) as u64,
                latencies_us,
            })
        }
        Workload::Disconnect => Ok(Outcome {
            transferred_bytes: 0,
            latencies_us: Vec::new(),
        }),
    }
}

async fn run_relay(
    topology: Topology,
    mut client: interprocess::local_socket::tokio::Stream,
    daemon_path: &str,
) -> io::Result<()> {
    match topology {
        Topology::Direct => unreachable!("direct topology has no relay"),
        Topology::Current => {
            running_process::broker::session_relay::relay_session(client, daemon_path).await
        }
        Topology::Tuned => {
            let mut daemon =
                interprocess::local_socket::tokio::Stream::connect(name(daemon_path)?).await?;
            tokio::io::copy_bidirectional_with_sizes(
                &mut client,
                &mut daemon,
                TUNED_BUFFER_BYTES,
                TUNED_BUFFER_BYTES,
            )
            .await?;
            Ok(())
        }
        #[cfg(target_os = "linux")]
        Topology::Splice => splice_linux::relay(client, daemon_path).await,
    }
}

async fn accept_daemon(
    listener: interprocess::local_socket::tokio::Listener,
    case: Case,
) -> io::Result<()> {
    let mut tasks = JoinSet::new();
    for session in 0..case.sessions {
        let stream = listener.accept().await?;
        tasks.spawn(run_server(stream, case, session));
    }
    while let Some(task) = tasks.join_next().await {
        task.map_err(io::Error::other)??;
    }
    Ok(())
}

async fn accept_broker(
    listener: interprocess::local_socket::tokio::Listener,
    case: Case,
    daemon_path: String,
) -> io::Result<()> {
    let mut tasks = JoinSet::new();
    for _ in 0..case.sessions {
        let stream = listener.accept().await?;
        let path = daemon_path.clone();
        tasks.spawn(async move { run_relay(case.topology, stream, &path).await });
    }
    while let Some(task) = tasks.join_next().await {
        task.map_err(io::Error::other)??;
    }
    Ok(())
}

struct TaskGuard(Option<JoinHandle<io::Result<()>>>);

impl TaskGuard {
    fn new(task: JoinHandle<io::Result<()>>) -> Self {
        Self(Some(task))
    }

    async fn finish(&mut self) -> io::Result<bool> {
        let task = self.0.as_mut().expect("task already finished");
        match tokio::time::timeout(Duration::from_secs(2), &mut *task).await {
            Ok(result) => {
                self.0.take();
                result.map_err(io::Error::other)??;
                Ok(true)
            }
            Err(_) => {
                task.abort();
                let _ = task.await;
                self.0.take();
                Ok(false)
            }
        }
    }
}

impl Drop for TaskGuard {
    fn drop(&mut self) {
        if let Some(task) = &self.0 {
            task.abort();
        }
    }
}

struct EndpointGuard(Vec<String>);

impl Drop for EndpointGuard {
    fn drop(&mut self) {
        for path in &self.0 {
            remove_endpoint(path);
        }
    }
}

struct RssSampler {
    keep_sampling: Arc<AtomicBool>,
    task: Option<JoinHandle<u64>>,
}

impl RssSampler {
    fn start(pid: u32) -> Self {
        let keep_sampling = Arc::new(AtomicBool::new(true));
        let sampler_flag = Arc::clone(&keep_sampling);
        let task = tokio::task::spawn_blocking(move || sample_peak_rss(sampler_flag, pid));
        Self {
            keep_sampling,
            task: Some(task),
        }
    }

    async fn finish(mut self) -> io::Result<u64> {
        self.keep_sampling.store(false, Ordering::Relaxed);
        self.task
            .take()
            .expect("sampler already finished")
            .await
            .map_err(io::Error::other)
    }
}

impl Drop for RssSampler {
    fn drop(&mut self) {
        self.keep_sampling.store(false, Ordering::Relaxed);
    }
}

struct BrokerWorker {
    child: Child,
    lines: Lines<BufReader<ChildStdout>>,
}

async fn start_broker_worker(
    topology: Topology,
    sessions: usize,
    broker_path: &str,
    daemon_path: &str,
) -> io::Result<BrokerWorker> {
    let executable = std::env::current_exe()?;
    let mut child = Command::new(executable)
        .arg("__relay-worker")
        .arg(topology.label())
        .arg(sessions.to_string())
        .arg(broker_path)
        .arg(daemon_path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("worker stdout missing"))?;
    let mut lines = BufReader::new(stdout).lines();
    let ready = tokio::time::timeout(Duration::from_secs(10), lines.next_line())
        .await
        .map_err(|_| {
            io::Error::new(io::ErrorKind::TimedOut, "relay worker did not become ready")
        })??;
    if ready.as_deref() != Some("READY") {
        return Err(io::Error::other(format!(
            "unexpected relay worker readiness: {ready:?}"
        )));
    }
    Ok(BrokerWorker { child, lines })
}

async fn finish_broker_worker(worker: &mut BrokerWorker) -> io::Result<(Usage, bool)> {
    let result = match tokio::time::timeout(Duration::from_secs(2), worker.lines.next_line()).await
    {
        Ok(result) => result?,
        Err(_) => {
            let _ = worker.child.start_kill();
            let _ = worker.child.wait().await;
            return Ok((
                Usage {
                    cpu_us: 0,
                    context_switches: None,
                },
                false,
            ));
        }
    };
    let line = result.ok_or_else(|| io::Error::other("relay worker exited without metrics"))?;
    let fields: Vec<_> = line.split(',').collect();
    if fields.len() != 3 || fields[0] != "RESULT" {
        return Err(io::Error::other(format!(
            "invalid relay worker result: {line}"
        )));
    }
    let cpu_us = fields[1]
        .parse()
        .map_err(|_| io::Error::other("invalid worker CPU metric"))?;
    let context_switches = if fields[2] == "NA" {
        None
    } else {
        Some(
            fields[2]
                .parse()
                .map_err(|_| io::Error::other("invalid worker context-switch metric"))?,
        )
    };
    let status = worker.child.wait().await?;
    if !status.success() {
        return Err(io::Error::other(format!(
            "relay worker exited with {status}"
        )));
    }
    Ok((
        Usage {
            cpu_us,
            context_switches,
        },
        true,
    ))
}

async fn relay_worker_main(args: &[String]) -> io::Result<()> {
    if args.len() != 4 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid relay worker arguments",
        ));
    }
    let topology = match args[0].as_str() {
        "current" => Topology::Current,
        "tuned-64k" => Topology::Tuned,
        #[cfg(target_os = "linux")]
        "splice" => Topology::Splice,
        value => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("invalid relay worker topology: {value}"),
            ))
        }
    };
    let sessions = args[1].parse::<usize>().map_err(|_| {
        io::Error::new(io::ErrorKind::InvalidInput, "invalid relay worker sessions")
    })?;
    let listener = ListenerOptions::new()
        .name(name(&args[2])?)
        .create_tokio()?;
    println!("READY");
    std::io::Write::flush(&mut std::io::stdout())?;
    let before = process_usage()?;
    let case = Case {
        topology,
        workload: Workload::Stdout,
        sessions,
        chunk_bytes: 0,
        bytes_per_direction: 0,
        ping_count: 0,
    };
    let result = accept_broker(listener, case, args[3].clone()).await;
    let after = process_usage()?;
    result?;
    let context_switches = after
        .context_switches
        .zip(before.context_switches)
        .map(|(after, before)| after.saturating_sub(before));
    println!(
        "RESULT,{},{}",
        after.cpu_us.saturating_sub(before.cpu_us),
        context_switches.map_or_else(|| "NA".to_owned(), |value| value.to_string())
    );
    Ok(())
}

async fn run_case(case: Case, case_id: u64) -> io::Result<(Outcome, Duration, Usage, u64, bool)> {
    let daemon_path = endpoint("daemon", case_id);
    let broker_path = endpoint("broker", case_id);
    remove_endpoint(&daemon_path);
    remove_endpoint(&broker_path);
    let _endpoint_guard = EndpointGuard(vec![daemon_path.clone(), broker_path.clone()]);

    let daemon_listener = ListenerOptions::new()
        .name(name(&daemon_path)?)
        .create_tokio()?;
    let mut daemon_task = TaskGuard::new(tokio::spawn(accept_daemon(daemon_listener, case)));
    let mut broker_worker = if case.topology == Topology::Direct {
        None
    } else {
        Some(start_broker_worker(case.topology, case.sessions, &broker_path, &daemon_path).await?)
    };
    // Sample the broker from the parent so RSS instrumentation does not consume
    // CPU in the process whose relay cost is being measured.
    let broker_sampler = broker_worker
        .as_ref()
        .map(|worker| {
            worker
                .child
                .id()
                .map(RssSampler::start)
                .ok_or_else(|| io::Error::other("relay worker PID missing"))
        })
        .transpose()?;

    let started = Instant::now();
    let mut clients = JoinSet::new();
    for session in 0..case.sessions {
        let path = if case.topology == Topology::Direct {
            &daemon_path
        } else {
            &broker_path
        };
        let stream = interprocess::local_socket::tokio::Stream::connect(name(path)?).await?;
        clients.spawn(run_client(stream, case, session));
    }

    let mut combined = Outcome {
        transferred_bytes: 0,
        latencies_us: Vec::new(),
    };
    while let Some(client) = clients.join_next().await {
        let outcome = client.map_err(io::Error::other)??;
        combined.transferred_bytes += outcome.transferred_bytes;
        combined.latencies_us.extend(outcome.latencies_us);
    }
    let elapsed = started.elapsed();
    let mut graceful_teardown = daemon_task.finish().await?;
    let (usage, broker_graceful) = if let Some(worker) = &mut broker_worker {
        finish_broker_worker(worker).await?
    } else {
        (
            Usage {
                cpu_us: 0,
                context_switches: None,
            },
            true,
        )
    };
    let peak_rss = if let Some(sampler) = broker_sampler {
        sampler.finish().await?
    } else {
        0
    };
    graceful_teardown &= broker_graceful;
    Ok((combined, elapsed, usage, peak_rss, graceful_teardown))
}

fn percentile(values: &mut [u64], percentile: usize) -> u64 {
    if values.is_empty() {
        return 0;
    }
    values.sort_unstable();
    let index = ((values.len() - 1) * percentile) / 100;
    values[index]
}

fn sample_peak_rss(keep_sampling: Arc<AtomicBool>, pid: u32) -> u64 {
    use sysinfo::{Pid, System};

    let pid = Pid::from_u32(pid);
    let mut system = System::new();
    let mut peak = 0;
    while keep_sampling.load(Ordering::Relaxed) {
        system.refresh_process(pid);
        if let Some(process) = system.process(pid) {
            peak = peak.max(process.memory());
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    system.refresh_process(pid);
    if let Some(process) = system.process(pid) {
        peak = peak.max(process.memory());
    }
    peak
}

#[cfg(unix)]
fn process_usage() -> io::Result<Usage> {
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::zeroed();
    // SAFETY: getrusage initializes the complete rusage object on success and
    // the pointer is valid for one writable rusage.
    if unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the successful getrusage call above initialized every field.
    let usage = unsafe { usage.assume_init() };
    let timeval_us = |value: libc::timeval| {
        (value.tv_sec as u64)
            .saturating_mul(1_000_000)
            .saturating_add(value.tv_usec as u64)
    };
    Ok(Usage {
        cpu_us: timeval_us(usage.ru_utime).saturating_add(timeval_us(usage.ru_stime)),
        context_switches: Some((usage.ru_nvcsw as u64).saturating_add(usage.ru_nivcsw as u64)),
    })
}

#[cfg(windows)]
fn process_usage() -> io::Result<Usage> {
    use std::mem::MaybeUninit;
    use winapi::shared::minwindef::FILETIME;
    use winapi::um::processthreadsapi::{GetCurrentProcess, GetProcessTimes};

    let mut creation = MaybeUninit::<FILETIME>::zeroed();
    let mut exit = MaybeUninit::<FILETIME>::zeroed();
    let mut kernel = MaybeUninit::<FILETIME>::zeroed();
    let mut user = MaybeUninit::<FILETIME>::zeroed();
    // SAFETY: all four FILETIME pointers are valid and writable, and the
    // pseudo-handle returned by GetCurrentProcess is valid in this process.
    if unsafe {
        GetProcessTimes(
            GetCurrentProcess(),
            creation.as_mut_ptr(),
            exit.as_mut_ptr(),
            kernel.as_mut_ptr(),
            user.as_mut_ptr(),
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let ticks =
        |value: FILETIME| ((value.dwHighDateTime as u64) << 32) | value.dwLowDateTime as u64;
    // SAFETY: GetProcessTimes succeeded and initialized the output FILETIMEs.
    let kernel = unsafe { kernel.assume_init() };
    // SAFETY: same successful GetProcessTimes call as above.
    let user = unsafe { user.assume_init() };
    Ok(Usage {
        cpu_us: ticks(kernel).saturating_add(ticks(user)) / 10,
        // GetProcessTimes exposes CPU time but not context switches. Emit NA
        // rather than conflating an unavailable counter with a real zero.
        context_switches: None,
    })
}

#[cfg(target_os = "linux")]
mod splice_linux {
    use std::io;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

    use interprocess::local_socket::tokio::prelude::*;
    use tokio::io::unix::AsyncFd;

    use super::name;

    fn duplicate(fd: i32) -> io::Result<OwnedFd> {
        // SAFETY: fd is borrowed from a live socket half. fcntl either returns
        // a new independently owned descriptor or a negative error sentinel.
        let duplicated = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 0) };
        if duplicated < 0 {
            Err(io::Error::last_os_error())
        } else {
            // SAFETY: a nonnegative F_DUPFD_CLOEXEC result is a fresh descriptor
            // whose ownership is transferred exactly once to OwnedFd.
            Ok(unsafe { OwnedFd::from_raw_fd(duplicated) })
        }
    }

    fn pipe() -> io::Result<(OwnedFd, OwnedFd)> {
        let mut fds = [-1; 2];
        // SAFETY: fds points to space for the two descriptors pipe2 writes.
        if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) } != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: successful pipe2 initialized two distinct owned descriptors,
        // each transferred exactly once into OwnedFd.
        Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
    }

    fn splice_once(from: i32, to: i32, count: usize) -> io::Result<usize> {
        // SAFETY: from/to are live descriptors, offsets are null as required
        // for pipes/sockets, and count names readable/writable kernel buffers.
        let result = unsafe {
            libc::splice(
                from,
                std::ptr::null_mut(),
                to,
                std::ptr::null_mut(),
                count,
                libc::SPLICE_F_MOVE | libc::SPLICE_F_NONBLOCK,
            )
        };
        if result < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(result as usize)
        }
    }

    async fn one_way<R: std::os::fd::AsFd, W: std::os::fd::AsFd>(
        reader: R,
        writer: W,
    ) -> io::Result<u64> {
        let source = AsyncFd::new(duplicate(reader.as_fd().as_raw_fd())?)?;
        let destination = AsyncFd::new(duplicate(writer.as_fd().as_raw_fd())?)?;
        let (pipe_read, pipe_write) = pipe()?;
        let mut total = 0_u64;

        loop {
            let moved = loop {
                let mut ready = source.readable().await?;
                match ready.try_io(|fd| {
                    splice_once(fd.get_ref().as_raw_fd(), pipe_write.as_raw_fd(), 64 * 1024)
                }) {
                    Ok(result) => break result?,
                    Err(_) => continue,
                }
            };
            if moved == 0 {
                // SAFETY: destination is a duplicated live socket descriptor;
                // shutdown does not take ownership and SHUT_WR is valid.
                unsafe {
                    libc::shutdown(destination.get_ref().as_raw_fd(), libc::SHUT_WR);
                }
                return Ok(total);
            }
            let mut remaining = moved;
            while remaining != 0 {
                let written = loop {
                    let mut ready = destination.writable().await?;
                    match ready.try_io(|fd| {
                        splice_once(pipe_read.as_raw_fd(), fd.get_ref().as_raw_fd(), remaining)
                    }) {
                        Ok(result) => break result?,
                        Err(_) => continue,
                    }
                };
                if written == 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::WriteZero,
                        "splice destination made no progress",
                    ));
                }
                remaining -= written;
                total += written as u64;
            }
        }
    }

    pub async fn relay(
        client: interprocess::local_socket::tokio::Stream,
        daemon_path: &str,
    ) -> io::Result<()> {
        let daemon = interprocess::local_socket::tokio::Stream::connect(name(daemon_path)?).await?;
        let (client_read, client_write) = client.split();
        let (daemon_read, daemon_write) = daemon.split();
        // The portable dispatch enums intentionally omit raw-fd traits. On
        // Unix, extract their sole concrete implementation as documented by
        // interprocess before handing descriptors to splice(2).
        let client_read = match client_read {
            interprocess::local_socket::tokio::RecvHalf::UdSocket(value) => value,
        };
        let client_write = match client_write {
            interprocess::local_socket::tokio::SendHalf::UdSocket(value) => value,
        };
        let daemon_read = match daemon_read {
            interprocess::local_socket::tokio::RecvHalf::UdSocket(value) => value,
        };
        let daemon_write = match daemon_write {
            interprocess::local_socket::tokio::SendHalf::UdSocket(value) => value,
        };
        tokio::try_join!(
            one_way(client_read, daemon_write),
            one_way(daemon_read, client_write)
        )?;
        Ok(())
    }
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> io::Result<()> {
    let raw_args: Vec<String> = std::env::args().collect();
    if raw_args.get(1).map(String::as_str) == Some("__relay-worker") {
        return relay_worker_main(&raw_args[2..]).await;
    }
    let (
        quick,
        smoke,
        topologies,
        selected_sessions,
        selected_workload,
        selected_bytes_mib,
        selected_chunk_kib,
    ) = parse_topologies()?;
    let sessions = if let Some(value) = selected_sessions {
        vec![value]
    } else if smoke {
        vec![1]
    } else {
        vec![1, 16, 64]
    };
    let workloads = if let Some(value) = selected_workload {
        vec![value]
    } else if smoke {
        vec![Workload::Stdout]
    } else {
        vec![Workload::Stdout, Workload::Duplex, Workload::PingPong]
    };
    let chunks = if let Some(value) = selected_chunk_kib {
        vec![value * 1024]
    } else if quick {
        vec![8 * 1024]
    } else {
        vec![8 * 1024, 64 * 1024]
    };
    let bytes_per_direction = if let Some(value) = selected_bytes_mib {
        value * 1024 * 1024
    } else if quick {
        256 * 1024
    } else {
        8 * 1024 * 1024
    };
    let ping_count = if quick { 8 } else { 256 };

    eprintln!(
        "host={} arch={} cpus={} quick={} (Soldr full-proxy protocol unchanged)",
        std::env::consts::OS,
        std::env::consts::ARCH,
        std::thread::available_parallelism().map_or(1, usize::from),
        quick
    );
    println!("platform,topology,workload,sessions,chunk_bytes,bytes,wall_ms,throughput_mib_s,broker_cpu_ms,broker_cpu_per_gib_ms,broker_rss_bytes,broker_context_switches,p50_us,p99_us,graceful_teardown");

    let mut case_id = 0_u64;
    for topology in topologies {
        for &sessions in &sessions {
            for &chunk_bytes in &chunks {
                for &workload in &workloads {
                    if matches!(workload, Workload::PingPong) && chunk_bytes != 8 * 1024 {
                        continue;
                    }
                    case_id += 1;
                    let case = Case {
                        topology,
                        workload,
                        sessions,
                        chunk_bytes,
                        bytes_per_direction,
                        ping_count,
                    };
                    let (mut outcome, elapsed, usage, peak_rss, graceful_teardown) =
                        tokio::time::timeout(Duration::from_secs(120), run_case(case, case_id))
                            .await
                            .map_err(|_| {
                                io::Error::new(
                                    io::ErrorKind::TimedOut,
                                    "benchmark case exceeded 120s",
                                )
                            })??;
                    let seconds = elapsed.as_secs_f64();
                    let gib = outcome.transferred_bytes as f64 / (1024.0 * 1024.0 * 1024.0);
                    let throughput = outcome.transferred_bytes as f64
                        / (1024.0 * 1024.0)
                        / seconds.max(f64::EPSILON);
                    let cpu_per_gib_ms = if outcome.transferred_bytes == 0 {
                        0.0
                    } else {
                        usage.cpu_us as f64 / 1000.0 / gib
                    };
                    let p50 = percentile(&mut outcome.latencies_us, 50);
                    let p99 = percentile(&mut outcome.latencies_us, 99);
                    println!(
                        "{},{},{},{},{},{},{:.3},{:.3},{:.3},{:.3},{},{},{},{},{}",
                        std::env::consts::OS,
                        topology.label(),
                        workload.label(),
                        sessions,
                        chunk_bytes,
                        outcome.transferred_bytes,
                        elapsed.as_secs_f64() * 1000.0,
                        throughput,
                        usage.cpu_us as f64 / 1000.0,
                        cpu_per_gib_ms,
                        peak_rss,
                        usage
                            .context_switches
                            .map_or_else(|| "NA".to_owned(), |value| value.to_string()),
                        p50,
                        p99,
                        graceful_teardown
                    );
                }
            }
        }
    }
    Ok(())
}
