//! Minimal MP4 reader for the files this engine writes: progressive clips (moov + mdat) and
//! fragmented recordings (moov/mvex + moof/mdat pairs). H.264 video, AAC audio, no B-frames.

use anyhow::{Context, Result, bail};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Video,
    Audio,
}

#[derive(Clone, Copy, Debug)]
pub struct Sample {
    /// Decode/presentation time in the track timescale (no B-frames, so they are equal).
    pub pts: u64,
    pub duration: u32,
    pub offset: u64,
    pub size: u32,
    pub key: bool,
}

#[derive(Debug)]
pub struct Track {
    pub id: u32,
    pub kind: Kind,
    pub timescale: u32,
    pub name: String,
    pub enabled: bool,
    pub samples: Vec<Sample>,
    pub width: u32,
    pub height: u32,
    pub sps: Vec<u8>,
    pub pps: Vec<u8>,
    pub asc: Vec<u8>,
    pub channels: u32,
    pub sample_rate: u32,
    // trex defaults for fragments
    def_duration: u32,
    def_size: u32,
    def_flags: u32,
}

impl Track {
    pub fn duration_secs(&self) -> f64 {
        self.samples.last().map_or(0.0, |s| (s.pts + s.duration as u64) as f64 / self.timescale as f64)
    }
}

pub struct Mp4 {
    pub tracks: Vec<Track>,
    file: File,
}

struct Reader<'a> {
    b: &'a [u8],
    p: usize,
}

impl<'a> Reader<'a> {
    fn new(b: &'a [u8]) -> Self {
        Self { b, p: 0 }
    }
    fn left(&self) -> usize {
        self.b.len().saturating_sub(self.p)
    }
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if self.left() < n {
            bail!("truncated box");
        }
        let s = &self.b[self.p..self.p + n];
        self.p += n;
        Ok(s)
    }
    fn skip(&mut self, n: usize) -> Result<()> {
        self.take(n).map(|_| ())
    }
    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }
    fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_be_bytes(self.take(2)?.try_into()?))
    }
    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_be_bytes(self.take(4)?.try_into()?))
    }
    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_be_bytes(self.take(8)?.try_into()?))
    }
    /// Next child box: (type, body).
    fn child(&mut self) -> Result<Option<([u8; 4], &'a [u8])>> {
        if self.left() < 8 {
            return Ok(None);
        }
        let size = self.u32()? as u64;
        let kind: [u8; 4] = self.take(4)?.try_into()?;
        let body_len = match size {
            0 => self.left() as u64,
            1 => self.u64()? - 16,
            n => n.checked_sub(8).context("bad box size")?,
        };
        Ok(Some((kind, self.take(body_len as usize)?)))
    }
}

fn children(b: &[u8]) -> Result<Vec<([u8; 4], &[u8])>> {
    let mut r = Reader::new(b);
    let mut out = Vec::new();
    while let Some(c) = r.child()? {
        out.push(c);
    }
    Ok(out)
}

fn find<'a>(b: &'a [u8], kind: &[u8; 4]) -> Result<Option<&'a [u8]>> {
    Ok(children(b)?.into_iter().find(|(k, _)| k == kind).map(|(_, v)| v))
}

