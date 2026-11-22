//! Keeps the clip library under a size limit (Settings > Storage).
//!
//! When the library is over the limit, the oldest clips are re-encoded smaller (each clip
//! once; the engine marks the file, so a renamed clip is still recognized), but only the N
//! oldest. When those are all compressed and it's still not enough, the oldest clip goes to
//! the Recycle Bin and the next one in line gets compressed, until the library fits. Clips saved in the last few minutes and the recording in progress are never
//! touched, and compression waits while a game runs so it never costs frames.

use crate::library::{self, Clip};
use crate::{AppState, editor, settings};
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

/// What `run` needs from the outside world; faked in the tests.
trait Ops {
    /// Current settings (they may change during a long pass).
    fn settings(&self) -> settings::Storage;
    fn game_running(&self) -> bool;
    /// Could compressing this clip still shrink it? (Not already compressed, not tiny.)
    fn can_compress(&mut self, c: &Clip) -> bool;
    /// New size, or `None` if it would not have saved enough (then it counts as done).
    fn compress(&mut self, c: &Clip) -> Result<Option<u64>, String>;
    fn recycle(&mut self, paths: &[PathBuf]) -> bool;
}

#[derive(Default)]
struct Outcome {
    act: Activity,
    waiting_for_game: bool,
}

/// One cleanup pass over `list` (oldest first, only clips we may touch); `used` is the
/// whole library's size.
///
/// Only the `compress_oldest` oldest clips are ever compressed. When all of those are
/// compressed and space is still short, the oldest clip goes to the Recycle Bin, which brings
/// the next clip into that window to be compressed, and so on until the library fits.
fn run(ops: &mut impl Ops, mut list: Vec<Clip>, mut used: u64, limit: u64) -> Outcome {
    let mut out = Outcome::default();
    let mut done: HashSet<PathBuf> = HashSet::new();
    while used > limit && !list.is_empty() {
        let cfg = ops.settings();
        if !cfg.enabled {
            break;
        }
        if cfg.compress {
            let window = list.len().min(cfg.compress_oldest.max(1) as usize);
            let next = (0..window).find(|&i| !done.contains(&list[i].path) && {
                let ok = ops.can_compress(&list[i]);
                if !ok {
                    done.insert(list[i].path.clone());
                }
                ok
            });
            if let Some(i) = next {
                // Recycling a clip that could still be compressed (once the game closes)
                // would throw it away early: everything waits.
                if ops.game_running() {
                    out.waiting_for_game = true;
                    break;
                }
                done.insert(list[i].path.clone());
                match ops.compress(&list[i]) {
                    Ok(Some(bytes)) => {
                        let saved = list[i].bytes.saturating_sub(bytes);
                        used = used.saturating_sub(saved);
                        list[i].bytes = bytes;
                        out.act.compressed += 1;
                        out.act.saved_bytes += saved;
                    }
                    Ok(None) | Err(_) => {}
                }
                continue;
            }
        }
        if !cfg.delete {
            break;
        }
        // Without compression nothing enters a window: recycle everything needed at once.
        let take = if cfg.compress {
            1
        } else {
            let mut freed = 0;
            list.iter().take_while(|c| {
                let need = used - freed > limit;
                freed += c.bytes;
                need
            })
            .count()
            .max(1)
        };
        let batch: Vec<Clip> = list.drain(..take.min(list.len())).collect();
        let paths: Vec<PathBuf> = batch.iter().map(|c| c.path.clone()).collect();
        if !ops.recycle(&paths) {
            break;
        }
        let bytes: u64 = batch.iter().map(|c| c.bytes).sum();
        used = used.saturating_sub(bytes);
        out.act.recycled += batch.len() as u32;
        out.act.recycled_bytes += bytes;
    }
    out
}

