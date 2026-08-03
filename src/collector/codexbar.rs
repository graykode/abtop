//! Optional, bounded CodexBar quota provider.
//!
//! The poller asks CodexBar for its configured providers and retains only
//! canonical provider IDs plus bounded quota-window metadata. Account identity,
//! credits, pace, dashboard data, raw provider failures, and unknown fields are
//! discarded before a snapshot enters application state.

use crate::model::{RateLimitProvenance, RateLimitWindow};
#[cfg(test)]
use crate::model::{MAX_RATE_LIMIT_WINDOW_ID_BYTES, MAX_RATE_LIMIT_WINDOW_LABEL_BYTES};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const POLL_INTERVAL: Duration = Duration::from_secs(60);
const COMMAND_TIMEOUT: Duration = Duration::from_secs(15);
const OUTPUT_DRAIN_TIMEOUT: Duration = Duration::from_millis(100);
const CHILD_REAP_TIMEOUT: Duration = Duration::from_millis(100);
#[cfg(windows)]
const READER_CANCEL_TIMEOUT: Duration = Duration::from_millis(100);
const MAX_OUTPUT: usize = 1024 * 1024;
const MAX_PROVIDERS: usize = 64;
const MAX_WINDOWS_PER_PROVIDER: usize = 32;
const MAX_PROVIDER_ID_BYTES: usize = 64;
const CODEXBAR_ARGS: [&str; 5] = ["usage", "--format", "json", "--json-only", "--no-color"];

/// Content-free state of the optional CodexBar quota poller.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CodexBarQuotaState {
    Off,
    Checking,
    Available,
    Partial,
    Unavailable,
}

/// Sanitized failure category. Provider output and OS error text are never retained.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CodexBarPollError {
    NotRunnable,
    TimedOut,
    OutputTooLarge,
    ProcessFailed,
    InvalidResponse,
    UnsupportedResponse,
    Cancelled,
    InternalError,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub(crate) struct CodexBarQuotaStatus {
    pub(crate) state: CodexBarQuotaState,
    pub(crate) last_checked_at: Option<u64>,
    pub(crate) error: Option<CodexBarPollError>,
}

/// Sanitized failure for one provider row. CodexBar's raw error is discarded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CodexBarProviderError {
    Unavailable,
    InvalidResponse,
    DuplicateProvider,
    TooManyWindows,
}

/// One bounded quota window returned by CodexBar.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CodexBarWindow {
    pub(crate) id: String,
    pub(crate) label: String,
    pub(crate) used_pct: f64,
    pub(crate) resets_at: Option<u64>,
    pub(crate) window_minutes: Option<u64>,
}

/// Sanitized result for one configured CodexBar provider.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CodexBarProviderSnapshot {
    pub(crate) provider: String,
    pub(crate) windows: Vec<CodexBarWindow>,
    pub(crate) updated_at: Option<u64>,
    pub(crate) error: Option<CodexBarProviderError>,
}

impl CodexBarProviderSnapshot {
    fn failure(provider: String, error: CodexBarProviderError) -> Self {
        Self {
            provider,
            windows: Vec::new(),
            updated_at: None,
            error: Some(error),
        }
    }

    pub(crate) fn is_available(&self) -> bool {
        self.error.is_none() && !self.windows.is_empty()
    }
}

/// One atomic CodexBar poll. Response order is retained; consumers may sort.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct CodexBarSnapshot {
    pub(crate) providers: Vec<CodexBarProviderSnapshot>,
}

