//! Audio: WASAPI system loopback + microphone → continuous, QPC-locked timelines → mixed and
//! separate AAC tracks.
//!
//! Every WASAPI packet carries the QPC time of its first frame. Each source is written onto
//! a timeline whose sample 0 is a fixed QPC instant:
//! - packets that arrive late leave a gap → filled with silence;
//! - packets that overlap what was already written are trimmed;
//! - when a source goes quiet (loopback stops delivering when nothing plays, unplugged mic)
//!   the timeline is padded with silence up to "now − latency".
//! So sample N of every track always means the same wall-clock instant as the video frame
//! stamped with that time. That is the whole A/V sync story.

use crate::clock::{self, HNS_PER_SEC};
use crate::encoder::ComScope;
use crate::mp4::AAC_FRAME;
use crate::ring::{Packet, PacketRing};
use anyhow::{Context, Result, anyhow, bail};
use parking_lot::Mutex;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use windows::Win32::Media::Audio::*;
use windows::Win32::Media::KernelStreaming::WAVE_FORMAT_EXTENSIBLE;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::Media::Multimedia::KSDATAFORMAT_SUBTYPE_IEEE_FLOAT;
use windows::Win32::System::Com::{CLSCTX_ALL, CoCreateInstance, CoTaskMemFree};

pub const RATE: u32 = 48_000;
pub const CHANNELS: u16 = 2;
/// How far behind "now" a silent source is padded. Must exceed WASAPI packet latency.
const PAD_LAG_HNS: i64 = 1_000_000; // 100 ms
/// Timestamp jitter absorbed before inserting silence / dropping samples.
const TOLERANCE_FRAMES: i64 = 480; // 10 ms
const AAC_BYTES_PER_SEC: u32 = 24_000; // 192 kbps

/// Encoder delay of the Microsoft AAC encoder in samples: output packet `i` decodes to the
/// input samples starting at `i * 1024 - AAC_DELAY`. Measured with `--selftest-aac`: the
/// Microsoft encoder already emits sample-aligned output (clicks decode at their input index).
pub const AAC_DELAY: i64 = 0;

// ------------------------------------------------------------------------------------------
// Timeline

pub struct Timeline {
    origin_hns: i64,
    /// Interleaved stereo samples not yet consumed by the mixer.
    pending: VecDeque<f32>,
    /// Total frames ever written (consumed + pending).
    written: i64,
}

impl Timeline {
    pub fn new(origin_hns: i64) -> Self {
        Self {
            origin_hns,
            pending: VecDeque::new(),
            written: 0,
        }
    }

    fn index_of(&self, t_hns: i64) -> i64 {
        ((t_hns - self.origin_hns) as i128 * RATE as i128 / HNS_PER_SEC as i128) as i64
    }

    pub fn written(&self) -> i64 {
        self.written
    }

    fn silence(&mut self, frames: i64) {
        let n = frames.max(0) as usize * CHANNELS as usize;
        self.pending.extend(std::iter::repeat_n(0.0, n));
        self.written += frames.max(0);
    }

    /// Places `data` (interleaved stereo) whose first frame was captured at `t_hns`.
    pub fn push(&mut self, t_hns: i64, data: &[f32]) {
        let frames = (data.len() / CHANNELS as usize) as i64;
        let ideal = self.index_of(t_hns);
        let mut skip = 0i64;
        if ideal > self.written + TOLERANCE_FRAMES {
            self.silence(ideal - self.written);
        } else if ideal < self.written - TOLERANCE_FRAMES {
            skip = (self.written - ideal).min(frames);
        }
        let data = &data[skip as usize * CHANNELS as usize..];
        self.pending.extend(data.iter().copied());
        self.written += frames - skip;
    }

    /// Pads with silence so the timeline reaches at least `t_hns`.
    pub fn pad_until(&mut self, t_hns: i64) {
        let target = self.index_of(t_hns);
        if target > self.written {
            self.silence(target - self.written);
        }
    }

