//! Keeps the clip library under a size limit (Settings > Storage).
//!
//! When the library is over the limit, the oldest clips are first re-encoded smaller (each
//! clip once; the engine marks the file, so a renamed clip is still recognized). If that is
//! not enough, or compression is off, the oldest clips go to the Recycle Bin until the
//! library fits. Clips saved in the last few minutes and the recording in progress are never
//! touched, and compression waits while a game runs so it never costs frames.

use crate::library::{self, Clip};
use crate::{AppState, editor};
use parking_lot::Mutex;
use serde::Serialize;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{RecvTimeoutError, Sender, channel};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tauri::{AppHandle, Emitter, Manager};

/// A compressed clip ends up at about this share of its size.
const COMPRESS_TO: f64 = 0.4;
/// Keep the result only if it saves at least this much.
const KEEP_IF_BELOW: f64 = 0.85;
/// Whole-file bitrate below which a clip is already small (compressing would only blur it).
const MIN_KBPS: f64 = 2_500.0;
/// Re-encoding holds the new video in memory: longer recordings are only ever deleted.
const MAX_COMPRESS_SECS: f64 = 15.0 * 60.0;
/// Clips this fresh are left alone: just saved, probably being watched or edited.
const FRESH_MS: u64 = 10 * 60 * 1000;
/// Re-check this often even without new clips (a game may have closed, settings changed).
const PERIOD: Duration = Duration::from_secs(10 * 60);
/// Let the app finish starting before the first pass.
const STARTUP_DELAY: Duration = Duration::from_secs(20);

#[derive(Clone, Debug, Default, Serialize)]
pub struct Status {
    /// What is happening right now, e.g. "Compressing VALORANT 2026-11-21 16-44-25".
    pub busy: Option<String>,
    /// Why the library is still over the limit, if it is.
    pub note: Option<String>,
    /// The last pass that changed something.
    pub last: Option<Activity>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Activity {
    /// ms since the Unix epoch.
    pub at: u64,
    pub compressed: u32,
    pub saved_bytes: u64,
    pub recycled: u32,
    pub recycled_bytes: u64,
}

pub struct Storage {
    tx: Mutex<Sender<()>>,
    status: Arc<Mutex<Status>>,
}

impl Storage {
    pub fn start(app: AppHandle) -> Self {
        let (tx, rx) = channel::<()>();
        let status = Arc::new(Mutex::new(Status::default()));
        let st = status.clone();
        std::thread::Builder::new()
            .name("storage".into())
            .spawn(move || {
                std::thread::sleep(STARTUP_DELAY);
                let mut failed: HashSet<(PathBuf, u64, u64)> = HashSet::new();
                loop {
                    // Coalesce bursts (a save plus a settings change) into one pass.
                    while rx.try_recv().is_ok() {}
                    pass(&app, &st, &mut failed);
                    match rx.recv_timeout(PERIOD) {
                        Ok(()) => std::thread::sleep(Duration::from_secs(2)),
                        Err(RecvTimeoutError::Timeout) => {}
                        Err(RecvTimeoutError::Disconnected) => return,
                    }
                }
            })
            .expect("spawn storage thread");
        Self {
            tx: Mutex::new(tx),
            status,
        }
    }

    /// Asks for a pass soon (after a save, a settings change, or "Check now").
    pub fn kick(&self) {
        let _ = self.tx.lock().send(());
    }

