pub mod orphan;
pub mod profiles;
pub mod prompts;
pub mod pty;
pub mod status;

use crate::error::AetherError;
use crate::events::{
    now_millis, AccessCodeEvent, LogBatch, LogEvent, ACCESS_CODE_EVENT, LOG_EVENT, STATUS_EVENT,
};
use crate::state::ConnectionState;
use profiles::ConnectionProfile;
use pty::{PtySession, PtySignal, SessionSignals};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter, Manager};

/// How long after spawn the UI stays in `Launching` before it is treated as
/// "past the prompt phase" and moves to `Connecting`.
///
/// BUGFIX (R1): `prompts_done` only flipped once *every* rule in
/// `PROMPT_TABLE` had been answered. Since Aether >= 1.1.1 receives the whole
/// profile as CLI flags, those prompts normally never appear at all, so the
/// flag never flipped, the state machine never announced `Connecting`, and
/// the entire live-progress UI (elapsed timer + scan-budget progress bar,
/// which render only in the `Connecting` branch) was unreachable in the
/// happy path. The app just sat on "Starting Aether… / Answering setup
/// prompts" for up to 5 minutes. Prompt completion is now a fast path, not
/// the only path.
const PROMPT_PHASE_GRACE: Duration = Duration::from_millis(2_000);

/// Poll interval while waiting for the SOCKS port to come up.
const CONNECT_POLL: Duration = Duration::from_millis(400);
/// Poll interval for process liveness once connected.
const CONNECTED_POLL: Duration = Duration::from_millis(500);
/// How often to re-verify the SOCKS listener while `Connected`.
const CONNECTED_HEALTH_INTERVAL: Duration = Duration::from_secs(5);
/// Consecutive failed health probes before declaring the tunnel dead.
const CONNECTED_HEALTH_STRIKES: u8 = 2;
/// Log-batch flush cadence (mirrors the frontend's coalescing window).
const LOG_FLUSH: Duration = Duration::from_millis(120);
const LOG_BATCH_MAX: usize = 256;
/// Grace given to a killed child before we fall back to a blocking wait.
const REAP_GRACE: Duration = Duration::from_millis(500);

pub struct AetherManager {
    session: Option<PtySession>,
    /// Lock-free view of the live session's reader thread. Read by the
    /// monitor threads without taking the manager mutex.
    signals: Option<Arc<SessionSignals>>,
    state: ConnectionState,
    user_requested_stop: bool,
    /// Consecutive auto-retry attempts for the current connection lineage.
    retry_count: u32,
    /// BUGFIX (R2): monotonic epoch. Every background thread captures the
    /// generation it was born under and refuses to mutate state or spawn a
    /// process once it no longer matches.
    ///
    /// Without this, cancelling during the retry backoff and immediately
    /// reconnecting produced *two* live Aether processes: `request_disconnect`
    /// set `user_requested_stop = true` and returned, the new `connect()`
    /// reset that flag inside `spawn_and_monitor`, and the old retry thread
    /// then woke from its (up to 10s) sleep, saw a cleared flag, and spawned
    /// a second child — overwriting `session` so the first one was dropped
    /// without ever being killed. That orphan then held the SOCKS port.
    generation: u64,
}

impl AetherManager {
    pub fn new() -> Self {
        Self {
            session: None,
            signals: None,
            state: ConnectionState::Idle,
            user_requested_stop: false,
            retry_count: 0,
            generation: 0,
        }
    }

    pub fn status(&self) -> ConnectionState {
        self.state.clone()
    }
}

impl Default for AetherManager {
    fn default() -> Self {
        Self::new()
    }
}

type Shared = Arc<Mutex<AetherManager>>;

/// BUGFIX (R3): every call site used `.lock().unwrap()`. A single panic in
/// any thread while holding this mutex poisoned it permanently, after which
/// *every* Tauri command (`get_status` included) panicked on invocation and
/// the window became a frozen, unclosable shell. A poisoned manager is
/// recoverable here: the invariants it protects are re-established by the
/// generation guard, so take the inner value and carry on.
fn lock(manager: &Mutex<AetherManager>) -> MutexGuard<'_, AetherManager> {
    manager.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn app_data_dir(app: &AppHandle) -> PathBuf {
    app.path()
        .app_data_dir()
        .unwrap_or_else(|_| std::env::temp_dir())
}

