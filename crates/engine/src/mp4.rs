//! A small, deliberate MP4 writer for H.264 + AAC.
//!
//! Rules that keep A/V sync player-independent:
//! - no edit lists: every track starts at t = 0 at the first video keyframe;
//! - no B-frames (PTS == DTS), so no `ctts`;
//! - faststart (`moov` before `mdat`) so browsers/Discord can preview immediately;
//! - written to `<name>.tmp` and atomically renamed, so a crash never leaves a broken clip.

use crate::ring::Packet;
use anyhow::{Context, Result, bail};
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;

pub const VIDEO_TIMESCALE: u32 = 90_000;
const MOVIE_TIMESCALE: u32 = 1000;
pub const AAC_FRAME: u32 = 1024;

pub struct VideoTrack<'a> {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub sps: &'a [u8],
    pub pps: &'a [u8],
    pub packets: &'a [Packet],
}

pub struct AudioTrack<'a> {
    pub name: &'a str,
    pub sample_rate: u32,
    pub channels: u16,
    /// Players play the first enabled audio track; separate stems are disabled.
    pub enabled: bool,
    pub packets: &'a [Packet],
}

#[derive(Debug, Clone, Copy)]
pub struct MuxStats {
    pub duration_hns: i64,
    pub bytes: u64,
    pub video_frames: usize,
}

struct SampleTable {
    sizes: Vec<u32>,
    offsets: Vec<u64>, // relative to mdat payload start until finalized
    durations: Vec<u32>,
    keys: Vec<u32>, // 1-based indices of sync samples (video only)
    total_duration: u64,
}

pub fn write_mp4(path: &Path, video: &VideoTrack, audio: &[AudioTrack]) -> Result<MuxStats> {
    if video.packets.is_empty() {
        bail!("no video packets to write");
    }
    if !video.packets[0].key {
        bail!("clip must start with a keyframe");
    }
    if video.sps.len() < 4 || video.pps.is_empty() {
        bail!("missing H.264 parameter sets");
    }
    let origin = video.packets[0].pts;
    let to_vts = |pts: i64| -> u64 {
        (((pts - origin).max(0) as i128 * VIDEO_TIMESCALE as i128 + 5_000_000) / 10_000_000) as u64
    };

    // --- Sample tables -----------------------------------------------------------------
    let nominal = (VIDEO_TIMESCALE / video.fps.max(1)).max(1);
    let mut vt = SampleTable {
        sizes: vec![],
        offsets: vec![],
        durations: vec![],
        keys: vec![],
        total_duration: 0,
    };
    for (i, p) in video.packets.iter().enumerate() {
        let start = to_vts(p.pts);
        let dur = match video.packets.get(i + 1) {
            Some(next) => (to_vts(next.pts).saturating_sub(start)).max(1) as u32,
            None => nominal,
        };
        vt.sizes.push(p.data.len() as u32);
        vt.durations.push(dur);
        if p.key {
            vt.keys.push(i as u32 + 1);
        }
        vt.total_duration += dur as u64;
    }
    let mut ats: Vec<SampleTable> = audio
        .iter()
        .map(|a| SampleTable {
            sizes: a.packets.iter().map(|p| p.data.len() as u32).collect(),
            offsets: vec![],
            durations: vec![AAC_FRAME; a.packets.len()],
            keys: vec![],
            total_duration: a.packets.len() as u64 * AAC_FRAME as u64,
        })
        .collect();

    // --- Interleave by presentation time -----------------------------------------------
    // order: (track, index); track 0 = video, 1.. = audio
    let mut order: Vec<(usize, usize, i64)> = Vec::new();
    for (i, p) in video.packets.iter().enumerate() {
        order.push((0, i, p.pts));
    }
    for (t, a) in audio.iter().enumerate() {
        for (i, p) in a.packets.iter().enumerate() {
            order.push((t + 1, i, p.pts));
        }
    }
    order.sort_by_key(|&(t, i, pts)| (pts, t, i));
    let mut cursor = 0u64;
    for &(t, i, _) in &order {
        let len = if t == 0 { vt.sizes[i] } else { ats[t - 1].sizes[i] } as u64;
        if t == 0 {
            vt.offsets.push(cursor);
        } else {
            ats[t - 1].offsets.push(cursor);
        }
        let _ = i;
        cursor += len;
    }
    let payload = cursor;

    // --- Header + moov (built twice: size first, then real offsets) --------------------
    let ftyp = build_ftyp();
    let probe = build_moov(video, &vt, audio, &ats, 0);
    let base = ftyp.len() as u64 + probe.len() as u64 + 16;
    let moov = build_moov(video, &vt, audio, &ats, base);
    debug_assert_eq!(moov.len(), probe.len());

    // --- Write ---------------------------------------------------------------------------
    let tmp = path.with_extension("mp4.tmp");
    {
        let f = File::create(&tmp).with_context(|| format!("create {}", tmp.display()))?;
        let mut w = BufWriter::with_capacity(1 << 20, f);
        w.write_all(&ftyp)?;
        w.write_all(&moov)?;
        w.write_all(&1u32.to_be_bytes())?;
        w.write_all(b"mdat")?;
        w.write_all(&(16 + payload).to_be_bytes())?;
        for &(t, i, _) in &order {
            let data = if t == 0 {
                &video.packets[i].data
            } else {
                &audio[t - 1].packets[i].data
            };
            w.write_all(data)?;
        }
        let f = w.into_inner().map_err(|e| e.into_error())?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path).with_context(|| format!("rename to {}", path.display()))?;

    Ok(MuxStats {
        duration_hns: vt.total_duration as i64 * 10_000_000 / VIDEO_TIMESCALE as i64,
        bytes: base + payload,
        video_frames: video.packets.len(),
    })
}

