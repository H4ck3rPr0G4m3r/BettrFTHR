//! Editor back end, run as separate engine invocations so capture is never disturbed:
//! - `probe`: duration, keyframes and per-source waveforms for the editor UI;
//! - `export`: trim (lossless video, cut on keyframes), per-source volume and muted
//!   sections, a fresh Mix, and the per-source tracks kept (with the edits applied).
//!
//! Audio is processed in one-second chunks and video is streamed from disk, so long
//! recordings export without holding them in memory.

use crate::aacdec::AacDecoder;
use crate::audio::{self, AacEncoder};
use crate::clock::HNS_PER_SEC;
use crate::demux::{Kind, Mp4, Sample};
use crate::mp4::{self, AAC_FRAME};
use crate::ring::Packet;
use crate::transcode;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;

const CH: usize = audio::CHANNELS as usize;
const RATE: u64 = audio::RATE as u64;
/// Packets decoded before the wanted range, so AAC's overlap-add has its history.
const PREROLL: usize = 2;
/// Fade length at the edges of muted sections (no clicks).
const FADE: f64 = 0.012;

/// The editable sources: the per-source tracks, or the only audio track if there are none.
/// Returns (mix track index, source track indexes).
fn layout(m: &Mp4) -> (Option<usize>, Vec<usize>) {
    let audio: Vec<usize> = (0..m.tracks.len()).filter(|&i| m.tracks[i].kind == Kind::Audio).collect();
    match audio.len() {
        0 => (None, vec![]),
        1 => (None, audio),
        // Clips: the first, enabled track is the Mix; the disabled ones are the sources.
        _ => {
            let mix = audio.iter().copied().find(|&i| m.tracks[i].enabled).unwrap_or(audio[0]);
            (Some(mix), audio.into_iter().filter(|&i| i != mix).collect())
        }
    }
}

/// Sequential decoder over one audio track, positioned at a frame and read in chunks.
struct Stream {
    dec: AacDecoder,
    samples: Vec<Sample>,
    next: usize,
    /// Decoded, not yet consumed audio (interleaved stereo) and the frame index of its start.
    buf: Vec<f32>,
    buf_frame: u64,
    want: u64,
    flushed: bool,
}

impl Stream {
    fn open(m: &Mp4, track: usize, from_frame: u64) -> Result<Self> {
        let t = &m.tracks[track];
        let dec = AacDecoder::new(&t.asc, t.sample_rate.max(1), t.channels.max(1))?;
        let first = ((from_frame / AAC_FRAME as u64) as usize).saturating_sub(PREROLL);
        Ok(Self {
            dec,
            samples: t.samples.clone(),
            next: first,
            buf: Vec::new(),
            buf_frame: first as u64 * AAC_FRAME as u64,
            want: from_frame,
            flushed: false,
        })
    }

    /// The next `n` frames (stereo, zero-padded past the end of the track).
    fn read(&mut self, m: &mut Mp4, n: usize) -> Result<Vec<f32>> {
        let need_end = self.want + n as u64;
        while self.buf_frame + (self.buf.len() / CH) as u64 <= need_end && self.next < self.samples.len() {
            let s = self.samples[self.next];
            self.next += 1;
            let au = m.read(&s)?;
            let mut out = Vec::new();
            self.dec.decode(&au, &mut out)?;
            self.push(out);
        }
        if self.next >= self.samples.len() && !self.flushed {
            self.flushed = true;
            let mut out = Vec::new();
            self.dec.finish(&mut out)?;
            self.push(out);
        }
        // Drop what lies before the wanted position (pre-roll, or a previous read).
        let skip = self.want.saturating_sub(self.buf_frame) as usize;
        let skip = skip.min(self.buf.len() / CH);
        self.buf.drain(..skip * CH);
        self.buf_frame += skip as u64;
        let have = (self.buf.len() / CH).min(n);
        let mut out: Vec<f32> = self.buf.drain(..have * CH).collect();
        out.resize(n * CH, 0.0);
        self.buf_frame += have as u64;
        self.want += n as u64;
        Ok(out)
    }

