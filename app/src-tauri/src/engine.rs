//! Runs `bettr-engine.exe` as a child process and keeps it alive.
//!
//! The engine is a separate process on purpose: a driver or capture crash takes down the
//! engine, never the UI, and the supervisor brings it back with backoff.

use parking_lot::Mutex;
use serde::Serialize;
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    /// Not wanted (e.g. buffer only runs during games, and none is running).
    Idle,
    Starting,
    Running,
    /// Exited unexpectedly; will be restarted after a delay.
    Crashed,
    /// The engine executable could not be found or started.
    Missing,
}

/// What the UI needs to draw the engine area; also sent as `engine-state` on every change.
#[derive(Clone, Debug, Serialize)]
pub struct EngineState {
    pub phase: Phase,
    /// The `ready` event: adapter, encoder, width, height, fps, audio_tracks.
    pub info: Option<Value>,
    /// Latest `status` event (buffered seconds, dropped frames, sources…).
    pub status: Option<Value>,
    pub recording: Option<PathBuf>,
    pub recording_since: Option<u64>,
    pub game: Option<String>,
    pub last_error: Option<String>,
    pub restarts: u32,
}

enum Ctl {
    Restart,
    Want(bool),
    Quit,
}

pub struct Engine {
    ctl: Sender<Ctl>,
    stdin: Arc<Mutex<Option<ChildStdin>>>,
    state: Arc<Mutex<EngineState>>,
    thread: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl Engine {
    pub fn start(app: AppHandle, exe: Option<PathBuf>, config: PathBuf, log: PathBuf, want: bool) -> Self {
        let (tx, rx) = channel();
        let stdin = Arc::new(Mutex::new(None));
        let state = Arc::new(Mutex::new(EngineState {
            phase: if exe.is_none() { Phase::Missing } else { Phase::Idle },
            info: None,
            status: None,
            recording: None,
            recording_since: None,
            game: None,
            last_error: exe.is_none().then(|| "bettr-engine.exe was not found next to the app".to_string()),
            restarts: 0,
        }));
        let sup = Supervisor {
            app,
            exe,
            config,
            log,
            stdin: stdin.clone(),
            state: state.clone(),
        };
        let thread = std::thread::Builder::new()
            .name("engine-supervisor".into())
            .spawn(move || sup.run(rx, want))
            .expect("spawn supervisor");
        Self {
            ctl: tx,
            stdin,
            state,
            thread: Mutex::new(Some(thread)),
        }
    }

    pub fn state(&self) -> EngineState {
        self.state.lock().clone()
    }

    /// Sends a command line to the engine. Fails if it is not running.
    pub fn send(&self, cmd: Value) -> Result<(), String> {
        let mut guard = self.stdin.lock();
        let Some(stdin) = guard.as_mut() else {
            return Err("the capture engine is not running".into());
        };
        writeln!(stdin, "{cmd}")
            .and_then(|_| stdin.flush())
            .map_err(|e| format!("engine pipe: {e}"))
    }

    pub fn restart(&self) {
        let _ = self.ctl.send(Ctl::Restart);
    }

    pub fn set_wanted(&self, want: bool) {
        let _ = self.ctl.send(Ctl::Want(want));
    }

    /// Remembers the game (re-sent after every engine restart) and tells the engine.
    pub fn set_game(&self, game: Option<String>) {
        self.state.lock().game = game.clone();
        let _ = self.send(json!({"cmd": "set_game", "game": game}));
    }

    /// Stops the engine (finishing any recording) and waits for it.
    pub fn shutdown(&self) {
        let _ = self.ctl.send(Ctl::Quit);
        if let Some(t) = self.thread.lock().take() {
            let _ = t.join();
        }
    }
}

struct Supervisor {
    app: AppHandle,
    exe: Option<PathBuf>,
    config: PathBuf,
    log: PathBuf,
    stdin: Arc<Mutex<Option<ChildStdin>>>,
    state: Arc<Mutex<EngineState>>,
}

impl Supervisor {
    fn publish(&self) {
        let s = self.state.lock().clone();
        let _ = self.app.emit("engine-state", s);
    }

