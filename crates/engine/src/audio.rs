//! Audio: WASAPI system loopback + microphone → continuous, QPC-locked timelines → mixed and
//! separate AAC tracks.
//!
//! Every WASAPI packet carries the QPC time of its first frame. Each source is written onto
//! a timeline whose sample 0 is a fixed QPC instant:
//! - packets that arrive late leave a gap → filled with silence;
//! - packets that overlap what was already written are trimmed;
//! - when a source goes quiet (loopback stops delivering when nothing plays, unplugged mic)
//!   the timeline is padded with silence up to "now − latency";
//! - stamps a driver puts in the future (Realtek USB loopback: ~1 s) are pulled back to
//!   when the audio was really captured, and stamps impossibly far in the past are pulled
//!   forward (`StampFix`);
//! - a microphone that stops delivering packets, or delivers nothing but digital zeros
//!   (which no real microphone does), is reopened instead of padding silence forever.
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
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
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
pub const AAC_BYTES_PER_SEC: u32 = 24_000; // 192 kbps

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

    #[cfg(test)]
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
// Device timestamp sanity

/// Removes a device's timestamp error.
///
/// Some drivers (seen: Realtek USB Audio in loopback) stamp packets ~1 s or more after the
/// moment their audio was captured, so trusting the stamp places that source's sound late in
/// every clip. A packet's first frame can never have been captured after `now - duration`;
/// how far a stamp lands beyond that is the device's error minus our (small, positive)
/// pickup delay. The largest such excess over a recent window estimates the error, and it is
/// subtracted from every stamp. Honest devices never exceed the bound, so they are untouched.
///
/// The opposite error is as bad: a stamp far in the *past* lands behind what the timeline
/// already padded with silence, so the whole packet is dropped as overlap and the track
/// records silence. We poll every 10 ms, so when even the freshest packet of a window is well
/// behind real time the stamps are wrong, and are pulled forward by that much.
#[derive(Default)]
pub struct StampFix {
    correction: i64,
    window_max: i64,
    window_end: i64,
    warned: bool,
}

/// How long a lower error must persist before the correction relaxes to it.
const STAMP_WINDOW_HNS: i64 = 2 * HNS_PER_SEC;
/// Freshest packet of a window older than this: the device stamps in the past.
const STAMP_LATE_HNS: i64 = 1_000_000; // 100 ms
/// Older than the 200 ms capture buffer plus slack: impossible, corrected at once.
const STAMP_IMPOSSIBLE_HNS: i64 = 3_000_000; // 300 ms

impl StampFix {
    /// Corrected capture time for a packet stamped `stamp` that reached us at `now` and
    /// holds `dur` of audio (all hns, QPC timebase).
    pub fn correct(&mut self, stamp: i64, now: i64, dur: i64) -> i64 {
        let excess = stamp - (now - dur);
        // Never place audio in the future: raise immediately.
        self.correction = self.correction.max(excess);
        // Nor impossibly far in the past (this also drops a stale "future" correction fast).
        if excess - self.correction < -STAMP_IMPOSSIBLE_HNS {
            self.correction = excess;
        }
        if self.window_end == 0 {
            self.window_end = now + STAMP_WINDOW_HNS;
            self.window_max = excess;
        } else {
            self.window_max = self.window_max.max(excess);
        }
        if now >= self.window_end {
            // Follow a device whose error changed (or vanished) since.
            self.correction = if self.window_max < -STAMP_LATE_HNS {
                self.window_max
            } else {
                self.window_max.max(0)
            };
            self.window_max = i64::MIN;
            self.window_end = now + STAMP_WINDOW_HNS;
        }
        stamp - self.correction
    }

    /// Current correction in hns (0 for honest devices).
    pub fn correction(&self) -> i64 {
        self.correction
    }

    /// True once per source, the first time the error is large enough to mention.
    fn should_warn(&mut self) -> bool {
        if !self.warned && self.correction.abs() > 50 * HNS_PER_SEC / 1000 {
            self.warned = true;
            return true;
        }
        false
    }
}

