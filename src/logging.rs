use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tracing_subscriber::fmt::writer::MakeWriter;
use tracing_subscriber::EnvFilter;

const DEFAULT_MAX_LOG_BYTES: u64 = 5 * 1024 * 1024;
const DEFAULT_RETAINED_LOG_FILES: usize = 0;

/// Threshold above which a render frame is considered slow and worth logging at WARN.
/// At 60 FPS a frame is ~16 ms; double that is a visible hitch.
pub(crate) const SLOW_RENDER_THRESHOLD: Duration = Duration::from_millis(33);
/// Threshold above which an input batch is considered slow (keystroke -> state update).
/// Anything over 50 ms is perceptible as input lag.
pub(crate) const SLOW_INPUT_THRESHOLD: Duration = Duration::from_millis(50);
/// Threshold above which a PTY byte-processing call is considered slow.
/// PTY chunks are small (≤8 KiB) so the parser should be fast.
pub(crate) const SLOW_PTY_THRESHOLD: Duration = Duration::from_millis(20);
/// Threshold above which an API roundtrip is considered slow.
pub(crate) const SLOW_API_THRESHOLD: Duration = Duration::from_millis(100);
/// Threshold above which a single iteration of the main app loop is considered
/// slow. Captures stalls in drain/scheduling work that isn't covered by the
/// per-subsystem timers (render, input, api, pty).
pub(crate) const SLOW_LOOP_ITER_THRESHOLD: Duration = Duration::from_millis(40);

pub(crate) fn init_file_logging(file_name: &str) {
    let Ok(make_writer) = RotatingFileMakeWriter::new(
        crate::session::data_dir(),
        file_name,
        DEFAULT_MAX_LOG_BYTES,
        DEFAULT_RETAINED_LOG_FILES,
    ) else {
        return;
    };

    let filter =
        EnvFilter::try_from_env("HERDR_LOG").unwrap_or_else(|_| EnvFilter::new("herdr=info"));

    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(make_writer)
        .with_ansi(false)
        .with_target(true)
        .try_init();
}

pub(crate) fn help_log_paths_summary() -> String {
    let dir = crate::session::data_dir();
    format!(
        "{} (plus herdr-client.log, herdr-server.log)",
        dir.join("herdr.log").display()
    )
}

pub(crate) fn startup(role: &'static str) {
    tracing::info!(
        event = "app.startup",
        subsystem = role,
        outcome = "started",
        pid = std::process::id(),
        "herdr starting"
    );
}

pub(crate) fn shutdown(role: &'static str) {
    tracing::info!(
        event = "app.shutdown",
        subsystem = role,
        outcome = "completed",
        pid = std::process::id(),
        "herdr exiting"
    );
}

pub(crate) fn api_request_started(request_id: &str, method: &'static str, changes_ui: bool) {
    let event = "api.request.start";
    let subsystem = "api";
    let outcome = "started";
    let message = "api request received";
    if changes_ui && !is_routine_api_method(method) {
        tracing::info!(
            event,
            subsystem,
            outcome,
            request_id,
            method,
            changes_ui,
            "{message}"
        );
    } else {
        tracing::debug!(
            event,
            subsystem,
            outcome,
            request_id,
            method,
            changes_ui,
            "{message}"
        );
    }
}

pub(crate) fn api_request_completed(
    request_id: &str,
    method: &'static str,
    outcome: &'static str,
    changes_ui: bool,
) {
    let event = "api.request.complete";
    let subsystem = "api";
    let message = "api request completed";
    if outcome != "ok" || (changes_ui && !is_routine_api_method(method)) {
        tracing::info!(event, subsystem, outcome, request_id, method, "{message}");
    } else {
        tracing::debug!(event, subsystem, outcome, request_id, method, "{message}");
    }
}

fn is_routine_api_method(method: &str) -> bool {
    matches!(
        method,
        "pane.get"
            | "pane.read"
            | "pane.list"
            | "workspace.list"
            | "tab.list"
            | "pane.report_agent"
            | "pane.report_agent_session"
            | "pane.report_metadata"
    )
}