    fn set_phase(&self, phase: Phase, error: Option<String>) {
        {
            let mut s = self.state.lock();
            s.phase = phase;
            if phase != Phase::Running {
                s.info = None;
                s.status = None;
                s.recording = None;
                s.recording_since = None;
            }
            if error.is_some() || phase == Phase::Running {
                s.last_error = error;
            }
        }
        self.publish();
    }

    fn run(self, rx: Receiver<Ctl>, mut want: bool) {
        let Some(exe) = self.exe.clone() else {
            // Nothing to supervise; just drain control messages until quit.
            while let Ok(c) = rx.recv() {
                if matches!(c, Ctl::Quit) {
                    return;
                }
            }
            return;
        };
        let mut child: Option<Child> = None;
        let mut crashes_in_row = 0u32;
        let mut next_start = Instant::now();
        let mut last_status = Instant::now();
        loop {
            match rx.recv_timeout(Duration::from_millis(250)) {
                Ok(Ctl::Quit) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    if let Some(c) = child.take() {
                        self.stop_child(c);
                    }
                    self.set_phase(Phase::Idle, None);
                    return;
                }
                Ok(Ctl::Restart) => {
                    if let Some(c) = child.take() {
                        self.stop_child(c);
                    }
                    crashes_in_row = 0;
                    next_start = Instant::now();
                }
                Ok(Ctl::Want(w)) => want = w,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            }

            if let Some(c) = child.as_mut() {
                match c.try_wait() {
                    Ok(Some(status)) => {
                        child = None;
                        *self.stdin.lock() = None;
                        crashes_in_row += 1;
                        let delay = Duration::from_secs([1, 2, 5, 10, 30][(crashes_in_row as usize - 1).min(4)]);
                        next_start = Instant::now() + delay;
                        self.state.lock().restarts += 1;
                        tracing::error!(%status, ?delay, "engine exited unexpectedly");
                        let why = self.last_log_error().unwrap_or_else(|| status.to_string());
                        self.set_phase(
                            Phase::Crashed,
                            Some(format!("capture engine stopped: {why}. Retrying in {}s.", delay.as_secs())),
                        );
                    }
                    Ok(None) => {
                        if last_status.elapsed() >= Duration::from_secs(1) {
                            last_status = Instant::now();
                            if let Some(s) = self.stdin.lock().as_mut() {
                                let _ = writeln!(s, r#"{{"cmd":"status"}}"#).and_then(|_| s.flush());
                            }
                        }
                        // A healthy minute resets the backoff.
                        if crashes_in_row > 0 && next_start.elapsed() > Duration::from_secs(60) {
                            crashes_in_row = 0;
                        }
                    }
                    Err(e) => tracing::warn!("engine wait: {e}"),
                }
            }

            if !want {
                if let Some(c) = child.take() {
                    self.stop_child(c);
                    self.set_phase(Phase::Idle, None);
                } else if self.state.lock().phase == Phase::Crashed {
                    self.set_phase(Phase::Idle, None);
                }
                continue;
            }
            if child.is_none() && Instant::now() >= next_start {
                match self.spawn(&exe) {
                    Ok(c) => child = Some(c),
                    Err(e) => {
                        tracing::error!("engine start failed: {e:#}");
                        crashes_in_row += 1;
                        next_start = Instant::now() + Duration::from_secs(5);
                        self.set_phase(Phase::Crashed, Some(format!("could not start the capture engine: {e:#}")));
                    }
                }
            }
        }
    }