    pub fn status(&self) -> Status {
        self.status.lock().clone()
    }
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

fn publish(app: &AppHandle, status: &Mutex<Status>, f: impl FnOnce(&mut Status)) {
    let s = {
        let mut s = status.lock();
        f(&mut s);
        s.clone()
    };
    let _ = app.emit("storage", s);
}

fn key(c: &Clip) -> (PathBuf, u64, u64) {
    (c.path.clone(), c.bytes, c.modified)
}

/// Marker in the thumbnail cache: this version of the clip can't be compressed (further).
/// Keyed by path, size and date like thumbnails, so a changed file is looked at again.
fn marker(c: &Clip, thumbs: &Path) -> Option<PathBuf> {
    library::cache_path(&c.path, thumbs, "nocompress")
}

fn pass(app: &AppHandle, status: &Mutex<Status>, failed: &mut HashSet<(PathBuf, u64, u64)>) {
    let Some(state) = app.try_state::<AppState>() else { return };
    let (settings, thumbs, exports) = (
        state.settings.lock().clone(),
        state.paths.thumbs.clone(),
        state.paths.exports.clone(),
    );
    let cfg = &settings.storage;
    let limit = cfg.limit_bytes();
    let scan = || library::scan(&settings.out_dir, &thumbs);
    let total = |clips: &[Clip]| clips.iter().map(|c| c.bytes).sum::<u64>();

    let mut clips = scan();
    if !cfg.enabled || total(&clips) <= limit {
        publish(app, status, |s| s.note = None);
        return;
    }
    let recording = state.engine.state().recording;
    let touchable = |clips: &[Clip]| -> Vec<Clip> {
        let now = now_ms();
        // `scan` lists newest first: walk from the oldest.
        clips
            .iter()
            .rev()
            .filter(|c| now.saturating_sub(c.modified) > FRESH_MS && Some(&c.path) != recording.as_ref())
            .cloned()
            .collect()
    };

    let mut act = Activity::default();
    let mut waiting_for_game = false;
    if cfg.compress {
        let mut used = total(&clips);
        for c in touchable(&clips) {
            if used <= limit {
                break;
            }
            // Settings may change during a long pass.
            let now_cfg = state.settings.lock().storage.clone();
            if !now_cfg.enabled || !now_cfg.compress {
                break;
            }
            if failed.contains(&key(&c)) || marker(&c, &thumbs).is_some_and(|m| m.is_file()) {
                continue;
            }
            if state.game.lock().is_some() {
                waiting_for_game = true;
                break;
            }
            let info = match editor::inspect(&c.path) {
                Ok(i) => i,
                Err(e) => {
                    tracing::warn!(clip = %c.path.display(), "storage: can't read clip: {e}");
                    failed.insert(key(&c));
                    continue;
                }
            };
            if info.compressed || info.duration > MAX_COMPRESS_SECS || info.kbps < MIN_KBPS {
                mark(&c, &thumbs);
                continue;
            }
            publish(app, status, |s| s.busy = Some(format!("Compressing {}", c.title)));
            match compress(&c, &info, &thumbs, &exports) {
                Ok(Some(bytes)) => {
                    let saved = c.bytes.saturating_sub(bytes);
                    tracing::info!(clip = %c.path.display(), before = c.bytes, after = bytes, "storage: compressed");
                    used = used.saturating_sub(saved);
                    act.compressed += 1;
                    act.saved_bytes += saved;
                    let _ = app.emit("library-changed", ());
                }
                Ok(None) => mark(&c, &thumbs),
                Err(e) => {
                    tracing::warn!(clip = %c.path.display(), "storage: compression failed: {e}");
                    failed.insert(key(&c));
                }
            }
        }
        publish(app, status, |s| s.busy = None);
        clips = scan();
    }

    // Deleting a clip that compression could still shrink (once the game closes) would be
    // throwing it away early, so deletion waits too in that case.
    let still_on = state.settings.lock().storage.clone();
    if still_on.enabled && still_on.delete && !waiting_for_game && total(&clips) > limit {
        let mut used = total(&clips);
        let mut batch = Vec::new();
        for c in touchable(&clips) {
            if used <= limit {
                break;
            }
            used -= c.bytes;
            act.recycled_bytes += c.bytes;
            batch.push(c.path.clone());
        }
        if !batch.is_empty() {
            match library::recycle(&batch) {
                Ok(()) => {
                    tracing::info!(count = batch.len(), bytes = act.recycled_bytes, "storage: moved to the Recycle Bin");
                    act.recycled = batch.len() as u32;
                    let _ = app.emit("library-changed", ());
                }
                Err(e) => {
                    tracing::warn!("storage: could not move clips to the Recycle Bin: {e:#}");
                    act.recycled_bytes = 0;
                }
            }
        }
        clips = scan();
    }

    let over = total(&clips) > limit;
    let note = if !over {
        None
    } else if waiting_for_game {
        Some("Compressing waits until you close the game.".to_string())
    } else if !cfg.delete {
        Some("Still over the limit: nothing left that compressing would shrink.".to_string())
    } else {
        Some("Still over the limit: what's left was saved in the last 10 minutes.".to_string())
    };
    publish(app, status, |s| {
        s.note = note;
        if act.compressed + act.recycled > 0 {
            act.at = now_ms();
            s.last = Some(act);
        }
    });
}

fn mark(c: &Clip, thumbs: &Path) {
    if let Some(m) = marker(c, thumbs) {
        let _ = std::fs::create_dir_all(thumbs);
        let _ = std::fs::write(m, b"");
    }
}

/// Re-encodes `c` smaller in place. `Ok(None)`: it would not have saved enough.
fn compress(c: &Clip, info: &editor::Inspect, thumbs: &Path, exports: &Path) -> Result<Option<u64>, String> {
    // Same folder (same drive), so the final step is an atomic rename; not `.mp4`, so the
    // library never lists the half-written file.
    let out = c.path.with_extension("compressing");
    let _ = std::fs::remove_file(&out);
    let job = editor::ExportJob {
        input: c.path.clone(),
        output: out.clone(),
        start: 0.0,
        end: info.duration + 1.0,
        sources: Vec::new(),
        keep_sources: true,
        fit_bytes: Some((c.bytes as f64 * COMPRESS_TO) as u64),
        compressed: true,
    };
    let _ = std::fs::create_dir_all(exports);
    let job_file = exports.join("storage-job.json");
    let fail = |e: String| {
        let _ = std::fs::remove_file(&out);
        Err(e)
    };
    if let Err(e) = editor::export_with(&job, &job_file, true, |_| {}) {
        return fail(e);
    }
    let bytes = std::fs::metadata(&out).map_err(|e| e.to_string())?.len();
    if bytes as f64 > c.bytes as f64 * KEEP_IF_BELOW {
        let _ = std::fs::remove_file(&out);
        return Ok(None);
    }
    // Keep the clip's date, so it stays where it was in the library (and stays "oldest").
    let date = UNIX_EPOCH + Duration::from_millis(c.modified);
    if let Err(e) = std::fs::File::options().write(true).open(&out).and_then(|f| f.set_modified(date)) {
        return fail(e.to_string());
    }
    for suffix in ["jpg", "json", "probe.json"] {
        if let Some(p) = library::cache_path(&c.path, thumbs, suffix) {
            let _ = std::fs::remove_file(p);
        }
    }
    // Fails if the clip is open elsewhere without sharing; then it is retried next launch.
    if let Err(e) = std::fs::rename(&out, &c.path) {
        return fail(format!("replace the original: {e}"));
    }
    Ok(Some(bytes))
}
