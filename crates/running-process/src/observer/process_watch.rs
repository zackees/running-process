//! Process-watch selectors, provenance, bounded results, and cursor delivery.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, SystemTime};

use running_process_platform_internal::platform::process::{
    exact_trace_capability, ExactTraceEvent, ExactTraceEventKind, NonInvasiveObservationGrade,
    TraceOriginArtifact,
};

const DEFAULT_RETAINED_MATCHES: usize = 256;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ObservationPolicy {
    #[default]
    NonInvasive,
    AllowTracing,
    RequireExact,
}

impl ObservationPolicy {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NonInvasive => "non_invasive",
            Self::AllowTracing => "allow_tracing",
            Self::RequireExact => "require_exact",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ObservationGrade {
    ExactTrace,
    ExactEvent,
    KernelNotification,
    KernelHintReconciled,
    SnapshotInferred,
}

impl ObservationGrade {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ExactTrace => "exact_trace",
            Self::ExactEvent => "exact_event",
            Self::KernelNotification => "kernel_notification",
            Self::KernelHintReconciled => "kernel_hint_reconciled",
            Self::SnapshotInferred => "snapshot_inferred",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StackCapture {
    OriginPreferred,
    OriginRequired,
    OwnerAllThreads,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StackDump {
    pub capture: StackCapture,
    pub directory: Option<PathBuf>,
    pub symbolize_immediately: bool,
}

impl Default for StackDump {
    fn default() -> Self {
        Self {
            capture: StackCapture::OriginPreferred,
            directory: None,
            symbolize_immediately: false,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum WatchSelector {
    Spawn,
    Exec {
        basename: Option<String>,
        path: Option<PathBuf>,
    },
    Exit {
        code: Option<i32>,
        signal: Option<i32>,
        basename: Option<String>,
        failure_only: bool,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessWatch {
    selector: WatchSelector,
    pub dump: Option<StackDump>,
    pub limit: Option<usize>,
    pub cooldown: Duration,
    pub label: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessWatchConfigurationError(pub String);

impl std::fmt::Display for ProcessWatchConfigurationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for ProcessWatchConfigurationError {}

impl ProcessWatch {
    pub fn on_spawn(
        dump: Option<StackDump>,
        limit: Option<usize>,
        cooldown: Duration,
        label: impl Into<String>,
    ) -> Result<Self, ProcessWatchConfigurationError> {
        Self::new(WatchSelector::Spawn, dump, limit, cooldown, label)
    }

    pub fn on_exec(
        basename: Option<String>,
        path: Option<PathBuf>,
        dump: Option<StackDump>,
        limit: Option<usize>,
        cooldown: Duration,
        label: impl Into<String>,
    ) -> Result<Self, ProcessWatchConfigurationError> {
        if basename.is_some() && path.is_some() {
            return Err(ProcessWatchConfigurationError(
                "on_exec accepts basename or path, not both".to_owned(),
            ));
        }
        if basename.as_ref().is_some_and(|name| name.is_empty()) {
            return Err(ProcessWatchConfigurationError(
                "basename must not be empty".to_owned(),
            ));
        }
        Self::new(
            WatchSelector::Exec { basename, path },
            dump,
            limit,
            cooldown,
            label,
        )
    }

    pub fn on_exit(
        code: Option<i32>,
        signal: Option<i32>,
        basename: Option<String>,
        dump: Option<StackDump>,
        limit: Option<usize>,
        cooldown: Duration,
        label: impl Into<String>,
    ) -> Result<Self, ProcessWatchConfigurationError> {
        if code.is_some() && signal.is_some() {
            return Err(ProcessWatchConfigurationError(
                "on_exit accepts code or signal, not both".to_owned(),
            ));
        }
        Self::new(
            WatchSelector::Exit {
                code,
                signal,
                basename,
                failure_only: false,
            },
            dump,
            limit,
            cooldown,
            label,
        )
    }

    pub fn on_failure(
        basename: Option<String>,
        dump: Option<StackDump>,
        limit: Option<usize>,
        cooldown: Duration,
        label: impl Into<String>,
    ) -> Result<Self, ProcessWatchConfigurationError> {
        Self::new(
            WatchSelector::Exit {
                code: None,
                signal: None,
                basename,
                failure_only: true,
            },
            dump,
            limit,
            cooldown,
            label,
        )
    }

    fn new(
        selector: WatchSelector,
        dump: Option<StackDump>,
        limit: Option<usize>,
        cooldown: Duration,
        label: impl Into<String>,
    ) -> Result<Self, ProcessWatchConfigurationError> {
        let label = label.into();
        if label.trim().is_empty() {
            return Err(ProcessWatchConfigurationError(
                "watch label must not be empty".to_owned(),
            ));
        }
        if limit == Some(0) {
            return Err(ProcessWatchConfigurationError(
                "watch limit must be positive or None".to_owned(),
            ));
        }
        if dump
            .as_ref()
            .is_some_and(|request| request.symbolize_immediately)
        {
            return Err(ProcessWatchConfigurationError(
                "immediate remote symbolization is not implemented; use deferred artifacts"
                    .to_owned(),
            ));
        }
        if dump
            .as_ref()
            .is_some_and(|request| request.capture == StackCapture::OwnerAllThreads)
        {
            return Err(ProcessWatchConfigurationError(
                "owner all-thread event-time capture is not implemented".to_owned(),
            ));
        }
        Ok(Self {
            selector,
            dump,
            limit,
            cooldown,
            label,
        })
    }

    fn non_invasive_unsupported_requirement(&self) -> Option<&'static str> {
        if self
            .dump
            .as_ref()
            .is_some_and(|dump| dump.capture != StackCapture::OriginPreferred)
        {
            return Some("the selected stack capture provenance is unavailable non-invasively");
        }
        match &self.selector {
            WatchSelector::Exit {
                code,
                signal,
                basename,
                failure_only,
            } if code.is_some() || signal.is_some() || basename.is_some() || *failure_only => Some(
                "the exit selector needs status or executable fields this backend cannot provide",
            ),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessIdentity {
    pub pid: u32,
    pub start_key: Option<u64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProcessEventKind {
    Spawn,
    Exec,
    Exit,
    Loss,
}

#[derive(Clone, Debug)]
pub struct ProcessEvent {
    pub kind: ProcessEventKind,
    pub process: ProcessIdentity,
    pub parent: Option<ProcessIdentity>,
    pub timestamp: SystemTime,
    pub executable: Option<PathBuf>,
    pub argv: Option<Vec<String>>,
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
    pub raw_exit_status: Option<i64>,
    pub backend: &'static str,
    pub observation_grade: ObservationGrade,
    pub coverage_complete: bool,
    pub loss_detected: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CaptureSource {
    RemoteSpawningThread,
    ManagedSpawnBoundary,
    OwnerEventTimeSnapshot,
    None,
}

impl CaptureSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::RemoteSpawningThread => "remote_spawning_thread",
            Self::ManagedSpawnBoundary => "managed_spawn_boundary",
            Self::OwnerEventTimeSnapshot => "owner_event_time_snapshot",
            Self::None => "none",
        }
    }
}

#[derive(Clone, Debug)]
pub struct DumpResult {
    pub capture_source: CaptureSource,
    pub artifacts: Vec<PathBuf>,
    pub symbolized: bool,
    pub error: Option<String>,
}

#[derive(Clone, Debug)]
pub struct ProcessWatchMatch {
    pub sequence: u64,
    pub watch: ProcessWatch,
    pub event: ProcessEvent,
    pub dump: Option<DumpResult>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessWatchGap {
    pub first_missing: u64,
    pub last_missing: u64,
}

#[derive(Clone, Debug)]
pub struct ProcessWatchLoss {
    pub sequence: u64,
    pub event: ProcessEvent,
    pub reason: String,
}

#[derive(Clone, Debug)]
pub enum ProcessWatchRead {
    Match(Box<ProcessWatchMatch>),
    Loss(Box<ProcessWatchLoss>),
    Gap(ProcessWatchGap),
    Timeout,
    Eof,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessObservationCapabilities {
    pub exact_available: bool,
    pub exact_backend: &'static str,
    pub reason: &'static str,
    pub non_invasive_backend: &'static str,
    pub non_invasive_grade: ObservationGrade,
}

impl ProcessObservationCapabilities {
    pub fn current() -> Self {
        let capability = exact_trace_capability();
        Self {
            exact_available: capability.available,
            exact_backend: capability.backend,
            reason: capability.reason,
            non_invasive_backend: capability.non_invasive_backend,
            non_invasive_grade: match capability.non_invasive_grade {
                NonInvasiveObservationGrade::KernelNotification => {
                    ObservationGrade::KernelNotification
                }
                NonInvasiveObservationGrade::KernelHintReconciled => {
                    ObservationGrade::KernelHintReconciled
                }
                NonInvasiveObservationGrade::SnapshotInferred => ObservationGrade::SnapshotInferred,
            },
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessObservation {
    pub backend: &'static str,
    pub grade: ObservationGrade,
    pub fallback_reason: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessObservationError(pub String);

impl std::fmt::Display for ProcessObservationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for ProcessObservationError {}

struct WatchRuntime {
    watch: ProcessWatch,
    matched: usize,
    last_match: Option<SystemTime>,
}

struct LogState {
    entries: VecDeque<ProcessWatchRecord>,
    first_sequence: u64,
    next_sequence: u64,
    closed: bool,
    coverage_complete: bool,
}

#[derive(Clone, Debug)]
enum ProcessWatchRecord {
    Match(ProcessWatchMatch),
    Loss(ProcessWatchLoss),
}

struct SharedLog {
    state: Mutex<LogState>,
    wake: Condvar,
}

struct PendingMatch {
    watch: ProcessWatch,
    event: ProcessEvent,
    dump_request: Option<StackDump>,
    native: ExactTraceEvent,
}

enum PendingDelivery {
    Match(Box<PendingMatch>),
    Loss { event: ProcessEvent, reason: String },
}

struct PendingOverflow {
    event: ProcessEvent,
    reason: String,
    additional_dropped: usize,
    native_loss_reasons: Vec<String>,
}

impl PendingDelivery {
    fn into_overflow(self) -> PendingOverflow {
        match self {
            Self::Match(pending) => PendingOverflow {
                event: pending.event,
                reason: "process-watch delivery queue overflow".to_owned(),
                additional_dropped: 0,
                native_loss_reasons: Vec::new(),
            },
            Self::Loss { event, reason } => PendingOverflow {
                event,
                reason: "process-watch delivery queue overflow".to_owned(),
                additional_dropped: 0,
                native_loss_reasons: vec![reason],
            },
        }
    }

    fn merge_into(self, overflow: &mut PendingOverflow) {
        overflow.additional_dropped = overflow.additional_dropped.saturating_add(1);
        if let Self::Loss { reason, .. } = self {
            if !overflow.native_loss_reasons.contains(&reason) {
                overflow.native_loss_reasons.push(reason);
            }
        }
    }
}

pub(crate) struct ProcessWatchEmitter {
    watches: Mutex<Vec<WatchRuntime>>,
    log: Arc<SharedLog>,
    observation: ProcessObservation,
    descendant_stop:
        Arc<running_process_platform_internal::platform::process::DescendantMonitorStop>,
    exact_delivery_active: std::sync::atomic::AtomicBool,
    delivery_tx: std::sync::mpsc::SyncSender<PendingDelivery>,
    delivery_overflow: Arc<Mutex<Option<PendingOverflow>>>,
    delivery_closing: Arc<std::sync::atomic::AtomicBool>,
}

impl ProcessWatchEmitter {
    pub(crate) fn new(
        watches: Vec<ProcessWatch>,
        policy: ObservationPolicy,
    ) -> Result<(Arc<Self>, ProcessWatchSubscriber), ProcessObservationError> {
        let capabilities = ProcessObservationCapabilities::current();
        if policy == ObservationPolicy::RequireExact && !capabilities.exact_available {
            return Err(ProcessObservationError(format!(
                "exact process observation is unavailable: {}",
                capabilities.reason
            )));
        }
        let exact = policy != ObservationPolicy::NonInvasive && capabilities.exact_available;
        if !exact {
            for watch in &watches {
                if let Some(requirement) = watch.non_invasive_unsupported_requirement() {
                    return Err(ProcessObservationError(format!(
                        "process watch '{}': {requirement}; select an exact tracing backend",
                        watch.label
                    )));
                }
            }
        }
        let observation = if exact {
            ProcessObservation {
                backend: capabilities.exact_backend,
                grade: ObservationGrade::ExactTrace,
                fallback_reason: None,
            }
        } else {
            let backend = capabilities.non_invasive_backend;
            let grade = capabilities.non_invasive_grade;
            ProcessObservation {
                backend,
                grade,
                fallback_reason: (policy == ObservationPolicy::AllowTracing
                    && !capabilities.exact_available)
                    .then(|| capabilities.reason.to_owned()),
            }
        };
        let log = Arc::new(SharedLog {
            state: Mutex::new(LogState {
                entries: VecDeque::new(),
                first_sequence: 1,
                next_sequence: 1,
                closed: false,
                coverage_complete: true,
            }),
            wake: Condvar::new(),
        });
        let (delivery_tx, delivery_rx) = std::sync::mpsc::sync_channel(DEFAULT_RETAINED_MATCHES);
        let delivery_closing = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let delivery_overflow = Arc::new(Mutex::new(None));
        let worker_log = Arc::clone(&log);
        let worker_closing = Arc::clone(&delivery_closing);
        let worker_overflow = Arc::clone(&delivery_overflow);
        std::thread::Builder::new()
            .name("rp-watch-writer".to_owned())
            .spawn(move || {
                delivery_loop(delivery_rx, worker_log, worker_closing, worker_overflow);
            })
            .map_err(|error| {
                ProcessObservationError(format!("spawn process-watch artifact writer: {error}"))
            })?;
        let emitter = Arc::new(Self {
            watches: Mutex::new(
                watches
                    .into_iter()
                    .map(|watch| WatchRuntime {
                        watch,
                        matched: 0,
                        last_match: None,
                    })
                    .collect(),
            ),
            log: Arc::clone(&log),
            observation: observation.clone(),
            descendant_stop: Arc::new(
                running_process_platform_internal::platform::process::DescendantMonitorStop::new(),
            ),
            exact_delivery_active: std::sync::atomic::AtomicBool::new(false),
            delivery_tx,
            delivery_overflow,
            delivery_closing,
        });
        Ok((emitter, ProcessWatchSubscriber { log, observation }))
    }

    pub(crate) fn uses_exact_trace(&self) -> bool {
        self.observation.grade == ObservationGrade::ExactTrace
    }

    pub(crate) fn emit_exact(&self, native: ExactTraceEvent) {
        if self
            .exact_delivery_active
            .compare_exchange(
                false,
                true,
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
            )
            .is_err()
        {
            let mut log = self.log.state.lock().unwrap_or_else(|e| e.into_inner());
            log.coverage_complete = false;
            return;
        }
        struct DeliveryGuard<'a>(&'a std::sync::atomic::AtomicBool);
        impl Drop for DeliveryGuard<'_> {
            fn drop(&mut self) {
                self.0.store(false, std::sync::atomic::Ordering::Release);
            }
        }
        let _delivery_guard = DeliveryGuard(&self.exact_delivery_active);
        if let ExactTraceEventKind::Loss { reason } = &native.kind {
            let mut log = self.log.state.lock().unwrap_or_else(|e| e.into_inner());
            log.coverage_complete = false;
            drop(log);
            let event = event_from_exact(&native, self.observation.clone(), false);
            self.queue_delivery(PendingDelivery::Loss {
                event,
                reason: reason.clone(),
            });
            return;
        }
        let event = event_from_exact(&native, self.observation.clone(), self.coverage_complete());
        let now = SystemTime::now();
        let mut watches = self.watches.lock().unwrap_or_else(|e| e.into_inner());
        for runtime in &mut *watches {
            if runtime
                .watch
                .limit
                .is_some_and(|limit| runtime.matched >= limit)
                || !selector_matches(&runtime.watch.selector, &event)
                || runtime.last_match.is_some_and(|last| {
                    now.duration_since(last).unwrap_or_default() < runtime.watch.cooldown
                })
            {
                continue;
            }
            runtime.matched += 1;
            runtime.last_match = Some(now);
            let pending = PendingMatch {
                watch: runtime.watch.clone(),
                event: event.clone(),
                dump_request: runtime.watch.dump.clone(),
                native: native.clone(),
            };
            self.queue_delivery(PendingDelivery::Match(Box::new(pending)));
        }
    }

    pub(crate) fn emit_inferred(&self, pid: u32, started: bool) {
        let command_line = super::read_process_cmdline(pid).ok();
        let executable = command_line
            .as_deref()
            .and_then(|line| line.split_ascii_whitespace().next())
            .filter(|value| !value.is_empty())
            .map(PathBuf::from);
        let event = ProcessEvent {
            kind: if started {
                ProcessEventKind::Spawn
            } else {
                ProcessEventKind::Exit
            },
            process: ProcessIdentity {
                pid,
                start_key: None,
            },
            parent: None,
            timestamp: SystemTime::now(),
            executable,
            argv: command_line.map(|line| vec![line]),
            exit_code: None,
            signal: None,
            raw_exit_status: None,
            backend: self.observation.backend,
            observation_grade: self.observation.grade,
            coverage_complete: false,
            loss_detected: false,
        };
        self.emit_inferred_event(event.clone());
        if started {
            self.emit_inferred_event(ProcessEvent {
                kind: ProcessEventKind::Exec,
                ..event
            });
        }
    }

    fn emit_inferred_event(&self, event: ProcessEvent) {
        let mut watches = self.watches.lock().unwrap_or_else(|e| e.into_inner());
        let now = SystemTime::now();
        for runtime in &mut *watches {
            if runtime
                .watch
                .limit
                .is_some_and(|limit| runtime.matched >= limit)
                || !selector_matches(&runtime.watch.selector, &event)
                || runtime.last_match.is_some_and(|last| {
                    now.duration_since(last).unwrap_or_default() < runtime.watch.cooldown
                })
            {
                continue;
            }
            runtime.matched += 1;
            runtime.last_match = Some(now);
            let dump = runtime.watch.dump.as_ref().map(|request| DumpResult {
                capture_source: CaptureSource::None,
                artifacts: Vec::new(),
                symbolized: false,
                error: Some(match request.capture {
                    StackCapture::OwnerAllThreads => {
                        "owner all-thread capture is unavailable from this backend".to_owned()
                    }
                    _ => "origin capture requires an exact trace backend".to_owned(),
                }),
            });
            self.push(runtime.watch.clone(), event.clone(), dump);
        }
    }

    pub(crate) fn descendant_stop(
        &self,
    ) -> Arc<running_process_platform_internal::platform::process::DescendantMonitorStop> {
        Arc::clone(&self.descendant_stop)
    }

    pub(crate) fn close(&self) {
        self.descendant_stop.stop();
        self.finish_delivery();
    }

    pub(crate) fn finish_delivery(&self) {
        self.delivery_closing
            .store(true, std::sync::atomic::Ordering::Release);
        self.log.wake.notify_all();
    }

    fn coverage_complete(&self) -> bool {
        self.log
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .coverage_complete
    }

    fn push(&self, watch: ProcessWatch, event: ProcessEvent, dump: Option<DumpResult>) {
        push_match(&self.log, watch, event, dump);
    }

    fn queue_delivery(&self, delivery: PendingDelivery) {
        // This mutex is an ordering gate, not a blocking delivery path. Once
        // one bounded-channel overflow occurs, producers aggregate subsequent
        // drops here until the worker has drained every earlier accepted item
        // and publishes exactly one ordered loss record.
        let mut overflow = self
            .delivery_overflow
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Some(pending) = overflow.as_mut() {
            delivery.merge_into(pending);
            self.mark_coverage_incomplete();
            return;
        }
        match self.delivery_tx.try_send(delivery) {
            Ok(()) => {}
            Err(std::sync::mpsc::TrySendError::Full(delivery)) => {
                *overflow = Some(delivery.into_overflow());
                self.mark_coverage_incomplete();
            }
            Err(std::sync::mpsc::TrySendError::Disconnected(delivery)) => {
                self.mark_coverage_incomplete();
                let pending = delivery.into_overflow();
                push_loss(&self.log, pending.event, pending.reason);
            }
        }
    }

    fn mark_coverage_incomplete(&self) {
        self.log
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .coverage_complete = false;
    }
}

fn delivery_loop(
    receiver: std::sync::mpsc::Receiver<PendingDelivery>,
    log: Arc<SharedLog>,
    closing: Arc<std::sync::atomic::AtomicBool>,
    overflow: Arc<Mutex<Option<PendingOverflow>>>,
) {
    loop {
        match receiver.recv_timeout(Duration::from_millis(10)) {
            Ok(PendingDelivery::Match(pending)) => {
                let pending = *pending;
                let dump = pending
                    .dump_request
                    .as_ref()
                    .map(|request| write_dump(request, &pending.watch.label, &pending.native));
                push_match(&log, pending.watch, pending.event, dump);
            }
            Ok(PendingDelivery::Loss { event, reason }) => {
                push_loss(&log, event, reason);
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                let pending = overflow
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .take();
                if let Some(mut pending) = pending {
                    if pending.additional_dropped != 0 {
                        pending.reason.push_str(&format!(
                            "; {} additional deliveries dropped",
                            pending.additional_dropped
                        ));
                    }
                    for reason in pending.native_loss_reasons {
                        pending.reason.push_str("; native trace loss: ");
                        pending.reason.push_str(&reason);
                    }
                    push_loss(&log, pending.event, pending.reason);
                }
                if closing.load(std::sync::atomic::Ordering::Acquire) {
                    let mut state = log.state.lock().unwrap_or_else(|e| e.into_inner());
                    state.closed = true;
                    log.wake.notify_all();
                    return;
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                let mut state = log.state.lock().unwrap_or_else(|e| e.into_inner());
                state.closed = true;
                log.wake.notify_all();
                return;
            }
        }
    }
}

fn push_loss(log: &SharedLog, event: ProcessEvent, reason: String) {
    let mut state = log.state.lock().unwrap_or_else(|e| e.into_inner());
    if state.closed {
        return;
    }
    let sequence = state.next_sequence;
    state.next_sequence += 1;
    state
        .entries
        .push_back(ProcessWatchRecord::Loss(ProcessWatchLoss {
            sequence,
            event,
            reason,
        }));
    trim_log(&mut state);
    log.wake.notify_all();
}

fn push_match(log: &SharedLog, watch: ProcessWatch, event: ProcessEvent, dump: Option<DumpResult>) {
    let mut state = log.state.lock().unwrap_or_else(|e| e.into_inner());
    if state.closed {
        return;
    }
    let sequence = state.next_sequence;
    state.next_sequence += 1;
    state
        .entries
        .push_back(ProcessWatchRecord::Match(ProcessWatchMatch {
            sequence,
            watch,
            event,
            dump,
        }));
    trim_log(&mut state);
    log.wake.notify_all();
}

fn trim_log(log: &mut LogState) {
    while log.entries.len() > DEFAULT_RETAINED_MATCHES {
        log.entries.pop_front();
        log.first_sequence += 1;
    }
}

pub struct ProcessWatchSubscriber {
    log: Arc<SharedLog>,
    observation: ProcessObservation,
}

impl ProcessWatchSubscriber {
    pub fn observation(&self) -> &ProcessObservation {
        &self.observation
    }

    pub fn snapshot(&self) -> Vec<ProcessWatchMatch> {
        self.log
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entries
            .iter()
            .filter_map(|record| match record {
                ProcessWatchRecord::Match(item) => Some(item.clone()),
                ProcessWatchRecord::Loss(_) => None,
            })
            .collect()
    }

    pub fn cursor(&self) -> ProcessWatchCursor {
        let next_sequence = self
            .log
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .first_sequence;
        ProcessWatchCursor {
            log: Arc::clone(&self.log),
            next_sequence,
        }
    }
}

pub struct ProcessWatchCursor {
    log: Arc<SharedLog>,
    next_sequence: u64,
}

impl ProcessWatchCursor {
    pub fn read_next(&mut self, timeout: Option<Duration>) -> ProcessWatchRead {
        let mut state = self.log.state.lock().unwrap_or_else(|e| e.into_inner());
        let deadline = timeout.map(|duration| std::time::Instant::now() + duration);
        loop {
            if self.next_sequence < state.first_sequence {
                let gap = ProcessWatchGap {
                    first_missing: self.next_sequence,
                    last_missing: state.first_sequence - 1,
                };
                self.next_sequence = state.first_sequence;
                return ProcessWatchRead::Gap(gap);
            }
            if self.next_sequence < state.next_sequence {
                let index = (self.next_sequence - state.first_sequence) as usize;
                let item = state.entries[index].clone();
                self.next_sequence += 1;
                return match item {
                    ProcessWatchRecord::Match(item) => ProcessWatchRead::Match(Box::new(item)),
                    ProcessWatchRecord::Loss(item) => ProcessWatchRead::Loss(Box::new(item)),
                };
            }
            if state.closed {
                return ProcessWatchRead::Eof;
            }
            state = if let Some(deadline) = deadline {
                let Some(remaining) = deadline.checked_duration_since(std::time::Instant::now())
                else {
                    return ProcessWatchRead::Timeout;
                };
                let (state, result) = self
                    .log
                    .wake
                    .wait_timeout(state, remaining)
                    .unwrap_or_else(|e| e.into_inner());
                if result.timed_out() {
                    return ProcessWatchRead::Timeout;
                }
                state
            } else {
                self.log.wake.wait(state).unwrap_or_else(|e| e.into_inner())
            };
        }
    }
}

fn event_from_exact(
    native: &ExactTraceEvent,
    observation: ProcessObservation,
    coverage_complete: bool,
) -> ProcessEvent {
    let (kind, exit_code, signal, raw_exit_status, loss_detected) = match &native.kind {
        ExactTraceEventKind::Spawn => (ProcessEventKind::Spawn, None, None, None, false),
        ExactTraceEventKind::Exec => (ProcessEventKind::Exec, None, None, None, false),
        ExactTraceEventKind::Exit {
            exit_code,
            signal,
            raw_status,
        } => (
            ProcessEventKind::Exit,
            *exit_code,
            *signal,
            Some(*raw_status),
            false,
        ),
        ExactTraceEventKind::Loss { .. } => (ProcessEventKind::Loss, None, None, None, true),
    };
    ProcessEvent {
        kind,
        process: ProcessIdentity {
            pid: native.pid,
            start_key: native.start_key,
        },
        parent: native.parent_pid.map(|pid| ProcessIdentity {
            pid,
            start_key: native.parent_start_key,
        }),
        timestamp: native.timestamp,
        executable: native.executable.clone(),
        argv: native.argv.as_ref().map(|args| {
            args.iter()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect()
        }),
        exit_code,
        signal,
        raw_exit_status,
        backend: observation.backend,
        observation_grade: observation.grade,
        coverage_complete,
        loss_detected,
    }
}

fn selector_matches(selector: &WatchSelector, event: &ProcessEvent) -> bool {
    match selector {
        WatchSelector::Spawn => event.kind == ProcessEventKind::Spawn,
        WatchSelector::Exec { basename, path } => {
            event.kind == ProcessEventKind::Exec
                && executable_matches(
                    event.executable.as_deref(),
                    basename.as_deref(),
                    path.as_deref(),
                )
        }
        WatchSelector::Exit {
            code,
            signal,
            basename,
            failure_only,
        } => {
            event.kind == ProcessEventKind::Exit
                && executable_matches(event.executable.as_deref(), basename.as_deref(), None)
                && code.is_none_or(|wanted| exit_code_matches(wanted, event.exit_code))
                && signal.is_none_or(|wanted| event.signal == Some(wanted))
                && (!failure_only
                    || event.signal.is_some()
                    || event.exit_code.is_some_and(|c| c != 0))
        }
    }
}

fn executable_matches(
    executable: Option<&Path>,
    basename: Option<&str>,
    exact_path: Option<&Path>,
) -> bool {
    if let Some(expected) = exact_path {
        return executable == Some(expected);
    }
    if let Some(expected) = basename {
        return executable
            .and_then(Path::file_name)
            .and_then(|name| name.to_str())
            == Some(expected);
    }
    true
}

fn exit_code_matches(requested: i32, observed: Option<i32>) -> bool {
    if requested == -1 {
        return observed == Some(255) || observed == Some(-1);
    }
    observed == Some(requested)
}

fn write_dump(request: &StackDump, label: &str, event: &ExactTraceEvent) -> DumpResult {
    if request.capture == StackCapture::OwnerAllThreads {
        return DumpResult {
            capture_source: CaptureSource::None,
            artifacts: Vec::new(),
            symbolized: false,
            error: Some("owner all-thread capture is unavailable for this event".to_owned()),
        };
    }
    let Some(origin) = event.origin.as_ref() else {
        return DumpResult {
            capture_source: CaptureSource::None,
            artifacts: Vec::new(),
            symbolized: false,
            error: Some(match request.capture {
                StackCapture::OwnerAllThreads => {
                    "owner all-thread capture is unavailable for this event".to_owned()
                }
                _ => "spawning-thread origin capture is unavailable for this event".to_owned(),
            }),
        };
    };
    let directory = request
        .directory
        .clone()
        .unwrap_or_else(|| std::env::temp_dir().join("running-process-watch"));
    if let Err(error) = std::fs::create_dir_all(&directory) {
        return dump_error(error);
    }
    let safe_label: String = label
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                character
            } else {
                '_'
            }
        })
        .collect();
    let path = directory.join(format!(
        "{safe_label}-{}-{}.rpstack",
        event.pid, event.sequence
    ));
    let bytes = render_origin(origin);
    if let Err(error) = std::fs::write(&path, bytes) {
        return dump_error(error);
    }
    DumpResult {
        capture_source: CaptureSource::RemoteSpawningThread,
        artifacts: vec![path],
        symbolized: false,
        error: request.symbolize_immediately.then(|| {
            "immediate remote symbolization is unavailable; retained a deferred raw artifact"
                .to_owned()
        }),
    }
}

fn dump_error(error: std::io::Error) -> DumpResult {
    DumpResult {
        capture_source: CaptureSource::None,
        artifacts: Vec::new(),
        symbolized: false,
        error: Some(error.to_string()),
    }
}

fn render_origin(origin: &TraceOriginArtifact) -> Vec<u8> {
    let mut text = format!(
        "format=running-process-origin-v2\norigin_pid={}\nthread_id={}\narchitecture={}\nregister_format={}\norigin_executable={:?}\nstack_pointer={:?}\ninstruction_pointer={:?}\nregister_bytes={}\nstack_bytes={}\nstack_truncated={}\nmodule_map_bytes={}\nmodule_map_truncated={}\nregisters=",
        origin.origin_pid,
        origin.thread_id,
        origin.architecture,
        origin.register_format,
        origin.executable,
        origin.stack_pointer,
        origin.instruction_pointer,
        origin.registers.len(),
        origin.stack.len(),
        origin.truncated,
        origin.module_map.len(),
        origin.module_map_truncated,
    );
    push_hex(&mut text, &origin.registers);
    text.push_str("\nstack=");
    push_hex(&mut text, &origin.stack);
    text.push_str("\nmodule_map=");
    push_hex(&mut text, &origin.module_map);
    text.push('\n');
    text.into_bytes()
}

fn push_hex(output: &mut String, bytes: &[u8]) {
    use std::fmt::Write;
    for byte in bytes {
        let _ = write!(output, "{byte:02x}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minus_one_matches_unix_truncated_status() {
        assert!(exit_code_matches(-1, Some(255)));
        assert!(exit_code_matches(-1, Some(-1)));
        assert!(!exit_code_matches(-1, Some(254)));
        assert!(!exit_code_matches(-1, None));
    }

    #[test]
    fn ambiguous_exec_selector_is_rejected() {
        assert!(ProcessWatch::on_exec(
            Some("soldr".to_owned()),
            Some(PathBuf::from("/usr/bin/soldr")),
            None,
            Some(1),
            Duration::ZERO,
            "recursive-soldr",
        )
        .is_err());
    }

    #[test]
    fn bounded_log_reports_an_explicit_cursor_gap() {
        let watch = ProcessWatch::on_spawn(None, None, Duration::ZERO, "all-spawns").unwrap();
        let (emitter, subscriber) =
            ProcessWatchEmitter::new(vec![watch], ObservationPolicy::NonInvasive).unwrap();
        let mut cursor = subscriber.cursor();
        for pid in 1..=(DEFAULT_RETAINED_MATCHES as u32 + 7) {
            emitter.emit_inferred(pid, true);
        }
        let ProcessWatchRead::Gap(gap) = cursor.read_next(Some(Duration::ZERO)) else {
            panic!("expected an explicit cursor gap");
        };
        assert_eq!(gap.first_missing, 1);
        assert_eq!(gap.last_missing, 7);
    }

    #[test]
    fn limit_and_cooldown_bound_matching() {
        let limited = ProcessWatch::on_spawn(None, Some(2), Duration::ZERO, "limited").unwrap();
        let cooled = ProcessWatch::on_spawn(None, None, Duration::from_secs(60), "cooled").unwrap();
        let (emitter, subscriber) =
            ProcessWatchEmitter::new(vec![limited, cooled], ObservationPolicy::NonInvasive)
                .unwrap();
        for pid in 1..=4 {
            emitter.emit_inferred(pid, true);
        }
        let matches = subscriber.snapshot();
        assert_eq!(
            matches
                .iter()
                .filter(|item| item.watch.label == "limited")
                .count(),
            2
        );
        assert_eq!(
            matches
                .iter()
                .filter(|item| item.watch.label == "cooled")
                .count(),
            1
        );
    }

    #[test]
    fn native_loss_is_delivered_without_a_matching_selector() {
        let watch = ProcessWatch::on_spawn(None, Some(1), Duration::ZERO, "spawn").unwrap();
        let (emitter, subscriber) =
            ProcessWatchEmitter::new(vec![watch], ObservationPolicy::NonInvasive).unwrap();
        let mut cursor = subscriber.cursor();
        emitter.emit_exact(ExactTraceEvent {
            sequence: 7,
            pid: 42,
            parent_pid: None,
            parent_start_key: None,
            start_key: Some(11),
            timestamp: SystemTime::now(),
            kind: ExactTraceEventKind::Loss {
                reason: "resume failed".to_owned(),
            },
            executable: None,
            argv: None,
            origin: None,
        });
        let ProcessWatchRead::Loss(loss) = cursor.read_next(Some(Duration::from_secs(1))) else {
            panic!("expected an explicit loss record");
        };
        assert_eq!(loss.reason, "resume failed");
        assert!(loss.event.loss_detected);
        assert!(!loss.event.coverage_complete);
    }
}
