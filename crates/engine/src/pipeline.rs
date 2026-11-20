//! Video pipeline: WGC capture → constant-rate ticker → GPU NV12 conversion → hardware
//! encoder → replay ring. Timestamps are absolute QPC time (hns), so audio captured on the
//! same clock lines up without any offset bookkeeping.

use crate::capture::{LatestFrame, MonitorCapture};
use crate::clock::{self, HNS_PER_SEC};
use crate::convert::{Converter, output_size};
use crate::d3d::{self, Gpu, Sendable};
use crate::encoder::{EncodeJob, EncoderConfig, VideoEncoder};
use crate::ring::{Packet, PacketRing};
use anyhow::{Context, Result};
use parking_lot::Mutex;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::JoinHandle;
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Graphics::Gdi::HMONITOR;
use windows::Win32::System::Threading::{
    CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, CreateWaitableTimerExW, INFINITE, SetWaitableTimer,
    TIMER_ALL_ACCESS, WaitForSingleObject,
};

const NV12_POOL: usize = 8;
const MAX_REPLAY_BYTES: usize = 1536 << 20;

#[derive(Clone, Debug)]
pub struct VideoConfig {
    pub fps: u32,
    pub bitrate_kbps: u32,
    /// 0 = native resolution.
    pub target_height: u32,
    pub cursor: bool,
    pub replay_seconds: u32,
}

#[derive(Default)]
pub struct Stats {
    pub ticks: AtomicU64,
    pub dropped: AtomicU64,
    pub encoded: AtomicU64,
    pub captured: AtomicU64,
}

pub struct Shared {
    pub ring: Mutex<PacketRing>,
    /// Latest SPS/PPS seen on a keyframe.
    pub params: Mutex<Option<(Vec<u8>, Vec<u8>)>>,
    pub stats: Stats,
    pub failed: AtomicBool,
}

pub struct VideoSnapshot {
    pub packets: Vec<Packet>,
    pub sps: Vec<u8>,
    pub pps: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
}

pub struct VideoPipeline {
    pub gpu: Gpu,
    pub encoder_name: String,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub shared: Arc<Shared>,
    stop: Arc<AtomicBool>,
    ticker: Option<JoinHandle<()>>,
    encoder: Option<VideoEncoder>,
    capture: Option<MonitorCapture>,
}

impl VideoPipeline {
    pub fn start(monitor: Option<HMONITOR>, cfg: &VideoConfig) -> Result<Self> {
        let monitor = monitor.unwrap_or_else(d3d::primary_monitor);
        let gpu = d3d::create_for_monitor(monitor)?;
        let capture =
            MonitorCapture::start(&gpu, monitor, cfg.cursor).context("start screen capture")?;
        let (out_w, out_h) = output_size(capture.width, capture.height, cfg.target_height);
        let fps = cfg.fps.clamp(10, 240);

        let src_tex = capture.latest.lock().0.tex.clone();
        let converter = Converter::new(
            &gpu,
            &src_tex,
            capture.width,
            capture.height,
            out_w,
            out_h,
            fps,
            NV12_POOL,
        )?;

        let shared = Arc::new(Shared {
            ring: Mutex::new(PacketRing::video(
                cfg.replay_seconds as i64 * HNS_PER_SEC,
                MAX_REPLAY_BYTES,
            )),
            params: Mutex::new(None),
            stats: Stats::default(),
            failed: AtomicBool::new(false),
        });

        // Bounded so a stalled encoder drops frames instead of overwriting textures it still reads.
        let (tx, rx) = crossbeam_channel::bounded::<EncodeJob>(NV12_POOL / 2 - 1);
        let enc_cfg = EncoderConfig {
            width: out_w,
            height: out_h,
            fps,
            bitrate_kbps: cfg.bitrate_kbps,
            gop_frames: fps,
        };
        let on_frame_shared = shared.clone();
        let on_error_shared = shared.clone();
        let encoder = VideoEncoder::start(
            &gpu.device,
            gpu.luid,
            gpu.adapter_name.clone(),
            enc_cfg,
            rx,
            move |frame| {
                let s = &on_frame_shared;
                s.stats.encoded.fetch_add(1, Ordering::Relaxed);
                if let (Some(sps), Some(pps)) = (&frame.au.sps, &frame.au.pps) {
                    *s.params.lock() = Some((sps.clone(), pps.clone()));
                }
                if frame.au.avcc.is_empty() {
                    return;
                }
                s.ring.lock().push(Packet {
                    pts: frame.pts,
                    duration: frame.duration,
                    key: frame.au.idr,
                    data: Arc::from(frame.au.avcc),
                });
            },
            move |_| on_error_shared.failed.store(true, Ordering::SeqCst),
        )?;

        let stop = Arc::new(AtomicBool::new(false));
        let ticker = {
            let stop = stop.clone();
            let shared = shared.clone();
            let latest = capture.latest.clone();
            let converter = Sendable(converter);
            std::thread::Builder::new()
                .name("video-ticker".into())
                .spawn(move || {
                    let converter = converter;
                    if let Err(e) = run_ticker(&stop, &shared, &latest, &converter.0, fps, tx) {
                        tracing::error!("video ticker failed: {e:#}");
                        shared.failed.store(true, Ordering::SeqCst);
                    }
                })?
        };

        tracing::info!(adapter = %gpu.adapter_name, encoder = %encoder.name, out_w, out_h, fps, "video pipeline running");
        Ok(Self {
            gpu,
            encoder_name: encoder.name.clone(),
            width: out_w,
            height: out_h,
            fps,
            shared,
            stop,
            ticker: Some(ticker),
            encoder: Some(encoder),
            capture: Some(capture),
        })
    }