fn resolve_binary(app: &AppHandle) -> Result<PathBuf, AetherError> {
    let dir = app
        .path()
        .resource_dir()
        .map_err(|e| AetherError::Internal(e.to_string()))?;
    let name = if cfg!(windows) { "aether.exe" } else { "aether" };
    let path = dir.join("binaries").join(name);
    if !path.exists() {
        return Err(AetherError::BinaryMissing(path.display().to_string()));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755));
    }
    Ok(path)
}

/// Applies a state transition only if the caller still owns the current
/// generation. Returns false when the caller has been superseded and should
/// unwind without touching anything else.
fn set_state_if_current(
    app: &AppHandle,
    manager: &Shared,
    generation: u64,
    new_state: ConnectionState,
) -> bool {
    {
        let mut mgr = lock(manager);
        if mgr.generation != generation {
            return false;
        }
        mgr.state = new_state.clone();
    }
    let _ = app.emit(STATUS_EVENT, &new_state);
    true
}

pub fn start_connect(
    app: AppHandle,
    manager: Shared,
    profile_override: Option<ConnectionProfile>,
) -> Result<(), AetherError> {
    let profile = profile_override.unwrap_or_else(|| profiles::load(&app));
    let binary = resolve_binary(&app)?;
    let data_dir = app_data_dir(&app);
    std::fs::create_dir_all(&data_dir).map_err(|e| AetherError::Internal(e.to_string()))?;

    // PERF (R8): probe the port BEFORE taking the manager lock. This is a
    // blocking TCP connect with a 300ms timeout; holding the global mutex
    // across it stalled `get_status`/`disconnect` IPC for the duration.
    // The subsequent state check under the lock still serialises a
    // double-click, which is what that guard actually exists for.
    let socks = status::parse_bind_address(&profile.bind_address);
    if status::port_is_live(&socks) {
        return Err(AetherError::PortInUse(socks.port()));
    }

    let generation = {
        let mut mgr = lock(&manager);
        if !matches!(
            mgr.state,
            ConnectionState::Idle | ConnectionState::Error { .. }
        ) {
            return Err(AetherError::AlreadyRunning);
        }
        // New lineage: invalidates any in-flight retry or monitor thread.
        mgr.generation = mgr.generation.wrapping_add(1);
        mgr.state = ConnectionState::Launching;
        mgr.retry_count = 0;
        mgr.user_requested_stop = false;
        mgr.generation
    };
    let _ = app.emit(STATUS_EVENT, &ConnectionState::Launching);

    spawn_and_monitor(app, manager, generation, binary, data_dir, profile)
}

fn spawn_and_monitor(
    app: AppHandle,
    manager: Shared,
    generation: u64,
    binary: PathBuf,
    data_dir: PathBuf,
    profile: ConnectionProfile,
) -> Result<(), AetherError> {
    let (tx, rx) = mpsc::channel::<PtySignal>();
    let session = match pty::spawn(&binary, &data_dir, profile.clone(), tx) {
        Ok(session) => session,
        Err(e) => {
            set_state_if_current(
                &app,
                &manager,
                generation,
                ConnectionState::Error {
                    message: e.to_string(),
                    phase: "launching".into(),
                },
            );
            return Err(e);
        }
    };
    orphan::write_pid(&data_dir, session.pid());
    let signals = session.signals();

    {
        let mut mgr = lock(&manager);
        if mgr.generation != generation {
            // Superseded between spawn and registration — kill immediately
            // rather than leaking the child.
            drop(mgr);
            let mut session = session;
            session.kill_and_reap(REAP_GRACE);
            return Ok(());
        }
        mgr.session = Some(session);
        mgr.signals = Some(Arc::clone(&signals));
        mgr.user_requested_stop = false;
    }

    spawn_signal_pump(app.clone(), rx);

    {
        let app = app.clone();
        let manager = Arc::clone(&manager);
        let signals = Arc::clone(&signals);
        std::thread::Builder::new()
            .name("aether-monitor".into())
            .spawn(move || {
                monitor_connect(app, manager, generation, signals, binary, data_dir, profile)
            })
            .map_err(|e| AetherError::Internal(e.to_string()))?;
    }

    Ok(())
}