pub(crate) fn api_request_failed(request_id: &str, method: &'static str, err: &str) {
    tracing::warn!(
        event = "api.request.fail",
        subsystem = "api",
        outcome = "error",
        request_id,
        method,
        err,
        "api request failed"
    );
}

pub(crate) fn api_wait_started(request_id: &str, pane_id: &str, timeout_ms: Option<u64>) {
    tracing::info!(
        event = "api.wait.start",
        subsystem = "api",
        outcome = "started",
        request_id,
        pane_id,
        timeout_ms,
        "api output wait started"
    );
}

pub(crate) fn api_wait_completed(request_id: &str, pane_id: &str, outcome: &'static str) {
    tracing::info!(
        event = "api.wait.complete",
        subsystem = "api",
        outcome,
        request_id,
        pane_id,
        "api output wait finished"
    );
}

pub(crate) fn api_wait_timed_out(request_id: &str, pane_id: &str) {
    tracing::warn!(
        event = "api.wait.timeout",
        subsystem = "api",
        outcome = "timeout",
        request_id,
        pane_id,
        "api output wait timed out"
    );
}

pub(crate) fn pane_spawn_started(
    pane_id: u32,
    rows: u16,
    cols: u16,
    scrollback_limit_bytes: usize,
) {
    tracing::info!(
        event = "pane.spawn.start",
        subsystem = "pane",
        outcome = "started",
        pane_id,
        rows,
        cols,
        scrollback_limit_bytes,
        "spawning pane terminal"
    );
}

pub(crate) fn pane_spawned(pane_id: u32, pid: u32) {
    tracing::info!(
        event = "pane.spawned",
        subsystem = "pane",
        outcome = "ok",
        pane_id,
        pid,
        "pane child spawned"
    );
}

pub(crate) fn pane_exited(pane_id: u32, status: &str) {
    tracing::info!(
        event = "pane.exit",
        subsystem = "pane",
        outcome = "completed",
        pane_id,
        status,
        "pane child exited"
    );
}

pub(crate) fn pane_exit_failed(pane_id: u32, err: &str) {
    tracing::error!(
        event = "pane.exit",
        subsystem = "pane",
        outcome = "error",
        pane_id,
        err,
        "pane child wait failed"
    );
}

pub(crate) fn workspace_created(workspace_id: &str, root_pane_id: u32) {
    tracing::info!(
        event = "workspace.create",
        subsystem = "workspace",
        outcome = "ok",
        workspace_id,
        pane_id = root_pane_id,
        "workspace created"
    );
}

pub(crate) fn workspace_focused(workspace_id: &str) {
    tracing::info!(
        event = "workspace.focus",
        subsystem = "workspace",
        outcome = "ok",
        workspace_id,
        "workspace focused"
    );
}

pub(crate) fn workspace_closed(workspace_id: &str) {
    tracing::info!(
        event = "workspace.close",
        subsystem = "workspace",
        outcome = "ok",
        workspace_id,
        "workspace closed"
    );
}

pub(crate) fn workspace_renamed(workspace_id: &str) {
    tracing::info!(
        event = "workspace.rename",
        subsystem = "workspace",
        outcome = "ok",
        workspace_id,
        "workspace renamed"
    );
}

pub(crate) fn tab_created(workspace_id: &str, tab_id: &str, root_pane_id: u32) {
    tracing::info!(
        event = "tab.create",
        subsystem = "tab",
        outcome = "ok",
        workspace_id,
        tab_id,
        pane_id = root_pane_id,
        "tab created"
    );
}

pub(crate) fn tab_focused(workspace_id: &str, tab_id: &str) {
    tracing::info!(
        event = "tab.focus",
        subsystem = "tab",
        outcome = "ok",
        workspace_id,
        tab_id,
        "tab focused"
    );
}

pub(crate) fn tab_closed(workspace_id: &str, tab_id: &str) {
    tracing::info!(
        event = "tab.close",
        subsystem = "tab",
        outcome = "ok",
        workspace_id,
        tab_id,
        "tab closed"
    );
}

