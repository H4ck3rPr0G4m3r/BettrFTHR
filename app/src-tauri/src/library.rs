//! The clip library: files in the output folder (one subfolder per game), plus a small cache
//! of thumbnails and durations that the UI produces from the video itself.

use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::ffi::{OsStr, OsString};
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

fn has_mp4_ext(p: &Path) -> bool {
    p.extension().is_some_and(|e| e.eq_ignore_ascii_case("mp4"))
}

fn is_clip(p: &Path) -> bool {
    has_mp4_ext(p) && p.is_file()
}

/// All clips under `root` (top level and one level of game folders), newest first.
///
/// Runs on every focus and save, so it touches each clip once: the directory listing says
/// what is a file or folder, one listing of the cache says which thumbnails exist, and only
/// size and mtime are read per clip (from the file itself: NTFS listings lag behind a file
/// that is still being written, like a live recording).
pub fn scan(root: &Path, cache: &Path) -> Vec<Clip> {
    let mut files: Vec<(PathBuf, Option<String>)> = Vec::new();
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    for e in entries.flatten() {
        let p = e.path();
        if entry_is(&e, &p, std::fs::FileType::is_dir) {
            let game = p.file_name().map(|n| n.to_string_lossy().into_owned());
            if let Ok(sub) = std::fs::read_dir(&p) {
                files.extend(
                    sub.flatten()
                        .filter_map(|e| {
                            let p = e.path();
                            (has_mp4_ext(&p) && entry_is(&e, &p, std::fs::FileType::is_file)).then_some(p)
                        })
                        .map(|p| (p, game.clone())),
                );
            }
        } else if has_mp4_ext(&p) && entry_is(&e, &p, std::fs::FileType::is_file) {
            files.push((p, None));
        }
    }
    let cached: HashSet<OsString> = std::fs::read_dir(cache)
        .map(|d| d.flatten().map(|e| e.file_name()).collect())
        .unwrap_or_default();
    let mut clips: Vec<Clip> = files
        .into_iter()
        .filter_map(|(path, game)| {
            let (bytes, modified) = file_info(&path)?;
            let title = path.file_stem()?.to_string_lossy().into_owned();
            let key = cache_key(&path, bytes, modified);
            let thumb_name = format!("{key}.jpg");
            let meta_name = format!("{key}.json");
            let duration = cached
                .contains(OsStr::new(&meta_name))
                .then(|| std::fs::read(cache.join(&meta_name)).ok())
                .flatten()
                .and_then(|b| serde_json::from_slice::<Meta>(&b).ok())
                .map(|m| m.duration);
            Some(Clip {
                kind: if title.contains("Recording") { "recording" } else { "clip" },
                title,
                game,
                bytes,
                modified,
                duration,
                thumb: cached.contains(OsStr::new(&thumb_name)).then(|| cache.join(thumb_name)),
                path,
            })
        })
        .collect();
    clips.sort_by(|a, b| b.modified.cmp(&a.modified));
    clips
}

/// The entry's type from the directory listing itself (no extra file open), following a
/// link the way `Path::is_file`/`is_dir` would.
fn entry_is(e: &std::fs::DirEntry, p: &Path, is: fn(&std::fs::FileType) -> bool) -> bool {
    match e.file_type() {
        Ok(t) if !t.is_symlink() => is(&t),
        _ => std::fs::metadata(p).is_ok_and(|m| is(&m.file_type())),
    }
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

/// Cache file for derived data of a clip (e.g. `probe.json`), invalidated when it changes.
pub fn cache_path(clip: &Path, cache: &Path, suffix: &str) -> Option<PathBuf> {
    let (bytes, modified) = file_info(clip)?;
    Some(cache.join(format!("{}.{suffix}", cache_key(clip, bytes, modified))))
}

/// `<dir>/<title> (edit).mp4`, numbered if taken.
pub fn edit_path(original: &Path) -> PathBuf {
    let dir = original.parent().unwrap_or(Path::new("."));
    let stem = original.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let stem = stem.trim_end_matches(" (edit)").to_owned();
    let mut p = dir.join(format!("{stem} (edit).mp4"));
    let mut n = 2;
    while p.exists() {
        p = dir.join(format!("{stem} (edit {n}).mp4"));
        n += 1;
    }
    p
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

/// Moves clips to the Recycle Bin (recoverable), never a hard delete. One shell operation
/// for the whole batch, so deleting 40 clips is one quick step.
pub fn recycle(paths: &[PathBuf]) -> anyhow::Result<()> {
    use windows::Win32::UI::Shell::{
        FO_DELETE, FOF_ALLOWUNDO, FOF_NOCONFIRMATION, FOF_NOERRORUI, FOF_SILENT, SHFILEOPSTRUCTW, SHFileOperationW,
    };
    anyhow::ensure!(!paths.is_empty(), "nothing to delete");
    let mut from: Vec<u16> = Vec::new();
    for p in paths {
        from.extend(p.as_os_str().encode_wide_vec());
        from.push(0);
    }
    from.push(0); // double-NUL terminated list
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
