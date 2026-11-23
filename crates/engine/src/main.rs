//! bettr-engine: capture/encode/replay process.
//!
//! Protocol: JSON lines. Commands on stdin (`{"cmd":"save"}` or just `save`), events on
//! stdout. Logs go to stderr so stdout stays machine-readable.

mod aacdec;
mod audio;
mod capture;
mod clock;
mod convert;
mod d3d;
mod demux;
mod edit;
mod encoder;
mod h264;
mod hotkey;
mod mp4;
mod pipeline;
mod recording;
mod transcode;
mod ring;

use anyhow::{Context, Result};
use audio::{AudioConfig, AudioEngine, SourceKind, SourceSpec};
use pipeline::{VideoConfig, VideoPipeline};
use serde_json::json;
use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use windows::Win32::Media::MediaFoundation::{MF_VERSION, MFSTARTUP_FULL, MFStartup};
use windows::Win32::System::SystemInformation::GetLocalTime;

/// Engine settings as JSON (all fields optional). The app writes this file and restarts the
/// engine when settings change, which is simpler and more robust than live reconfiguration.
///
/// ```json
/// { "out_dir": "D:/Clips", "replay_seconds": 60, "fps": 60, "bitrate_kbps": 25000,
///   "height": 1080, "cursor": true, "hotkeys": { "save": "F9", "record": "F10" },
///   "audio": [ { "kind": "output" }, { "kind": "input", "device": "Shure", "gain": 0.8 } ] }
/// ```
#[derive(Debug, Default, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
struct EngineConfig {
    out_dir: Option<PathBuf>,
    replay_seconds: Option<u32>,
    fps: Option<u32>,
    bitrate_kbps: Option<u32>,
    height: Option<u32>,
    cursor: Option<bool>,
    hotkeys: Option<HotkeyConfig>,
    audio: Option<Vec<SourceSpec>>,
}

#[derive(Debug, Default, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
struct HotkeyConfig {
    save: Option<String>,
    record: Option<String>,
}

impl EngineConfig {
    fn apply(self, a: &mut Args) {
        if let Some(v) = self.out_dir {
            a.out_dir = v;
        }
        if let Some(v) = self.replay_seconds {
            a.video.replay_seconds = v.clamp(5, 600);
        }
        if let Some(v) = self.fps {
            a.video.fps = v;
        }
        if let Some(v) = self.bitrate_kbps {
            a.video.bitrate_kbps = v.clamp(1_000, 150_000);
        }
        if let Some(v) = self.height {
            a.video.target_height = v;
        }
        if let Some(v) = self.cursor {
            a.video.cursor = v;
        }
        if let Some(h) = self.hotkeys {
            if let Some(k) = h.save {
                a.save_key = k;
            }
            if let Some(k) = h.record {
                a.record_key = k;
            }
        }
        if let Some(sources) = self.audio {
            a.audio.sources = sources;
            a.explicit_audio = true;
        }
    }
}

#[derive(Debug)]
struct Args {
    /// Audio sources were configured explicitly (an empty list then means "no audio").
    explicit_audio: bool,
    out_dir: PathBuf,
    video: VideoConfig,
    audio: AudioConfig,
    save_key: String,
    record_key: String,
    list_encoders: bool,
    list_audio: bool,
    selftest_aac: Option<PathBuf>,
    meter: Option<SourceSpec>,
    inspect: Option<PathBuf>,
    probe: Option<PathBuf>,
    export: Option<PathBuf>,
}