    /// Removes `frames` frames from the front into `out` (interleaved).
    pub fn take(&mut self, frames: usize, out: &mut Vec<f32>) {
        out.clear();
        out.extend(self.pending.drain(..frames * CHANNELS as usize));
    }

    pub fn pending_frames(&self) -> usize {
        self.pending.len() / CHANNELS as usize
    }
}

// ------------------------------------------------------------------------------------------
// WASAPI source

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceKind {
    SystemLoopback,
    Microphone,
}

struct Source {
    kind: SourceKind,
    client: IAudioClient,
    capture: IAudioCaptureClient,
    name: String,
}

fn float_stereo_48k() -> WAVEFORMATEXTENSIBLE {
    let block = CHANNELS * 4;
    WAVEFORMATEXTENSIBLE {
        Format: WAVEFORMATEX {
            wFormatTag: WAVE_FORMAT_EXTENSIBLE as u16,
            nChannels: CHANNELS,
            nSamplesPerSec: RATE,
            nAvgBytesPerSec: RATE * block as u32,
            nBlockAlign: block,
            wBitsPerSample: 32,
            cbSize: 22,
        },
        Samples: WAVEFORMATEXTENSIBLE_0 {
            wValidBitsPerSample: 32,
        },
        dwChannelMask: 0x3, // front left | front right
        SubFormat: KSDATAFORMAT_SUBTYPE_IEEE_FLOAT,
    }
}

impl Source {
    fn open(kind: SourceKind) -> Result<Self> {
        unsafe {
            let enumerator: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
            let flow = if kind == SourceKind::SystemLoopback {
                eRender
            } else {
                eCapture
            };
            let device = enumerator
                .GetDefaultAudioEndpoint(flow, eConsole)
                .context("no default audio device")?;
            let name = device_name(&device).unwrap_or_else(|| "unknown device".into());
            let client: IAudioClient = device.Activate(CLSCTX_ALL, None)?;
            let fmt = float_stereo_48k();
            let mut flags = AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;
            if kind == SourceKind::SystemLoopback {
                flags |= AUDCLNT_STREAMFLAGS_LOOPBACK;
            }
            // 200 ms buffer; we poll every 10 ms.
            client
                .Initialize(
                    AUDCLNT_SHAREMODE_SHARED,
                    flags,
                    2_000_000,
                    0,
                    &fmt as *const _ as *const WAVEFORMATEX,
                    None,
                )
                .context("IAudioClient::Initialize")?;
            let capture: IAudioCaptureClient = client.GetService()?;
            client.Start()?;
            tracing::info!(?kind, device = %name, "audio source opened");
            Ok(Self {
                kind,
                client,
                capture,
                name,
            })
        }
    }

    /// Drains all available packets into the timeline.
    fn drain(&mut self, timeline: &mut Timeline, scratch: &mut Vec<f32>) -> Result<()> {
        unsafe {
            loop {
                let next = self.capture.GetNextPacketSize()?;
                if next == 0 {
                    return Ok(());
                }
                let mut data = std::ptr::null_mut();
                let mut frames = 0u32;
                let mut flags = 0u32;
                let mut qpc = 0u64;
                self.capture
                    .GetBuffer(&mut data, &mut frames, &mut flags, None, Some(&mut qpc))?;
                let n = frames as usize * CHANNELS as usize;
                scratch.clear();
                if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 || data.is_null() {
                    scratch.resize(n, 0.0);
                } else {
                    scratch.extend_from_slice(std::slice::from_raw_parts(data as *const f32, n));
                }
                self.capture.ReleaseBuffer(frames)?;
                // qpc is already in 100 ns units on the QPC timebase.
                let t = if qpc != 0 { qpc as i64 } else { clock::now_hns() };
                timeline.push(t, scratch);
            }
        }
    }
}

impl Drop for Source {
    fn drop(&mut self) {
        unsafe {
            let _ = self.client.Stop();
        }
    }
}

fn device_name(device: &IMMDevice) -> Option<String> {
    use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
    use windows::Win32::System::Com::STGM_READ;
    unsafe {
        let store = device.OpenPropertyStore(STGM_READ).ok()?;
        let v = store.GetValue(&PKEY_Device_FriendlyName).ok()?;
        Some(v.to_string())
    }
}