impl CodexBarSnapshot {
    fn state(&self) -> CodexBarQuotaState {
        let available = self
            .providers
            .iter()
            .filter(|provider| provider.is_available())
            .count();
        let unavailable = self.providers.len().saturating_sub(available);
        match (available, unavailable) {
            (0, _) => CodexBarQuotaState::Unavailable,
            (_, 0) => CodexBarQuotaState::Available,
            _ => CodexBarQuotaState::Partial,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawCodexBarWindow {
    used_percent: f64,
    #[serde(default)]
    window_minutes: Option<u64>,
    #[serde(default)]
    resets_at: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawExtraWindow {
    id: String,
    #[serde(default)]
    title: Option<String>,
    window: RawCodexBarWindow,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TypedCodexBarUsage {
    #[serde(default)]
    primary: Option<RawCodexBarWindow>,
    #[serde(default)]
    secondary: Option<RawCodexBarWindow>,
    #[serde(default)]
    tertiary: Option<RawCodexBarWindow>,
    #[serde(default)]
    extra_rate_windows: Option<Vec<RawExtraWindow>>,
    #[serde(default)]
    updated_at: Option<String>,
}

struct PollResult {
    generation: u64,
    result: Result<CodexBarSnapshot, CodexBarPollError>,
}

struct CommandOutput {
    stdout: Vec<u8>,
    success: bool,
}

#[derive(Debug)]
struct ChildProcessSlot {
    generation: u64,
    pid: Option<u32>,
    #[cfg(windows)]
    job: Option<Arc<WindowsCommandJob>>,
    #[cfg(windows)]
    process: Option<Arc<WindowsProcessHandle>>,
}

impl ChildProcessSlot {
    fn new(generation: u64) -> Self {
        Self {
            generation,
            pid: None,
            #[cfg(windows)]
            job: None,
            #[cfg(windows)]
            process: None,
        }
    }
}

#[derive(Debug, Clone)]
struct ChildProcessHandle {
    pid: u32,
    #[cfg(windows)]
    job: Option<Arc<WindowsCommandJob>>,
    #[cfg(windows)]
    process: Option<Arc<WindowsProcessHandle>>,
}

impl ChildProcessHandle {
    fn for_child(child: &Child) -> Self {
        Self {
            pid: child.id(),
            #[cfg(windows)]
            job: WindowsCommandJob::assign(child).map(Arc::new),
            #[cfg(windows)]
            process: WindowsProcessHandle::duplicate(child).map(Arc::new),
        }
    }

    fn terminate(&self) {
        #[cfg(windows)]
        {
            if let Some(job) = &self.job {
                job.terminate();
            }
            if let Some(process) = &self.process {
                process.terminate();
            }
        }
        #[cfg(not(windows))]
        terminate_pid(self.pid);
    }

    fn can_register(&self) -> bool {
        #[cfg(windows)]
        {
            self.process.is_some()
        }
        #[cfg(not(windows))]
        {
            true
        }
    }
}

struct ChildProcessRegistration {
    slot: Arc<Mutex<ChildProcessSlot>>,
    generation: u64,
    pid: u32,
}

impl ChildProcessRegistration {
    fn claim(
        slot: &Arc<Mutex<ChildProcessSlot>>,
        generation: u64,
        child: &ChildProcessHandle,
    ) -> Option<Self> {
        if !claim_child_process(slot, generation, child) {
            return None;
        }
        Some(Self {
            slot: slot.clone(),
            generation,
            pid: child.pid,
        })
    }
}

impl Drop for ChildProcessRegistration {
    fn drop(&mut self) {
        release_child_process(&self.slot, self.generation, self.pid);
    }
}

/// Runs one nonblocking child observation while holding the registration lock.
///
/// A successful result means the observation reaped the direct child. Removing
/// the matching slot before releasing the lock prevents a concurrent setting
/// toggle from signalling a newly reused numeric PID. The caller's independent
/// Windows Job handle remains available for exact descendant cleanup.
fn observe_registered_child<T, E>(
    registration: &mut Option<ChildProcessRegistration>,
    observe: impl FnOnce() -> Result<Option<T>, E>,
) -> Result<Option<T>, E> {
    let Some(current) = registration.as_ref() else {
        return observe();
    };
    let slot = current.slot.clone();
    let generation = current.generation;
    let pid = current.pid;
    let (result, _released) = {
        let mut slot = child_slot_guard(&slot);
        let result = observe()?;
        let released = if result.is_some() && slot.generation == generation && slot.pid == Some(pid)
        {
            take_slot_child(&mut slot)
        } else {
            None
        };
        (result, released)
    };
    if result.is_some() {
        // The locked path already removed the exact registration. Dropping the
        // guard afterward is intentionally a harmless second validation.
        drop(registration.take());
    }
    Ok(result)
}

#[cfg(unix)]
fn observe_unreaped_unix_child(
    registration: &mut Option<ChildProcessRegistration>,
    process: &ChildProcessHandle,
) -> Result<bool, CodexBarPollError> {
    let Some(current) = registration.as_ref() else {
        return child_exited_without_reaping(process.pid);
    };
    let slot = current.slot.clone();
    let generation = current.generation;
    let pid = current.pid;
    let (exited, _released) = {
        let mut slot = child_slot_guard(&slot);
        let exited = child_exited_without_reaping(pid)?;
        let released = if exited && slot.generation == generation && slot.pid == Some(pid) {
            // The zombie leader still reserves the PGID. Signal and remove the
            // numeric registration under the same lock so a toggle can never
            // carry this PGID past the worker's later reap.
            process.terminate();
            take_slot_child(&mut slot)
        } else {
            None
        };
        (exited, released)
    };
    if exited {
        drop(registration.take());
    }
    Ok(exited)
}

#[cfg(unix)]
fn signal_and_release_registered_unix_child(
    registration: &mut Option<ChildProcessRegistration>,
    process: &ChildProcessHandle,
) {
    let Some(current) = registration.as_ref() else {
        // No shared numeric identity was published, so only this worker can
        // transition the unreaped child and its process group.
        process.terminate();
        return;
    };
    let slot = current.slot.clone();
    let generation = current.generation;
    let pid = current.pid;
    let _released = {
        let mut slot = child_slot_guard(&slot);
        if slot.generation == generation && slot.pid == Some(pid) {
            process.terminate();
            take_slot_child(&mut slot)
        } else {
            // A generation rotation already removed and signalled it while
            // holding this same lock.
            None
        }
    };
    drop(registration.take());
}

fn child_slot_guard(slot: &Mutex<ChildProcessSlot>) -> std::sync::MutexGuard<'_, ChildProcessSlot> {
    slot.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn take_slot_child(slot: &mut ChildProcessSlot) -> Option<ChildProcessHandle> {
    let pid = slot.pid.take();
    #[cfg(windows)]
    let job = slot.job.take();
    #[cfg(windows)]
    let process = slot.process.take();
    pid.map(|pid| ChildProcessHandle {
        pid,
        #[cfg(windows)]
        job,
        #[cfg(windows)]
        process,
    })
}

fn child_generation_matches(slot: &Mutex<ChildProcessSlot>, generation: u64) -> bool {
    child_slot_guard(slot).generation == generation
}

fn claim_child_process(
    slot: &Mutex<ChildProcessSlot>,
    generation: u64,
    child: &ChildProcessHandle,
) -> bool {
    if child.pid == 0 || !child.can_register() {
        return false;
    }
    let mut slot = child_slot_guard(slot);
    if slot.generation != generation || slot.pid.is_some() {
        return false;
    }
    slot.pid = Some(child.pid);
    #[cfg(windows)]
    {
        slot.job = child.job.clone();
        slot.process = child.process.clone();
    }
    true
}

fn release_child_process(slot: &Mutex<ChildProcessSlot>, generation: u64, pid: u32) -> bool {
    let released = {
        let mut slot = child_slot_guard(slot);
        if slot.generation == generation && slot.pid == Some(pid) {
            take_slot_child(&mut slot)
        } else {
            None
        }
    };
    // On Windows this can close the final kill-on-close job handle. Keep that
    // operation outside the slot lock.
    released.is_some()
}

fn rotate_child_generation(
    slot: &Mutex<ChildProcessSlot>,
    generation: u64,
) -> Option<ChildProcessHandle> {
    let mut slot = child_slot_guard(slot);
    let child = take_slot_child(&mut slot);
    #[cfg(unix)]
    if let Some(child) = &child {
        // The worker cannot reap while this lock is held. Signal the process
        // group before releasing its numeric identity.
        child.terminate();
    }
    slot.generation = generation;
    #[cfg(unix)]
    {
        None
    }
    #[cfg(not(unix))]
    {
        child
    }
}

pub(crate) struct CodexBarQuotaPoller {
    enabled: bool,
    generation: u64,
    cached: Option<CodexBarSnapshot>,
    in_flight: Option<u64>,
    last_started: Option<Instant>,
    last_checked_at: Option<u64>,
    last_error: Option<CodexBarPollError>,
    child_process: Arc<Mutex<ChildProcessSlot>>,
    tx: mpsc::Sender<PollResult>,
    rx: mpsc::Receiver<PollResult>,
}

impl CodexBarQuotaPoller {
    pub(crate) fn new(enabled: bool) -> Self {
        let (tx, rx) = mpsc::channel();
        Self {
            enabled,
            generation: 0,
            cached: None,
            in_flight: None,
            last_started: None,
            last_checked_at: None,
            last_error: None,
            child_process: Arc::new(Mutex::new(ChildProcessSlot::new(0))),
            tx,
            rx,
        }
    }

    pub(crate) fn set_enabled(&mut self, enabled: bool) {
        if self.enabled == enabled {
            return;
        }
        self.enabled = enabled;
        self.generation = self.generation.wrapping_add(1);
        let child = rotate_child_generation(&self.child_process, self.generation);
        self.cached = None;
        self.last_started = None;
        self.last_checked_at = None;
        self.last_error = None;
        self.in_flight = None;
        if let Some(child) = child {
            child.terminate();
        }
    }

    pub(crate) fn status(&self) -> CodexBarQuotaStatus {
        let state = if !self.enabled {
            CodexBarQuotaState::Off
        } else if self.in_flight.is_some() {
            CodexBarQuotaState::Checking
        } else {
            let cached_state = self
                .cached
                .as_ref()
                .map(CodexBarSnapshot::state)
                .unwrap_or(CodexBarQuotaState::Unavailable);
            if self.last_error.is_some()
                && matches!(
                    cached_state,
                    CodexBarQuotaState::Available | CodexBarQuotaState::Partial
                )
            {
                CodexBarQuotaState::Partial
            } else {
                cached_state
            }
        };
        CodexBarQuotaStatus {
            state,
            last_checked_at: self.last_checked_at,
            error: self.last_error,
        }
    }

    pub(crate) fn update(&mut self) -> Option<CodexBarSnapshot> {
        self.poll_completed();
        if self.should_start() {
            self.start();
        }
        self.cached.clone()
    }

    pub(crate) fn wait_for_initial(&mut self, timeout: Duration) -> Option<CodexBarSnapshot> {
        self.update();
        if self.cached.is_some() || self.in_flight.is_none() {
            return self.cached.clone();
        }
        let deadline = Instant::now() + timeout;
        while self.in_flight.is_some() {
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                break;
            };
            match self
                .rx
                .recv_timeout(remaining.min(Duration::from_millis(100)))
            {
                Ok(result) => self.apply_result(result),
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    self.in_flight = None;
                    self.last_checked_at = Some(now_secs());
                    self.last_error = Some(CodexBarPollError::InternalError);
                    break;
                }
            }
        }
        self.cached.clone()
    }

    fn poll_completed(&mut self) {
        loop {
            match self.rx.try_recv() {
                Ok(result) => self.apply_result(result),
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    if self.in_flight.take().is_some() {
                        self.last_checked_at = Some(now_secs());
                        self.last_error = Some(CodexBarPollError::InternalError);
                    }
                    break;
                }
            }
        }
    }

    fn apply_result(&mut self, result: PollResult) {
        if self.in_flight == Some(result.generation) {
            self.in_flight = None;
        }
        if self.enabled && result.generation == self.generation {
            self.last_checked_at = Some(now_secs());
            match result.result {
                Ok(snapshot) => {
                    // A structurally valid poll atomically replaces the complete
                    // configured-provider set. Provider failures therefore clear
                    // old values for that provider instead of leaving ghosts.
                    self.cached = Some(snapshot);
                    self.last_error = None;
                }
                Err(error) => {
                    // Transport and envelope failures retain the prior bounded
                    // snapshot as stale data, while exposing only the category.
                    self.last_error = Some(error);
                }
            }
        }
    }

    fn should_start(&self) -> bool {
        self.enabled
            && self.in_flight.is_none()
            && self
                .last_started
                .is_none_or(|started| started.elapsed() >= POLL_INTERVAL)
    }

    fn start(&mut self) {
        let generation = self.generation;
        self.in_flight = Some(generation);
        self.last_started = Some(Instant::now());
        let tx = self.tx.clone();
        let child_process = self.child_process.clone();
        std::thread::spawn(move || {
            let result = run_codexbar(&child_process, generation).and_then(parse_command_output);
            let _ = tx.send(PollResult { generation, result });
        });
    }
}

impl Drop for CodexBarQuotaPoller {
    fn drop(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        let child = rotate_child_generation(&self.child_process, self.generation);
        if let Some(child) = child {
            child.terminate();
        }
    }
}

fn parse_command_output(output: CommandOutput) -> Result<CodexBarSnapshot, CodexBarPollError> {
    match parse_codexbar(&output.stdout) {
        Ok(snapshot) if output.success || !snapshot.providers.is_empty() => Ok(snapshot),
        Ok(_) => Err(CodexBarPollError::ProcessFailed),
        Err(_) if !output.success => Err(CodexBarPollError::ProcessFailed),
        Err(error) => Err(error),
    }
}

fn parse_codexbar(bytes: &[u8]) -> Result<CodexBarSnapshot, CodexBarPollError> {
    if bytes.len() > MAX_OUTPUT {
        return Err(CodexBarPollError::OutputTooLarge);
    }
    let envelope: Value =
        serde_json::from_slice(bytes).map_err(|_| CodexBarPollError::InvalidResponse)?;
    let rows = match envelope {
        Value::Array(rows) => rows,
        Value::Object(_) => vec![envelope],
        _ => return Err(CodexBarPollError::UnsupportedResponse),
    };
    if rows.len() > MAX_PROVIDERS {
        return Err(CodexBarPollError::UnsupportedResponse);
    }
    let response_was_empty = rows.is_empty();

    let mut providers = Vec::with_capacity(rows.len());
    let mut indexes = HashMap::<String, usize>::new();
    for row in rows {
        let Some(provider) = row
            .get("provider")
            .and_then(Value::as_str)
            .and_then(canonical_provider_id)
        else {
            // Without a safe stable provider identity there is no row that can
            // be exposed or atomically cleared. Other valid providers survive.
            continue;
        };

        if let Some(index) = indexes.get(&provider).copied() {
            providers[index] = CodexBarProviderSnapshot::failure(
                provider,
                CodexBarProviderError::DuplicateProvider,
            );
            continue;
        }

        let snapshot = parse_provider_row(provider.clone(), &row);
        indexes.insert(provider, providers.len());
        providers.push(snapshot);
    }

    if !providers.is_empty() || response_was_empty {
        Ok(CodexBarSnapshot { providers })
    } else {
        Err(CodexBarPollError::InvalidResponse)
    }
}

pub(crate) fn canonical_provider_id(raw: &str) -> Option<String> {
    if raw.is_empty()
        || raw.len() > MAX_PROVIDER_ID_BYTES
        || !raw
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return None;
    }
    Some(raw.to_ascii_lowercase())
}

fn parse_provider_row(provider: String, row: &Value) -> CodexBarProviderSnapshot {
    let has_error = row.get("error").is_some_and(|error| !error.is_null());
    let usage = row.get("usage").filter(|usage| !usage.is_null());
    if has_error {
        return CodexBarProviderSnapshot::failure(
            provider,
            if usage.is_some() {
                CodexBarProviderError::InvalidResponse
            } else {
                CodexBarProviderError::Unavailable
            },
        );
    }
    let Some(usage) = usage else {
        return CodexBarProviderSnapshot::failure(provider, CodexBarProviderError::InvalidResponse);
    };
    let Ok(usage) = serde_json::from_value::<TypedCodexBarUsage>(usage.clone()) else {
        return CodexBarProviderSnapshot::failure(provider, CodexBarProviderError::InvalidResponse);
    };

    let extra_windows = usage.extra_rate_windows.unwrap_or_default();
    let built_in_count = [
        usage.primary.as_ref(),
        usage.secondary.as_ref(),
        usage.tertiary.as_ref(),
    ]
    .into_iter()
    .flatten()
    .count();
    if built_in_count.saturating_add(extra_windows.len()) > MAX_WINDOWS_PER_PROVIDER {
        return CodexBarProviderSnapshot::failure(provider, CodexBarProviderError::TooManyWindows);
    }

    let updated_at = match usage.updated_at.as_deref().map(parse_timestamp).transpose() {
        Ok(updated_at) => updated_at,
        Err(()) => {
            return CodexBarProviderSnapshot::failure(
                provider,
                CodexBarProviderError::InvalidResponse,
            );
        }
    };
    let mut windows = Vec::with_capacity(built_in_count + extra_windows.len());
    let mut window_ids = HashSet::with_capacity(windows.capacity());
    for (id, label, raw) in [
        ("primary", "Primary", usage.primary.as_ref()),
        ("secondary", "Secondary", usage.secondary.as_ref()),
        ("tertiary", "Tertiary", usage.tertiary.as_ref()),
    ] {
        let Some(raw) = raw else { continue };
        let Ok(window) = parse_window(id, label, raw) else {
            return CodexBarProviderSnapshot::failure(
                provider,
                CodexBarProviderError::InvalidResponse,
            );
        };
        window_ids.insert(window.id.to_ascii_lowercase());
        windows.push(window);
    }
    for extra in extra_windows {
        let label = extra.title.as_deref().unwrap_or(&extra.id);
        let Ok(window) = parse_window(&extra.id, label, &extra.window) else {
            return CodexBarProviderSnapshot::failure(
                provider,
                CodexBarProviderError::InvalidResponse,
            );
        };
        if !window_ids.insert(window.id.to_ascii_lowercase()) {
            return CodexBarProviderSnapshot::failure(
                provider,
                CodexBarProviderError::InvalidResponse,
            );
        }
        windows.push(window);
    }
    if windows.is_empty() {
        return CodexBarProviderSnapshot::failure(provider, CodexBarProviderError::InvalidResponse);
    }

    CodexBarProviderSnapshot {
        provider,
        windows,
        updated_at,
        error: None,
    }
}

fn parse_window(id: &str, label: &str, raw: &RawCodexBarWindow) -> Result<CodexBarWindow, ()> {
    let resets_at = raw.resets_at.as_deref().map(parse_timestamp).transpose()?;
    let validated = RateLimitWindow::try_new(
        id,
        label,
        raw.used_percent,
        resets_at,
        raw.window_minutes,
        RateLimitProvenance::CodexBar,
    )
    .ok_or(())?;
    Ok(CodexBarWindow {
        id: validated.id,
        label: validated.label,
        used_pct: validated.used_pct,
        resets_at: validated.resets_at,
        window_minutes: validated.window_minutes,
    })
}

fn parse_timestamp(raw: &str) -> Result<u64, ()> {
    let timestamp = chrono::DateTime::parse_from_rfc3339(raw)
        .map_err(|_| ())?
        .timestamp();
    u64::try_from(timestamp).map_err(|_| ())
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn run_codexbar(
    child_slot: &Arc<Mutex<ChildProcessSlot>>,
    generation: u64,
) -> Result<CommandOutput, CodexBarPollError> {
    if !child_generation_matches(child_slot, generation) {
        return Err(CodexBarPollError::Cancelled);
    }
    let mut command = Command::new("codexbar");
    command
        .args(CODEXBAR_ARGS)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }

    let mut child = command
        .spawn()
        .map_err(|_| CodexBarPollError::NotRunnable)?;
    let child_process = ChildProcessHandle::for_child(&child);
    let Some(registration) =
        ChildProcessRegistration::claim(child_slot, generation, &child_process)
    else {
        terminate_child_bounded(
            child,
            &child_process,
            &mut None,
            bounded_cleanup_deadline(Instant::now() + COMMAND_TIMEOUT),
        );
        return Err(CodexBarPollError::Cancelled);
    };
    let mut registration = Some(registration);
    let Some(stdout) = child.stdout.take() else {
        terminate_child_bounded(
            child,
            &child_process,
            &mut registration,
            bounded_cleanup_deadline(Instant::now() + COMMAND_TIMEOUT),
        );
        return Err(CodexBarPollError::InternalError);
    };
    let Some(stderr) = child.stderr.take() else {
        terminate_child_bounded(
            child,
            &child_process,
            &mut registration,
            bounded_cleanup_deadline(Instant::now() + COMMAND_TIMEOUT),
        );
        return Err(CodexBarPollError::InternalError);
    };
    let (tx, rx) = mpsc::channel();
    let overall_deadline = Instant::now() + COMMAND_TIMEOUT;
    #[cfg(windows)]
    let deadline = overall_deadline - READER_CANCEL_TIMEOUT;
    #[cfg(not(windows))]
    let deadline = overall_deadline;
    let mut readers = BoundedReaderTasks::new(
        [
            spawn_command_pipe_reader(stdout, 0, tx.clone()),
            spawn_command_pipe_reader(stderr, 1, tx),
        ],
        overall_deadline,
    );
    let status = loop {
        #[cfg(unix)]
        match observe_unreaped_unix_child(&mut registration, &child_process) {
            Ok(true) => {
                // The helper killed the dedicated group and removed its slot
                // under one lock while the zombie leader reserved the PGID.
                match child.wait() {
                    Ok(status) => break status,
                    Err(_) => return Err(CodexBarPollError::InternalError),
                }
            }
            Ok(false) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
                continue;
            }
            Ok(false) => {
                terminate_child_bounded(child, &child_process, &mut registration, deadline);
                return Err(CodexBarPollError::TimedOut);
            }
            Err(_) => {
                terminate_child_bounded(
                    child,
                    &child_process,
                    &mut registration,
                    bounded_cleanup_deadline(deadline),
                );
                return Err(CodexBarPollError::InternalError);
            }
        }
        #[cfg(not(unix))]
        match observe_registered_child(&mut registration, || child.try_wait()) {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Ok(None) => {
                terminate_child_bounded(child, &child_process, &mut registration, deadline);
                return Err(CodexBarPollError::TimedOut);
            }
            Err(_) => {
                terminate_child_bounded(
                    child,
                    &child_process,
                    &mut registration,
                    bounded_cleanup_deadline(deadline),
                );
                return Err(CodexBarPollError::InternalError);
            }
        }
    };
    let drain_deadline = deadline.min(Instant::now() + OUTPUT_DRAIN_TIMEOUT);
    let mut streams: [Option<(Vec<u8>, bool, bool)>; 2] = [None, None];
    while streams.iter().any(Option::is_none) {
        let Some(remaining) = drain_deadline.checked_duration_since(Instant::now()) else {
            terminate_after_leader_reaped(&child_process);
            return Err(CodexBarPollError::InternalError);
        };
        match rx.recv_timeout(remaining) {
            Ok((slot, bytes, truncated, read_failed)) if slot < streams.len() => {
                if streams[slot].is_some() || !readers.finish(slot) {
                    terminate_after_leader_reaped(&child_process);
                    return Err(CodexBarPollError::InternalError);
                }
                streams[slot] = Some((bytes, truncated, read_failed));
            }
            Ok(_) => {
                terminate_after_leader_reaped(&child_process);
                return Err(CodexBarPollError::InternalError);
            }
            Err(_) => {
                terminate_after_leader_reaped(&child_process);
                return Err(CodexBarPollError::InternalError);
            }
        }
    }
    let (stdout, stdout_truncated, stdout_read_failed) = streams[0].take().unwrap_or_default();
    let (_stderr, stderr_truncated, stderr_read_failed) = streams[1].take().unwrap_or_default();
    if stdout_truncated || stderr_truncated {
        terminate_after_leader_reaped(&child_process);
        return Err(CodexBarPollError::OutputTooLarge);
    }
    if stdout_read_failed || stderr_read_failed {
        terminate_after_leader_reaped(&child_process);
        return Err(CodexBarPollError::InternalError);
    }
    // Windows job identity remains valid after leader reap. Terminate it even
    // when both pipes closed so a detached descendant cannot outlive the poll.
    // Unix already killed the group while the zombie leader anchored its PGID.
    terminate_after_leader_reaped(&child_process);
    drop(registration);
    // A provider-specific failure can make CodexBar exit nonzero while stdout
    // still contains a valid mixed snapshot. Preserve only the exit predicate;
    // parsing decides whether the bounded stdout is usable. Stderr is discarded.
    Ok(CommandOutput {
        stdout,
        success: status.success(),
    })
}