fn parse_args() -> Result<Args> {
    let mut a = Args {
        explicit_audio: false,
        out_dir: PathBuf::from("clips"),
        video: VideoConfig {
            fps: 60,
            bitrate_kbps: 20_000,
            target_height: 0,
            cursor: true,
            replay_seconds: 30,
        },
        audio: AudioConfig {
            sources: Vec::new(),
            replay_seconds: 30,
        },
        save_key: "F9".into(),
        record_key: "F10".into(),
        list_encoders: false,
        list_audio: false,
        selftest_aac: None,
        meter: None,
        inspect: None,
        probe: None,
        export: None,
    };
    let mut no_mic = false;
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let mut val = || it.next().with_context(|| format!("{arg} needs a value"));
        match arg.as_str() {
            // Settings file written by the app; flags after it override single values.
            "--config" => {
                let path = val()?;
                let text = std::fs::read_to_string(&path).with_context(|| format!("read {path}"))?;
                let c: EngineConfig = serde_json::from_str(&text).with_context(|| format!("parse {path}"))?;
                c.apply(&mut a);
            }
            "--out" => a.out_dir = PathBuf::from(val()?),
            "--fps" => a.video.fps = val()?.parse()?,
            "--bitrate" => a.video.bitrate_kbps = val()?.parse()?,
            "--height" => a.video.target_height = val()?.parse()?,
            "--seconds" => a.video.replay_seconds = val()?.parse()?,
            "--save-key" => a.save_key = val()?,
            "--record-key" => a.record_key = val()?,
            "--no-cursor" => a.video.cursor = false,
            // Repeatable: --output default --input "Shure" --input "Line 1@0.7"
            "--output" => a.audio.sources.push(SourceSpec::parse(SourceKind::Output, &val()?)?),
            "--input" => a.audio.sources.push(SourceSpec::parse(SourceKind::Input, &val()?)?),
            "--no-mic" => no_mic = true,
            "--list-encoders" => a.list_encoders = true,
            "--list-audio" => a.list_audio = true,
            "--selftest-aac" => a.selftest_aac = Some(PathBuf::from(val()?)),
            // Diagnostics: `--meter "front panel"` (input) or `--meter out:default` (loopback).
            "--meter" => {
                let v = val()?;
                a.meter = Some(match v.strip_prefix("out:") {
                    Some(d) => SourceSpec::parse(SourceKind::Output, d)?,
                    None => SourceSpec::parse(SourceKind::Input, &v)?,
                });
            }
            // Editor back end (no capture): waveform/keyframe probe, and export jobs.
            "--probe" => a.probe = Some(PathBuf::from(val()?)),
            // Quick facts for storage cleanup (no decoding).
            "--inspect" => a.inspect = Some(PathBuf::from(val()?)),
            "--export" => a.export = Some(PathBuf::from(val()?)),
            "--cli" => {}
            other => anyhow::bail!("unknown argument {other}"),
        }
    }
    a.audio.replay_seconds = a.video.replay_seconds;
    if a.audio.sources.is_empty() && !a.explicit_audio {
        a.audio.sources.push(SourceSpec::default_output());
        if !no_mic {
            a.audio.sources.push(SourceSpec::default_input());
        }
    }
    Ok(a)
}

/// Human-readable device list (stderr when stdout carries the JSON protocol).
fn print_devices(devices: &[audio::DeviceInfo]) {
    for kind in [SourceKind::Output, SourceKind::Input] {
        eprintln!(
            "{}:",
            if kind == SourceKind::Output {
                "Outputs (use with --output)"
            } else {
                "Inputs (use with --input)"
            }
        );
        for d in devices.iter().filter(|d| d.kind == kind) {
            eprintln!("  {} {}", if d.default { "*" } else { " " }, d.name);
        }
    }
    eprintln!("  (* = Windows default; pass part of a name, e.g. --input \"Shure\")");
}

fn emit(v: serde_json::Value) {
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{v}");
    let _ = out.flush();
}

#[derive(Debug, PartialEq)]
enum Command {
    /// Save the last N seconds (default: the whole replay buffer).
    Save(Option<u32>),
    RecordStart,
    RecordStop,
    RecordToggle,
    Status,
    ListAudio,
    /// The game in the foreground (from the app's detector); names and groups files.
    SetGame(Option<String>),
    Help,
    Quit,
    Unknown(String),
}

const HELP: &str = "commands: save [seconds], record, record_start, record_stop, status, list_audio, \
                    set_game <name>, help, quit";

/// Accepts `{"cmd":"save","seconds":15}` from the app, and forgiving plain text from a human:
/// `save 15`, `list-audio`, `--list-audio`, `Status`.
fn parse_command(line: &str) -> Option<Command> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    let (name, arg) = if line.starts_with('{') {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            return Some(Command::Unknown(line.to_owned()));
        };
        let name = v.get("cmd").and_then(|c| c.as_str()).unwrap_or_default().to_owned();
        let arg = ["seconds", "game"]
            .iter()
            .find_map(|k| v.get(*k))
            .map(|a| a.as_str().map(str::to_owned).unwrap_or_else(|| a.to_string()));
        (name, arg)
    } else {
        let (n, a) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
        let a = a.trim().trim_matches('"');
        (n.to_owned(), (!a.is_empty()).then(|| a.to_owned()))
    };
    let norm = name.trim_start_matches('-').to_ascii_lowercase().replace('-', "_");
    Some(match norm.as_str() {
        "save" | "clip" => match arg.as_deref().map(str::parse::<u32>) {
            None => Command::Save(None),
            Some(Ok(s)) if s > 0 => Command::Save(Some(s)),
            Some(_) => Command::Unknown(line.to_owned()),
        },
        "record" | "record_toggle" => Command::RecordToggle,
        "record_start" => Command::RecordStart,
        "record_stop" => Command::RecordStop,
        "status" => Command::Status,
        "list_audio" | "audio" | "devices" => Command::ListAudio,
        "set_game" | "game" => Command::SetGame(arg.filter(|g| g != "null")),
        "help" | "?" => Command::Help,
        "quit" | "exit" => Command::Quit,
        _ => Command::Unknown(line.to_owned()),
    })
}