// ------------------------------------------------------------------------------------------
// AAC encoder (Microsoft AAC encoder MFT, synchronous)

pub struct AacEncoder {
    transform: IMFTransform,
    frames_in: i64,
    out_size: u32,
}

impl AacEncoder {
    pub fn new() -> Result<Self> {
        let transform = find_aac_encoder()?;
        unsafe {
            let out = MFCreateMediaType()?;
            out.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
            out.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_AAC)?;
            out.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
            out.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, RATE)?;
            out.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, CHANNELS as u32)?;
            out.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, AAC_BYTES_PER_SEC)?;
            out.SetUINT32(&MF_MT_AAC_PAYLOAD_TYPE, 0)?; // raw AAC
            out.SetUINT32(&MF_MT_AAC_AUDIO_PROFILE_LEVEL_INDICATION, 0x29)?;

            let inp = MFCreateMediaType()?;
            inp.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
            inp.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_PCM)?;
            inp.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
            inp.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, RATE)?;
            inp.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, CHANNELS as u32)?;
            inp.SetUINT32(&MF_MT_AUDIO_BLOCK_ALIGNMENT, CHANNELS as u32 * 2)?;
            inp.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, RATE * CHANNELS as u32 * 2)?;

            transform.SetInputType(0, &inp, 0).context("AAC SetInputType")?;
            transform.SetOutputType(0, &out, 0).context("AAC SetOutputType")?;
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
            let info = transform.GetOutputStreamInfo(0)?;
            Ok(Self {
                transform,
                frames_in: 0,
                out_size: info.cbSize.max(8192),
            })
        }
    }

    /// Encodes interleaved stereo f32; returns zero or more raw AAC frames.
    pub fn encode(&mut self, pcm: &[f32]) -> Result<Vec<Vec<u8>>> {
        let frames = (pcm.len() / CHANNELS as usize) as i64;
        let bytes: Vec<u8> = pcm
            .iter()
            .flat_map(|&s| ((s.clamp(-1.0, 1.0) * 32767.0).round() as i16).to_le_bytes())
            .collect();
        let mut out = Vec::new();
        unsafe {
            let buffer = MFCreateMemoryBuffer(bytes.len() as u32)?;
            let mut ptr = std::ptr::null_mut();
            buffer.Lock(&mut ptr, None, None)?;
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr, bytes.len());
            buffer.Unlock()?;
            buffer.SetCurrentLength(bytes.len() as u32)?;
            let sample = MFCreateSample()?;
            sample.AddBuffer(&buffer)?;
            sample.SetSampleTime(self.frames_in * HNS_PER_SEC / RATE as i64)?;
            sample.SetSampleDuration(frames * HNS_PER_SEC / RATE as i64)?;
            self.transform.ProcessInput(0, &sample, 0).context("AAC ProcessInput")?;
            self.frames_in += frames;
            loop {
                let s = MFCreateSample()?;
                s.AddBuffer(&MFCreateMemoryBuffer(self.out_size)?)?;
                let mut buf = [MFT_OUTPUT_DATA_BUFFER {
                    dwStreamID: 0,
                    pSample: std::mem::ManuallyDrop::new(Some(s)),
                    dwStatus: 0,
                    pEvents: std::mem::ManuallyDrop::new(None),
                }];
                let mut status = 0u32;
                let r = self.transform.ProcessOutput(0, &mut buf, &mut status);
                let s = std::mem::ManuallyDrop::take(&mut buf[0].pSample);
                std::mem::ManuallyDrop::drop(&mut buf[0].pEvents);
                match r {
                    Ok(()) => {}
                    Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => break,
                    Err(e) => return Err(e).context("AAC ProcessOutput"),
                }
                let Some(s) = s else { break };
                let b = s.ConvertToContiguousBuffer()?;
                let mut p = std::ptr::null_mut();
                let mut len = 0u32;
                b.Lock(&mut p, None, Some(&mut len))?;
                out.push(std::slice::from_raw_parts(p, len as usize).to_vec());
                b.Unlock()?;
            }
        }
        Ok(out)
    }
}