    fn push(&mut self, pcm: Vec<f32>) {
        match self.dec.channels() {
            1 => self.buf.extend(pcm.iter().flat_map(|&s| [s, s])),
            2 => self.buf.extend_from_slice(&pcm),
            c => {
                for f in pcm.chunks_exact(c) {
                    self.buf.extend_from_slice(&f[..2]);
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------------------
// Probe

#[derive(Serialize)]
pub struct ProbeTrack {
    pub name: String,
    /// Peak level per bucket, 0-255 (square-root scaled so quiet speech is visible).
    pub peaks: Vec<u8>,
}

#[derive(Serialize)]
pub struct Probe {
    pub duration: f64,
    pub width: u32,
    pub height: u32,
    pub fps: f64,
    /// Keyframe times (s): where a lossless cut can start.
    pub keyframes: Vec<f64>,
    pub peaks_per_second: f64,
    pub sources: Vec<ProbeTrack>,
    pub has_mix: bool,
}

pub fn probe(path: &Path) -> Result<Probe> {
    let mut m = Mp4::open(path)?;
    let v = m.video().context("no video track")?;
    let ts = v.timescale as f64;
    let duration = v.duration_secs();
    let keyframes = v.samples.iter().filter(|s| s.key).map(|s| s.pts as f64 / ts).collect();
    let fps = if duration > 0.0 { v.samples.len() as f64 / duration } else { 0.0 };
    let (width, height) = (v.width, v.height);
    let (mix, sources) = layout(&m);

    let pps = (4000.0 / duration.max(1.0)).clamp(4.0, 50.0);
    let frames_per_bucket = (RATE as f64 / pps).round().max(1.0) as usize;
    let buckets = (duration * pps).ceil() as usize;
    let mut out = Vec::new();
    for &ti in &sources {
        let name = m.tracks[ti].name.clone();
        let mut s = Stream::open(&m, ti, 0)?;
        let mut peaks = Vec::with_capacity(buckets);
        for _ in 0..buckets {
            let pcm = s.read(&mut m, frames_per_bucket)?;
            let peak = pcm.iter().fold(0f32, |a, &x| a.max(x.abs())).min(1.0);
            peaks.push((peak.sqrt() * 255.0).round() as u8);
        }
        out.push(ProbeTrack { name, peaks });
    }
    Ok(Probe {
        duration,
        width,
        height,
        fps,
        keyframes,
        peaks_per_second: RATE as f64 / frames_per_bucket as f64,
        sources: out,
        has_mix: mix.is_some(),
    })
}

// ---------------------------------------------------------------------------------------
// Export

#[derive(Deserialize, Default, Clone)]
pub struct SourceEdit {
    #[serde(default = "one")]
    pub gain: f32,
    /// Muted sections in clip seconds (of the original file).
    #[serde(default)]
    pub mutes: Vec<(f64, f64)>,
}

fn one() -> f32 {
    1.0
}

#[derive(Deserialize)]
pub struct ExportJob {
    pub input: PathBuf,
    pub output: PathBuf,
    pub start: f64,
    pub end: f64,
    /// One entry per probe source, in the same order (missing = unchanged).
    #[serde(default)]
    pub sources: Vec<SourceEdit>,
    /// Keep the per-source tracks (for editing again). Discord copies leave them out.
    #[serde(default = "yes")]
    pub keep_sources: bool,
    /// Upload limit: if the lossless cut is bigger, re-encode to fit (frame-accurate).
    #[serde(default)]
    pub fit_bytes: Option<u64>,
}

fn yes() -> bool {
    true
}

#[derive(Serialize)]
pub struct ExportResult {
    pub path: PathBuf,
    pub start: f64,
    pub seconds: f64,
    pub bytes: u64,
    /// Set when the clip was re-encoded to fit the limit.
    pub reencoded: Option<transcode::Plan>,
}

/// Volume at clip time `t` for one source: gain, faded to zero inside muted sections.
fn envelope(edit: &SourceEdit, t: f64) -> f32 {
    let mut f = 1.0f64;
    for &(a, b) in &edit.mutes {
        let (a, b) = (a.min(b), a.max(b));
        let g = if t < a {
            ((a - t) / FADE).min(1.0)
        } else if t > b {
            ((t - b) / FADE).min(1.0)
        } else {
            0.0
        };
        f = f.min(g);
    }
    edit.gain * f as f32
}

struct Encoded {
    enc: AacEncoder,
    frames: Vec<Vec<u8>>,
}

impl Encoded {
    fn new() -> Result<Self> {
        Ok(Self {
            enc: AacEncoder::new()?,
            frames: Vec::new(),
        })
    }
    fn push(&mut self, pcm: &[f32]) -> Result<()> {
        self.frames.extend(self.enc.encode(pcm)?);
        Ok(())
    }
    /// Flushes with silence and keeps exactly the frames covering `total` samples.
    fn finish(mut self, total: u64) -> Result<Vec<Packet>> {
        self.push(&vec![0.0; 3 * AAC_FRAME as usize * CH])?;
        let count = total.div_ceil(AAC_FRAME as u64) as usize;
        let hns = |f: u64| (f as i128 * HNS_PER_SEC as i128 / RATE as i128) as i64;
        Ok(self
            .frames
            .into_iter()
            .take(count)
            .enumerate()
            .map(|(k, data)| Packet {
                pts: hns(k as u64 * AAC_FRAME as u64),
                duration: hns(AAC_FRAME as u64),
                key: true,
                data: Arc::from(data),
            })
            .collect())
    }
}

pub fn export(job: &ExportJob, progress: &mut dyn FnMut(&str, f64)) -> Result<ExportResult> {
    if job.end <= job.start {
        bail!("the cut is empty");
    }
    let mut m = Mp4::open(&job.input)?;
    let vi = m.tracks.iter().position(|t| t.kind == Kind::Video).context("no video track")?;
    let v = &m.tracks[vi];
    let ts = v.timescale as u64;
    let secs = |pts: u64| pts as f64 / ts as f64;
    let src_duration = v.duration_secs();
    let src_fps = (v.samples.len() as f64 / src_duration.max(0.001)).round().max(1.0) as u32;

    // Lossless selection: from the keyframe at or before the start to the end.
    let start_idx = v
        .samples
        .iter()
        .rposition(|s| s.key && secs(s.pts) <= job.start + 1e-6)
        .unwrap_or(0);
    let end_idx = v
        .samples
        .iter()
        .rposition(|s| secs(s.pts) < job.end - 1e-6)
        .unwrap_or(v.samples.len().saturating_sub(1))
        .max(start_idx);
    let sel: Vec<Sample> = v.samples[start_idx..=end_idx].to_vec();
    let base = sel[0].pts;
    let last = sel[sel.len() - 1];
    let (width, height, sps, pps) = (v.width, v.height, v.sps.clone(), v.pps.clone());

    let (_, sources) = layout(&m);
    let keep = job.keep_sources && sources.len() > 1;
    let audio_tracks = if sources.is_empty() { 0 } else { 1 + if keep { sources.len() } else { 0 } };

    // Too big for the upload limit as a lossless cut? Then re-encode, frame-accurately.
    let lossless_bytes: u64 = sel.iter().map(|s| s.size as u64).sum::<u64>()
        + (secs(last.pts + last.duration as u64) - secs(base)) as u64 * 24_000 * audio_tracks as u64;
    let reencode = job.fit_bytes.is_some_and(|limit| lossless_bytes > limit);
    let audio_kbps = (audio::AAC_BYTES_PER_SEC * 8 / 1000) * audio_tracks as u32;
    let mut plan = match (reencode, job.fit_bytes) {
        (true, Some(limit)) => {
            let len = job.end.min(src_duration) - job.start.max(0.0);
            Some(transcode::plan(limit, len, audio_kbps, width, height, src_fps)?)
        }
        _ => None,
    };
    let (t0, t1) = if let Some(p) = &plan {
        // Whole output frames, so audio and video end together.
        let t0 = job.start.max(0.0);
        let frames = ((job.end.min(src_duration) - t0) * p.fps as f64).round().max(1.0);
        (t0, t0 + frames / p.fps as f64)
    } else {
        (secs(base), secs(last.pts + last.duration as u64))
    };
    let audio_share = if reencode { 0.15 } else { 0.5 };

    // Audio: decode every source, apply volume and mutes, mix, re-encode.
    let names: Vec<String> = sources.iter().map(|&i| m.tracks[i].name.clone()).collect();
    let f0 = (t0 * RATE as f64).round() as u64;
    let total = ((t1 - t0) * RATE as f64).round() as u64;
    let edits: Vec<SourceEdit> = (0..sources.len())
        .map(|i| job.sources.get(i).cloned().unwrap_or(SourceEdit { gain: 1.0, mutes: vec![] }))
        .collect();
    let mut streams = sources.iter().map(|&ti| Stream::open(&m, ti, f0)).collect::<Result<Vec<_>>>()?;
    let mut mix_enc = if sources.is_empty() { None } else { Some(Encoded::new()?) };
    let mut src_enc = if keep {
        (0..sources.len()).map(|_| Encoded::new()).collect::<Result<Vec<_>>>()?
    } else {
        Vec::new()
    };
    let chunk = RATE as usize;
    let mut done = 0u64;
    while done < total {
        let n = chunk.min((total - done) as usize);
        let mut mix = vec![0f32; n * CH];
        for (si, stream) in streams.iter_mut().enumerate() {
            let mut pcm = stream.read(&mut m, n)?;
            for f in 0..n {
                let t = t0 + (done + f as u64) as f64 / RATE as f64;
                let g = envelope(&edits[si], t);
                for c in 0..CH {
                    let x = pcm[f * CH + c] * g;
                    pcm[f * CH + c] = x;
                    mix[f * CH + c] += x;
                }
            }
            if keep {
                src_enc[si].push(&pcm)?;
            }
        }
        if let Some(e) = mix_enc.as_mut() {
            for x in mix.iter_mut() {
                *x = audio::soft_clip(*x);
            }
            e.push(&mix)?;
        }
        done += n as u64;
        progress("audio", audio_share * done as f64 / total.max(1) as f64);
    }
    let mix_packets = match mix_enc {
        Some(e) => e.finish(total)?,
        None => Vec::new(),
    };
    let src_packets = src_enc.into_iter().map(|e| e.finish(total)).collect::<Result<Vec<_>>>()?;
    let mut tracks = Vec::new();
    if !mix_packets.is_empty() {
        tracks.push(mp4::AudioTrack {
            name: if sources.len() == 1 { &names[0] } else { "Mix" },
            sample_rate: audio::RATE,
            channels: audio::CHANNELS,
            enabled: true,
            packets: &mix_packets,
        });
    }
    for (name, packets) in names.iter().zip(&src_packets) {
        tracks.push(mp4::AudioTrack {
            name,
            sample_rate: audio::RATE,
            channels: audio::CHANNELS,
            enabled: false,
            packets,
        });
    }
    if let Some(dir) = job.output.parent() {
        std::fs::create_dir_all(dir)?;
    }

    let (stats, plan) = if let (Some(mut plan), Some(limit)) = (plan.take(), job.fit_bytes) {
        let mut attempt = 0;
        loop {
            let enc = transcode::video(&job.input, t0, t1, &plan, &mut |p| progress("video", 0.15 + 0.8 * p))?;
            let stats = mp4::write_mp4(
                &job.output,
                &mp4::VideoTrack {
                    width: plan.width,
                    height: plan.height,
                    fps: plan.fps,
                    sps: &enc.sps,
                    pps: &enc.pps,
                    packets: &enc.packets,
                },
                &tracks,
            )?;
            attempt += 1;
            if stats.bytes <= limit || attempt >= 3 {
                if stats.bytes > limit {
                    bail!("could not get the clip under {} MB; trim it shorter", limit / 1024 / 1024);
                }
                break (stats, Some(plan));
            }
            // Rate control overshot: aim lower by the overshoot plus a margin, and retry.
            let scale = limit as f64 / stats.bytes as f64 * 0.95;
            tracing::warn!(bytes = stats.bytes, limit, scale, "export over the limit, retrying");
            plan.video_kbps = ((plan.video_kbps as f64) * scale) as u32;
        }
    } else {
        let to_hns = |d: u64| (d as i128 * HNS_PER_SEC as i128 / ts as i128) as i64;
        let vpackets: Vec<Packet> = sel
            .iter()
            .map(|s| Packet {
                pts: to_hns(s.pts - base),
                duration: to_hns(s.duration as u64),
                key: s.key,
                data: Arc::from(Vec::new()),
            })
            .collect();
        let sizes: Vec<u32> = sel.iter().map(|s| s.size).collect();
        let count = sel.len();
        let mut fetch = |i: usize| -> Result<Vec<u8>> {
            progress("video", 0.5 + 0.5 * i as f64 / count as f64);
            m.read(&sel[i])
        };
        let stats = mp4::write_mp4_with(
            &job.output,
            &mp4::VideoTrack {
                width,
                height,
                fps: src_fps,
                sps: &sps,
                pps: &pps,
                packets: &vpackets,
            },
            Some(mp4::VideoSource {
                sizes: &sizes,
                fetch: &mut fetch,
            }),
            &tracks,
        )?;
        (stats, None)
    };
    Ok(ExportResult {
        path: job.output.clone(),
        start: t0,
        seconds: stats.duration_hns as f64 / HNS_PER_SEC as f64,
        bytes: stats.bytes,
        reencoded: plan,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Encoder + decoder round trip must not shift audio: clicks come back where they were
    /// (the editor remixes decoded audio against video, so any offset would be A/V desync).
    #[test]
    fn aac_round_trip_keeps_timing() {
        let _com = crate::encoder::ComScope::init();
        unsafe {
            windows::Win32::Media::MediaFoundation::MFStartup(
                windows::Win32::Media::MediaFoundation::MF_VERSION,
                windows::Win32::Media::MediaFoundation::MFSTARTUP_FULL,
            )
            .unwrap()
        };
        let frames = RATE as usize;
        let clicks = [12_000usize, 30_011];
        let mut pcm = vec![0f32; frames * CH];
        for &c in &clicks {
            for k in 0..8 {
                pcm[(c + k) * CH] = 0.9;
                pcm[(c + k) * CH + 1] = 0.9;
            }
        }
        let mut enc = Encoded::new().unwrap();
        enc.push(&pcm).unwrap();
        let packets = enc.finish(frames as u64).unwrap();
        let asc = mp4::aac_lc_asc(audio::RATE, audio::CHANNELS);
        let mut dec = AacDecoder::new(&asc, audio::RATE, 2).unwrap();
        let mut out = Vec::new();
        for p in &packets {
            dec.decode(&p.data, &mut out).unwrap();
        }
        dec.finish(&mut out).unwrap();
        let mono: Vec<f32> = out.chunks_exact(2).map(|f| f[0].abs()).collect();
        for &c in &clicks {
            let lo = c.saturating_sub(3000);
            let (pos, _) = mono[lo..c + 3000]
                .iter()
                .enumerate()
                .fold((0, 0f32), |b, (i, &x)| if x > b.1 { (i, x) } else { b });
            let found = lo + pos;
            let off = found as i64 - c as i64;
            assert!(off.abs() <= 12, "click at {c} decoded at {found} (offset {off} samples)");
        }
    }

    #[test]
    fn mute_envelope_fades_and_silences() {
        let e = SourceEdit {
            gain: 0.5,
            mutes: vec![(2.0, 3.0)],
        };
        assert_eq!(envelope(&e, 1.0), 0.5);
        assert_eq!(envelope(&e, 2.5), 0.0);
        assert!(envelope(&e, 2.0 - FADE / 2.0) > 0.2 && envelope(&e, 2.0 - FADE / 2.0) < 0.3);
        assert_eq!(envelope(&e, 3.5), 0.5);
    }
}