impl Mp4 {
    pub fn open(path: &Path) -> Result<Self> {
        let mut file = File::open(path).with_context(|| format!("open {}", path.display()))?;
        let len = file.metadata()?.len();
        let mut tracks: Vec<Track> = Vec::new();
        let mut pos = 0u64;
        let mut have_moov = false;
        // Walk top-level boxes without reading mdat payloads.
        while pos + 8 <= len {
            file.seek(SeekFrom::Start(pos))?;
            let mut h = [0u8; 16];
            file.read_exact(&mut h[..8])?;
            let mut size = u32::from_be_bytes(h[0..4].try_into()?) as u64;
            let kind: [u8; 4] = h[4..8].try_into()?;
            let mut header = 8u64;
            if size == 1 {
                file.read_exact(&mut h[8..16])?;
                size = u64::from_be_bytes(h[8..16].try_into()?);
                header = 16;
            } else if size == 0 {
                size = len - pos;
            }
            if size < header || pos + size > len {
                // A recording cut off by a crash: keep what is complete.
                break;
            }
            match &kind {
                b"moov" | b"moof" => {
                    let mut body = vec![0u8; (size - header) as usize];
                    file.read_exact(&mut body)?;
                    if &kind == b"moov" {
                        tracks = parse_moov(&body)?;
                        have_moov = true;
                    } else if have_moov {
                        parse_moof(&body, pos, &mut tracks)?;
                    }
                }
                _ => {}
            }
            pos += size;
        }
        if !have_moov {
            bail!("not a playable MP4 (no moov)");
        }
        Ok(Self { tracks, file })
    }

    pub fn read(&mut self, s: &Sample) -> Result<Vec<u8>> {
        let mut buf = vec![0u8; s.size as usize];
        self.file.seek(SeekFrom::Start(s.offset))?;
        self.file.read_exact(&mut buf)?;
        Ok(buf)
    }

    pub fn video(&self) -> Option<&Track> {
        self.tracks.iter().find(|t| t.kind == Kind::Video)
    }
}

fn parse_moov(b: &[u8]) -> Result<Vec<Track>> {
    let mut tracks = Vec::new();
    for (k, body) in children(b)? {
        match &k {
            b"trak" => {
                if let Some(t) = parse_trak(body)? {
                    tracks.push(t);
                }
            }
            b"mvex" => {
                for (k, trex) in children(body)? {
                    if &k != b"trex" {
                        continue;
                    }
                    let mut r = Reader::new(trex);
                    r.skip(4)?;
                    let id = r.u32()?;
                    r.skip(4)?; // sample description index
                    let (d, s, f) = (r.u32()?, r.u32()?, r.u32()?);
                    if let Some(t) = tracks.iter_mut().find(|t: &&mut Track| t.id == id) {
                        t.def_duration = d;
                        t.def_size = s;
                        t.def_flags = f;
                    }
                }
            }
            _ => {}
        }
    }
    Ok(tracks)
}