fn find_aac_encoder() -> Result<IMFTransform> {
    let input = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Audio,
        guidSubtype: MFAudioFormat_PCM,
    };
    let output = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Audio,
        guidSubtype: MFAudioFormat_AAC,
    };
    let mut ptr: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count = 0u32;
    unsafe {
        MFTEnumEx(
            MFT_CATEGORY_AUDIO_ENCODER,
            MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_SORTANDFILTER,
            Some(&input),
            Some(&output),
            &mut ptr,
            &mut count,
        )?;
        if ptr.is_null() || count == 0 {
            bail!("no AAC encoder installed (Windows N edition without the Media Feature Pack?)");
        }
        let slots = std::slice::from_raw_parts_mut(ptr, count as usize);
        let first = slots[0].take();
        for s in slots.iter_mut() {
            s.take();
        }
        CoTaskMemFree(Some(ptr as *const _));
        Ok(first.ok_or_else(|| anyhow!("AAC activate missing"))?.ActivateObject()?)
    }
}

// ------------------------------------------------------------------------------------------
// Engine

#[derive(Clone, Debug)]
pub struct AudioConfig {
    pub mic: bool,
    pub game_gain: f32,
    pub mic_gain: f32,
    pub replay_seconds: u32,
}

pub struct AudioShared {
    pub mix: Mutex<PacketRing>,
    pub game: Mutex<PacketRing>,
    pub mic: Mutex<PacketRing>,
    pub mic_enabled: bool,
    pub game_device: Mutex<String>,
    pub mic_device: Mutex<String>,
}

impl AudioShared {
    /// Audio packets covering `[start, end)` (hns), starting at the packet nearest `start`.
    pub fn snapshot(&self, start: i64, end: i64) -> AudioSnapshot {
        let half = AAC_FRAME as i64 * HNS_PER_SEC / RATE as i64 / 2;
        let pick = |r: &Mutex<PacketRing>| r.lock().range(start - half, end);
        AudioSnapshot {
            mix: pick(&self.mix),
            game: pick(&self.game),
            mic: if self.mic_enabled { pick(&self.mic) } else { Vec::new() },
        }
    }

    /// The newest audio time already encoded (hns), across all tracks.
    pub fn encoded_until(&self) -> i64 {
        self.mix.lock().newest_end().unwrap_or(i64::MIN)
    }
}

pub struct AudioSnapshot {
    pub mix: Vec<Packet>,
    pub game: Vec<Packet>,
    pub mic: Vec<Packet>,
}

pub struct AudioEngine {
    pub shared: Arc<AudioShared>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

const AUDIO_MAX_BYTES: usize = 256 << 20;

impl AudioEngine {
    pub fn start(cfg: AudioConfig) -> Result<Self> {
        // Keep a little more audio than video so alignment never runs short.
        let span = (cfg.replay_seconds as i64 + 3) * HNS_PER_SEC;
        let shared = Arc::new(AudioShared {
            mix: Mutex::new(PacketRing::audio(span, AUDIO_MAX_BYTES)),
            game: Mutex::new(PacketRing::audio(span, AUDIO_MAX_BYTES)),
            mic: Mutex::new(PacketRing::audio(span, AUDIO_MAX_BYTES)),
            mic_enabled: cfg.mic,
            game_device: Mutex::new(String::new()),
            mic_device: Mutex::new(String::new()),
        });
        let stop = Arc::new(AtomicBool::new(false));
        let (init_tx, init_rx) = crossbeam_channel::bounded::<Result<()>>(1);
        let thread = {
            let shared = shared.clone();
            let stop = stop.clone();
            std::thread::Builder::new().name("audio".into()).spawn(move || {
                let _com = ComScope::init();
                if let Err(e) = run_audio(&cfg, &shared, &stop, init_tx) {
                    tracing::error!("audio engine stopped: {e:#}");
                }
            })?
        };
        init_rx.recv().map_err(|_| anyhow!("audio thread died during init"))??;
        Ok(Self {
            shared,
            stop,
            thread: Some(thread),
        })
    }

    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for AudioEngine {
    fn drop(&mut self) {
        self.stop();
    }
}

struct Track {
    encoder: AacEncoder,
    packets_out: i64,
}

impl Track {
    fn new() -> Result<Self> {
        Ok(Self {
            encoder: AacEncoder::new()?,
            packets_out: 0,
        })
    }