struct RealOps<'a> {
    app: &'a AppHandle,
    status: &'a Mutex<Status>,
    failed: &'a mut HashSet<(PathBuf, u64, u64)>,
    thumbs: PathBuf,
    exports: PathBuf,
    /// Inspect results of clips about to be compressed.
    info: Option<(PathBuf, editor::Inspect)>,
}

impl Ops for RealOps<'_> {
    fn settings(&self) -> settings::Storage {
        self.app.state::<AppState>().settings.lock().storage.clone()
    }

    fn game_running(&self) -> bool {
        self.app.state::<AppState>().game.lock().is_some()
    }

    fn can_compress(&mut self, c: &Clip) -> bool {
        if self.failed.contains(&key(c)) || marker(c, &self.thumbs).is_some_and(|m| m.is_file()) {
            return false;
        }
        match editor::inspect(&c.path) {
            Ok(info) if info.compressed || info.duration > MAX_COMPRESS_SECS || info.kbps < MIN_KBPS => {
                mark(c, &self.thumbs);
                false
            }
            Ok(info) => {
                self.info = Some((c.path.clone(), info));
                true
            }
            Err(e) => {
                tracing::warn!(clip = %c.path.display(), "storage: can't read clip: {e}");
                self.failed.insert(key(c));
                false
            }
        }
    }

    fn compress(&mut self, c: &Clip) -> Result<Option<u64>, String> {
        let info = match self.info.take() {
            Some((p, i)) if p == c.path => i,
            _ => editor::inspect(&c.path)?,
        };
        publish(self.app, self.status, |s| s.busy = Some(format!("Compressing {}", c.title)));
        let res = compress(c, &info, &self.thumbs, &self.exports);
        publish(self.app, self.status, |s| s.busy = None);
        match &res {
            Ok(Some(bytes)) => {
                tracing::info!(clip = %c.path.display(), before = c.bytes, after = bytes, "storage: compressed");
                let _ = self.app.emit("library-changed", ());
            }
            Ok(None) => mark(c, &self.thumbs),
            Err(e) => {
                tracing::warn!(clip = %c.path.display(), "storage: compression failed: {e}");
                self.failed.insert(key(c));
            }
        }
        res
    }

    fn recycle(&mut self, paths: &[PathBuf]) -> bool {
        match library::recycle(paths) {
            Ok(()) => {
                tracing::info!(count = paths.len(), "storage: moved to the Recycle Bin");
                let _ = self.app.emit("library-changed", ());
                true
            }
            Err(e) => {
                tracing::warn!("storage: could not move clips to the Recycle Bin: {e:#}");
                false
            }
        }
    }
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
    let clips = library::scan(&settings.out_dir, &thumbs);
    let used: u64 = clips.iter().map(|c| c.bytes).sum();
    if !cfg.enabled || used <= limit {
        publish(app, status, |s| s.note = None);
        return;
    }
    let recording = state.engine.state().recording;
    let now = now_ms();
    // `scan` lists newest first: walk from the oldest.
    let list: Vec<Clip> = clips
        .iter()
        .rev()
        .filter(|c| now.saturating_sub(c.modified) > FRESH_MS && Some(&c.path) != recording.as_ref())
        .cloned()
        .collect();
    let mut ops = RealOps {
        app,
        status,
        failed,
        thumbs: thumbs.clone(),
        exports,
        info: None,
    };
    let Outcome { mut act, waiting_for_game } = run(&mut ops, list, used, limit);

    let over = library::scan(&settings.out_dir, &thumbs).iter().map(|c| c.bytes).sum::<u64>() > limit;
    let cfg = state.settings.lock().storage.clone();
    let note = if !over || !cfg.enabled {
        None
    } else if waiting_for_game {
        Some("Compressing waits until you close the game.".to_string())
    } else if !cfg.delete {
        Some(format!(
            "Still over the limit: the {} oldest are as small as they get. Raise that number or turn on the Recycle Bin.",
            cfg.compress_oldest
        ))
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

#[cfg(test)]
mod tests {
    use super::*;

    /// 100-byte clips that compress to 50, no game running.
    struct Fake {
        cfg: settings::Storage,
        log: Vec<String>,
        compressed: HashSet<PathBuf>,
        game: bool,
    }

    impl Ops for Fake {
        fn settings(&self) -> settings::Storage {
            self.cfg.clone()
        }
        fn game_running(&self) -> bool {
            self.game
        }
        fn can_compress(&mut self, c: &Clip) -> bool {
            !self.compressed.contains(&c.path)
        }
        fn compress(&mut self, c: &Clip) -> Result<Option<u64>, String> {
            self.compressed.insert(c.path.clone());
            self.log.push(format!("compress {}", c.title));
            Ok(Some(c.bytes / 2))
        }
        fn recycle(&mut self, paths: &[PathBuf]) -> bool {
            for p in paths {
                self.log.push(format!("recycle {}", p.display()));
            }
            true
        }
    }

    fn clips(n: usize) -> Vec<Clip> {
        (1..=n)
            .map(|i| Clip {
                path: PathBuf::from(i.to_string()),
                title: i.to_string(),
                game: None,
                kind: "clip",
                bytes: 100,
                modified: i as u64,
                duration: None,
                thumb: None,
            })
            .collect()
    }

    fn fake(compress: bool, delete: bool, compress_oldest: u32) -> Fake {
        Fake {
            cfg: settings::Storage {
                enabled: true,
                limit_gb: 1.0,
                compress,
                delete,
                compress_oldest,
            },
            log: Vec::new(),
            compressed: HashSet::new(),
            game: false,
        }
    }

    #[test]
    fn compresses_the_window_then_alternates_recycle_and_compress() {
        // 20 clips, 2000 bytes, limit 1400, only the 5 oldest may be compressed.
        let mut f = fake(true, true, 5);
        let out = run(&mut f, clips(20), 2000, 1400);
        let expect = [
            "compress 1", "compress 2", "compress 3", "compress 4", "compress 5", // 1750
            "recycle 1", "compress 6", // 1700, 1650
            "recycle 2", "compress 7", // 1600, 1550
            "recycle 3", "compress 8", // 1500, 1450
            "recycle 4",               // 1400: fits
        ];
        assert_eq!(f.log, expect);
        assert_eq!((out.act.compressed, out.act.recycled), (8, 4));
        assert_eq!(out.act.saved_bytes, 400);
    }

    #[test]
    fn stops_once_it_fits() {
        let mut f = fake(true, true, 10);
        run(&mut f, clips(20), 2000, 1900);
        assert_eq!(f.log, ["compress 1", "compress 2"]);
    }

    #[test]
    fn without_recycling_only_the_window_is_compressed() {
        let mut f = fake(true, false, 3);
        run(&mut f, clips(20), 2000, 1000);
        assert_eq!(f.log, ["compress 1", "compress 2", "compress 3"]);
    }

    #[test]
    fn without_compression_recycles_the_oldest_at_once() {
        let mut f = fake(false, true, 10);
        let out = run(&mut f, clips(20), 2000, 1750);
        assert_eq!(f.log, ["recycle 1", "recycle 2", "recycle 3"]);
        assert_eq!(out.act.recycled_bytes, 300);
    }

    #[test]
    fn already_compressed_clips_are_not_counted_twice() {
        let mut f = fake(true, true, 2);
        f.compressed.insert(PathBuf::from("1"));
        run(&mut f, clips(5), 500, 400);
        // 1 is done already, 2 is compressed (450); then 1 goes (350).
        assert_eq!(f.log, ["compress 2", "recycle 1"]);
    }

    #[test]
    fn a_running_game_pauses_everything() {
        let mut f = fake(true, true, 5);
        f.game = true;
        let out = run(&mut f, clips(20), 2000, 1000);
        assert!(f.log.is_empty());
        assert!(out.waiting_for_game);
    }
}