// ---------------------------------------------------------------------------------------
// Box writer

struct W(Vec<u8>);

impl W {
    fn u8(&mut self, v: u8) {
        self.0.push(v);
    }
    fn u16(&mut self, v: u16) {
        self.0.extend_from_slice(&v.to_be_bytes());
    }
    fn u24(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_be_bytes()[1..]);
    }
    fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_be_bytes());
    }
    fn u64(&mut self, v: u64) {
        self.0.extend_from_slice(&v.to_be_bytes());
    }
    fn bytes(&mut self, b: &[u8]) {
        self.0.extend_from_slice(b);
    }
    fn zeros(&mut self, n: usize) {
        self.0.resize(self.0.len() + n, 0);
    }
    fn boxed(&mut self, kind: &[u8; 4], f: impl FnOnce(&mut W)) {
        let start = self.0.len();
        self.u32(0);
        self.bytes(kind);
        f(self);
        let size = (self.0.len() - start) as u32;
        self.0[start..start + 4].copy_from_slice(&size.to_be_bytes());
    }
    fn full(&mut self, kind: &[u8; 4], version: u8, flags: u32, f: impl FnOnce(&mut W)) {
        self.boxed(kind, |w| {
            w.u8(version);
            w.u24(flags);
            f(w);
        });
    }
    fn matrix(&mut self) {
        for v in [0x0001_0000u32, 0, 0, 0, 0x0001_0000, 0, 0, 0, 0x4000_0000] {
            self.u32(v);
        }
    }
}

fn build_ftyp() -> Vec<u8> {
    let mut w = W(Vec::new());
    w.boxed(b"ftyp", |w| {
        w.bytes(b"isom");
        w.u32(0x200);
        for b in [b"isom", b"iso2", b"avc1", b"mp41"] {
            w.bytes(b);
        }
    });
    w.0
}

fn to_movie(duration: u64, timescale: u32) -> u32 {
    (duration * MOVIE_TIMESCALE as u64 / timescale as u64) as u32
}

fn build_moov(video: &VideoTrack, vt: &SampleTable, audio: &[AudioTrack], ats: &[SampleTable], base: u64) -> Vec<u8> {
    let mut w = W(Vec::new());
    let movie_dur = to_movie(vt.total_duration, VIDEO_TIMESCALE);
    w.boxed(b"moov", |w| {
        w.full(b"mvhd", 0, 0, |w| {
            w.u32(0);
            w.u32(0);
            w.u32(MOVIE_TIMESCALE);
            w.u32(movie_dur);
            w.u32(0x0001_0000); // rate 1.0
            w.u16(0x0100); // volume 1.0
            w.zeros(10);
            w.matrix();
            w.zeros(24);
            w.u32(audio.len() as u32 + 2); // next track id
        });
        // Video track
        w.boxed(b"trak", |w| {
            track_header(w, 1, 3, movie_dur, 0, false, video.width, video.height);
            w.boxed(b"mdia", |w| {
                media_header(w, VIDEO_TIMESCALE, vt.total_duration);
                handler(w, b"vide", "Video");
                w.boxed(b"minf", |w| {
                    w.full(b"vmhd", 0, 1, |w| w.zeros(8));
                    data_info(w);
                    w.boxed(b"stbl", |w| {
                        w.full(b"stsd", 0, 0, |w| {
                            w.u32(1);
                            avc1(w, video);
                        });
                        sample_tables(w, vt, base, true);
                    });
                });
            });
        });
        // Audio tracks
        for (i, (a, t)) in audio.iter().zip(ats).enumerate() {
            let dur = to_movie(t.total_duration, a.sample_rate);
            w.boxed(b"trak", |w| {
                let flags = if a.enabled { 3 } else { 2 };
                track_header(w, i as u32 + 2, flags, dur, 1, true, 0, 0);
                w.boxed(b"mdia", |w| {
                    media_header(w, a.sample_rate, t.total_duration);
                    handler(w, b"soun", a.name);
                    w.boxed(b"minf", |w| {
                        w.full(b"smhd", 0, 0, |w| w.zeros(4));
                        data_info(w);
                        w.boxed(b"stbl", |w| {
                            w.full(b"stsd", 0, 0, |w| {
                                w.u32(1);
                                mp4a(w, a);
                            });
                            sample_tables(w, t, base, false);
                        });
                    });
                });
            });
        }
    });
    w.0
}