struct BoundedReaderTask {
    cancelled: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl BoundedReaderTask {
    fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }
}

struct BoundedReaderTasks {
    tasks: [Option<BoundedReaderTask>; 2],
    #[cfg(windows)]
    overall_deadline: Instant,
}

impl BoundedReaderTasks {
    fn new(tasks: [BoundedReaderTask; 2], overall_deadline: Instant) -> Self {
        #[cfg(not(windows))]
        let _ = overall_deadline;
        Self {
            tasks: tasks.map(Some),
            #[cfg(windows)]
            overall_deadline,
        }
    }

    fn finish(&mut self, slot: usize) -> bool {
        let Some(task) = self.tasks.get_mut(slot).and_then(Option::take) else {
            return false;
        };
        drop(task.handle);
        true
    }
}

impl Drop for BoundedReaderTasks {
    fn drop(&mut self) {
        #[cfg(windows)]
        {
            let deadline = self
                .overall_deadline
                .min(Instant::now() + READER_CANCEL_TIMEOUT);
            loop {
                let mut pending = false;
                for task in self.tasks.iter().flatten() {
                    if task
                        .handle
                        .as_ref()
                        .is_some_and(|handle| !handle.is_finished())
                    {
                        pending = true;
                        // Windows readers never issue an unproven blocking
                        // read. Reassert cancellation until the polling thread
                        // observes it or the overall command bound expires.
                        task.cancel();
                    }
                }
                if !pending || Instant::now() >= deadline {
                    break;
                }
                std::thread::sleep(
                    deadline
                        .saturating_duration_since(Instant::now())
                        .min(Duration::from_millis(1)),
                );
            }
        }
        #[cfg(not(windows))]
        for task in self.tasks.iter().flatten() {
            task.cancel();
        }
    }
}