    fn encode(&mut self, origin: i64, pcm: &[f32], ring: &Mutex<PacketRing>) -> Result<()> {
        let frame_hns = AAC_FRAME as i64 * HNS_PER_SEC / RATE as i64;
        for data in self.encoder.encode(pcm)? {
            let first_sample = self.packets_out * AAC_FRAME as i64 - AAC_DELAY;
            self.packets_out += 1;
            if first_sample < 0 {
                continue; // pure priming
            }
            let pts = origin + (first_sample as i128 * HNS_PER_SEC as i128 / RATE as i128) as i64;
            ring.lock().push(Packet {
                pts,
                duration: frame_hns,
                key: true,
                data: Arc::from(data),
            });
        }
        Ok(())
    }
}

struct SourceSlot {
    kind: SourceKind,
    source: Option<Source>,
    timeline: Timeline,
    retry_at: i64,
}

impl SourceSlot {
    fn poll(&mut self, scratch: &mut Vec<f32>, device_label: &Mutex<String>) {
        let now = clock::now_hns();
        if self.source.is_none() && now >= self.retry_at {
            match Source::open(self.kind) {
                Ok(s) => {
                    *device_label.lock() = s.name.clone();
                    self.source = Some(s);
                }
                Err(e) => {
                    tracing::warn!(kind = ?self.kind, "audio source unavailable: {e:#}");
                    device_label.lock().clear();
                    self.retry_at = now + 2 * HNS_PER_SEC;
                }
            }
        }
        if let Some(s) = &mut self.source {
            if let Err(e) = s.drain(&mut self.timeline, scratch) {
                // Typically AUDCLNT_E_DEVICE_INVALIDATED: headset unplugged / default changed.
                tracing::warn!(kind = ?self.kind, "audio source lost: {e:#}; reopening");
                self.source = None;
                self.retry_at = now + HNS_PER_SEC / 2;
            }
        }
        self.timeline.pad_until(now - PAD_LAG_HNS);
    }
}

fn run_audio(
    cfg: &AudioConfig,
    shared: &AudioShared,
    stop: &AtomicBool,
    init: crossbeam_channel::Sender<Result<()>>,
) -> Result<()> {
    let origin = clock::now_hns();
    let mut tracks = match (|| -> Result<_> { Ok((Track::new()?, Track::new()?, Track::new()?)) })() {
        Ok(t) => t,
        Err(e) => {
            let _ = init.send(Err(anyhow!("{e:#}")));
            return Err(e);
        }
    };
    let mut game = SourceSlot {
        kind: SourceKind::SystemLoopback,
        source: None,
        timeline: Timeline::new(origin),
        retry_at: 0,
    };
    let mut mic = SourceSlot {
        kind: SourceKind::Microphone,
        source: None,
        timeline: Timeline::new(origin),
        retry_at: 0,
    };
    let _ = init.send(Ok(()));

    let block = AAC_FRAME as usize;
    let (mut scratch, mut g, mut m, mut mix) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    let mut silence = Vec::new();
    while !stop.load(Ordering::Relaxed) {
        game.poll(&mut scratch, &shared.game_device);
        if cfg.mic {
            mic.poll(&mut scratch, &shared.mic_device);
        }
        loop {
            let ready = if cfg.mic {
                game.timeline.pending_frames().min(mic.timeline.pending_frames())
            } else {
                game.timeline.pending_frames()
            };
            if ready < block {
                break;
            }
            game.timeline.take(block, &mut g);
            if cfg.mic {
                mic.timeline.take(block, &mut m);
            } else {
                silence.resize(g.len(), 0.0);
                m.clone_from(&silence);
            }
            mix.clear();
            mix.extend(
                g.iter()
                    .zip(&m)
                    .map(|(a, b)| soft_clip(a * cfg.game_gain + b * cfg.mic_gain)),
            );
            tracks.0.encode(origin, &mix, &shared.mix)?;
            tracks.1.encode(origin, &g, &shared.game)?;
            if cfg.mic {
                tracks.2.encode(origin, &m, &shared.mic)?;
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    Ok(())
}

/// Transparent below ~-1 dBFS, then a smooth knee instead of hard clipping.
fn soft_clip(x: f32) -> f32 {
    const KNEE: f32 = 0.9;
    let a = x.abs();
    if a <= KNEE {
        x
    } else {
        let over = a - KNEE;
        let y = KNEE + (1.0 - KNEE) * (over / (1.0 - KNEE)).tanh();
        y.copysign(x)
    }
}

/// Encodes a click train and writes ADTS so the encoder delay can be measured externally.
pub fn selftest_aac(path: &std::path::Path) -> Result<()> {
    let mut enc = AacEncoder::new()?;
    let total = RATE as usize * 2;
    let mut pcm = vec![0f32; total * 2];
    // Single-sample clicks at 0.5 s and 1.0 s.
    for at in [RATE as usize / 2, RATE as usize] {
        pcm[at * 2] = 0.9;
        pcm[at * 2 + 1] = 0.9;
    }
    let mut out = Vec::new();
    for chunk in pcm.chunks(AAC_FRAME as usize * 2) {
        for f in enc.encode(chunk)? {
            let len = f.len() + 7;
            let (profile, sf, ch) = (1u8, 3u8, 2u8);
            out.extend_from_slice(&[
                0xFF,
                0xF1,
                (profile << 6) | (sf << 2) | (ch >> 2),
                ((ch & 3) << 6) | ((len >> 11) as u8 & 0x3),
                ((len >> 3) & 0xFF) as u8,
                (((len & 7) << 5) as u8) | 0x1F,
                0xFC,
            ]);
            out.extend_from_slice(&f);
        }
    }
    std::fs::write(path, out)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frames(n: usize) -> Vec<f32> {
        vec![0.5; n * 2]
    }

    fn hns(frames: i64) -> i64 {
        frames * HNS_PER_SEC / RATE as i64
    }

    #[test]
    fn contiguous_packets_append() {
        let mut t = Timeline::new(0);
        t.push(0, &frames(480));
        t.push(hns(480), &frames(480));
        assert_eq!(t.written(), 960);
    }

    #[test]
    fn late_packet_inserts_silence() {
        let mut t = Timeline::new(0);
        t.push(0, &frames(480));
        t.push(hns(48_000), &frames(480)); // 1 s later
        assert_eq!(t.written(), 48_000 + 480);
        let mut out = Vec::new();
        t.take(481, &mut out);
        assert_eq!(out[480 * 2], 0.0, "gap is silence");
    }

    #[test]
    fn overlapping_packet_is_trimmed() {
        let mut t = Timeline::new(0);
        t.push(0, &frames(4800));
        t.push(hns(2400), &frames(4800)); // overlaps by 2400
        assert_eq!(t.written(), 7200);
    }

    #[test]
    fn small_jitter_is_absorbed() {
        let mut t = Timeline::new(0);
        t.push(0, &frames(480));
        t.push(hns(480 + 100), &frames(480)); // 2 ms late: within tolerance
        assert_eq!(t.written(), 960);
    }

    #[test]
    fn padding_reaches_target() {
        let mut t = Timeline::new(1_000);
        t.pad_until(1_000 + HNS_PER_SEC);
        assert_eq!(t.written(), 48_000);
    }

    #[test]
    fn soft_clip_is_bounded_and_transparent() {
        assert_eq!(soft_clip(0.5), 0.5);
        assert!(soft_clip(5.0) <= 1.0);
        assert!(soft_clip(-5.0) >= -1.0);
    }
}