pub(crate) fn tab_renamed(workspace_id: &str, tab_id: &str) {
    tracing::info!(
        event = "tab.rename",
        subsystem = "tab",
        outcome = "ok",
        workspace_id,
        tab_id,
        "tab renamed"
    );
}

pub(crate) fn session_saved(path: &Path, workspaces: usize) {
    tracing::info!(
        event = "persist.save",
        subsystem = "persist",
        outcome = "ok",
        path = %path.display(),
        workspaces,
        "session saved"
    );
}

pub(crate) fn session_save_failed(path: &Path, err: &str) {
    tracing::error!(
        event = "persist.save",
        subsystem = "persist",
        outcome = "error",
        path = %path.display(),
        err,
        "failed to save session"
    );
}

pub(crate) fn session_cleared(path: &Path) {
    tracing::info!(
        event = "persist.clear",
        subsystem = "persist",
        outcome = "ok",
        path = %path.display(),
        "session cleared"
    );
}

pub(crate) fn session_clear_failed(path: &Path, err: &str) {
    tracing::error!(
        event = "persist.clear",
        subsystem = "persist",
        outcome = "error",
        path = %path.display(),
        err,
        "failed to clear session"
    );
}

pub(crate) fn session_restored(workspaces: usize, outcome: &'static str) {
    tracing::info!(
        event = "persist.restore",
        subsystem = "persist",
        outcome,
        workspaces,
        "session restore evaluated"
    );
}

pub(crate) fn update_check_started() {
    tracing::info!(
        event = "update.check.start",
        subsystem = "update",
        outcome = "started",
        "checking for updates"
    );
}

pub(crate) fn update_check_failed(err: &str) {
    tracing::warn!(
        event = "update.check.complete",
        subsystem = "update",
        outcome = "error",
        err,
        "update check failed"
    );
}

pub(crate) fn update_available(version: &str) {
    tracing::info!(
        event = "update.available",
        subsystem = "update",
        outcome = "ok",
        version,
        "update available"
    );
}

pub(crate) fn config_write_failed(path: &Path, context: &str, err: &str) {
    tracing::warn!(
        event = "config.write",
        subsystem = "config",
        outcome = "error",
        path = %path.display(),
        context,
        err,
        "failed to write config"
    );
}

pub(crate) fn integration_action(
    action: &'static str,
    target: &'static str,
    outcome: &'static str,
) {
    tracing::info!(
        event = "integration.action",
        subsystem = "integration",
        outcome,
        action,
        target,
        "integration action finished"
    );
}

/// Lightweight scoped timer for hot-path latency tracking.
///
/// Construct at the start of a section, call `.elapsed()` at the end and pass to
/// one of the `*_frame_observed` / `*_observed` helpers below. The helpers only
/// emit log records when latency exceeds the relevant slow-path threshold, so
/// the steady state is silent.
pub(crate) struct Stopwatch {
    start: Instant,
}

impl Stopwatch {
    pub(crate) fn start() -> Self {
        Self {
            start: Instant::now(),
        }
    }

    pub(crate) fn elapsed(&self) -> Duration {
        self.start.elapsed()
    }
}

/// Emit a render-frame latency event. Below threshold logs at TRACE; above at WARN.
/// `pane_count` and `full_redraw` are context for triage.
pub(crate) fn render_frame_observed(elapsed: Duration, pane_count: usize, full_redraw: bool) {
    let duration_ms = duration_ms(elapsed);
    let event = "render.frame";
    let subsystem = "render";
    if elapsed >= SLOW_RENDER_THRESHOLD {
        tracing::warn!(
            event,
            subsystem,
            outcome = "slow",
            duration_ms,
            pane_count,
            full_redraw,
            threshold_ms = SLOW_RENDER_THRESHOLD.as_millis() as u64,
            "render frame exceeded threshold"
        );
    } else {
        tracing::trace!(
            event,
            subsystem,
            outcome = "ok",
            duration_ms,
            pane_count,
            full_redraw,
            "render frame"
        );
    }
}