#[cfg(not(windows))]
fn spawn_command_pipe_reader(
    reader: impl Read + Send + 'static,
    slot: usize,
    tx: mpsc::Sender<(usize, Vec<u8>, bool, bool)>,
) -> BoundedReaderTask {
    spawn_bounded_reader(reader, slot, tx)
}

#[cfg(windows)]
#[link(name = "kernel32")]
extern "system" {
    #[link_name = "PeekNamedPipe"]
    fn peek_named_pipe(
        named_pipe: windows_sys::Win32::Foundation::HANDLE,
        buffer: *mut core::ffi::c_void,
        buffer_size: u32,
        bytes_read: *mut u32,
        total_bytes_available: *mut u32,
        bytes_left_this_message: *mut u32,
    ) -> i32;
}

#[cfg(windows)]
fn spawn_command_pipe_reader<R>(
    mut reader: R,
    slot: usize,
    tx: mpsc::Sender<(usize, Vec<u8>, bool, bool)>,
) -> BoundedReaderTask
where
    R: Read + Send + std::os::windows::io::AsRawHandle + 'static,
{
    use windows_sys::Win32::Foundation::{
        GetLastError, ERROR_BROKEN_PIPE, ERROR_PIPE_NOT_CONNECTED,
    };

    let cancelled = Arc::new(AtomicBool::new(false));
    let reader_cancelled = cancelled.clone();
    let handle = std::thread::spawn(move || {
        let pipe = reader.as_raw_handle() as windows_sys::Win32::Foundation::HANDLE;
        let mut output = Vec::new();
        let mut truncated = false;
        let mut read_failed = false;
        let mut buffer = [0_u8; 8192];
        loop {
            if reader_cancelled.load(Ordering::Acquire) {
                break;
            }
            let mut available = 0_u32;
            let peeked = unsafe {
                peek_named_pipe(
                    pipe,
                    std::ptr::null_mut(),
                    0,
                    std::ptr::null_mut(),
                    &mut available,
                    std::ptr::null_mut(),
                )
            } != 0;
            if !peeked {
                if reader_cancelled.load(Ordering::Acquire) {
                    break;
                }
                let error = unsafe { GetLastError() };
                if !matches!(error, ERROR_BROKEN_PIPE | ERROR_PIPE_NOT_CONNECTED) {
                    read_failed = true;
                }
                break;
            }
            if available == 0 {
                std::thread::sleep(Duration::from_millis(1));
                continue;
            }

            let remaining = MAX_OUTPUT.saturating_sub(output.len());
            if remaining == 0 {
                // Peek already proved that bytes exist beyond the retained cap.
                truncated = true;
                break;
            }
            let requested = (available as usize).min(buffer.len()).min(remaining);
            match reader.read(&mut buffer[..requested]) {
                Ok(0) => break,
                Ok(read) => {
                    output.extend_from_slice(&buffer[..read]);
                    if output.len() == MAX_OUTPUT && available as usize > read {
                        truncated = true;
                        break;
                    }
                }
                Err(_) if reader_cancelled.load(Ordering::Acquire) => break,
                Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => break,
                Err(_) => {
                    read_failed = true;
                    break;
                }
            }
        }
        let _ = tx.send((slot, output, truncated, read_failed));
    });
    BoundedReaderTask {
        cancelled,
        handle: Some(handle),
    }
}