#[allow(clippy::too_many_arguments)]
fn track_header(w: &mut W, id: u32, flags: u32, duration: u32, alt_group: u16, audio: bool, width: u32, height: u32) {
    w.full(b"tkhd", 0, flags, |w| {
        w.u32(0);
        w.u32(0);
        w.u32(id);
        w.u32(0);
        w.u32(duration);
        w.zeros(8);
        w.u16(0); // layer
        w.u16(alt_group);
        w.u16(if audio { 0x0100 } else { 0 });
        w.u16(0);
        w.matrix();
        w.u32(width << 16);
        w.u32(height << 16);
    });
}

fn media_header(w: &mut W, timescale: u32, duration: u64) {
    w.full(b"mdhd", 0, 0, |w| {
        w.u32(0);
        w.u32(0);
        w.u32(timescale);
        w.u32(duration as u32);
        w.u16(0x55c4); // 'und'
        w.u16(0);
    });
}

fn handler(w: &mut W, kind: &[u8; 4], name: &str) {
    w.full(b"hdlr", 0, 0, |w| {
        w.u32(0);
        w.bytes(kind);
        w.zeros(12);
        w.bytes(name.as_bytes());
        w.u8(0);
    });
}

fn data_info(w: &mut W) {
    w.boxed(b"dinf", |w| {
        w.full(b"dref", 0, 0, |w| {
            w.u32(1);
            w.full(b"url ", 0, 1, |_| {});
        });
    });
}

fn avc1(w: &mut W, v: &VideoTrack) {
    w.boxed(b"avc1", |w| {
        w.zeros(6);
        w.u16(1); // data reference index
        w.zeros(16);
        w.u16(v.width as u16);
        w.u16(v.height as u16);
        w.u32(0x0048_0000);
        w.u32(0x0048_0000);
        w.u32(0);
        w.u16(1); // frame count
        let mut name = [0u8; 32];
        let label = b"BettrFTHR H.264";
        name[0] = label.len() as u8;
        name[1..1 + label.len()].copy_from_slice(label);
        w.bytes(&name);
        w.u16(0x0018);
        w.u16(0xffff);
        w.boxed(b"avcC", |w| {
            w.u8(1);
            w.u8(v.sps[1]); // profile
            w.u8(v.sps[2]); // constraint flags
            w.u8(v.sps[3]); // level
            w.u8(0xff); // 4-byte NAL lengths
            w.u8(0xe1); // one SPS
            w.u16(v.sps.len() as u16);
            w.bytes(v.sps);
            w.u8(1);
            w.u16(v.pps.len() as u16);
            w.bytes(v.pps);
            if matches!(v.sps[1], 100 | 110 | 122 | 144) {
                w.u8(0xfd); // chroma_format_idc = 1 (4:2:0)
                w.u8(0xf8); // bit_depth_luma_minus8 = 0
                w.u8(0xf8); // bit_depth_chroma_minus8 = 0
                w.u8(0); // no SPS extensions
            }
        });
    });
}

pub fn aac_lc_asc(sample_rate: u32, channels: u16) -> [u8; 2] {
    const RATES: [u32; 13] = [
        96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000, 7350,
    ];
    let idx = RATES.iter().position(|&r| r == sample_rate).unwrap_or(3) as u16;
    let v: u16 = (2 << 11) | (idx << 7) | ((channels & 0xf) << 3);
    v.to_be_bytes()
}