fn parse_trak(b: &[u8]) -> Result<Option<Track>> {
    let tkhd = find(b, b"tkhd")?.context("trak without tkhd")?;
    let mut r = Reader::new(tkhd);
    let vf = r.u32()?;
    let version = vf >> 24;
    r.skip(if version == 1 { 16 } else { 8 })?;
    let id = r.u32()?;
    let enabled = vf & 1 != 0;

    let mdia = find(b, b"mdia")?.context("trak without mdia")?;
    let mdhd = find(mdia, b"mdhd")?.context("no mdhd")?;
    let mut r = Reader::new(mdhd);
    let v = r.u32()? >> 24;
    r.skip(if v == 1 { 16 } else { 8 })?;
    let timescale = r.u32()?;

    let hdlr = find(mdia, b"hdlr")?.context("no hdlr")?;
    let mut r = Reader::new(hdlr);
    r.skip(8)?;
    let handler: [u8; 4] = r.take(4)?.try_into()?;
    r.skip(12)?;
    let name_bytes = r.take(r.left())?;
    let name = String::from_utf8_lossy(name_bytes.split(|&c| c == 0).next().unwrap_or_default()).into_owned();
    let kind = match &handler {
        b"vide" => Kind::Video,
        b"soun" => Kind::Audio,
        _ => return Ok(None),
    };

    let stbl = find(find(mdia, b"minf")?.context("no minf")?, b"stbl")?.context("no stbl")?;
    let mut t = Track {
        id,
        kind,
        timescale,
        name,
        enabled,
        samples: Vec::new(),
        width: 0,
        height: 0,
        sps: Vec::new(),
        pps: Vec::new(),
        asc: Vec::new(),
        channels: 2,
        sample_rate: timescale,
        def_duration: 0,
        def_size: 0,
        def_flags: 0,
    };
    let stsd = find(stbl, b"stsd")?.context("no stsd")?;
    parse_stsd(&stsd[8..], &mut t)?;

    let mut durations: Vec<u32> = Vec::new();
    let mut sizes: Vec<u32> = Vec::new();
    let mut chunk_offsets: Vec<u64> = Vec::new();
    let mut stsc: Vec<(u32, u32)> = Vec::new();
    let mut sync: Option<Vec<u32>> = None;
    for (k, body) in children(stbl)? {
        let mut r = Reader::new(body);
        match &k {
            b"stts" => {
                r.skip(4)?;
                for _ in 0..r.u32()? {
                    let (count, delta) = (r.u32()?, r.u32()?);
                    durations.extend(std::iter::repeat_n(delta, count as usize));
                }
            }
            b"stsz" => {
                r.skip(4)?;
                let (fixed, count) = (r.u32()?, r.u32()?);
                for _ in 0..count {
                    sizes.push(if fixed != 0 { fixed } else { r.u32()? });
                }
            }
            b"stco" => {
                r.skip(4)?;
                for _ in 0..r.u32()? {
                    chunk_offsets.push(r.u32()? as u64);
                }
            }
            b"co64" => {
                r.skip(4)?;
                for _ in 0..r.u32()? {
                    chunk_offsets.push(r.u64()?);
                }
            }
            b"stsc" => {
                r.skip(4)?;
                for _ in 0..r.u32()? {
                    let (first, per) = (r.u32()?, r.u32()?);
                    r.skip(4)?;
                    stsc.push((first, per));
                }
            }
            b"stss" => {
                r.skip(4)?;
                let mut v = Vec::new();
                for _ in 0..r.u32()? {
                    v.push(r.u32()?);
                }
                sync = Some(v);
            }
            _ => {}
        }
    }
    // Expand chunks into per-sample offsets.
    let mut sample = 0usize;
    let mut pts = 0u64;
    for (ci, &chunk_off) in chunk_offsets.iter().enumerate() {
        let chunk_no = ci as u32 + 1;
        let per = stsc.iter().rev().find(|(first, _)| *first <= chunk_no).map_or(1, |(_, p)| *p);
        let mut off = chunk_off;
        for _ in 0..per {
            let (Some(&size), Some(&dur)) = (sizes.get(sample), durations.get(sample)) else {
                break;
            };
            let key = match &sync {
                Some(v) => v.binary_search(&(sample as u32 + 1)).is_ok(),
                None => true,
            };
            t.samples.push(Sample {
                pts,
                duration: dur,
                offset: off,
                size,
                key,
            });
            off += size as u64;
            pts += dur as u64;
            sample += 1;
        }
    }
    Ok(Some(t))
}

fn parse_stsd(b: &[u8], t: &mut Track) -> Result<()> {
    let mut r = Reader::new(b);
    let Some((k, entry)) = r.child()? else { return Ok(()) };
    match &k {
        b"avc1" | b"avc3" => {
            let mut e = Reader::new(entry);
            e.skip(24)?;
            t.width = e.u16()? as u32;
            t.height = e.u16()? as u32;
            e.skip(50)?;
            if let Some(avcc) = find(&entry[78..], b"avcC")? {
                let mut a = Reader::new(avcc);
                a.skip(5)?;
                let nsps = a.u8()? & 0x1f;
                for i in 0..nsps {
                    let n = a.u16()? as usize;
                    let s = a.take(n)?;
                    if i == 0 {
                        t.sps = s.to_vec();
                    }
                }
                let npps = a.u8()?;
                for i in 0..npps {
                    let n = a.u16()? as usize;
                    let s = a.take(n)?;
                    if i == 0 {
                        t.pps = s.to_vec();
                    }
                }
            }
        }
        b"mp4a" => {
            let mut e = Reader::new(entry);
            e.skip(16)?;
            t.channels = e.u16()? as u32;
            e.skip(6)?;
            t.sample_rate = e.u32()? >> 16;
            if let Some(esds) = find(&entry[28..], b"esds")? {
                t.asc = find_descriptor(&esds[4..], 0x05).unwrap_or_default();
            }
        }
        _ => {}
    }
    Ok(())
}