    fn spawn(&self, exe: &PathBuf) -> anyhow::Result<Child> {
        // Keep the log bounded: start fresh once it passes 4 MB, keeping one old copy.
        if std::fs::metadata(&self.log).map(|m| m.len() > 4 << 20).unwrap_or(false) {
            let _ = std::fs::rename(&self.log, self.log.with_extension("old.log"));
        }
        let log = std::fs::OpenOptions::new().create(true).append(true).open(&self.log)?;
        let mut child = Command::new(exe)
            .arg("--config")
            .arg(&self.config)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::from(log))
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()?;
        tracing::info!(pid = child.id(), "engine started");
        *self.stdin.lock() = child.stdin.take();
        self.set_phase(Phase::Starting, None);
        let stdout = child.stdout.take().expect("piped stdout");
        let app = self.app.clone();
        let state = self.state.clone();
        let stdin = self.stdin.clone();
        std::thread::Builder::new()
            .name("engine-events".into())
            .spawn(move || read_events(stdout, app, state, stdin))?;
        Ok(child)
    }

    /// The engine prints `Error: …` to stderr (our log) when it exits on a failure.
    fn last_log_error(&self) -> Option<String> {
        let text = std::fs::read(&self.log).ok()?;
        let tail = String::from_utf8_lossy(&text[text.len().saturating_sub(4096)..]).into_owned();
        tail.lines()
            .rev()
            .find_map(|l| l.strip_prefix("Error: ").map(|e| e.trim().to_owned()))
    }

    /// Asks the engine to quit (it finalizes recordings), then kills our own child if needed.
    fn stop_child(&self, mut child: Child) {
        if let Some(mut s) = self.stdin.lock().take() {
            let _ = writeln!(s, r#"{{"cmd":"quit"}}"#).and_then(|_| s.flush());
        }
        let deadline = Instant::now() + Duration::from_secs(6);
        while Instant::now() < deadline {
            if let Ok(Some(_)) = child.try_wait() {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        tracing::warn!(pid = child.id(), "engine did not quit in time; terminating it");
        let _ = child.kill();
        let _ = child.wait();
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn read_events(
    stdout: std::process::ChildStdout,
    app: AppHandle,
    state: Arc<Mutex<EngineState>>,
    stdin: Arc<Mutex<Option<ChildStdin>>>,
) {
    for line in BufReader::new(stdout).lines() {
        let Ok(line) = line else { break };
        let Ok(ev) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let kind = ev.get("event").and_then(|e| e.as_str()).unwrap_or_default().to_owned();
        let mut changed = true;
        {
            let mut s = state.lock();
            match kind.as_str() {
                "ready" => {
                    s.phase = Phase::Running;
                    s.info = Some(ev.clone());
                    s.last_error = None;
                    // A fresh engine doesn't know the current game yet.
                    if let (Some(game), Some(pipe)) = (s.game.clone(), stdin.lock().as_mut()) {
                        let _ = writeln!(pipe, "{}", json!({"cmd": "set_game", "game": game})).and_then(|_| pipe.flush());
                    }
                }
                "status" => {
                    s.status = Some(ev.clone());
                    changed = false; // sent below as its own lightweight event
                }
                "recording_started" => {
                    s.recording = ev.get("path").and_then(|p| p.as_str()).map(PathBuf::from);
                    s.recording_since = Some(now_ms());
                }
                "recording_stopped" => {
                    s.recording = None;
                    s.recording_since = None;
                }
                "error" | "warning" => {
                    s.last_error = ev.get("message").and_then(|m| m.as_str()).map(str::to_owned);
                }
                _ => changed = false,
            }
        }
        if changed {
            let _ = app.emit("engine-state", state.lock().clone());
        }
        let _ = app.emit("engine-event", ev);
    }
}

/// `bettr-engine.exe` next to the app, or (during development) in the release build folder.
pub fn find_exe() -> Option<PathBuf> {
    let dir = std::env::current_exe().ok()?.parent()?.to_path_buf();
    let beside = dir.join("bettr-engine.exe");
    let release = dir.join("..").join("release").join("bettr-engine.exe");
    // A debug engine is far too slow to judge capture by; dev builds of the app prefer release.
    let order = if cfg!(debug_assertions) { [release, beside] } else { [beside, release] };
    order.into_iter().find(|p| p.is_file())
}