#[cfg(any(not(windows), test))]
fn spawn_bounded_reader(
    mut reader: impl Read + Send + 'static,
    slot: usize,
    tx: mpsc::Sender<(usize, Vec<u8>, bool, bool)>,
) -> BoundedReaderTask {
    let cancelled = Arc::new(AtomicBool::new(false));
    let reader_cancelled = cancelled.clone();
    let handle = std::thread::spawn(move || {
        let mut output = Vec::new();
        let mut truncated = false;
        let mut read_failed = false;
        let mut buffer = [0_u8; 8192];
        loop {
            if reader_cancelled.load(Ordering::Acquire) {
                break;
            }
            match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => {
                    let remaining = MAX_OUTPUT.saturating_sub(output.len());
                    let retained = remaining.min(read);
                    output.extend_from_slice(&buffer[..retained]);
                    if retained < read {
                        // Once overflow is proven, close the read end instead
                        // of draining attacker-controlled output forever.
                        truncated = true;
                        break;
                    }
                }
                Err(_) if reader_cancelled.load(Ordering::Acquire) => break,
                Err(_) => {
                    read_failed = true;
                    break;
                }
            }
        }
        let _ = tx.send((slot, output, truncated, read_failed));
    });
    BoundedReaderTask {
        cancelled,
        handle: Some(handle),
    }
}

fn bounded_cleanup_deadline(command_deadline: Instant) -> Instant {
    command_deadline.min(Instant::now() + CHILD_REAP_TIMEOUT)
}

fn terminate_child_bounded(
    mut child: Child,
    process: &ChildProcessHandle,
    registration: &mut Option<ChildProcessRegistration>,
    deadline: Instant,
) {
    #[cfg(not(unix))]
    if let Ok(Some(_)) = observe_registered_child(registration, || child.try_wait()) {
        terminate_after_leader_reaped(process);
        return;
    }

    #[cfg(unix)]
    signal_and_release_registered_unix_child(registration, process);
    #[cfg(windows)]
    if let Some(job) = &process.job {
        job.terminate();
    }
    #[cfg(not(any(unix, windows)))]
    process.terminate();
    // `Child::kill` uses the exact owned process handle on Windows and the
    // still-unreaped child identity elsewhere.
    let _ = child.kill();
    loop {
        let observed = observe_registered_child(registration, || child.try_wait());
        match observed {
            Ok(Some(_)) => {
                drop(registration.take());
                terminate_after_leader_reaped(process);
                return;
            }
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(
                    deadline
                        .saturating_duration_since(Instant::now())
                        .min(Duration::from_millis(1)),
                );
            }
            Ok(None) | Err(_) => break,
        }
    }

    // The termination request has already been issued. Stop exposing the
    // numeric fallback before handing the exact Child handle to a detached
    // reaper, so the caller never blocks beyond its absolute deadline.
    drop(registration.take());
    let _ = std::thread::Builder::new()
        .name("abtop-codexbar-reaper".to_string())
        .spawn(move || {
            let _ = child.wait();
        });
}