/// A game name as a safe folder/file name component ("VALORANT", "Counter-Strike 2").
fn sanitize(name: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| {
            if c.is_control() || r#"<>:"/\|?*"#.contains(c) {
                ' '
            } else {
                c
            }
        })
        .collect();
    let s = s.split_whitespace().collect::<Vec<_>>().join(" ");
    let s = s.trim_end_matches('.').chars().take(60).collect::<String>();
    if s.is_empty() { "Unknown".into() } else { s }
}

/// `<out>/<Game>/<Game> 2026-11-21 10-12-57.mp4`, or `<out>/Clip ….mp4` with no game.
fn clip_path(dir: &Path, game: Option<&str>, kind: &str) -> PathBuf {
    let (dir, label) = match game {
        Some(g) => {
            let g = sanitize(g);
            (
                dir.join(&g),
                if kind == "Recording" {
                    format!("{g} Recording")
                } else {
                    g
                },
            )
        }
        None => (dir.to_path_buf(), kind.to_owned()),
    };
    let t = unsafe { GetLocalTime() };
    let base = format!(
        "{label} {:04}-{:02}-{:02} {:02}-{:02}-{:02}",
        t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond
    );
    let mut p = dir.join(format!("{base}.mp4"));
    let mut n = 2;
    while p.exists() {
        p = dir.join(format!("{base} ({n}).mp4"));
        n += 1;
    }
    p
}

fn start_recording(
    video: &VideoPipeline,
    audio: Option<&AudioEngine>,
    out_dir: &Path,
    game: Option<&str>,
) -> Option<recording::Recorder> {
    let src = recording::RecordingSource {
        video: video.shared.clone(),
        width: video.width,
        height: video.height,
        fps: video.fps,
        audio: audio.map(|a| a.shared.clone()),
    };
    match recording::Recorder::start(src, clip_path(out_dir, game, "Recording")) {
        Ok(r) => {
            emit(json!({"event": "recording_started", "path": r.path, "game": game}));
            Some(r)
        }
        Err(e) => {
            emit(json!({"event": "error", "message": format!("recording failed to start: {e:#}")}));
            None
        }
    }
}

fn finish_recording(r: recording::Recorder, reason: &str) {
    let path = r.path.clone();
    match r.stop() {
        Ok(stats) => emit(json!({
            "event": "recording_stopped",
            "path": path,
            "reason": reason,
            "seconds": stats.duration_hns as f64 / 1e7,
            "bytes": stats.bytes,
        })),
        Err(e) => emit(json!({"event": "error", "path": path, "message": format!("recording failed: {e:#}")})),
    }
}