/// Finds an MPEG-4 descriptor by tag (recursing through ES / DecoderConfig descriptors).
fn find_descriptor(b: &[u8], want: u8) -> Option<Vec<u8>> {
    let mut p = 0usize;
    while p < b.len() {
        let tag = b[p];
        p += 1;
        let mut len = 0usize;
        for _ in 0..4 {
            let c = *b.get(p)?;
            p += 1;
            len = (len << 7) | (c & 0x7f) as usize;
            if c & 0x80 == 0 {
                break;
            }
        }
        let body = b.get(p..p + len)?;
        if tag == want {
            return Some(body.to_vec());
        }
        let inner = match tag {
            0x03 => {
                // ES_ID(2) + flags(1) [+ optional fields we never write]
                let flags = *body.get(2)?;
                let mut skip = 3;
                if flags & 0x80 != 0 {
                    skip += 2;
                }
                if flags & 0x40 != 0 {
                    skip += 1 + *body.get(skip)? as usize;
                }
                if flags & 0x20 != 0 {
                    skip += 2;
                }
                body.get(skip..)
            }
            0x04 => body.get(13..),
            _ => None,
        };
        if let Some(found) = inner.and_then(|i| find_descriptor(i, want)) {
            return Some(found);
        }
        p += len;
    }
    None
}