/// Emit a main-loop iteration latency event. Covers the catch-all path
/// (event drain + scheduled tasks + select) that the per-subsystem timers
/// don't see — e.g., a slow `drain_api_requests` that doesn't fire a request
/// roundtrip, or a backed-up scheduled-task chain.
pub(crate) fn loop_iter_observed(elapsed: Duration, cause: &'static str) {
    let duration_ms = duration_ms(elapsed);
    let event = "app.loop_iter";
    let subsystem = "app";
    if elapsed >= SLOW_LOOP_ITER_THRESHOLD {
        tracing::warn!(
            event,
            subsystem,
            outcome = "slow",
            duration_ms,
            cause,
            threshold_ms = SLOW_LOOP_ITER_THRESHOLD.as_millis() as u64,
            "main loop iteration exceeded threshold"
        );
    } else {
        tracing::trace!(
            event,
            subsystem,
            outcome = "ok",
            duration_ms,
            cause,
            "main loop iteration"
        );
    }
}

/// Emit a raw-input batch latency event. Silent under threshold; WARN above it.
pub(crate) fn input_batch_observed(elapsed: Duration, events: usize) {
    let duration_ms = duration_ms(elapsed);
    let event = "input.batch";
    let subsystem = "input";
    if elapsed >= SLOW_INPUT_THRESHOLD {
        tracing::warn!(
            event,
            subsystem,
            outcome = "slow",
            duration_ms,
            events,
            threshold_ms = SLOW_INPUT_THRESHOLD.as_millis() as u64,
            "input batch exceeded threshold"
        );
    } else {
        tracing::trace!(
            event,
            subsystem,
            outcome = "ok",
            duration_ms,
            events,
            "input batch"
        );
    }
}

/// Emit a PTY byte-processing latency event. Silent under threshold; WARN above it.
pub(crate) fn pty_process_observed(elapsed: Duration, pane_id: u32, bytes: usize) {
    let duration_ms = duration_ms(elapsed);
    let event = "pty.process";
    let subsystem = "pty";
    if elapsed >= SLOW_PTY_THRESHOLD {
        tracing::warn!(
            event,
            subsystem,
            outcome = "slow",
            duration_ms,
            pane_id,
            bytes,
            threshold_ms = SLOW_PTY_THRESHOLD.as_millis() as u64,
            "pty byte processing exceeded threshold"
        );
    } else {
        tracing::trace!(
            event,
            subsystem,
            outcome = "ok",
            duration_ms,
            pane_id,
            bytes,
            "pty byte processing"
        );
    }
}

/// Emit an API server-side roundtrip latency event. Always emits at TRACE for
/// timing tape; bumps to WARN when slow.
pub(crate) fn api_server_roundtrip_observed(
    request_id: &str,
    method: &'static str,
    outcome: &'static str,
    elapsed: Duration,
) {
    let duration_ms = duration_ms(elapsed);
    let event = "api.request.duration";
    let subsystem = "api";
    if elapsed >= SLOW_API_THRESHOLD {
        tracing::warn!(
            event,
            subsystem,
            outcome = "slow",
            request_id,
            method,
            response_outcome = outcome,
            duration_ms,
            threshold_ms = SLOW_API_THRESHOLD.as_millis() as u64,
            "api server roundtrip exceeded threshold"
        );
    } else {
        tracing::trace!(
            event,
            subsystem,
            outcome,
            request_id,
            method,
            duration_ms,
            "api server roundtrip"
        );
    }
}