fn save_clip(video: &VideoPipeline, audio: Option<&AudioEngine>, seconds: u32, out_dir: &Path, game: Option<String>) {
    let started = std::time::Instant::now();
    let Some(snap) = video.snapshot(seconds) else {
        emit(json!({"event": "error", "message": "nothing buffered yet"}));
        return;
    };
    let start = snap.packets[0].pts;
    let end = snap.packets.last().map(|p| p.pts + p.duration).unwrap_or(start);
    let audio = audio.map(|a| a.shared.clone());
    let dir = out_dir.to_path_buf();
    // Muxing is pure CPU + disk; never block the command loop on it.
    std::thread::spawn(move || {
        // Audio runs ~100 ms behind real time (WASAPI latency + padding lag). Wait until it
        // has caught up with the last video frame, bounded so a dead source can't stall saves.
        if let Some(a) = &audio {
            let deadline = std::time::Instant::now() + std::time::Duration::from_millis(400);
            while a.encoded_until() < end && std::time::Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        }
        let sound = audio.map(|a| a.snapshot(start, end));
        let path = clip_path(&dir, game.as_deref(), "Clip");
        let mut tracks = Vec::new();
        if let Some(s) = &sound {
            // Track order matters: players and Discord play the first (enabled) one.
            // The separate per-source stems follow, disabled, for the editor.
            let all =
                std::iter::once(("Mix", &s.mix, true)).chain(s.tracks.iter().map(|(n, p)| (n.as_str(), p, false)));
            for (name, packets, enabled) in all {
                if !packets.is_empty() {
                    tracks.push(mp4::AudioTrack {
                        name,
                        sample_rate: audio::RATE,
                        channels: audio::CHANNELS,
                        enabled,
                        packets,
                    });
                }
            }
        }
        let audio_offset_ms = sound
            .as_ref()
            .and_then(|s| s.mix.first())
            .map(|p| (p.pts - start) as f64 / 1e4);
        let res = std::fs::create_dir_all(path.parent().unwrap_or(&dir))
            .map_err(anyhow::Error::from)
            .and_then(|_| {
                mp4::write_mp4(
                    &path,
                    &mp4::VideoTrack {
                        name: mp4::VIDEO,
                        width: snap.width,
                        height: snap.height,
                        fps: snap.fps,
                        sps: &snap.sps,
                        pps: &snap.pps,
                        packets: &snap.packets,
                    },
                    &tracks,
                )
            });
        match res {
            Ok(stats) => {
                tracing::info!(
                    path = %path.display(),
                    secs = stats.duration_hns as f64 / 1e7,
                    audio_tracks = tracks.len(),
                    ?audio_offset_ms,
                    ms = started.elapsed().as_millis() as u64,
                    "clip saved"
                );
                emit(json!({
                    "event": "clip_saved",
                    "path": path,
                    "seconds": stats.duration_hns as f64 / 1e7,
                    "bytes": stats.bytes,
                    "frames": stats.video_frames,
                    "audio_tracks": tracks.len(),
                    "game": game,
                }));
            }
            Err(e) => {
                tracing::error!("clip save failed: {e:#}");
                emit(json!({"event": "error", "message": format!("clip save failed: {e:#}")}));
            }
        }
    });
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();
    let args = parse_args()?;
    let _com = encoder::ComScope::init();
    unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL) }.context("MFStartup")?;

    if args.list_encoders {
        let gpu = d3d::create_for_monitor(d3d::primary_monitor())?;
        println!("capture adapter: {}", gpu.adapter_name);
        println!("encoders on it:  {:?}", encoder::list_hardware_h264(Some(gpu.luid)));
        println!("all encoders:    {:?}", encoder::list_hardware_h264(None));
        return Ok(());
    }
    if args.list_audio {
        let devices = audio::list_devices()?;
        if std::io::stdout().is_terminal() {
            print_devices(&devices);
        } else {
            for d in devices {
                println!("{}", serde_json::to_string(&d)?);
            }
        }
        return Ok(());
    }
    if let Some(path) = &args.inspect {
        println!("{}", serde_json::to_string(&edit::inspect(path)?)?);
        return Ok(());
    }
    if let Some(path) = &args.probe {
        println!("{}", serde_json::to_string(&edit::probe(path)?)?);
        return Ok(());
    }
    if let Some(job) = &args.export {
        let text = std::fs::read_to_string(job).with_context(|| format!("read {}", job.display()))?;
        let job: edit::ExportJob = serde_json::from_str(&text).context("parse export job")?;
        let mut last = -1.0f64;
        let res = edit::export(&job, &mut |phase, p| {
            // Throttle: about one progress line per percent.
            if p - last >= 0.01 || p >= 1.0 {
                last = p;
                emit(json!({"event": "progress", "phase": phase, "p": (p * 1000.0).round() / 1000.0}));
            }
        })?;
        emit(json!({"event": "exported", "result": res}));
        return Ok(());
    }
    if let Some(spec) = &args.meter {
        return audio::meter(spec, 6);
    }
    if let Some(path) = &args.selftest_aac {
        audio::selftest_aac(path)?;
        println!("wrote {} (clicks at 0.5 s and 1.0 s)", path.display());
        return Ok(());
    }

    let mut video = VideoPipeline::start(None, &args.video)?;
    let mut audio = match AudioEngine::start(args.audio.clone()) {
        Ok(a) => Some(a),
        Err(_) if args.audio.sources.is_empty() => None, // audio deliberately off
        Err(e) => {
            tracing::error!("audio disabled: {e:#}");
            emit(json!({"event": "warning", "message": format!("audio disabled: {e:#}")}));
            None
        }
    };
    emit(json!({
        "event": "ready",
        "adapter": video.gpu.adapter_name,
        "encoder": video.encoder_name,
        "width": video.width,
        "height": video.height,
        "fps": video.fps,
        "audio_tracks": audio.as_ref().map(|a| a.shared.tracks.iter().map(|t| t.name.clone()).collect::<Vec<_>>()),
    }));

    // Before the reader thread starts: on a pipe, this check blocks while another thread
    // has a read pending on stdin, which would hold up the loop below until a command arrives.
    let human = std::io::stdin().is_terminal();
    let (cmd_tx, cmd_rx) = crossbeam_channel::unbounded::<Command>();
    {
        let tx = cmd_tx.clone();
        std::thread::spawn(move || {
            for line in std::io::stdin().lock().lines() {
                let Ok(line) = line else { break };
                if let Some(c) = parse_command(&line) {
                    let _ = tx.send(c);
                }
            }
            let _ = tx.send(Command::Quit); // parent went away
        });
    }
    {
        let (hk_tx, hk_rx) = crossbeam_channel::unbounded::<i32>();
        let save = hotkey::Hotkey::parse(&args.save_key).with_context(|| format!("bad hotkey {}", args.save_key))?;
        let record =
            hotkey::Hotkey::parse(&args.record_key).with_context(|| format!("bad hotkey {}", args.record_key))?;
        hotkey::spawn(vec![(1, save), (2, record)], hk_tx);
        let tx = cmd_tx.clone();
        std::thread::spawn(move || {
            for id in hk_rx {
                let _ = tx.send(if id == 1 {
                    Command::Save(None)
                } else {
                    Command::RecordToggle
                });
            }
        });
    }

    // Encoder watchdog: frames are being captured but nothing comes out of the encoder.
    let mut last_encoded = 0u64;
    let mut last_progress = std::time::Instant::now();
    let mut restarts = 0u32;
    let mut recorder: Option<recording::Recorder> = None;
    let mut game: Option<String> = None;
    if human {
        eprintln!(
            "bettr-engine running. {} saves a clip, {} toggles recording. Type `help` for commands.",
            args.save_key, args.record_key
        );
    }
    loop {
        let cmd = cmd_rx.recv_timeout(std::time::Duration::from_secs(1));
        let cmd = match cmd {
            Ok(Command::RecordToggle) if recorder.is_some() => Ok(Command::RecordStop),
            Ok(Command::RecordToggle) => Ok(Command::RecordStart),
            other => other,
        };
        match cmd {
            Ok(Command::Save(seconds)) => {
                let max = args.video.replay_seconds;
                let secs = seconds.map_or(max, |s| s.min(max));
                save_clip(&video, audio.as_ref(), secs, &args.out_dir, game.clone());
            }
            Ok(Command::RecordStart) if recorder.is_none() => {
                recorder = start_recording(&video, audio.as_ref(), &args.out_dir, game.as_deref());
            }
            Ok(Command::RecordStop) => {
                if let Some(r) = recorder.take() {
                    finish_recording(r, "stopped");
                }
            }
            Ok(Command::RecordStart | Command::RecordToggle) => {}
            Ok(Command::ListAudio) => match audio::list_devices() {
                Ok(devices) => {
                    if human {
                        print_devices(&devices);
                    }
                    emit(json!({"event": "audio_devices", "devices": devices}));
                }
                Err(e) => emit(json!({"event": "error", "message": format!("list audio devices: {e:#}")})),
            },
            Ok(Command::Status) => {
                let s = &video.shared.stats;
                let ring = video.shared.ring.lock();
                let sources: Vec<_> = audio
                    .as_ref()
                    .map(|a| {
                        a.shared
                            .tracks
                            .iter()
                            .map(|t| {
                                json!({
                                    "track": t.name,
                                    "kind": t.spec.kind,
                                    "device": *t.device.lock(),
                                    "health": *t.health.lock(),
                                    // Peak since the last status, 0..1, after the source's gain.
                                    "level": (t.take_peak() * t.spec.gain).min(1.0),
                                })
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                emit(json!({
                    "event": "status",
                    "ticks": s.ticks.load(Ordering::Relaxed),
                    "encoded": s.encoded.load(Ordering::Relaxed),
                    "dropped": s.dropped.load(Ordering::Relaxed),
                    "captured": s.captured.load(Ordering::Relaxed),
                    "buffered_seconds": ring.span() as f64 / 1e7,
                    "buffered_mb": ring.bytes() as f64 / 1048576.0,
                    "audio_sources": sources,
                    "recording": recorder.as_ref().map(|r| &r.path),
                    "game": game,
                }));
            }
            Ok(Command::SetGame(g)) => {
                tracing::info!(game = ?g, "game changed");
                game = g;
                emit(json!({"event": "game", "game": game}));
            }
            Ok(Command::Help) => emit(json!({"event": "help", "message": HELP})),
            Ok(Command::Unknown(line)) => {
                emit(json!({"event": "error", "message": format!("unknown command `{line}`; {HELP}")}))
            }
            Ok(Command::Quit) | Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
        }
        if recorder.as_ref().is_some_and(|r| r.is_finished()) {
            finish_recording(recorder.take().unwrap(), "failed");
        }
        let encoded = video.shared.stats.encoded.load(Ordering::Relaxed);
        let captured = video.shared.stats.captured.load(Ordering::Relaxed);
        if encoded != last_encoded || captured == 0 {
            last_encoded = encoded;
            last_progress = std::time::Instant::now();
        }
        let stalled = last_progress.elapsed() > std::time::Duration::from_secs(3);
        if stalled {
            tracing::error!(encoded, captured, "video encoder stalled");
        }
        video.recover_blank_capture();
        if video.failed() || video.source_changed() || stalled {
            restarts += 1;
            tracing::warn!(restarts, "video pipeline needs a restart");
            emit(
                json!({"event": "restarting", "reason": if stalled { "encoder stalled" } else if video.failed() { "pipeline error" } else { "display changed" }}),
            );
            // A new encoder means new parameter sets: close the recording, continue in a new file.
            let was_recording = recorder
                .take()
                .map(|r| finish_recording(r, "pipeline restarted"))
                .is_some();
            video.stop();
            match VideoPipeline::start(None, &args.video) {
                Ok(v) => {
                    video = v;
                    last_encoded = 0;
                    last_progress = std::time::Instant::now();
                    emit(json!({"event": "restarted"}));
                    if was_recording {
                        std::thread::sleep(std::time::Duration::from_millis(1500)); // first keyframe
                        recorder = start_recording(&video, audio.as_ref(), &args.out_dir, game.as_deref());
                    }
                }
                Err(e) => {
                    emit(json!({"event": "error", "message": format!("restart failed: {e:#}")}));
                    std::thread::sleep(std::time::Duration::from_secs(2));
                }
            }
        }
    }
    if let Some(r) = recorder.take() {
        finish_recording(r, "engine stopping");
    }
    video.stop();
    if let Some(a) = &mut audio {
        a.stop();
    }
    emit(json!({"event": "stopped"}));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_are_forgiving() {
        assert_eq!(parse_command("save"), Some(Command::Save(None)));
        assert_eq!(parse_command("save 15"), Some(Command::Save(Some(15))));
        assert_eq!(
            parse_command(r#"{"cmd":"save","seconds":20}"#),
            Some(Command::Save(Some(20)))
        );
        assert_eq!(parse_command("--list-audio"), Some(Command::ListAudio));
        assert_eq!(parse_command("list-audio"), Some(Command::ListAudio));
        assert_eq!(parse_command("Status"), Some(Command::Status));
        assert_eq!(parse_command("record"), Some(Command::RecordToggle));
        assert_eq!(
            parse_command(r#"{"cmd":"set_game","game":"VALORANT"}"#),
            Some(Command::SetGame(Some("VALORANT".into())))
        );
        assert_eq!(
            parse_command(r#"{"cmd":"set_game","game":null}"#),
            Some(Command::SetGame(None))
        );
        assert_eq!(
            parse_command("game Counter-Strike 2"),
            Some(Command::SetGame(Some("Counter-Strike 2".into())))
        );
        assert_eq!(parse_command("save abc"), Some(Command::Unknown("save abc".into())));
        assert_eq!(parse_command("frobnicate"), Some(Command::Unknown("frobnicate".into())));
        assert_eq!(parse_command("   "), None);
    }

    #[test]
    fn game_names_are_safe_paths() {
        assert_eq!(sanitize("VALORANT"), "VALORANT");
        assert_eq!(sanitize("a/b:c?"), "a b c");
        assert_eq!(sanitize("..."), "Unknown");
        let p = clip_path(Path::new("out"), Some("Counter-Strike 2"), "Recording");
        assert!(p.starts_with(Path::new("out").join("Counter-Strike 2")));
        assert!(
            p.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("Counter-Strike 2 Recording ")
        );
    }
}
