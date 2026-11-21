//! The clip library: files in the output folder (one subfolder per game), plus a small cache
//! of thumbnails and durations that the UI produces from the video itself.

use serde::{Deserialize, Serialize};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

#[derive(Clone, Debug, Serialize)]
pub struct Clip {
    pub path: PathBuf,
    /// File name without `.mp4`; what the UI shows and renames.
    pub title: String,
    /// The game subfolder the clip lives in, if any.
    pub game: Option<String>,
    pub kind: &'static str,
    pub bytes: u64,
    /// Last write time, ms since the Unix epoch.
    pub modified: u64,
    pub duration: Option<f64>,
    pub thumb: Option<PathBuf>,
}

#[derive(Serialize, Deserialize)]
struct Meta {
    duration: f64,
}

/// Cache key: path + size + mtime, so an edited or replaced file gets a new thumbnail.
fn cache_key(path: &Path, bytes: u64, modified: u64) -> String {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    path.hash(&mut h);
    bytes.hash(&mut h);
    modified.hash(&mut h);
    format!("{:016x}", h.finish())
}

fn file_info(path: &Path) -> Option<(u64, u64)> {
    let m = std::fs::metadata(path).ok()?;
    let modified = m.modified().ok()?.duration_since(UNIX_EPOCH).ok()?.as_millis() as u64;
    Some((m.len(), modified))
}

fn is_clip(p: &Path) -> bool {
    p.is_file() && p.extension().is_some_and(|e| e.eq_ignore_ascii_case("mp4"))
}

/// All clips under `root` (top level and one level of game folders), newest first.
pub fn scan(root: &Path, cache: &Path) -> Vec<Clip> {
    let mut files: Vec<(PathBuf, Option<String>)> = Vec::new();
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            let game = p.file_name().map(|n| n.to_string_lossy().into_owned());
            if let Ok(sub) = std::fs::read_dir(&p) {
                files.extend(sub.flatten().map(|e| e.path()).filter(|p| is_clip(p)).map(|p| (p, game.clone())));
            }
        } else if is_clip(&p) {
            files.push((p, None));
        }
    }
    let mut clips: Vec<Clip> = files
        .into_iter()
        .filter_map(|(path, game)| {
            let (bytes, modified) = file_info(&path)?;
            let title = path.file_stem()?.to_string_lossy().into_owned();
            let key = cache_key(&path, bytes, modified);
            let thumb = cache.join(format!("{key}.jpg"));
            let duration = std::fs::read(cache.join(format!("{key}.json")))
                .ok()
                .and_then(|b| serde_json::from_slice::<Meta>(&b).ok())
                .map(|m| m.duration);
            Some(Clip {
                kind: if title.contains("Recording") { "recording" } else { "clip" },
                title,
                game,
                bytes,
                modified,
                duration,
                thumb: thumb.is_file().then_some(thumb),
                path,
            })
        })
        .collect();
    clips.sort_by(|a, b| b.modified.cmp(&a.modified));
    clips
}

/// Stores a UI-rendered thumbnail (JPEG bytes) and the measured duration.
pub fn store_thumb(clip: &Path, cache: &Path, jpeg: &[u8], duration: f64) -> anyhow::Result<PathBuf> {
    let (bytes, modified) = file_info(clip).ok_or_else(|| anyhow::anyhow!("clip not found"))?;
    std::fs::create_dir_all(cache)?;
    let key = cache_key(clip, bytes, modified);
    let thumb = cache.join(format!("{key}.jpg"));
    std::fs::write(&thumb, jpeg)?;
    std::fs::write(cache.join(format!("{key}.json")), serde_json::to_vec(&Meta { duration })?)?;
    Ok(thumb)
}

/// Only files inside the library may be renamed or deleted from the UI.
pub fn ensure_inside(root: &Path, path: &Path) -> anyhow::Result<()> {
    let root = std::fs::canonicalize(root)?;
    let p = std::fs::canonicalize(path)?;
    anyhow::ensure!(p.starts_with(&root) && is_clip(&p), "not a clip in the library");
    Ok(())
}

pub fn rename(path: &Path, title: &str) -> anyhow::Result<PathBuf> {
    let title = title.trim();
    anyhow::ensure!(!title.is_empty(), "the name can't be empty");
    anyhow::ensure!(
        !title.chars().any(|c| c.is_control() || r#"<>:"/\|?*"#.contains(c)),
        r#"names can't contain < > : " / \ | ? *"#
    );
    anyhow::ensure!(title.chars().count() <= 120, "that name is too long");
    let target = path.with_file_name(format!("{title}.mp4"));
    if target == path {
        return Ok(target);
    }
    anyhow::ensure!(!target.exists(), "a clip with that name already exists");
    std::fs::rename(path, &target)?;
    Ok(target)
}

/// Moves a clip to the Recycle Bin (recoverable), never a hard delete.
pub fn recycle(path: &Path) -> anyhow::Result<()> {
    use windows::Win32::UI::Shell::{
        FO_DELETE, FOF_ALLOWUNDO, FOF_NOCONFIRMATION, FOF_NOERRORUI, FOF_SILENT, SHFILEOPSTRUCTW, SHFileOperationW,
    };
    let mut from: Vec<u16> = path.as_os_str().encode_wide_vec();
    from.extend([0, 0]); // double-NUL terminated list
    let mut op = SHFILEOPSTRUCTW {
        wFunc: FO_DELETE,
        pFrom: windows::core::PCWSTR(from.as_ptr()),
        fFlags: (FOF_ALLOWUNDO | FOF_NOCONFIRMATION | FOF_SILENT | FOF_NOERRORUI).0 as u16,
        ..Default::default()
    };
    let rc = unsafe { SHFileOperationW(&mut op) };
    anyhow::ensure!(rc == 0 && !op.fAnyOperationsAborted.as_bool(), "could not move to the Recycle Bin ({rc})");
    Ok(())
}

trait EncodeWideVec {
    fn encode_wide_vec(&self) -> Vec<u16>;
}

impl EncodeWideVec for std::ffi::OsStr {
    fn encode_wide_vec(&self) -> Vec<u16> {
        use std::os::windows::ffi::OsStrExt;
        self.encode_wide().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_finds_top_level_and_game_folders() {
        let root = std::env::temp_dir().join(format!("bettr-lib-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("VALORANT")).unwrap();
        std::fs::write(root.join("Clip 2026-01-01 10-00-00.mp4"), b"x").unwrap();
        std::fs::write(root.join("VALORANT").join("VALORANT Recording 2026.mp4"), b"xy").unwrap();
        std::fs::write(root.join("VALORANT").join("notes.txt"), b"no").unwrap();
        let clips = scan(&root, &root.join("cache"));
        assert_eq!(clips.len(), 2);
        let v = clips.iter().find(|c| c.game.is_some()).unwrap();
        assert_eq!(v.game.as_deref(), Some("VALORANT"));
        assert_eq!(v.kind, "recording");
        let renamed = rename(&clips[0].path, "best ace").unwrap();
        assert!(renamed.ends_with("best ace.mp4"));
        assert!(rename(&renamed, "bad/name").is_err());
        assert!(ensure_inside(&root, &renamed).is_ok());
        assert!(ensure_inside(&root, &root.join("VALORANT").join("notes.txt")).is_err());
        std::fs::remove_dir_all(&root).unwrap();
    }
}