/// Emit an API client-side roundtrip latency event. The client view includes
/// socket connect + write + read, so it reflects network/IPC overhead in
/// addition to server processing time.
pub(crate) fn api_client_roundtrip_observed(
    method: &'static str,
    outcome: &'static str,
    elapsed: Duration,
) {
    let duration_ms = duration_ms(elapsed);
    let event = "api.client.roundtrip";
    let subsystem = "api";
    if elapsed >= SLOW_API_THRESHOLD {
        tracing::warn!(
            event,
            subsystem,
            outcome = "slow",
            method,
            response_outcome = outcome,
            duration_ms,
            threshold_ms = SLOW_API_THRESHOLD.as_millis() as u64,
            "api client roundtrip exceeded threshold"
        );
    } else {
        tracing::trace!(
            event,
            subsystem,
            outcome,
            method,
            duration_ms,
            "api client roundtrip"
        );
    }
}

fn duration_ms(elapsed: Duration) -> u64 {
    // Saturating cast: Duration::as_millis returns u128; latencies that would
    // overflow u64 are pathological and clamp is fine for logging.
    u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
}

struct RotatingFileMakeWriter {
    state: Arc<Mutex<RotatingFileState>>,
}

impl RotatingFileMakeWriter {
    fn new(
        dir: PathBuf,
        file_name: &str,
        max_bytes: u64,
        retained_files: usize,
    ) -> io::Result<Self> {
        fs::create_dir_all(&dir)?;
        let path = dir.join(file_name);
        let mut state = RotatingFileState {
            path,
            max_bytes,
            retained_files,
            file: None,
            current_size: 0,
            disabled: false,
        };
        state.open_current_file()?;
        Ok(Self {
            state: Arc::new(Mutex::new(state)),
        })
    }
}

impl<'a> MakeWriter<'a> for RotatingFileMakeWriter {
    type Writer = RotatingFileGuard;

    fn make_writer(&'a self) -> Self::Writer {
        RotatingFileGuard {
            state: Arc::clone(&self.state),
        }
    }
}

struct RotatingFileGuard {
    state: Arc<Mutex<RotatingFileState>>,
}

impl Write for RotatingFileGuard {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let Ok(mut state) = self.state.lock() else {
            return Ok(buf.len());
        };
        if state.disabled {
            return Ok(buf.len());
        }
        if state.rotate_if_needed(buf.len() as u64).is_err() {
            state.disabled = true;
            return Ok(buf.len());
        }
        if let Some(file) = state.file.as_mut() {
            match file.write(buf) {
                Ok(written) => {
                    state.current_size = state.current_size.saturating_add(written as u64);
                    Ok(written)
                }
                Err(_) => {
                    state.disabled = true;
                    Ok(buf.len())
                }
            }
        } else {
            Ok(buf.len())
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        let Ok(mut state) = self.state.lock() else {
            return Ok(());
        };
        if state.disabled {
            return Ok(());
        }
        match state.file.as_mut() {
            Some(file) => match file.flush() {
                Ok(()) => Ok(()),
                Err(_) => {
                    state.disabled = true;
                    Ok(())
                }
            },
            None => Ok(()),
        }
    }
}

struct RotatingFileState {
    path: PathBuf,
    max_bytes: u64,
    retained_files: usize,
    file: Option<File>,
    current_size: u64,
    disabled: bool,
}

impl RotatingFileState {
    fn rotate_if_needed(&mut self, incoming_len: u64) -> io::Result<()> {
        if self.file.is_none() {
            self.open_current_file()?;
        }
        if self.max_bytes == 0 || self.current_size.saturating_add(incoming_len) <= self.max_bytes {
            return Ok(());
        }
        self.rotate_files()?;
        self.open_current_file()
    }

    fn open_current_file(&mut self) -> io::Result<()> {
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        self.current_size = file.metadata().map(|meta| meta.len()).unwrap_or(0);
        self.file = Some(file);
        Ok(())
    }

    fn rotate_files(&mut self) -> io::Result<()> {
        self.file.take();
        if self.retained_files == 0 {
            match fs::remove_file(&self.path) {
                Ok(()) => {}
                Err(err) if err.kind() == io::ErrorKind::NotFound => {}
                Err(err) => return Err(err),
            }
            self.current_size = 0;
            return Ok(());
        }

        let oldest = rotated_log_path(&self.path, self.retained_files);
        match fs::remove_file(&oldest) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(err),
        }