fn mp4a(w: &mut W, a: &AudioTrack) {
    let asc = aac_lc_asc(a.sample_rate, a.channels);
    w.boxed(b"mp4a", |w| {
        w.zeros(6);
        w.u16(1);
        w.zeros(8);
        w.u16(a.channels);
        w.u16(16);
        w.u32(0);
        w.u32(a.sample_rate << 16);
        w.full(b"esds", 0, 0, |w| {
            let dsi_len = asc.len() as u8;
            let dcd_len = 13 + 2 + dsi_len;
            let es_len = 3 + 2 + dcd_len + 3;
            w.u8(0x03);
            w.u8(es_len);
            w.u16(0); // ES_ID
            w.u8(0);
            w.u8(0x04);
            w.u8(dcd_len);
            w.u8(0x40); // MPEG-4 audio
            w.u8(0x15); // audio stream
            w.u24(0);
            w.u32(0);
            w.u32(0);
            w.u8(0x05);
            w.u8(dsi_len);
            w.bytes(&asc);
            w.u8(0x06);
            w.u8(1);
            w.u8(0x02);
        });
    });
}

fn sample_tables(w: &mut W, t: &SampleTable, base: u64, video: bool) {
    // stts: run-length encoded durations
    let mut runs: Vec<(u32, u32)> = Vec::new();
    for &d in &t.durations {
        match runs.last_mut() {
            Some((n, dur)) if *dur == d => *n += 1,
            _ => runs.push((1, d)),
        }
    }
    w.full(b"stts", 0, 0, |w| {
        w.u32(runs.len() as u32);
        for (n, d) in &runs {
            w.u32(*n);
            w.u32(*d);
        }
    });
    if video {
        w.full(b"stss", 0, 0, |w| {
            w.u32(t.keys.len() as u32);
            for k in &t.keys {
                w.u32(*k);
            }
        });
    }
    // one sample per chunk keeps stsc trivial and interleaving exact
    w.full(b"stsc", 0, 0, |w| {
        w.u32(1);
        w.u32(1);
        w.u32(1);
        w.u32(1);
    });
    w.full(b"stsz", 0, 0, |w| {
        w.u32(0);
        w.u32(t.sizes.len() as u32);
        for s in &t.sizes {
            w.u32(*s);
        }
    });
    w.full(b"co64", 0, 0, |w| {
        w.u32(t.offsets.len() as u32);
        for o in &t.offsets {
            w.u64(base + o);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn asc_for_48k_stereo() {
        assert_eq!(aac_lc_asc(48000, 2), [0x11, 0x90]);
        assert_eq!(aac_lc_asc(44100, 2), [0x12, 0x10]);
    }

    #[test]
    fn writes_consistent_file() {
        let sps = [0x67, 100, 0, 40, 0xac];
        let pps = [0x68, 0xee, 0x3c, 0x80];
        let vp: Vec<Packet> = (0..60)
            .map(|i| Packet {
                pts: 1_000_000 + i * 166_667,
                duration: 166_667,
                key: i % 30 == 0,
                data: Arc::from(vec![0, 0, 0, 2, 0x65, i as u8]),
            })
            .collect();
        let ap: Vec<Packet> = (0..47)
            .map(|i| Packet {
                pts: 1_000_000 + i * 213_333,
                duration: 213_333,
                key: true,
                data: Arc::from(vec![0x21u8; 10]),
            })
            .collect();
        let dir = std::env::temp_dir().join(format!("bettr-mp4-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.mp4");
        let stats = write_mp4(
            &path,
            &VideoTrack {
                width: 1920,
                height: 1080,
                fps: 60,
                sps: &sps,
                pps: &pps,
                packets: &vp,
            },
            &[AudioTrack {
                name: "Mix",
                sample_rate: 48000,
                channels: 2,
                enabled: true,
                packets: &ap,
            }],
        )
        .unwrap();
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(bytes.len() as u64, stats.bytes);
        assert_eq!(&bytes[4..8], b"ftyp");
        // moov directly after ftyp (faststart)
        let ftyp_len = u32::from_be_bytes(bytes[0..4].try_into().unwrap()) as usize;
        assert_eq!(&bytes[ftyp_len + 4..ftyp_len + 8], b"moov");
        assert_eq!(stats.video_frames, 60);
        assert!((stats.duration_hns - 10_000_000).abs() < 2_000);
        std::fs::remove_dir_all(&dir).ok();
    }
}
