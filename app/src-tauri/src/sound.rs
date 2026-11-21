//! Short confirmation sounds, synthesized once and played from memory (no asset files).

use std::sync::OnceLock;
use windows::Win32::Media::Audio::{PlaySoundW, SND_ASYNC, SND_MEMORY, SND_NODEFAULT};
use windows::core::PCWSTR;

#[derive(Clone, Copy)]
pub enum Cue {
    Saved,
    RecordStart,
    RecordStop,
}

const RATE: u32 = 44_100;

/// A sequence of (frequency Hz, duration ms) notes as a 16-bit mono WAV file.
fn wav(notes: &[(f32, u32)], volume: f32) -> Vec<u8> {
    let mut pcm: Vec<i16> = Vec::new();
    for &(freq, ms) in notes {
        let n = (RATE * ms / 1000) as usize;
        for i in 0..n {
            let t = i as f32 / RATE as f32;
            // Fast attack, exponential decay: a soft "blip", not a beep.
            let env = (i as f32 / 220.0).min(1.0) * (-(i as f32) / (n as f32 * 0.45)).exp();
            let s = (t * freq * std::f32::consts::TAU).sin() * 0.8 + (t * freq * 2.0 * std::f32::consts::TAU).sin() * 0.2;
            pcm.push((s * env * volume * i16::MAX as f32) as i16);
        }
    }
    let data_len = (pcm.len() * 2) as u32;
    let mut out = Vec::with_capacity(44 + data_len as usize);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&1u16.to_le_bytes()); // mono
    out.extend_from_slice(&RATE.to_le_bytes());
    out.extend_from_slice(&(RATE * 2).to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    for s in pcm {
        out.extend_from_slice(&s.to_le_bytes());
    }
    out
}

fn sounds() -> &'static [Vec<u8>; 3] {
    static S: OnceLock<[Vec<u8>; 3]> = OnceLock::new();
    S.get_or_init(|| {
        [
            wav(&[(988.0, 70), (1480.0, 110)], 0.35),
            wav(&[(660.0, 90), (990.0, 120)], 0.3),
            wav(&[(990.0, 90), (660.0, 140)], 0.3),
        ]
    })
}

pub fn play(cue: Cue) {
    let buf = &sounds()[cue as usize];
    // SND_MEMORY: the pointer is the WAV image itself; the static buffer outlives playback.
    unsafe {
        let _ = PlaySoundW(PCWSTR(buf.as_ptr() as *const u16), None, SND_MEMORY | SND_ASYNC | SND_NODEFAULT);
    }
}