#[cfg(unix)]
fn child_exited_without_reaping(pid: u32) -> Result<bool, CodexBarPollError> {
    let mut information = std::mem::MaybeUninit::<libc::siginfo_t>::zeroed();
    let result = unsafe {
        libc::waitid(
            libc::P_PID,
            pid as libc::id_t,
            information.as_mut_ptr(),
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    };
    if result != 0 {
        return Err(CodexBarPollError::InternalError);
    }
    let information = unsafe { information.assume_init() };
    Ok(unsafe { information.si_pid() } != 0)
}

#[cfg(unix)]
fn terminate_after_leader_reaped(_: &ChildProcessHandle) {}

#[cfg(windows)]
fn terminate_after_leader_reaped(process: &ChildProcessHandle) {
    // A Job Object remains an exact process-tree handle after the leader is
    // reaped. The direct-PID fallback does not: that numeric PID may already
    // have been reused, so it must never be signalled from this path.
    if let Some(job) = &process.job {
        job.terminate();
    }
}

#[cfg(not(any(unix, windows)))]
fn terminate_after_leader_reaped(_: &ChildProcessHandle) {}

#[cfg(windows)]
#[derive(Debug)]
struct WindowsProcessHandle {
    handle: std::os::windows::io::OwnedHandle,
}

#[cfg(windows)]
impl WindowsProcessHandle {
    fn duplicate(child: &Child) -> Option<Self> {
        use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
        use windows_sys::Win32::Foundation::{DuplicateHandle, DUPLICATE_SAME_ACCESS};
        use windows_sys::Win32::System::Threading::GetCurrentProcess;

        let current_process = unsafe { GetCurrentProcess() };
        let mut duplicate = std::ptr::null_mut();
        let duplicated = unsafe {
            DuplicateHandle(
                current_process,
                child.as_raw_handle() as _,
                current_process,
                &mut duplicate,
                0,
                0,
                DUPLICATE_SAME_ACCESS,
            )
        } != 0;
        if !duplicated || duplicate.is_null() {
            return None;
        }
        Some(Self {
            // The native child handle includes PROCESS_TERMINATE and
            // SYNCHRONIZE. The duplicate remains tied to that exact process
            // object even after the numeric PID becomes reusable.
            handle: unsafe { OwnedHandle::from_raw_handle(duplicate as _) },
        })
    }

    fn terminate(&self) {
        use std::os::windows::io::AsRawHandle;

        unsafe {
            windows_sys::Win32::System::Threading::TerminateProcess(
                self.handle.as_raw_handle() as _,
                1,
            );
        }
    }
}

#[cfg(windows)]
#[derive(Debug)]
struct WindowsCommandJob {
    handle: std::os::windows::io::OwnedHandle,
}

#[cfg(windows)]
impl WindowsCommandJob {
    fn assign(child: &Child) -> Option<Self> {
        use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
        use windows_sys::Win32::System::JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
            SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        };

        let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if handle.is_null() {
            return None;
        }
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let configured = unsafe {
            SetInformationJobObject(
                handle,
                JobObjectExtendedLimitInformation,
                (&raw const limits).cast(),
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        } != 0;
        let assigned = configured
            && unsafe { AssignProcessToJobObject(handle, child.as_raw_handle() as _) } != 0;
        if !assigned {
            unsafe {
                windows_sys::Win32::Foundation::CloseHandle(handle);
            }
            return None;
        }
        Some(Self {
            handle: unsafe { OwnedHandle::from_raw_handle(handle as _) },
        })
    }

    fn terminate(&self) {
        use std::os::windows::io::AsRawHandle;
        unsafe {
            windows_sys::Win32::System::JobObjects::TerminateJobObject(
                self.handle.as_raw_handle() as _,
                1,
            );
        }
    }
}

#[cfg(not(windows))]
fn terminate_pid(pid: u32) {
    if pid == 0 {
        return;
    }
    #[cfg(unix)]
    unsafe {
        libc::kill(-(pid as i32), libc::SIGKILL);
    }
}

#[cfg(test)]
mod multi_provider_tests {
    use super::*;

    fn mixed_response() -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!([
            {
                "provider": "claude",
                "source": "claude",
                "usage": {
                    "primary": {
                        "usedPercent": 28.0,
                        "windowMinutes": 300,
                        "resetsAt": "2026-08-03T12:59:00Z"
                    },
                    "secondary": {
                        "usedPercent": 6.0,
                        "windowMinutes": 10080,
                        "resetsAt": "2026-08-10T07:59:00Z"
                    },
                    "tertiary": {
                        "usedPercent": 2.0
                    },
                    "extraRateWindows": [{
                        "title": "Fable only",
                        "window": {
                            "usedPercent": 0.0,
                            "windowMinutes": 10080,
                            "resetsAt": "2026-08-10T07:59:00Z"
                        },
                        "id": "claude-weekly-scoped-fable"
                    }],
                    "updatedAt": "2026-08-03T11:12:34Z",
                    "identity": {
                        "accountEmail": "private@example.com",
                        "accountOrganization": "private-org"
                    }
                },
                "credits": {"remaining": 123},
                "pace": {"primary": {"summary": "private pace"}}
            },
            {
                "provider": "codex",
                "source": "oauth",
                "usage": {
                    "primary": null,
                    "secondary": {
                        "usedPercent": 48.0,
                        "windowMinutes": 10080,
                        "resetsAt": "2026-08-09T09:22:51Z"
                    },
                    "updatedAt": "2026-08-03T11:12:35Z",
                    "accountEmail": "private@example.com"
                }
            },
            {
                "provider": "kimi",
                "source": "web",
                "error": {
                    "kind": "provider",
                    "code": 1,
                    "message": "raw private failure"
                }
            },
            {
                "provider": "grok",
                "source": "grok-web",
                "usage": {
                    "primary": {
                        "usedPercent": 18.0,
                        "resetsAt": "2026-08-06T06:36:08Z"
                    },
                    "updatedAt": "2026-08-03T11:12:38Z",
                    "identity": {"accountEmail": "private@example.com"}
                }
            }
        ]))
        .unwrap()
    }

    fn success(provider: &str, used_percent: f64) -> Value {
        serde_json::json!({
            "provider": provider,
            "source": "any-configured-source",
            "usage": {
                "primary": {"usedPercent": used_percent},
                "updatedAt": "2026-08-03T11:12:38Z"
            }
        })
    }

    fn snapshot(provider: &str) -> CodexBarSnapshot {
        CodexBarSnapshot {
            providers: vec![CodexBarProviderSnapshot {
                provider: provider.to_string(),
                windows: vec![CodexBarWindow {
                    id: "primary".to_string(),
                    label: "Primary".to_string(),
                    used_pct: 12.0,
                    resets_at: None,
                    window_minutes: None,
                }],
                updated_at: Some(1),
                error: None,
            }],
        }
    }

    #[cfg(not(windows))]
    fn child_handle(pid: u32) -> ChildProcessHandle {
        ChildProcessHandle {
            pid,
            #[cfg(windows)]
            job: None,
            #[cfg(windows)]
            process: None,
        }
    }

    #[test]
    fn parser_keeps_all_valid_providers_and_sanitizes_provider_errors() {
        let parsed = parse_codexbar(&mixed_response()).expect("mixed response");
        assert_eq!(
            parsed
                .providers
                .iter()
                .map(|provider| provider.provider.as_str())
                .collect::<Vec<_>>(),
            ["claude", "codex", "kimi", "grok"]
        );
        assert_eq!(parsed.state(), CodexBarQuotaState::Partial);

        let claude = &parsed.providers[0];
        assert_eq!(claude.windows.len(), 4);
        assert_eq!(claude.windows[0].label, "Primary");
        assert_eq!(claude.windows[2].label, "Tertiary");
        assert_eq!(claude.windows[3].id, "claude-weekly-scoped-fable");
        assert_eq!(claude.windows[3].label, "Fable only");

        let codex = &parsed.providers[1];
        assert_eq!(codex.windows.len(), 1);
        assert_eq!(codex.windows[0].id, "secondary");

        let kimi = &parsed.providers[2];
        assert_eq!(kimi.error, Some(CodexBarProviderError::Unavailable));
        assert!(kimi.windows.is_empty());

        let grok = &parsed.providers[3];
        assert_eq!(grok.windows[0].window_minutes, None);
        assert_eq!(grok.windows[0].used_pct, 18.0);

        let debug = format!("{parsed:?}");
        for private in [
            "private@example.com",
            "private-org",
            "private pace",
            "raw private failure",
            "oauth",
            "grok-web",
        ] {
            assert!(
                !debug.contains(private),
                "retained private field: {private}"
            );
        }
    }

    #[test]
    fn parser_accepts_singleton_and_empty_configured_provider_sets() {
        let singleton = serde_json::to_vec(&success("GROK", 18.0)).unwrap();
        let parsed = parse_codexbar(&singleton).expect("singleton");
        assert_eq!(parsed.providers[0].provider, "grok");
        assert_eq!(parsed.state(), CodexBarQuotaState::Available);

        let empty = parse_codexbar(b"[]").expect("empty configured set");
        assert!(empty.providers.is_empty());
        assert_eq!(empty.state(), CodexBarQuotaState::Unavailable);
    }

    #[test]
    fn malformed_provider_is_isolated_from_valid_providers() {
        let response = serde_json::to_vec(&serde_json::json!([
            success("claude", 25.0),
            {
                "provider": "grok",
                "usage": {"primary": {"usedPercent": "not-a-number"}}
            },
            {"provider": "kimi", "error": {"message": "private"}}
        ]))
        .unwrap();
        let parsed = parse_codexbar(&response).expect("partially valid response");
        assert!(parsed.providers[0].is_available());
        assert_eq!(
            parsed.providers[1].error,
            Some(CodexBarProviderError::InvalidResponse)
        );
        assert_eq!(
            parsed.providers[2].error,
            Some(CodexBarProviderError::Unavailable)
        );
    }

    #[test]
    fn duplicate_provider_is_a_sanitized_provider_failure() {
        let response =
            serde_json::to_vec(&vec![success("Claude", 1.0), success("claude", 2.0)]).unwrap();
        let parsed = parse_codexbar(&response).expect("duplicate is isolated");
        assert_eq!(parsed.providers.len(), 1);
        assert_eq!(parsed.providers[0].provider, "claude");
        assert_eq!(
            parsed.providers[0].error,
            Some(CodexBarProviderError::DuplicateProvider)
        );
        assert!(parsed.providers[0].windows.is_empty());
    }

    #[test]
    fn provider_and_window_bounds_fail_closed() {
        let providers = (0..=MAX_PROVIDERS)
            .map(|index| success(&format!("provider-{index}"), 1.0))
            .collect::<Vec<_>>();
        assert_eq!(
            parse_codexbar(&serde_json::to_vec(&providers).unwrap()).unwrap_err(),
            CodexBarPollError::UnsupportedResponse
        );

        let extra = (0..MAX_WINDOWS_PER_PROVIDER)
            .map(|index| {
                serde_json::json!({
                    "id": format!("extra-{index}"),
                    "title": format!("Extra {index}"),
                    "window": {"usedPercent": 1.0}
                })
            })
            .collect::<Vec<_>>();
        let response = serde_json::to_vec(&serde_json::json!([{
            "provider": "claude",
            "usage": {
                "primary": {"usedPercent": 1.0},
                "extraRateWindows": extra
            }
        }]))
        .unwrap();
        let parsed = parse_codexbar(&response).expect("provider failure snapshot");
        assert_eq!(
            parsed.providers[0].error,
            Some(CodexBarProviderError::TooManyWindows)
        );

        assert_eq!(
            parse_codexbar(&vec![b'x'; MAX_OUTPUT + 1]).unwrap_err(),
            CodexBarPollError::OutputTooLarge
        );
    }

    #[test]
    fn provider_window_ids_and_labels_are_bounded_without_truncation() {
        let invalid_provider =
            serde_json::to_vec(&success(&"p".repeat(MAX_PROVIDER_ID_BYTES + 1), 1.0)).unwrap();
        assert_eq!(
            parse_codexbar(&invalid_provider).unwrap_err(),
            CodexBarPollError::InvalidResponse
        );

        for (id, title) in [
            (
                "i".repeat(MAX_RATE_LIMIT_WINDOW_ID_BYTES + 1),
                "Extra".to_string(),
            ),
            (
                "extra".to_string(),
                "l".repeat(MAX_RATE_LIMIT_WINDOW_LABEL_BYTES + 1),
            ),
            ("extra\nunsafe".to_string(), "Extra".to_string()),
            ("extra".to_string(), "Extra\u{1b}".to_string()),
            ("extra\u{202e}".to_string(), "Extra".to_string()),
            ("extra".to_string(), "Extra\u{2066}".to_string()),
        ] {
            let response = serde_json::to_vec(&serde_json::json!([{
                "provider": "claude",
                "usage": {
                    "extraRateWindows": [{
                        "id": id,
                        "title": title,
                        "window": {"usedPercent": 1.0}
                    }]
                }
            }]))
            .unwrap();
            let parsed = parse_codexbar(&response).expect("bounded provider failure");
            assert_eq!(
                parsed.providers[0].error,
                Some(CodexBarProviderError::InvalidResponse)
            );
        }
    }

    #[test]
    fn duplicate_window_ids_are_rejected_case_insensitively() {
        for extra_id in ["PRIMARY", "weekly", "WEEKLY"] {
            let extra_windows = if extra_id.eq_ignore_ascii_case("primary") {
                vec![serde_json::json!({
                    "id": extra_id,
                    "window": {"usedPercent": 2.0}
                })]
            } else {
                vec![
                    serde_json::json!({
                        "id": "weekly",
                        "window": {"usedPercent": 2.0}
                    }),
                    serde_json::json!({
                        "id": extra_id,
                        "window": {"usedPercent": 3.0}
                    }),
                ]
            };
            let response = serde_json::to_vec(&serde_json::json!([{
                "provider": "claude",
                "usage": {
                    "primary": {"usedPercent": 1.0},
                    "extraRateWindows": extra_windows
                }
            }]))
            .unwrap();
            let parsed = parse_codexbar(&response).expect("duplicate is provider-local");
            assert_eq!(
                parsed.providers[0].error,
                Some(CodexBarProviderError::InvalidResponse)
            );
        }
    }

    #[test]
    fn extra_window_title_falls_back_to_its_stable_id() {
        let response = serde_json::to_vec(&serde_json::json!([{
            "provider": "claude",
            "usage": {
                "extraRateWindows": [{
                    "id": "scoped-weekly",
                    "window": {"usedPercent": 3.0}
                }]
            }
        }]))
        .unwrap();
        let parsed = parse_codexbar(&response).expect("extra window without a title");
        assert_eq!(parsed.providers[0].windows[0].id, "scoped-weekly");
        assert_eq!(parsed.providers[0].windows[0].label, "scoped-weekly");
    }

    #[test]
    fn malformed_window_metadata_is_a_provider_failure() {
        for window in [
            serde_json::json!({"usedPercent": -1.0}),
            serde_json::json!({"usedPercent": 101.0}),
            serde_json::json!({"usedPercent": 1.0, "windowMinutes": 0}),
            serde_json::json!({"usedPercent": 1.0, "resetsAt": "not-a-date"}),
        ] {
            let response = serde_json::to_vec(&serde_json::json!([{
                "provider": "grok",
                "usage": {"primary": window}
            }]))
            .unwrap();
            let parsed = parse_codexbar(&response).expect("provider failure snapshot");
            assert_eq!(
                parsed.providers[0].error,
                Some(CodexBarProviderError::InvalidResponse)
            );
        }
    }

    #[test]
    fn valid_stdout_wins_even_when_process_exits_nonzero() {
        let parsed = parse_command_output(CommandOutput {
            stdout: mixed_response(),
            success: false,
        })
        .expect("mixed provider failure remains a valid snapshot");
        assert_eq!(parsed.providers.len(), 4);

        assert_eq!(
            parse_command_output(CommandOutput {
                stdout: b"[]".to_vec(),
                success: false,
            })
            .unwrap_err(),
            CodexBarPollError::ProcessFailed
        );

        assert_eq!(
            parse_command_output(CommandOutput {
                stdout: b"private shell failure".to_vec(),
                success: false,
            })
            .unwrap_err(),
            CodexBarPollError::ProcessFailed
        );
    }

    #[test]
    fn poller_atomically_replaces_valid_snapshots_and_retains_on_transport_failure() {
        let mut poller = CodexBarQuotaPoller::new(true);
        poller.apply_result(PollResult {
            generation: 0,
            result: Ok(snapshot("claude")),
        });
        assert_eq!(poller.status().state, CodexBarQuotaState::Available);

        poller.apply_result(PollResult {
            generation: 0,
            result: Ok(CodexBarSnapshot {
                providers: vec![CodexBarProviderSnapshot::failure(
                    "kimi".to_string(),
                    CodexBarProviderError::Unavailable,
                )],
            }),
        });
        assert_eq!(
            poller.cached.as_ref().unwrap().providers[0].provider,
            "kimi"
        );
        assert_eq!(poller.status().state, CodexBarQuotaState::Unavailable);

        poller.apply_result(PollResult {
            generation: 0,
            result: Ok(snapshot("grok")),
        });
        poller.apply_result(PollResult {
            generation: 0,
            result: Err(CodexBarPollError::TimedOut),
        });
        assert_eq!(
            poller.cached.as_ref().unwrap().providers[0].provider,
            "grok"
        );
        assert_eq!(poller.status().state, CodexBarQuotaState::Partial);
        assert_eq!(poller.status().error, Some(CodexBarPollError::TimedOut));
    }

    #[test]
    fn poller_is_opt_in_periodic_and_generation_safe() {
        let mut poller = CodexBarQuotaPoller::new(false);
        assert!(poller.update().is_none());
        assert!(!poller.should_start());
        assert_eq!(poller.status().state, CodexBarQuotaState::Off);

        poller.set_enabled(true);
        assert!(poller.should_start());
        poller.in_flight = Some(1);
        assert!(!poller.should_start());
        assert_eq!(poller.status().state, CodexBarQuotaState::Checking);

        poller.set_enabled(false);
        poller.apply_result(PollResult {
            generation: 1,
            result: Ok(snapshot("claude")),
        });
        assert!(poller.cached.is_none());
    }

    #[cfg(not(windows))]
    #[test]
    fn child_registration_rejects_old_generations_and_releases_exact_pid_only() {
        let slot = Mutex::new(ChildProcessSlot::new(7));
        assert!(claim_child_process(&slot, 7, &child_handle(101)));
        assert!(!claim_child_process(&slot, 7, &child_handle(102)));
        assert!(!release_child_process(&slot, 8, 101));
        assert!(release_child_process(&slot, 7, 101));
        child_slot_guard(&slot).generation = 8;
        assert!(claim_child_process(&slot, 8, &child_handle(202)));
        assert!(!claim_child_process(&slot, 7, &child_handle(303)));
        assert!(!release_child_process(&slot, 7, 101));
        assert!(release_child_process(&slot, 8, 202));
    }

    #[test]
    fn reaped_observation_removes_the_registered_identity_before_returning() {
        let slot = Arc::new(Mutex::new(ChildProcessSlot::new(7)));
        child_slot_guard(&slot).pid = Some(101);
        let mut registration = Some(ChildProcessRegistration {
            slot: slot.clone(),
            generation: 7,
            pid: 101,
        });

        let observed =
            observe_registered_child(&mut registration, || Ok::<_, ()>(Some("exited"))).unwrap();

        assert_eq!(observed, Some("exited"));
        assert!(registration.is_none());
        assert_eq!(child_slot_guard(&slot).pid, None);
    }

    #[test]
    fn output_reader_is_independently_bounded() {
        let (tx, rx) = mpsc::channel();
        let _task = spawn_bounded_reader(std::io::Cursor::new(vec![b'x'; MAX_OUTPUT + 1]), 4, tx);
        let (slot, bytes, truncated, read_failed) =
            rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(slot, 4);
        assert_eq!(bytes.len(), MAX_OUTPUT);
        assert!(truncated);
        assert!(!read_failed);
    }

    #[test]
    fn output_reader_stops_reading_as_soon_as_the_cap_is_exceeded() {
        struct EndlessReader(Arc<std::sync::atomic::AtomicUsize>);

        impl Read for EndlessReader {
            fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
                self.0.fetch_add(1, Ordering::Relaxed);
                buffer.fill(b'x');
                Ok(buffer.len())
            }
        }

        let reads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (tx, rx) = mpsc::channel();
        let _task = spawn_bounded_reader(EndlessReader(reads.clone()), 0, tx);
        let (_, bytes, truncated, read_failed) = rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(bytes.len(), MAX_OUTPUT);
        assert!(truncated);
        assert!(!read_failed);
        let completed_reads = reads.load(Ordering::Relaxed);
        std::thread::sleep(Duration::from_millis(10));
        assert_eq!(reads.load(Ordering::Relaxed), completed_reads);
    }

    #[cfg(unix)]
    #[test]
    fn termination_and_reaping_never_block_past_the_cleanup_bound() {
        use std::os::unix::process::CommandExt;

        let mut command = Command::new("/bin/sh");
        command
            .args(["-c", "sleep 5"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0);
        let child = command.spawn().unwrap();
        let process = ChildProcessHandle::for_child(&child);
        let slot = Arc::new(Mutex::new(ChildProcessSlot::new(3)));
        let mut registration = ChildProcessRegistration::claim(&slot, 3, &process);
        assert!(registration.is_some());

        let started = Instant::now();
        terminate_child_bounded(child, &process, &mut registration, Instant::now());

        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(registration.is_none());
        assert_eq!(child_slot_guard(&slot).pid, None);
    }

    #[cfg(windows)]
    #[test]
    fn windows_registration_retains_an_exact_process_handle() {
        let child = Command::new("cmd")
            .args(["/C", "ping -n 6 127.0.0.1 >NUL"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let process = ChildProcessHandle::for_child(&child);
        assert!(process.process.is_some());
        let slot = Arc::new(Mutex::new(ChildProcessSlot::new(4)));
        let mut registration = ChildProcessRegistration::claim(&slot, 4, &process);
        assert!(registration.is_some());
        let rotated = rotate_child_generation(&slot, 5).expect("exact registered process");
        assert!(rotated.process.is_some());
        rotated.terminate();
        terminate_child_bounded(
            child,
            &process,
            &mut registration,
            Instant::now() + CHILD_REAP_TIMEOUT,
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_reader_cancels_without_waiting_for_pipe_eof() {
        let mut child = Command::new("cmd")
            .args(["/C", "ping -n 6 127.0.0.1 >NUL"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let process = ChildProcessHandle::for_child(&child);
        let stdout = child.stdout.take().unwrap();
        let (tx, _rx) = mpsc::channel();
        let mut task = spawn_command_pipe_reader(stdout, 0, tx);

        std::thread::sleep(Duration::from_millis(10));
        task.cancel();
        let deadline = Instant::now() + READER_CANCEL_TIMEOUT;
        while task
            .handle
            .as_ref()
            .is_some_and(|handle| !handle.is_finished())
            && Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(1));
        }
        let handle = task.handle.take().unwrap();
        assert!(handle.is_finished());
        handle.join().unwrap();
        terminate_child_bounded(
            child,
            &process,
            &mut None,
            Instant::now() + CHILD_REAP_TIMEOUT,
        );
    }

    #[test]
    fn invocation_uses_codexbar_configured_providers_without_source_overrides() {
        assert_eq!(
            CODEXBAR_ARGS,
            ["usage", "--format", "json", "--json-only", "--no-color"]
        );
    }

    #[test]
    fn error_enums_have_stable_sanitized_wire_values() {
        assert_eq!(
            serde_json::to_string(&CodexBarProviderError::Unavailable).unwrap(),
            "\"unavailable\""
        );
        assert_eq!(
            serde_json::to_string(&CodexBarProviderError::DuplicateProvider).unwrap(),
            "\"duplicate_provider\""
        );
        assert_eq!(
            serde_json::to_string(&CodexBarQuotaState::Partial).unwrap(),
            "\"partial\""
        );
    }
}