/// PERF (R10): the old forwarder emitted one IPC message per log line. A
/// Thorough scan emits thousands, each costing a serde round-trip plus a
/// webview main-thread task — while the frontend was *already* coalescing
/// them into a 100ms window on arrival, so the extra traffic bought nothing.
/// Batch on this side of the boundary too.
fn spawn_signal_pump(app: AppHandle, rx: mpsc::Receiver<PtySignal>) {
    let _ = std::thread::Builder::new()
        .name("aether-log-pump".into())
        .spawn(move || {
            let mut batch: Vec<LogEvent> = Vec::with_capacity(LOG_BATCH_MAX);
            let mut next_flush = Instant::now() + LOG_FLUSH;
            let flush = |app: &AppHandle, batch: &mut Vec<LogEvent>| {
                if batch.is_empty() {
                    return;
                }
                let _ = app.emit(
                    LOG_EVENT,
                    &LogBatch {
                        lines: std::mem::take(batch),
                    },
                );
            };
            loop {
                let timeout = next_flush.saturating_duration_since(Instant::now());
                match rx.recv_timeout(timeout) {
                    Ok(PtySignal::Log(line)) => {
                        batch.push(line);
                        if batch.len() >= LOG_BATCH_MAX {
                            flush(&app, &mut batch);
                            next_flush = Instant::now() + LOG_FLUSH;
                        }
                    }
                    Ok(PtySignal::AccessCodeRequested { sequence }) => {
                        // Latency matters here (a human is waiting on an
                        // email code): flush ordering, then send immediately.
                        flush(&app, &mut batch);
                        next_flush = Instant::now() + LOG_FLUSH;
                        let _ = app.emit(
                            ACCESS_CODE_EVENT,
                            &AccessCodeEvent {
                                sequence,
                                requested_at_ms: now_millis(),
                            },
                        );
                    }
                    Err(RecvTimeoutError::Timeout) => {
                        flush(&app, &mut batch);
                        next_flush = Instant::now() + LOG_FLUSH;
                    }
                    Err(RecvTimeoutError::Disconnected) => {
                        flush(&app, &mut batch);
                        return;
                    }
                }
            }
        });
}

#[allow(clippy::too_many_arguments)]
fn handle_unexpected_failure(
    app: AppHandle,
    manager: Shared,
    generation: u64,
    binary: PathBuf,
    data_dir: PathBuf,
    profile: ConnectionProfile,
    failure_message: String,
    phase: &'static str,
) {
    let attempt = {
        let mut mgr = lock(&manager);
        if mgr.generation != generation || mgr.user_requested_stop {
            return;
        }
        mgr.session = None;
        mgr.signals = None;
        mgr.retry_count += 1;
        mgr.retry_count
    };
    orphan::clear_pid(&data_dir);

    if attempt > status::MAX_AUTO_RETRIES {
        set_state_if_current(
            &app,
            &manager,
            generation,
            ConnectionState::Error {
                message: format!(
                    "{failure_message} (gave up after {} retries)",
                    status::MAX_AUTO_RETRIES
                ),
                phase: phase.into(),
            },
        );
        return;
    }

    if !set_state_if_current(
        &app,
        &manager,
        generation,
        ConnectionState::Reconnecting {
            attempt,
            max_attempts: status::MAX_AUTO_RETRIES,
        },
    ) {
        return;
    }

    let backoff = status::RETRY_BACKOFF[(attempt - 1) as usize];
    let _ = std::thread::Builder::new()
        .name("aether-retry".into())
        .spawn(move || {
            // Sleep in slices so a cancel is observed promptly instead of
            // after the full (up to 10s) backoff.
            let deadline = Instant::now() + backoff;
            while Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(150));
                let mgr = lock(&manager);
                if mgr.generation != generation || mgr.user_requested_stop {
                    return;
                }
            }
            if !set_state_if_current(&app, &manager, generation, ConnectionState::Launching) {
                return;
            }
            let _ = spawn_and_monitor(app, manager, generation, binary, data_dir, profile);
        });
}

