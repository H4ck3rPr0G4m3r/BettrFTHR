//! Runs the engine's editor modes (`--probe`, `--export`) as short-lived processes.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io::{BufRead, BufReader};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;

fn engine() -> Result<PathBuf, String> {
    crate::engine::find_exe().ok_or_else(|| "bettr-engine.exe not found".to_string())
}

/// Last `Error: ...` line the engine printed, for a readable failure message.
fn engine_error(stderr: &str) -> String {
    stderr
        .lines()
        .rev()
        .find_map(|l| l.strip_prefix("Error: "))
        .unwrap_or("the engine failed")
        .trim()
        .to_owned()
}

pub fn probe(clip: &Path, cache: Option<PathBuf>) -> Result<Value, String> {
    if let Some(c) = &cache
        && let Ok(text) = std::fs::read_to_string(c)
        && let Ok(v) = serde_json::from_str(&text)
    {
        return Ok(v);
    }
    let out = Command::new(engine()?)
        .arg("--probe")
        .arg(clip)
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(engine_error(&String::from_utf8_lossy(&out.stderr)));
    }
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    let v: Value = serde_json::from_str(text.trim()).map_err(|e| e.to_string())?;
    if let Some(c) = cache {
        if let Some(dir) = c.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(c, text);
    }
    Ok(v)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SourceEdit {
    pub gain: f32,
    #[serde(default)]
    pub mutes: Vec<(f64, f64)>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ExportJob {
    pub input: PathBuf,
    pub output: PathBuf,
    pub start: f64,
    pub end: f64,
    pub sources: Vec<SourceEdit>,
    pub keep_sources: bool,
    /// Re-encode to fit when the lossless cut is bigger than this.
    pub fit_bytes: Option<u64>,
    /// Storage cleanup: mark the output as compressed so it is never compressed again.
    pub compressed: bool,
}

/// Runs an export; `progress` gets 0..1. Returns the engine's result object.
pub fn export(job: &ExportJob, job_file: &Path, progress: impl FnMut(f64)) -> Result<Value, String> {
    export_with(job, job_file, false, progress)
}

/// `background`: below-normal priority, for work nobody waits on (storage cleanup).
pub fn export_with(job: &ExportJob, job_file: &Path, background: bool, mut progress: impl FnMut(f64)) -> Result<Value, String> {
    const BELOW_NORMAL_PRIORITY_CLASS: u32 = 0x0000_4000;
    std::fs::write(job_file, serde_json::to_vec(job).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
    let flags = CREATE_NO_WINDOW | if background { BELOW_NORMAL_PRIORITY_CLASS } else { 0 };
    let mut child = Command::new(engine()?)
        .arg("--export")
        .arg(job_file)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .creation_flags(flags)
        .spawn()
        .map_err(|e| e.to_string())?;
    let stdout = child.stdout.take().expect("piped");
    let mut stderr = child.stderr.take().expect("piped");
    let err_reader = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = std::io::Read::read_to_string(&mut stderr, &mut s);
        s
    });
    let mut result = None;
    for line in BufReader::new(stdout).lines() {
        let Ok(line) = line else { break };
        let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
        match v.get("event").and_then(|e| e.as_str()) {
            Some("progress") => progress(v.get("p").and_then(|p| p.as_f64()).unwrap_or(0.0)),
            Some("exported") => result = v.get("result").cloned(),
            _ => {}
        }
    }
    let status = child.wait().map_err(|e| e.to_string())?;
    let stderr = err_reader.join().unwrap_or_default();
    let _ = std::fs::remove_file(job_file);
    match result {
        Some(r) if status.success() => Ok(r),
        _ => Err(engine_error(&stderr)),
    }
}

/// Header facts of a clip (`--inspect`): length, bitrate, already compressed.
#[derive(Clone, Debug, Deserialize)]
pub struct Inspect {
    pub duration: f64,
    pub kbps: f64,
    pub compressed: bool,
}

pub fn inspect(clip: &Path) -> Result<Inspect, String> {
    let out = Command::new(engine()?)
        .arg("--inspect")
        .arg(clip)
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(engine_error(&String::from_utf8_lossy(&out.stderr)));
    }
    serde_json::from_str(String::from_utf8_lossy(&out.stdout).trim()).map_err(|e| e.to_string())
}

/// Removes old Discord exports (they only need to live until pasted).
pub fn clean_exports(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let day = std::time::Duration::from_secs(24 * 3600);
    for e in entries.flatten() {
        let old = e
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age > day);
        if old {
            let _ = std::fs::remove_file(e.path());
        }
    }
}