fn parse_moof(b: &[u8], moof_pos: u64, tracks: &mut [Track]) -> Result<()> {
    for (k, traf) in children(b)? {
        if &k != b"traf" {
            continue;
        }
        let tfhd = find(traf, b"tfhd")?.context("traf without tfhd")?;
        let mut r = Reader::new(tfhd);
        let flags = r.u32()? & 0xff_ffff;
        let id = r.u32()?;
        let Some(t) = tracks.iter_mut().find(|t| t.id == id) else {
            continue;
        };
        let mut base = moof_pos;
        if flags & 0x1 != 0 {
            base = r.u64()?;
        }
        if flags & 0x2 != 0 {
            r.skip(4)?;
        }
        let def_dur = if flags & 0x8 != 0 { r.u32()? } else { t.def_duration };
        let def_size = if flags & 0x10 != 0 { r.u32()? } else { t.def_size };
        let def_flags = if flags & 0x20 != 0 { r.u32()? } else { t.def_flags };

        let mut pts = t.samples.last().map_or(0, |s| s.pts + s.duration as u64);
        if let Some(tfdt) = find(traf, b"tfdt")? {
            let mut r = Reader::new(tfdt);
            let v = r.u32()? >> 24;
            pts = if v == 1 { r.u64()? } else { r.u32()? as u64 };
        }
        for (k, trun) in children(traf)? {
            if &k != b"trun" {
                continue;
            }
            let mut r = Reader::new(trun);
            let tf = r.u32()? & 0xff_ffff;
            let count = r.u32()?;
            let mut off = base;
            if tf & 0x1 != 0 {
                off = (base as i64 + r.u32()? as i32 as i64) as u64;
            }
            let first_flags = if tf & 0x4 != 0 { Some(r.u32()?) } else { None };
            for i in 0..count {
                let dur = if tf & 0x100 != 0 { r.u32()? } else { def_dur };
                let size = if tf & 0x200 != 0 { r.u32()? } else { def_size };
                let mut sflags = if tf & 0x400 != 0 { r.u32()? } else { def_flags };
                if i == 0
                    && let Some(f) = first_flags
                {
                    sflags = f;
                }
                if tf & 0x800 != 0 {
                    r.skip(4)?;
                }
                t.samples.push(Sample {
                    pts,
                    duration: dur,
                    offset: off,
                    size,
                    key: t.kind == Kind::Audio || sflags & 0x0001_0000 == 0,
                });
                off += size as u64;
                pts += dur as u64;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mp4::{self, AudioTrack, FragmentedWriter, VideoTrack};
    use crate::ring::Packet;
    use std::sync::Arc;

    fn packets(n: i64, step: i64, key_every: i64, size: usize) -> Vec<Packet> {
        (0..n)
            .map(|i| Packet {
                pts: 1_000_000 + i * step,
                duration: step,
                key: i % key_every == 0,
                data: Arc::from(vec![i as u8; size + i as usize % 3]),
            })
            .collect()
    }

    #[test]
    fn reads_back_progressive_and_fragmented() {
        let sps = [0x67, 100, 0, 40, 0xac];
        let pps = [0x68, 0xee, 0x3c, 0x80];
        let vp = packets(60, 166_667, 30, 50);
        let ap = packets(94, 213_333, 1, 20);
        let video = VideoTrack {
            name: crate::mp4::VIDEO,
            width: 1920,
            height: 1080,
            fps: 60,
            sps: &sps,
            pps: &pps,
            packets: &vp,
        };
        let audio = [
            AudioTrack { name: "Mix", sample_rate: 48000, channels: 2, enabled: true, packets: &ap },
            AudioTrack { name: "Mic", sample_rate: 48000, channels: 2, enabled: false, packets: &ap },
        ];
        let dir = std::env::temp_dir();
        let p = dir.join(format!("bettr-demux-{}.mp4", std::process::id()));
        mp4::write_mp4(&p, &video, &audio).unwrap();
        let mut m = Mp4::open(&p).unwrap();
        let v = m.video().unwrap();
        assert_eq!((v.width, v.height, v.samples.len()), (1920, 1080, 60));
        assert_eq!(v.sps, sps);
        assert!(v.samples[0].key && !v.samples[1].key && v.samples[30].key);
        let a: Vec<_> = m.tracks.iter().filter(|t| t.kind == Kind::Audio).collect();
        assert_eq!(a.len(), 2);
        assert_eq!((a[0].name.as_str(), a[0].enabled), ("Mix", true));
        assert_eq!((a[1].name.as_str(), a[1].enabled), ("Mic", false));
        assert_eq!(a[0].asc, mp4::aac_lc_asc(48000, 2));
        let s = m.tracks[0].samples[5];
        assert_eq!(m.read(&s).unwrap(), vp[5].data.to_vec());
        std::fs::remove_file(&p).unwrap();

        // Fragmented: two GOP fragments.
        let f = dir.join(format!("bettr-demux-frag-{}.mp4", std::process::id()));
        let mut w = FragmentedWriter::create(&f, &video, &audio).unwrap();
        w.write_fragment(&vp[..30], Some(vp[30].pts), &[&ap[..47], &ap[..47]]).unwrap();
        w.write_fragment(&vp[30..], None, &[&ap[47..], &ap[47..]]).unwrap();
        w.finish().unwrap();
        let mut m = Mp4::open(&f).unwrap();
        let v = m.video().unwrap();
        assert_eq!(v.samples.len(), 60);
        assert!(v.samples[30].key && !v.samples[31].key);
        assert!((v.duration_secs() - 1.0).abs() < 0.02, "{}", v.duration_secs());
        let s = m.tracks[0].samples[40];
        assert_eq!(m.read(&s).unwrap(), vp[40].data.to_vec());
        let mic = m.tracks.iter().find(|t| t.name == "Mic").unwrap();
        assert_eq!(mic.samples.len(), 94);
        std::fs::remove_file(&f).unwrap();
    }
}