#[allow(clippy::too_many_arguments)]
fn monitor_connect(
    app: AppHandle,
    manager: Shared,
    generation: u64,
    signals: Arc<SessionSignals>,
    binary: PathBuf,
    data_dir: PathBuf,
    profile: ConnectionProfile,
) {
    let started = Instant::now();
    let deadline = started + status::connect_timeout(&profile.scan_mode);
    let socks = status::parse_bind_address(&profile.bind_address);
    let mut announced_connecting = false;

    loop {
        std::thread::sleep(CONNECT_POLL);

        // Phase 1 — everything that needs the lock, and nothing that blocks.
        let exit = {
            let mut mgr = lock(&manager);
            if mgr.generation != generation || mgr.user_requested_stop {
                return;
            }
            mgr.session.as_mut().and_then(|s| s.try_wait())
        };

        if let Some(exit) = exit {
            {
                let mut mgr = lock(&manager);
                mgr.session = None;
                mgr.signals = None;
            }
            handle_unexpected_failure(
                app,
                manager,
                generation,
                binary,
                data_dir,
                profile,
                format!("Aether exited before connecting ({exit})"),
                "connecting",
            );
            return;
        }

        // Phase 2 — lock-free. `signals` is atomics only.
        if !announced_connecting
            && (signals.prompts_done() || started.elapsed() >= PROMPT_PHASE_GRACE)
        {
            if !set_state_if_current(&app, &manager, generation, ConnectionState::Connecting) {
                return;
            }
            announced_connecting = true;
        }

        // Phase 3 — the blocking probe, deliberately OUTSIDE the lock.
        if status::port_is_live(&socks) {
            let new_state = ConnectionState::Connected {
                socks_addr: profile.bind_address.clone(),
                connected_at_ms: now_millis(),
            };
            {
                let mut mgr = lock(&manager);
                if mgr.generation != generation || mgr.user_requested_stop {
                    return;
                }
                mgr.state = new_state.clone();
                mgr.retry_count = 0;
            }
            let _ = app.emit(STATUS_EVENT, &new_state);
            profiles::save(&app, &profile);
            monitor_connected(app, manager, generation, binary, data_dir, profile);
            return;
        }

        if Instant::now() >= deadline {
            {
                let mut mgr = lock(&manager);
                if mgr.generation != generation || mgr.user_requested_stop {
                    return;
                }
                if let Some(session) = mgr.session.as_mut() {
                    session.kill_and_reap(REAP_GRACE);
                }
                mgr.session = None;
                mgr.signals = None;
            }
            handle_unexpected_failure(
                app,
                manager,
                generation,
                binary,
                data_dir,
                profile,
                "Timed out waiting for Aether to find a working route".into(),
                "connecting",
            );
            return;
        }
    }
}

/// Watches an established connection. Two independent failure signals:
///  * the process exits (fast path, polled every 500ms), and
///  * ROBUSTNESS (R11) the SOCKS listener disappears while the process is
///    still alive. The previous version only watched for process exit, so a
///    tunnel that died without taking its host process down left the GUI
///    proudly reporting `Connected` over a proxy that refused every
///    connection — the exact failure mode auto-reconnect exists to hide.
fn monitor_connected(
    app: AppHandle,
    manager: Shared,
    generation: u64,
    binary: PathBuf,
    data_dir: PathBuf,
    profile: ConnectionProfile,
) {
    let socks = status::parse_bind_address(&profile.bind_address);
    let mut next_health = Instant::now() + CONNECTED_HEALTH_INTERVAL;
    let mut strikes: u8 = 0;

    loop {
        std::thread::sleep(CONNECTED_POLL);

        let exit = {
            let mut mgr = lock(&manager);
            if mgr.generation != generation || mgr.user_requested_stop {
                return;
            }
            mgr.session.as_mut().and_then(|s| s.try_wait())
        };

        let reason = if let Some(exit) = exit {
            Some(format!("Lost connection unexpectedly ({exit})"))
        } else if Instant::now() >= next_health {
            next_health = Instant::now() + CONNECTED_HEALTH_INTERVAL;
            if status::port_is_live(&socks) {
                strikes = 0;
                None
            } else {
                strikes += 1;
                (strikes >= CONNECTED_HEALTH_STRIKES)
                    .then(|| "SOCKS5 proxy stopped responding".to_string())
            }
        } else {
            None
        };

        let Some(reason) = reason else { continue };

        {
            let mut mgr = lock(&manager);
            if mgr.generation != generation || mgr.user_requested_stop {
                return;
            }
            if let Some(session) = mgr.session.as_mut() {
                session.kill_and_reap(REAP_GRACE);
            }
            mgr.session = None;
            mgr.signals = None;
        }
        handle_unexpected_failure(
            app, manager, generation, binary, data_dir, profile, reason, "connected",
        );
        return;
    }
}

