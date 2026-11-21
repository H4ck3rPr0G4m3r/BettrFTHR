//! Confirmation sounds, synthesized in code (no asset files) and played from memory.
//!
//! Several "save" styles to pick from in Settings; recording start/stop use a soft bell
//! that rises or falls. Everything is short, peak-normalized, and scaled by the volume.

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::f32::consts::TAU;
use std::sync::Arc;
use windows::Win32::Media::Audio::{PlaySoundW, SND_ASYNC, SND_MEMORY, SND_NODEFAULT};
use windows::core::PCWSTR;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cue {
    Saved,
    RecordStart,
    RecordStop,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Style {
    /// Two bright bell notes with a short room tail.
    Glass,
    /// A round, bubbly pop.
    Pop,
    /// A camera shutter: "clip taken".
    Shutter,
    /// A quick three-note rise (the default).
    #[default]
    Rise,
    /// A barely-there tick, for headphones.
    Tick,
}


const RATE: u32 = 48_000;

fn secs(n: usize) -> f32 {
    n as f32 / RATE as f32
}

fn buffer(ms: u32) -> Vec<f32> {
    vec![0.0; (RATE * ms / 1000) as usize]
}

/// Bell-like tone: a few inharmonic partials, each decaying at its own rate.
fn bell(out: &mut [f32], at_ms: u32, freq: f32, decay: f32, gain: f32) {
    let start = (RATE * at_ms / 1000) as usize;
    let partials = [(1.0, 1.0, 1.0), (2.0, 0.45, 0.6), (2.76, 0.22, 0.45), (5.4, 0.08, 0.25)];
    for (i, o) in out.iter_mut().enumerate().skip(start) {
        let t = secs(i - start);
        let attack = (t / 0.004).min(1.0);
        let mut s = 0.0;
        for &(ratio, amp, d) in &partials {
            s += (TAU * freq * ratio * t).sin() * amp * (-t / (decay * d)).exp();
        }
        *o += s * attack * gain;
    }
}

/// Plucked triangle-ish tone (soft, rounded).
fn pluck(out: &mut [f32], at_ms: u32, freq: f32, decay: f32, gain: f32) {
    let start = (RATE * at_ms / 1000) as usize;
    for (i, o) in out.iter_mut().enumerate().skip(start) {
        let t = secs(i - start);
        let ph = (freq * t).fract();
        let tri = 4.0 * (ph - 0.5).abs() - 1.0;
        let sine = (TAU * freq * t).sin();
        let attack = (t / 0.003).min(1.0);
        *o += (0.55 * sine + 0.45 * tri) * attack * (-t / decay).exp() * gain;
    }
}

/// Sine with an exponential pitch drop: the "pop".
fn pop(out: &mut [f32], at_ms: u32, from: f32, to: f32, sweep: f32, decay: f32, gain: f32) {
    let start = (RATE * at_ms / 1000) as usize;
    let mut phase = 0.0f32;
    for (i, o) in out.iter_mut().enumerate().skip(start) {
        let t = secs(i - start);
        let f = to + (from - to) * (-t / sweep).exp();
        phase += TAU * f / RATE as f32;
        let attack = (t / 0.0015).min(1.0);
        *o += phase.sin() * attack * (-t / decay).exp() * gain;
    }
}

/// Band-limited noise burst (shutter blades).
fn click(out: &mut [f32], at_ms: u32, ms: f32, bright: f32, gain: f32, seed: &mut u32) {
    let start = (RATE * at_ms / 1000) as usize;
    let len = (RATE as f32 * ms / 1000.0) as usize;
    let (mut lp, mut hp_prev, mut hp) = (0.0f32, 0.0f32, 0.0f32);
    for i in 0..len.min(out.len().saturating_sub(start)) {
        *seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        let noise = (*seed >> 8) as f32 / (1u32 << 24) as f32 * 2.0 - 1.0;
        lp += bright * (noise - lp); // low-pass
        hp = 0.92 * (hp + lp - hp_prev); // high-pass
        hp_prev = lp;
        let t = secs(i);
        let env = (t / 0.0008).min(1.0) * (-t / (ms / 1000.0 / 3.0)).exp();
        out[start + i] += hp * env * gain;
    }
}

/// Small feedback echo: a hint of room so tones don't sound "dry beep".
fn room(out: &mut [f32], delay_ms: u32, feedback: f32, mix: f32) {
    let d = (RATE * delay_ms / 1000) as usize;
    let dry = out.to_vec();
    let mut wet = vec![0f32; out.len()];
    for i in d..out.len() {
        wet[i] = dry[i - d] + wet[i - d] * feedback;
    }
    for (o, w) in out.iter_mut().zip(wet) {
        *o += w * mix;
    }
}

fn render(cue: Cue, style: Style) -> Vec<f32> {
    let mut seed = 0x1234_5678u32;
    match (cue, style) {
        (Cue::Saved, Style::Glass) => {
            let mut b = buffer(700);
            bell(&mut b, 0, 1568.0, 0.20, 0.8); // G6
            bell(&mut b, 75, 2093.0, 0.30, 0.9); // C7
            room(&mut b, 47, 0.35, 0.22);
            b
        }
        (Cue::Saved, Style::Pop) => {
            let mut b = buffer(220);
            pop(&mut b, 0, 1500.0, 420.0, 0.018, 0.045, 1.0);
            pop(&mut b, 0, 3000.0, 840.0, 0.012, 0.012, 0.25);
            b
        }
        (Cue::Saved, Style::Shutter) => {
            let mut b = buffer(260);
            click(&mut b, 0, 22.0, 0.55, 1.0, &mut seed);
            pop(&mut b, 0, 220.0, 90.0, 0.02, 0.03, 0.35);
            click(&mut b, 85, 16.0, 0.45, 0.65, &mut seed);
            b
        }
        (Cue::Saved, Style::Rise) => {
            let mut b = buffer(480);
            for (k, f) in [1047.0, 1319.0, 1568.0].into_iter().enumerate() {
                pluck(&mut b, k as u32 * 60, f, 0.09 + k as f32 * 0.03, 0.8);
            }
            room(&mut b, 41, 0.3, 0.18);
            b
        }
        (Cue::Saved, Style::Tick) => {
            let mut b = buffer(80);
            pluck(&mut b, 0, 2400.0, 0.012, 1.0);
            b
        }
        (Cue::RecordStart, _) => {
            let mut b = buffer(520);
            bell(&mut b, 0, 1175.0, 0.16, 0.8); // D6
            bell(&mut b, 90, 1760.0, 0.22, 0.85); // A6
            room(&mut b, 47, 0.3, 0.18);
            b
        }
        (Cue::RecordStop, _) => {
            let mut b = buffer(520);
            bell(&mut b, 0, 1760.0, 0.16, 0.8);
            bell(&mut b, 90, 1175.0, 0.24, 0.85);
            room(&mut b, 47, 0.3, 0.18);
            b
        }
    }
}

/// 16-bit mono WAV, peak-normalized to -6 dBFS times `volume` (0..1).
pub fn wav(cue: Cue, style: Style, volume: f32) -> Vec<u8> {
    let mut pcm = render(cue, style);
    // Short fade-out so nothing ends in a click.
    let fade = (RATE as usize / 100).min(pcm.len());
    let n = pcm.len();
    for (k, s) in pcm[n - fade..].iter_mut().enumerate() {
        *s *= 1.0 - k as f32 / fade as f32;
    }
    let peak = pcm.iter().fold(0f32, |a, &x| a.max(x.abs())).max(1e-6);
    let scale = 0.5 / peak * volume.clamp(0.0, 1.0);
    let data: Vec<u8> = pcm
        .iter()
        .flat_map(|&s| (((s * scale).clamp(-1.0, 1.0) * i16::MAX as f32).round() as i16).to_le_bytes())
        .collect();
    let mut out = Vec::with_capacity(44 + data.len());
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&1u16.to_le_bytes()); // mono
    out.extend_from_slice(&RATE.to_le_bytes());
    out.extend_from_slice(&(RATE * 2).to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out.extend_from_slice(&data);
    out
}

/// The buffer being played. PlaySound with SND_MEMORY reads it while playing, so it is
/// only replaced after the next PlaySound call has stopped the previous sound.
static PLAYING: Mutex<Option<Arc<Vec<u8>>>> = Mutex::new(None);

pub fn play(cue: Cue, style: Style, volume: f32) {
    if volume <= 0.0 {
        return;
    }
    let buf = Arc::new(wav(cue, style, volume));
    let mut slot = PLAYING.lock();
    unsafe {
        let _ = PlaySoundW(PCWSTR(buf.as_ptr() as *const u16), None, SND_MEMORY | SND_ASYNC | SND_NODEFAULT);
    }
    *slot = Some(buf); // the old buffer is dropped only now, after playback switched over
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [Style; 5] = [Style::Glass, Style::Pop, Style::Shutter, Style::Rise, Style::Tick];

    #[test]
    fn every_sound_is_short_valid_and_normalized() {
        for style in ALL {
            for cue in [Cue::Saved, Cue::RecordStart, Cue::RecordStop] {
                let w = wav(cue, style, 1.0);
                assert_eq!(&w[..4], b"RIFF");
                let samples: Vec<i16> =
                    w[44..].chunks_exact(2).map(|c| i16::from_le_bytes([c[0], c[1]])).collect();
                let secs = samples.len() as f32 / RATE as f32;
                assert!(secs < 0.8, "{style:?} {cue:?} is {secs} s");
                let peak = samples.iter().map(|s| s.unsigned_abs()).max().unwrap();
                assert!((16000..=16500).contains(&peak), "{style:?} {cue:?} peak {peak}");
                assert!(samples.last().unwrap().unsigned_abs() < 200, "ends with a click");
            }
        }
    }

    /// `cargo test -p bettr-app dump_sounds -- --ignored` writes the sounds to SOUND_DIR.
    #[test]
    #[ignore]
    fn dump_sounds() {
        let dir = std::path::PathBuf::from(std::env::var("SOUND_DIR").unwrap_or_else(|_| ".".into()));
        for style in ALL {
            std::fs::write(dir.join(format!("saved-{style:?}.wav").to_lowercase()), wav(Cue::Saved, style, 1.0)).unwrap();
        }
        std::fs::write(dir.join("record-start.wav"), wav(Cue::RecordStart, Style::Glass, 1.0)).unwrap();
        std::fs::write(dir.join("record-stop.wav"), wav(Cue::RecordStop, Style::Glass, 1.0)).unwrap();
    }
}