// ------------------------------------------------------------------------------------------
// WASAPI source

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
    /// What a playback device plays (WASAPI loopback): game, Discord, music…
    Output,
    /// A recording device: microphone, line in, virtual cable…
    Input,
}

/// One audio source. Any number of each kind; every source becomes its own track and is
/// summed into the Mix track.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct SourceSpec {
    pub kind: SourceKind,
    /// Endpoint ID, or a case-insensitive part of the device name. `None` follows the
    /// Windows default device (also when the default changes).
    #[serde(default)]
    pub device: Option<String>,
    /// Track name in the clip; defaults to "Game"/"Mic" or the device text.
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default = "one")]
    pub gain: f32,
}

fn one() -> f32 {
    1.0
}

impl SourceSpec {
    pub fn default_output() -> Self {
        Self {
            kind: SourceKind::Output,
            device: None,
            label: None,
            gain: 1.0,
        }
    }

    pub fn default_input() -> Self {
        Self {
            kind: SourceKind::Input,
            device: None,
            label: None,
            gain: 1.0,
        }
    }

    /// CLI form: `default`, `<device text>`, optionally `@<gain>` (e.g. `Shure@0.8`).
    pub fn parse(kind: SourceKind, s: &str) -> Result<Self> {
        let (dev, gain) = match s.rsplit_once('@') {
            Some((d, g)) => (d, g.parse::<f32>().context("bad gain")?),
            None => (s, 1.0),
        };
        let device = (!dev.is_empty() && !dev.eq_ignore_ascii_case("default")).then(|| dev.to_owned());
        Ok(Self {
            kind,
            device,
            label: None,
            gain,
        })
    }