pub fn request_disconnect(app: &AppHandle, manager: &Shared) -> Result<(), AetherError> {
    let (generation, had_session) = {
        let mut mgr = lock(manager);
        let reconnecting = matches!(mgr.state, ConnectionState::Reconnecting { .. });
        if mgr.session.is_none() && !reconnecting {
            return Err(AetherError::NotConnected);
        }
        // Retire the current lineage: every monitor/retry thread born under
        // the old generation becomes a no-op the moment it next checks in.
        mgr.generation = mgr.generation.wrapping_add(1);
        mgr.user_requested_stop = true;
        mgr.retry_count = 0;
        if let Some(session) = mgr.session.as_ref() {
            session.send_ctrl_c();
        }
        (mgr.generation, mgr.session.is_some())
    };

    if !had_session {
        // Mid-backoff cancel: nothing to wait on.
        let mut mgr = lock(manager);
        mgr.user_requested_stop = false;
        mgr.state = ConnectionState::Idle;
        drop(mgr);
        let _ = app.emit(STATUS_EVENT, &ConnectionState::Idle);
        return Ok(());
    }

    set_state_if_current(app, manager, generation, ConnectionState::Disconnecting);

    let app = app.clone();
    let manager = Arc::clone(manager);
    let _ = std::thread::Builder::new()
        .name("aether-shutdown".into())
        .spawn(move || {
            let deadline = Instant::now() + status::GRACEFUL_SHUTDOWN_GRACE;
            loop {
                std::thread::sleep(Duration::from_millis(200));
                let mut mgr = lock(&manager);
                if mgr.generation != generation {
                    return;
                }
                let exited = mgr.session.as_mut().and_then(|s| s.try_wait()).is_some();
                if exited || Instant::now() >= deadline {
                    if !exited {
                        if let Some(session) = mgr.session.as_mut() {
                            session.kill_and_reap(REAP_GRACE);
                        }
                    }
                    mgr.session = None;
                    mgr.signals = None;
                    // BUGFIX (R2b): this flag must be cleared on *every* exit
                    // path, not only here — otherwise a stale `true` silently
                    // suppressed the next lineage's failure handling.
                    mgr.user_requested_stop = false;
                    mgr.state = ConnectionState::Idle;
                    drop(mgr);
                    orphan::clear_pid(&app_data_dir(&app));
                    let _ = app.emit(STATUS_EVENT, &ConnectionState::Idle);
                    return;
                }
            }
        });

    Ok(())
}

pub fn submit_access_code(manager: &Shared, code: String) -> Result<(), AetherError> {
    let manager = lock(manager);
    let session = manager.session.as_ref().ok_or(AetherError::NotConnected)?;
    session.send_access_code(&code)
}

/// Called from `RunEvent::Exit`.
pub fn shutdown_blocking(manager: &Shared, data_dir: &Path) {
    let mut mgr = lock(manager);
    mgr.generation = mgr.generation.wrapping_add(1);
    if let Some(session) = mgr.session.as_mut() {
        session.send_ctrl_c();
        // Poll instead of an unconditional 500ms sleep: Aether usually needs
        // far less, and this is directly in the window-close path.
        let deadline = Instant::now() + Duration::from_millis(500);
        while Instant::now() < deadline {
            if session.try_wait().is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        session.kill_and_reap(REAP_GRACE);
    }
    mgr.session = None;
    mgr.signals = None;
    drop(mgr);
    orphan::clear_pid(data_dir);
}
