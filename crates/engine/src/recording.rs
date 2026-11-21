//! Manual recording: follows the same encoded streams as the replay buffer (no second
//! encoder) and appends one fragment per GOP to a fragmented MP4.

use crate::audio::{self, AudioShared};
use crate::clock::HNS_PER_SEC;
use crate::mp4::{self, AAC_FRAME, FragmentedWriter, MuxStats};
use crate::pipeline::Shared as VideoShared;
use crate::ring::Packet;
use anyhow::{Context, Result, anyhow};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

pub struct RecordingSource {
    pub video: Arc<VideoShared>,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub audio: Option<Arc<AudioShared>>,
}

pub struct Recorder {
    pub path: PathBuf,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<Result<MuxStats>>>,
}

impl Recorder {
    pub fn start(src: RecordingSource, path: PathBuf) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let stop = Arc::new(AtomicBool::new(false));
        let (init_tx, init_rx) = crossbeam_channel::bounded::<Result<()>>(1);
        let thread = {
            let stop = stop.clone();
            let path = path.clone();
            std::thread::Builder::new()
                .name("recorder".into())
                .spawn(move || run(src, path, &stop, init_tx))?
        };
        init_rx
            .recv_timeout(Duration::from_secs(5))
            .map_err(|_| anyhow!("recording did not start (no video yet?)"))??;
        Ok(Self {
            path,
            stop,
            thread: Some(thread),
        })
    }

    /// True if the recorder thread ended on its own (error).
    pub fn is_finished(&self) -> bool {
        self.thread.as_ref().is_none_or(|t| t.is_finished())
    }

    pub fn stop(mut self) -> Result<MuxStats> {
        self.stop.store(true, Ordering::SeqCst);
        let t = self.thread.take().context("recorder already stopped")?;
        t.join().map_err(|_| anyhow!("recorder thread panicked"))?
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn run(
    src: RecordingSource,
    path: PathBuf,
    stop: &AtomicBool,
    init: crossbeam_channel::Sender<Result<()>>,
) -> Result<MuxStats> {
    // Begin at the latest keyframe (≤ 1 GOP before the key press).
    let deadline = Instant::now() + Duration::from_secs(4);
    let (first, sps, pps) = loop {
        let params = src.video.params.lock().clone();
        let packets = src.video.ring.lock().video_snapshot(0);
        if let (Some((sps, pps)), false) = (params, packets.is_empty()) {
            break (packets, sps, pps);
        }
        if Instant::now() > deadline {
            let e = anyhow!("no video available to record");
            let _ = init.send(Err(anyhow!("{e}")));
            return Err(e);
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let origin = first[0].pts;
    let names: Vec<String> = match &src.audio {
        Some(a) => std::iter::once("Mix".to_string())
            .chain(a.tracks.iter().map(|t| t.name.clone()))
            .collect(),
        None => Vec::new(),
    };
    let audio_infos: Vec<mp4::AudioTrack> = names
        .iter()
        .enumerate()
        .map(|(i, n)| mp4::AudioTrack {
            name: n,
            sample_rate: audio::RATE,
            channels: audio::CHANNELS,
            enabled: i == 0,
            packets: &[],
        })
        .collect();
    let video_info = mp4::VideoTrack {
        width: src.width,
        height: src.height,
        fps: src.fps,
        sps: &sps,
        pps: &pps,
        packets: &first[..1],
    };
    let mut writer = match FragmentedWriter::create(&path, &video_info, &audio_infos) {
        Ok(w) => w,
        Err(e) => {
            let _ = init.send(Err(anyhow!("{e:#}")));
            return Err(e);
        }
    };
    let _ = init.send(Ok(()));
    tracing::info!(path = %path.display(), "recording started");

    let half = AAC_FRAME as i64 * HNS_PER_SEC / audio::RATE as i64 / 2;
    let mut audio_cursor = vec![origin - half; names.len()];
    let mut last_video = first.last().map(|p| p.pts).unwrap_or(origin);
    let mut pending: Vec<Packet> = first;

    let take_audio = |cursors: &mut Vec<i64>, until: i64| -> Vec<Vec<Packet>> {
        let Some(a) = &src.audio else { return Vec::new() };
        let rings = std::iter::once(&a.mix).chain(a.tracks.iter().map(|t| &t.ring));
        rings
            .zip(cursors.iter_mut())
            .map(|(ring, cursor)| {
                let packets = ring.lock().range(*cursor, until);
                *cursor = until;
                packets
            })
            .collect()
    };
    let audio_ready = |until: i64| src.audio.as_ref().is_none_or(|a| a.encoded_until() >= until);

    loop {
        let stopping = stop.load(Ordering::SeqCst);
        {
            let ring = src.video.ring.lock();
            let new = ring.range(last_video + 1, i64::MAX);
            if let Some(p) = new.last() {
                last_video = p.pts;
            }
            pending.extend(new);
        }
        // One fragment per GOP, once audio has caught up (bounded so a dead source can't block).
        while let Some(k) = pending.iter().skip(1).position(|p| p.key).map(|i| i + 1) {
            let next = pending[k].pts;
            if !audio_ready(next) && last_video - next < HNS_PER_SEC {
                break;
            }
            let audio = take_audio(&mut audio_cursor, next);
            let refs: Vec<&[Packet]> = audio.iter().map(|v| v.as_slice()).collect();
            writer.write_fragment(&pending[..k], Some(next), &refs)?;
            pending.drain(..k);
        }
        if stopping {
            let end = pending.last().map(|p| p.pts + p.duration).unwrap_or(last_video);
            let wait_until = Instant::now() + Duration::from_millis(400);
            while !audio_ready(end) && Instant::now() < wait_until {
                std::thread::sleep(Duration::from_millis(10));
            }
            let audio = take_audio(&mut audio_cursor, end);
            let refs: Vec<&[Packet]> = audio.iter().map(|v| v.as_slice()).collect();
            writer.write_fragment(&pending, None, &refs)?;
            let stats = writer.finish()?;
            tracing::info!(path = %path.display(), secs = stats.duration_hns as f64 / 1e7, "recording stopped");
            return Ok(stats);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}