    fn flow(&self) -> EDataFlow {
        if self.kind == SourceKind::Output {
            eRender
        } else {
            eCapture
        }
    }
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct DeviceInfo {
    pub kind: SourceKind,
    pub id: String,
    pub name: String,
    pub default: bool,
}

/// Active audio endpoints, for the settings UI / `--list-audio`.
pub fn list_devices() -> Result<Vec<DeviceInfo>> {
    let mut out = Vec::new();
    unsafe {
        let enumerator: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        for (kind, flow) in [(SourceKind::Output, eRender), (SourceKind::Input, eCapture)] {
            let default_id = enumerator
                .GetDefaultAudioEndpoint(flow, eConsole)
                .ok()
                .and_then(|d| device_id(&d));
            let coll = enumerator.EnumAudioEndpoints(flow, DEVICE_STATE_ACTIVE)?;
            for i in 0..coll.GetCount()? {
                let d = coll.Item(i)?;
                let Some(id) = device_id(&d) else { continue };
                out.push(DeviceInfo {
                    kind,
                    default: default_id.as_deref() == Some(id.as_str()),
                    name: device_name(&d).unwrap_or_default(),
                    id,
                });
            }
        }
    }
    Ok(out)
}

fn device_id(d: &IMMDevice) -> Option<String> {
    unsafe {
        let p = d.GetId().ok()?;
        let s = p.to_string().ok();
        CoTaskMemFree(Some(p.0 as *const _));
        s
    }
}

fn resolve_device(spec: &SourceSpec) -> Result<IMMDevice> {
    unsafe {
        let enumerator: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        let Some(want) = spec.device.as_deref() else {
            return enumerator
                .GetDefaultAudioEndpoint(spec.flow(), eConsole)
                .context("no default audio device");
        };
        if let Ok(d) = enumerator.GetDevice(&windows::core::HSTRING::from(want)) {
            return Ok(d);
        }
        let needle = want.to_lowercase();
        let coll = enumerator.EnumAudioEndpoints(spec.flow(), DEVICE_STATE_ACTIVE)?;
        for i in 0..coll.GetCount()? {
            let d = coll.Item(i)?;
            if device_name(&d).is_some_and(|n| n.to_lowercase().contains(&needle)) {
                return Ok(d);
            }
        }
        bail!("audio device {want:?} not found (unplugged?)")
    }
}

/// What one `drain` call received.
#[derive(Default)]
struct Drained {
    packets: u32,
    /// Largest absolute sample; exactly 0.0 means digital silence.
    peak: f32,
}

struct Source {
    client: IAudioClient,
    capture: IAudioCaptureClient,
    name: String,
    stamps: StampFix,
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
    fn open(spec: &SourceSpec) -> Result<Self> {
        let kind = spec.kind;
        unsafe {
            let device = resolve_device(spec)?;
            let name = device_name(&device).unwrap_or_else(|| "unknown device".into());
            let client: IAudioClient = device.Activate(CLSCTX_ALL, None)?;
            let fmt = float_stereo_48k();
            let mut flags = AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;
            if kind == SourceKind::Output {
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
            Ok(Self {
                client,
                capture,
                name,
                stamps: StampFix::default(),
            })
        }
    }

    /// Drains all available packets into the timeline.
    fn drain(&mut self, timeline: &mut Timeline, scratch: &mut Vec<f32>) -> Result<Drained> {
        let mut got = Drained::default();
        unsafe {
            loop {
                let next = self.capture.GetNextPacketSize()?;
                if next == 0 {
                    return Ok(got);
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
                got.packets += 1;
                got.peak = scratch.iter().fold(got.peak, |p, &x| p.max(x.abs()));
                // qpc is already in 100 ns units on the QPC timebase.
                let now = clock::now_hns();
                let dur = frames as i64 * HNS_PER_SEC / RATE as i64;
                let stamp = if qpc != 0 { qpc as i64 } else { now - dur };
                let t = self.stamps.correct(stamp, now, dur);
                if self.stamps.should_warn() {
                    let ms = self.stamps.correction() / 10_000;
                    if ms > 0 {
                        tracing::warn!(device = %self.name, ms, "audio device timestamps run ahead of real time; correcting");
                    } else {
                        tracing::warn!(device = %self.name, ms, "audio device timestamps run behind real time; correcting");
                    }
                }
                timeline.push(t, scratch);
            }
        }
    }
}

/// `--meter`: opens one source for `secs` and prints, per second, its peak level and how far
/// the device's packet stamps sit from real time (negative = in the past). Diagnoses "my mic
/// records nothing" without saving clips.
pub fn meter(spec: &SourceSpec, secs: u32) -> Result<()> {
    let src = Source::open(spec)?;
    println!("device: {}", src.name);
    unsafe {
        let vol: windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume =
            resolve_device(spec)?.Activate(CLSCTX_ALL, None)?;
        println!("windows: muted={} volume={:.0}%", vol.GetMute()?.as_bool(), vol.GetMasterVolumeLevelScalar()? * 100.0);
    }
    let end = clock::now_hns() + secs as i64 * HNS_PER_SEC;
    let mut next = clock::now_hns() + HNS_PER_SEC;
    let (mut peak, mut packets, mut silent) = (0f32, 0u32, 0u32);
    let (mut lag_min, mut lag_max) = (i64::MAX, i64::MIN);
    while clock::now_hns() < end {
        unsafe {
            while src.capture.GetNextPacketSize()? > 0 {
                let mut data = std::ptr::null_mut();
                let (mut frames, mut flags, mut qpc) = (0u32, 0u32, 0u64);
                src.capture.GetBuffer(&mut data, &mut frames, &mut flags, None, Some(&mut qpc))?;
                let now = clock::now_hns();
                if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 || data.is_null() {
                    silent += 1;
                } else {
                    let n = frames as usize * CHANNELS as usize;
                    peak = std::slice::from_raw_parts(data as *const f32, n)
                        .iter()
                        .fold(peak, |p, &x| p.max(x.abs()));
                }
                let lag = qpc as i64 + frames as i64 * HNS_PER_SEC / RATE as i64 - now;
                lag_min = lag_min.min(lag);
                lag_max = lag_max.max(lag);
                packets += 1;
                src.capture.ReleaseBuffer(frames)?;
            }
        }
        if clock::now_hns() >= next {
            let lag = |v: i64| if packets > 0 { v / 10_000 } else { 0 };
            println!(
                "packets={packets} silent={silent} peak={:.1} dBFS stamps=[{} .. {}] ms",
                20.0 * peak.max(1e-9).log10(),
                lag(lag_min),
                lag(lag_max),
            );
            (peak, packets, silent) = (0.0, 0, 0);
            (lag_min, lag_max) = (i64::MAX, i64::MIN);
            next += HNS_PER_SEC;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    Ok(())
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
    pub sources: Vec<SourceSpec>,
    pub replay_seconds: u32,
}

pub const MAX_SOURCES: usize = 8;

pub struct TrackShared {
    pub spec: SourceSpec,
    pub name: String,
    pub ring: Mutex<PacketRing>,
    /// Friendly name of the device currently open ("" while unavailable).
    pub device: Mutex<String>,
    /// "ok", "no_signal" (a microphone delivering only digital silence) or "unavailable".
    pub health: Mutex<&'static str>,
    /// Peak sample since the last `take_peak` (f32 bits).
    peak: AtomicU32,
}

impl TrackShared {
    /// Peak level (0..) since the previous call, for the app's level meters.
    pub fn take_peak(&self) -> f32 {
        f32::from_bits(self.peak.swap(0, Ordering::Relaxed))
    }

    fn note_peak(&self, p: f32) {
        // Non-negative floats order like their bit patterns.
        self.peak.fetch_max(p.max(0.0).to_bits(), Ordering::Relaxed);
    }
}

pub struct AudioShared {
    pub mix: Mutex<PacketRing>,
    pub tracks: Vec<TrackShared>,
}

impl AudioShared {
    /// Audio packets covering `[start, end)` (hns), starting at the packet nearest `start`.
    pub fn snapshot(&self, start: i64, end: i64) -> AudioSnapshot {
        let half = AAC_FRAME as i64 * HNS_PER_SEC / RATE as i64 / 2;
        let pick = |r: &Mutex<PacketRing>| r.lock().range(start - half, end);
        AudioSnapshot {
            mix: pick(&self.mix),
            tracks: self.tracks.iter().map(|t| (t.name.clone(), pick(&t.ring))).collect(),
        }
    }

    /// The newest audio time already encoded (hns).
    pub fn encoded_until(&self) -> i64 {
        self.mix.lock().newest_end().unwrap_or(i64::MIN)
    }
}

pub struct AudioSnapshot {
    pub mix: Vec<Packet>,
    /// (track name, packets) per source, in configuration order.
    pub tracks: Vec<(String, Vec<Packet>)>,
}

pub struct AudioEngine {
    pub shared: Arc<AudioShared>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

const AUDIO_MAX_BYTES: usize = 256 << 20;

/// Track names: "Game"/"Mic" for default devices, otherwise the label or device text;
/// duplicates get a number.
fn track_names(sources: &[SourceSpec]) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for s in sources {
        let base = s.label.clone().unwrap_or_else(|| match (&s.device, s.kind) {
            (None, SourceKind::Output) => "Game".into(),
            (None, SourceKind::Input) => "Mic".into(),
            (Some(d), _) => d.clone(),
        });
        let mut name = base.clone();
        let mut n = 2;
        while names.contains(&name) {
            name = format!("{base} {n}");
            n += 1;
        }
        names.push(name);
    }
    names
}

impl AudioEngine {
    pub fn start(cfg: AudioConfig) -> Result<Self> {
        if cfg.sources.is_empty() {
            bail!("no audio sources configured");
        }
        if cfg.sources.len() > MAX_SOURCES {
            bail!("at most {MAX_SOURCES} audio sources are supported");
        }
        // Keep a little more audio than video so alignment never runs short.
        let span = (cfg.replay_seconds as i64 + 3) * HNS_PER_SEC;
        let tracks = cfg
            .sources
            .iter()
            .zip(track_names(&cfg.sources))
            .map(|(spec, name)| TrackShared {
                spec: spec.clone(),
                name,
                ring: Mutex::new(PacketRing::audio(span, AUDIO_MAX_BYTES)),
                device: Mutex::new(String::new()),
                health: Mutex::new("unavailable"),
                peak: AtomicU32::new(0),
            })
            .collect();
        let shared = Arc::new(AudioShared {
            mix: Mutex::new(PacketRing::audio(span, AUDIO_MAX_BYTES)),
            tracks,
        });
        let stop = Arc::new(AtomicBool::new(false));
        let (init_tx, init_rx) = crossbeam_channel::bounded::<Result<()>>(1);
        let thread = {
            let shared = shared.clone();
            let stop = stop.clone();
            std::thread::Builder::new().name("audio".into()).spawn(move || {
                let _com = ComScope::init();
                if let Err(e) = run_audio(&shared, &stop, init_tx) {
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

/// An open microphone that delivered no packet for this long has stalled.
const STALL_HNS: i64 = 15_000_000; // 1.5 s
/// Nothing but exact zeros for this long: muted, or the stream broke without an error.
const ZEROS_HNS: i64 = 30_000_000; // 3 s
/// Longest wait between reopens of a microphone that stays digitally silent.
const ZEROS_RETRY_MAX_HNS: i64 = 60 * HNS_PER_SEC;

struct SourceSlot {
    source: Option<Source>,
    timeline: Timeline,
    retry_at: i64,
    warned: bool,
    /// When the open source last delivered a packet / a non-zero sample.
    last_packet: i64,
    last_signal: i64,
    /// Reopen delay for a digitally silent microphone (doubles while it stays silent).
    zeros_wait: i64,
    zeros_warned: bool,
}

impl SourceSlot {
    fn new(origin: i64) -> Self {
        Self {
            source: None,
            timeline: Timeline::new(origin),
            retry_at: 0,
            warned: false,
            last_packet: 0,
            last_signal: 0,
            zeros_wait: ZEROS_HNS,
            zeros_warned: false,
        }
    }

    fn poll(&mut self, shared: &TrackShared, scratch: &mut Vec<f32>) {
        let now = clock::now_hns();
        if self.source.is_none() && now >= self.retry_at {
            match Source::open(&shared.spec) {
                Ok(s) => {
                    tracing::info!(track = %shared.name, device = %s.name, "audio source opened");
                    *shared.device.lock() = s.name.clone();
                    self.source = Some(s);
                    self.warned = false;
                    self.last_packet = now;
                    self.last_signal = now;
                }
                Err(e) => {
                    if !self.warned {
                        tracing::warn!(track = %shared.name, "audio source unavailable: {e:#}");
                        self.warned = true;
                    }
                    shared.device.lock().clear();
                    self.retry_at = now + 2 * HNS_PER_SEC;
                }
            }
        }
        if let Some(s) = &mut self.source {
            match s.drain(&mut self.timeline, scratch) {
                Ok(got) => {
                    if got.packets > 0 {
                        self.last_packet = now;
                        shared.note_peak(got.peak);
                    }
                    if got.peak > 0.0 {
                        self.last_signal = now;
                        self.zeros_wait = ZEROS_HNS;
                        if self.zeros_warned {
                            tracing::info!(track = %shared.name, "audio source has signal again");
                            self.zeros_warned = false;
                        }
                    }
                }
                Err(e) => {
                    // Typically AUDCLNT_E_DEVICE_INVALIDATED: headset unplugged / default changed.
                    tracing::warn!(track = %shared.name, "audio source lost: {e:#}; reopening");
                    self.source = None;
                    self.retry_at = now + HNS_PER_SEC / 2;
                }
            }
        }
        // Loopback delivers nothing while nothing plays, but a microphone always streams. One
        // that stops, or sends only zeros, broke without telling us: reopen it rather than
        // recording silence until the engine restarts.
        if shared.spec.kind == SourceKind::Input && self.source.is_some() {
            if now - self.last_packet > STALL_HNS {
                tracing::warn!(track = %shared.name, "audio source stopped delivering audio; reopening");
                self.source = None;
                self.retry_at = now;
            } else if now - self.last_signal > self.zeros_wait {
                if !self.zeros_warned {
                    tracing::warn!(
                        track = %shared.name,
                        "audio source records only digital silence (muted in Windows, or the device stopped); reopening"
                    );
                    self.zeros_warned = true;
                }
                self.source = None;
                self.retry_at = now;
                // Back off while it stays silent (a muted mic, a quiet virtual cable).
                self.zeros_wait = (self.zeros_wait * 2).min(ZEROS_RETRY_MAX_HNS);
            }
        }
        *shared.health.lock() = match &self.source {
            None if self.retry_at > now => "unavailable",
            _ if self.zeros_warned => "no_signal",
            _ => "ok",
        };
        // A missing or quiet source still advances, as silence.
        self.timeline.pad_until(now - PAD_LAG_HNS);
    }
}

fn run_audio(shared: &AudioShared, stop: &AtomicBool, init: crossbeam_channel::Sender<Result<()>>) -> Result<()> {
    let origin = clock::now_hns();
    let n = shared.tracks.len();
    let encoders = (|| -> Result<_> {
        let mix = Track::new()?;
        let per: Vec<Track> = (0..n).map(|_| Track::new()).collect::<Result<_>>()?;
        Ok((mix, per))
    })();
    let (mut mix_track, mut tracks) = match encoders {
        Ok(t) => t,
        Err(e) => {
            let _ = init.send(Err(anyhow!("{e:#}")));
            return Err(e);
        }
    };
    let mut slots: Vec<SourceSlot> = (0..n).map(|_| SourceSlot::new(origin)).collect();
    let _ = init.send(Ok(()));

    let block = AAC_FRAME as usize;
    let mut scratch = Vec::new();
    let mut bufs: Vec<Vec<f32>> = vec![Vec::new(); n];
    let mut mix = Vec::new();
    while !stop.load(Ordering::Relaxed) {
        for (slot, t) in slots.iter_mut().zip(&shared.tracks) {
            slot.poll(t, &mut scratch);
        }
        while slots.iter().map(|s| s.timeline.pending_frames()).min().unwrap_or(0) >= block {
            mix.clear();
            mix.resize(block * CHANNELS as usize, 0.0);
            for ((slot, buf), t) in slots.iter_mut().zip(bufs.iter_mut()).zip(&shared.tracks) {
                slot.timeline.take(block, buf);
                for (m, x) in mix.iter_mut().zip(buf.iter()) {
                    *m += x * t.spec.gain;
                }
            }
            for m in mix.iter_mut() {
                *m = soft_clip(*m);
            }
            mix_track.encode(origin, &mix, &shared.mix)?;
            for ((track, buf), t) in tracks.iter_mut().zip(&bufs).zip(&shared.tracks) {
                track.encode(origin, buf, &t.ring)?;
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    Ok(())
}

/// Transparent below ~-1 dBFS, then a smooth knee instead of hard clipping.
pub fn soft_clip(x: f32) -> f32 {
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

    // Packet of 480 frames (10 ms) that arrived at `now` and was truly captured `delay`
    // before it could have been (i.e. at now - 10 ms - delay), stamped with `bias` error.
    fn packet(fix: &mut StampFix, now: i64, delay: i64, bias: i64) -> i64 {
        let dur = hns(480);
        let truth = now - dur - delay;
        fix.correct(truth + bias, now, dur) - truth
    }

    #[test]
    fn honest_stamps_are_untouched() {
        let mut fix = StampFix::default();
        for i in 0..500 {
            let now = HNS_PER_SEC + i * hns(480);
            assert_eq!(packet(&mut fix, now, 20_000 + (i % 7) * 10_000, 0), 0);
        }
    }

    #[test]
    fn future_stamps_are_pulled_back() {
        // Realtek USB loopback measured ~975 ms in the future.
        let mut fix = StampFix::default();
        let bias = 9_750_000;
        let err = packet(&mut fix, HNS_PER_SEC, 0, bias);
        assert!(err.abs() <= hns(10), "first packet already corrected, err {err}");
        for i in 1..1000 {
            let now = HNS_PER_SEC + i * hns(480);
            let err = packet(&mut fix, now, (i % 5) * 20_000, bias);
            assert!(err <= hns(10) && err >= -hns(10), "packet {i}: err {err}");
        }
    }

    #[test]
    fn correction_relaxes_when_the_device_recovers() {
        let mut fix = StampFix::default();
        packet(&mut fix, HNS_PER_SEC, 0, 20_000_000);
        // Stamps become honest; after a few seconds no correction remains.
        let mut last = 0;
        for i in 1..1000 {
            last = packet(&mut fix, HNS_PER_SEC + i * hns(480), 0, 0);
        }
        assert_eq!(last, 0);
    }

    #[test]
    fn past_stamps_are_pulled_forward() {
        // A device stamping 800 ms in the past.
        let mut fix = StampFix::default();
        for i in 0..1000 {
            let now = HNS_PER_SEC + i * hns(480);
            let err = packet(&mut fix, now, (i % 5) * 20_000, -8_000_000);
            assert!(err.abs() <= hns(10), "packet {i}: err {err}");
        }
    }

    #[test]
    fn slightly_late_stamps_are_trusted() {
        // 60 ms of pickup delay is plausible on a busy system: leave it alone.
        let mut fix = StampFix::default();
        for i in 0..1000 {
            let now = HNS_PER_SEC + i * hns(480);
            assert_eq!(packet(&mut fix, now, 600_000, 0), 0);
        }
    }

    #[test]
    fn late_stamps_still_reach_the_timeline() {
        // Without the fix every packet stamped 1 s late lands behind the padded timeline, is
        // dropped as overlap, and the track records pure silence.
        let mut fix = StampFix::default();
        let mut t = Timeline::new(0);
        let mut kept = 0;
        for i in 0..500 {
            let now = 10 * HNS_PER_SEC + i * hns(480);
            t.pad_until(now - PAD_LAG_HNS);
            let before = t.pending.iter().filter(|&&x| x != 0.0).count();
            let dur = hns(480);
            t.push(fix.correct(now - dur - HNS_PER_SEC, now, dur), &frames(480));
            kept += t.pending.iter().filter(|&&x| x != 0.0).count() - before;
            let n = t.pending_frames();
            let mut out = Vec::new();
            t.take(n, &mut out);
        }
        assert!(kept > 2 * 480 * 450, "most audio kept, got {kept} samples");
    }

    #[test]
    fn padding_reaches_target() {
        let mut t = Timeline::new(1_000);
        t.pad_until(1_000 + HNS_PER_SEC);
        assert_eq!(t.written(), 48_000);
    }

    #[test]
    fn track_names_are_unique_and_friendly() {
        let specs = vec![
            SourceSpec::default_output(),
            SourceSpec::default_input(),
            SourceSpec::parse(SourceKind::Input, "Shure").unwrap(),
            SourceSpec::parse(SourceKind::Input, "Shure@0.5").unwrap(),
            SourceSpec {
                label: Some("Friend".into()),
                ..SourceSpec::default_input()
            },
        ];
        assert_eq!(track_names(&specs), vec!["Game", "Mic", "Shure", "Shure 2", "Friend"]);
        assert_eq!(specs[3].gain, 0.5);
        assert!(
            SourceSpec::parse(SourceKind::Output, "default")
                .unwrap()
                .device
                .is_none()
        );
    }

    #[test]
    fn soft_clip_is_bounded_and_transparent() {
        assert_eq!(soft_clip(0.5), 0.5);
        assert!(soft_clip(5.0) <= 1.0);
        assert!(soft_clip(-5.0) >= -1.0);
    }
}
