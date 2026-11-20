//! bettr-engine: capture/encode/replay process.
//!
//! Protocol: JSON lines. Commands on stdin (`{"cmd":"save"}` or just `save`), events on
//! stdout. Logs go to stderr so stdout stays machine-readable.

mod capture;
mod clock;
mod convert;
mod d3d;
mod encoder;
mod h264;
mod hotkey;
mod mp4;
mod pipeline;
mod ring;

use anyhow::{Context, Result};
use pipeline::{VideoConfig, VideoPipeline};
use serde_json::json;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use windows::Win32::Media::MediaFoundation::{MF_VERSION, MFSTARTUP_FULL, MFStartup};
use windows::Win32::System::SystemInformation::GetLocalTime;

#[derive(Debug)]
struct Args {
    out_dir: PathBuf,
    video: VideoConfig,
    save_key: String,
    list_encoders: bool,
}

fn parse_args() -> Result<Args> {
    let mut a = Args {
        out_dir: PathBuf::from("clips"),
        video: VideoConfig {
            fps: 60,
            bitrate_kbps: 20_000,
            target_height: 0,
            cursor: true,
            replay_seconds: 30,
        },
        save_key: "F9".into(),
        list_encoders: false,
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let mut val = || it.next().with_context(|| format!("{arg} needs a value"));
        match arg.as_str() {
            "--out" => a.out_dir = PathBuf::from(val()?),
            "--fps" => a.video.fps = val()?.parse()?,
            "--bitrate" => a.video.bitrate_kbps = val()?.parse()?,
            "--height" => a.video.target_height = val()?.parse()?,
            "--seconds" => a.video.replay_seconds = val()?.parse()?,
            "--save-key" => a.save_key = val()?,
            "--no-cursor" => a.video.cursor = false,
            "--list-encoders" => a.list_encoders = true,
            "--cli" => {}
            other => anyhow::bail!("unknown argument {other}"),
        }
    }
    Ok(a)
}

fn emit(v: serde_json::Value) {
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{v}");
    let _ = out.flush();
}

enum Command {
    Save,
    Status,
    Quit,
}

fn parse_command(line: &str) -> Option<Command> {
    let line = line.trim();
    let name = if line.starts_with('{') {
        serde_json::from_str::<serde_json::Value>(line)
            .ok()?
            .get("cmd")?
            .as_str()?
            .to_owned()
    } else {
        line.to_owned()
    };
    match name.as_str() {
        "save" => Some(Command::Save),
        "status" => Some(Command::Status),
        "quit" | "exit" => Some(Command::Quit),
        _ => None,
    }
}

fn clip_path(dir: &Path, label: &str) -> PathBuf {
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

fn save_clip(video: &VideoPipeline, seconds: u32, out_dir: &Path) {
    let started = std::time::Instant::now();
    let Some(snap) = video.snapshot(seconds) else {
        emit(json!({"event": "error", "message": "nothing buffered yet"}));
        return;
    };
    let dir = out_dir.to_path_buf();
    // Muxing is pure CPU + disk; never block the command loop on it.
    std::thread::spawn(move || {
        let path = clip_path(&dir, "Clip");
        let res = std::fs::create_dir_all(&dir)
            .map_err(anyhow::Error::from)
            .and_then(|_| {
                mp4::write_mp4(
                    &path,
                    &mp4::VideoTrack {
                        width: snap.width,
                        height: snap.height,
                        fps: snap.fps,
                        sps: &snap.sps,
                        pps: &snap.pps,
                        packets: &snap.packets,
                    },
                    &[],
                )
            });
        match res {
            Ok(stats) => {
                tracing::info!(path = %path.display(), secs = stats.duration_hns as f64 / 1e7, ms = started.elapsed().as_millis() as u64, "clip saved");
                emit(json!({
                    "event": "clip_saved",
                    "path": path,
                    "seconds": stats.duration_hns as f64 / 1e7,
                    "bytes": stats.bytes,
                    "frames": stats.video_frames,
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
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let args = parse_args()?;
    let _com = encoder::ComScope::init();
    unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL) }.context("MFStartup")?;

    if args.list_encoders {
        let gpu = d3d::create_for_monitor(d3d::primary_monitor())?;
        println!("capture adapter: {}", gpu.adapter_name);
        println!(
            "encoders on it:  {:?}",
            encoder::list_hardware_h264(Some(gpu.luid))
        );
        println!("all encoders:    {:?}", encoder::list_hardware_h264(None));
        return Ok(());
    }

    let mut video = VideoPipeline::start(None, &args.video)?;
    emit(json!({
        "event": "ready",
        "adapter": video.gpu.adapter_name,
        "encoder": video.encoder_name,
        "width": video.width,
        "height": video.height,
        "fps": video.fps,
    }));

    let (cmd_tx, cmd_rx) = crossbeam_channel::unbounded::<Command>();
    // stdin commands
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
    // hotkeys
    {
        let (hk_tx, hk_rx) = crossbeam_channel::unbounded::<i32>();
        let save = hotkey::Hotkey::parse(&args.save_key)
            .with_context(|| format!("bad hotkey {}", args.save_key))?;
        hotkey::spawn(vec![(1, save)], hk_tx);
        let tx = cmd_tx.clone();
        std::thread::spawn(move || {
            for id in hk_rx {
                if id == 1 {
                    let _ = tx.send(Command::Save);
                }
            }
        });
    }

    loop {
        match cmd_rx.recv_timeout(std::time::Duration::from_secs(1)) {
            Ok(Command::Save) => save_clip(&video, args.video.replay_seconds, &args.out_dir),
            Ok(Command::Status) => {
                let s = &video.shared.stats;
                let ring = video.shared.ring.lock();
                emit(json!({
                    "event": "status",
                    "ticks": s.ticks.load(Ordering::Relaxed),
                    "encoded": s.encoded.load(Ordering::Relaxed),
                    "dropped": s.dropped.load(Ordering::Relaxed),
                    "captured": s.captured.load(Ordering::Relaxed),
                    "buffered_seconds": ring.span() as f64 / 1e7,
                    "buffered_mb": ring.bytes() as f64 / 1048576.0,
                }));
            }
            Ok(Command::Quit) | Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
        }
        if video.failed() || video.source_changed() {
            tracing::warn!("video pipeline needs a restart");
            video.stop();
            match VideoPipeline::start(None, &args.video) {
                Ok(v) => {
                    video = v;
                    emit(json!({"event": "restarted"}));
                }
                Err(e) => {
                    emit(json!({"event": "error", "message": format!("restart failed: {e:#}")}));
                    std::thread::sleep(std::time::Duration::from_secs(2));
                }
            }
        }
    }
    video.stop();
    emit(json!({"event": "stopped"}));
    Ok(())
}