        for index in (1..=self.retained_files).rev() {
            let source = if index == 1 {
                self.path.clone()
            } else {
                rotated_log_path(&self.path, index - 1)
            };
            let target = rotated_log_path(&self.path, index);
            if !source.exists() {
                continue;
            }
            fs::rename(source, target)?;
        }

        self.current_size = 0;
        Ok(())
    }
}

fn rotated_log_path(path: &Path, index: usize) -> PathBuf {
    let suffix = format!(".{}", index);
    let file_name = path
        .file_name()
        .map(|name| {
            let mut name = name.to_os_string();
            name.push(&suffix);
            name
        })
        .unwrap_or_else(|| suffix.clone().into());
    path.with_file_name(file_name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_log_path(name: &str) -> PathBuf {
        let unique = format!(
            "herdr-logging-tests-{}-{}-{}",
            name,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        std::env::temp_dir().join(unique).join("herdr.log")
    }

    #[test]
    fn duration_ms_clamps_overflow() {
        assert_eq!(duration_ms(Duration::from_millis(0)), 0);
        assert_eq!(duration_ms(Duration::from_millis(42)), 42);
        assert_eq!(duration_ms(Duration::from_secs(1)), 1000);
        assert_eq!(duration_ms(Duration::MAX), u64::MAX);
    }

    #[test]
    fn slow_thresholds_are_sensible() {
        // Sanity: tighter paths must have tighter thresholds.
        assert!(SLOW_PTY_THRESHOLD < SLOW_RENDER_THRESHOLD);
        assert!(SLOW_RENDER_THRESHOLD < SLOW_INPUT_THRESHOLD);
        assert!(SLOW_INPUT_THRESHOLD < SLOW_API_THRESHOLD);
        // The loop-iter budget should sit between render and input — it covers
        // a superset of render work but still trips on perceptible stalls.
        assert!(SLOW_RENDER_THRESHOLD <= SLOW_LOOP_ITER_THRESHOLD);
        assert!(SLOW_LOOP_ITER_THRESHOLD <= SLOW_INPUT_THRESHOLD);
    }

    #[test]
    fn stopwatch_measures_elapsed_time() {
        let watch = Stopwatch::start();
        std::thread::sleep(Duration::from_millis(5));
        assert!(watch.elapsed() >= Duration::from_millis(5));
    }

    #[test]
    fn rotated_log_path_appends_numeric_suffix() {
        let path = PathBuf::from("/tmp/herdr.log");
        assert_eq!(
            rotated_log_path(&path, 2),
            PathBuf::from("/tmp/herdr.log.2")
        );
    }

    #[test]
    fn rotate_files_shifts_existing_generations() {
        let path = temp_log_path("rotate");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "current").unwrap();
        fs::write(rotated_log_path(&path, 1), "older").unwrap();

        let mut state = RotatingFileState {
            path: path.clone(),
            max_bytes: 128,
            retained_files: 2,
            file: None,
            current_size: 0,
            disabled: false,
        };
        state.rotate_files().unwrap();

        assert_eq!(
            fs::read_to_string(rotated_log_path(&path, 1)).unwrap(),
            "current"
        );
        assert_eq!(
            fs::read_to_string(rotated_log_path(&path, 2)).unwrap(),
            "older"
        );
        assert!(!path.exists());

        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn write_replaces_log_without_retained_files_when_size_limit_is_reached() {
        let path = temp_log_path("replace");
        let dir = path.parent().unwrap().to_path_buf();
        fs::create_dir_all(&dir).unwrap();

        let writer = RotatingFileMakeWriter::new(dir.clone(), "herdr.log", 8, 0).unwrap();
        {
            let mut guard = writer.make_writer();
            guard.write_all(b"12345678").unwrap();
            guard.write_all(b"abc").unwrap();
            guard.flush().unwrap();
        }

        assert_eq!(fs::read_to_string(&path).unwrap(), "abc");
        assert!(!rotated_log_path(&path, 1).exists());

        let _ = fs::remove_dir_all(dir);
    }
}
