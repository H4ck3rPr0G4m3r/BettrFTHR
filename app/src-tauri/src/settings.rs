//! App settings (one JSON file in the config dir) and the engine config derived from them.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
    Output,
    Input,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AudioSource {
    pub kind: SourceKind,
    /// Endpoint ID; `None` follows the Windows default device.
    #[serde(default)]
    pub device: Option<String>,
    /// Friendly device name, remembered so an unplugged device still shows up by name.
    #[serde(default)]
    pub device_name: Option<String>,
    /// Track name in the clip ("Game", "Mic", "Friend"…).
    pub label: String,
    #[serde(default = "one")]
    pub gain: f32,
}

fn one() -> f32 {
    1.0
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Hotkeys {
    pub save: String,
    pub record: String,
}

impl Default for Hotkeys {
    fn default() -> Self {
        Self {
            save: "F9".into(),
            record: "F10".into(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BufferMode {
    /// Replay buffer always running.
    Always,
    /// Only while a detected game is running (saves resources on the desktop).
    Games,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub out_dir: PathBuf,
    pub replay_seconds: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
    /// Output height; 0 keeps the monitor resolution.
    pub height: u32,
    pub cursor: bool,
    pub hotkeys: Hotkeys,
    pub audio: Vec<AudioSource>,
    pub buffer_mode: BufferMode,
    pub game_detection: bool,
    pub save_sound: bool,
    pub close_to_tray: bool,
    /// Discord upload limit in MB (10 free, 50 Nitro Basic, 500 Nitro): Discord copies are
    /// compressed to fit when bigger.
    pub discord_limit_mb: u32,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            out_dir: default_out_dir(),
            replay_seconds: 30,
            fps: 60,
            bitrate_kbps: 20_000,
            height: 0,
            cursor: true,
            hotkeys: Hotkeys::default(),
            audio: vec![
                AudioSource {
                    kind: SourceKind::Output,
                    device: None,
                    device_name: None,
                    label: "Game".into(),
                    gain: 1.0,
                },
                AudioSource {
                    kind: SourceKind::Input,
                    device: None,
                    device_name: None,
                    label: "Mic".into(),
                    gain: 1.0,
                },
            ],
            buffer_mode: BufferMode::Always,
            game_detection: true,
            save_sound: true,
            close_to_tray: true,
            discord_limit_mb: 10,
        }
    }
}

fn default_out_dir() -> PathBuf {
    let videos = std::env::var_os("USERPROFILE")
        .map(|p| PathBuf::from(p).join("Videos"))
        .unwrap_or_else(|| PathBuf::from("."));
    videos.join("BettrFTHR")
}

impl Settings {
    pub fn load(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(text) => match serde_json::from_str::<Settings>(&text) {
                Ok(s) => s.sanitized(),
                Err(e) => {
                    tracing::warn!("settings unreadable ({e}), using defaults");
                    let _ = std::fs::rename(path, path.with_extension("json.bad"));
                    Self::default()
                }
            },
            Err(_) => Self::default(),
        }
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(&tmp, path).context("replace settings")?;
        Ok(())
    }

    /// Clamp values to what the engine accepts, so a hand-edited file can't break startup.
    pub fn sanitized(mut self) -> Self {
        self.replay_seconds = self.replay_seconds.clamp(5, 600);
        self.fps = self.fps.clamp(24, 240);
        self.bitrate_kbps = self.bitrate_kbps.clamp(1_000, 150_000);
        if self.height != 0 {
            self.height = self.height.clamp(360, 4320);
        }
        self.audio.truncate(8);
        self.discord_limit_mb = self.discord_limit_mb.clamp(1, 2000);
        for a in &mut self.audio {
            a.gain = a.gain.clamp(0.0, 4.0);
        }
        if self.out_dir.as_os_str().is_empty() {
            self.out_dir = default_out_dir();
        }
        self
    }

    /// The engine's `--config` file contents (see crates/engine/src/main.rs `EngineConfig`).
    pub fn engine_config(&self) -> serde_json::Value {
        let audio: Vec<_> = self
            .audio
            .iter()
            .map(|a| {
                serde_json::json!({
                    "kind": a.kind,
                    "device": a.device,
                    "label": a.label,
                    "gain": a.gain,
                })
            })
            .collect();
        serde_json::json!({
            "out_dir": self.out_dir,
            "replay_seconds": self.replay_seconds,
            "fps": self.fps,
            "bitrate_kbps": self.bitrate_kbps,
            "height": self.height,
            "cursor": self.cursor,
            "hotkeys": { "save": self.hotkeys.save, "record": self.hotkeys.record },
            "audio": audio,
        })
    }

    /// True if changing from `self` to `other` needs the engine restarted.
    pub fn engine_differs(&self, other: &Settings) -> bool {
        self.engine_config() != other.engine_config()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_file_fills_defaults_and_clamps() {
        let s: Settings = serde_json::from_str(r#"{"fps": 1000, "replay_seconds": 1}"#).unwrap();
        let s = s.sanitized();
        assert_eq!(s.fps, 240);
        assert_eq!(s.replay_seconds, 5);
        assert_eq!(s.audio.len(), 2);
        assert_eq!(s.hotkeys.save, "F9");
    }

    #[test]
    fn engine_config_has_only_engine_fields() {
        let c = Settings::default().engine_config();
        let keys: Vec<_> = c.as_object().unwrap().keys().cloned().collect();
        for k in ["out_dir", "replay_seconds", "fps", "bitrate_kbps", "height", "cursor", "hotkeys", "audio"] {
            assert!(keys.contains(&k.to_string()), "{k}");
        }
        assert_eq!(keys.len(), 8);
        assert!(c["audio"][0].get("device_name").is_none());
    }
}