    pub fn snapshot(&self, seconds: u32) -> Option<VideoSnapshot> {
        let (sps, pps) = self.shared.params.lock().clone()?;
        let packets = self
            .shared
            .ring
            .lock()
            .video_snapshot(seconds as i64 * HNS_PER_SEC);
        if packets.is_empty() {
            return None;
        }
        Some(VideoSnapshot {
            packets,
            sps,
            pps,
            width: self.width,
            height: self.height,
            fps: self.fps,
        })
    }

    pub fn failed(&self) -> bool {
        self.shared.failed.load(Ordering::SeqCst)
    }

    pub fn source_changed(&self) -> bool {
        self.capture
            .as_ref()
            .is_some_and(|c| c.size_changed.load(Ordering::SeqCst))
    }

    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.ticker.take() {
            let _ = t.join(); // drops the job sender → encoder drains
        }
        if let Some(mut e) = self.encoder.take() {
            e.join();
        }
        self.capture.take();
    }
}

impl Drop for VideoPipeline {
    fn drop(&mut self) {
        self.stop();
    }
}

fn run_ticker(
    stop: &AtomicBool,
    shared: &Shared,
    latest: &Mutex<Sendable<LatestFrame>>,
    converter: &Converter,
    fps: u32,
    tx: crossbeam_channel::Sender<EncodeJob>,
) -> Result<()> {
    let timer = PreciseTimer::new()?;
    let fps = fps as i64;
    let t0 = clock::now_hns();
    let frame_time = |k: i64| t0 + k * HNS_PER_SEC / fps;
    let mut k: i64 = -1;
    let mut slot = 0usize;
    while !stop.load(Ordering::Relaxed) {
        timer.sleep_until(frame_time(k + 1));
        let target = (clock::now_hns() - t0) * fps / HNS_PER_SEC;
        if target <= k {
            continue;
        }
        if target > k + 1 {
            shared
                .stats
                .dropped
                .fetch_add((target - k - 1) as u64, Ordering::Relaxed);
        }
        k = target;
        shared.stats.ticks.fetch_add(1, Ordering::Relaxed);
        {
            let l = latest.lock();
            if l.0.frames == 0 {
                continue; // nothing captured yet; don't encode black
            }
            shared.stats.captured.store(l.0.frames, Ordering::Relaxed);
            converter.convert(slot)?;
        }
        let pts = frame_time(k);
        let job = EncodeJob {
            texture: Sendable(converter.texture(slot).clone()),
            pts,
            duration: frame_time(k + 1) - pts,
        };
        match tx.try_send(job) {
            Ok(()) => slot = (slot + 1) % converter.pool_len(),
            Err(crossbeam_channel::TrySendError::Full(_)) => {
                shared.stats.dropped.fetch_add(1, Ordering::Relaxed);
            }
            Err(crossbeam_channel::TrySendError::Disconnected(_)) => break,
        }
    }
    Ok(())
}

/// Sub-millisecond sleeps without raising the global timer resolution.
pub struct PreciseTimer(HANDLE);
unsafe impl Send for PreciseTimer {}

impl PreciseTimer {
    pub fn new() -> Result<Self> {
        let h = unsafe {
            CreateWaitableTimerExW(
                None,
                None,
                CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
                TIMER_ALL_ACCESS.0,
            )
        }
        .context("CreateWaitableTimerExW")?;
        Ok(Self(h))
    }

    pub fn sleep_until(&self, deadline_hns: i64) {
        let wait = deadline_hns - clock::now_hns();
        if wait <= 0 {
            return;
        }
        let due = -wait; // negative = relative, in hns
        unsafe {
            if SetWaitableTimer(self.0, &due, 0, None, None, false).is_ok() {
                WaitForSingleObject(self.0, INFINITE);
            } else {
                std::thread::sleep(std::time::Duration::from_nanos(wait as u64 * 100));
            }
        }
    }
}

impl Drop for PreciseTimer {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}
