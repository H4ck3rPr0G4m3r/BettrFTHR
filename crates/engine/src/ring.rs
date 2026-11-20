//! Encoded packet ring buffers. Video is trimmed by whole GOPs so the oldest packet is
//! always a keyframe; audio is trimmed per packet.

use std::collections::VecDeque;
use std::sync::Arc;

#[derive(Clone, Debug)]
pub struct Packet {
    /// Presentation time in hns on the engine timeline (QPC based).
    pub pts: i64,
    pub duration: i64,
    pub key: bool,
    pub data: Arc<[u8]>,
}

pub struct PacketRing {
    packets: VecDeque<Packet>,
    /// Keep at least this much history (hns).
    max_span: i64,
    max_bytes: usize,
    bytes: usize,
    gop_trim: bool,
}

impl PacketRing {
    pub fn video(max_span: i64, max_bytes: usize) -> Self {
        Self {
            packets: VecDeque::new(),
            max_span,
            max_bytes,
            bytes: 0,
            gop_trim: true,
        }
    }

    pub fn audio(max_span: i64, max_bytes: usize) -> Self {
        Self {
            packets: VecDeque::new(),
            max_span,
            max_bytes,
            bytes: 0,
            gop_trim: false,
        }
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.packets.len()
    }

    pub fn bytes(&self) -> usize {
        self.bytes
    }

    pub fn span(&self) -> i64 {
        match (self.packets.front(), self.packets.back()) {
            (Some(a), Some(b)) => b.pts + b.duration - a.pts,
            _ => 0,
        }
    }

    pub fn newest_end(&self) -> Option<i64> {
        self.packets.back().map(|p| p.pts + p.duration)
    }

    pub fn push(&mut self, p: Packet) {
        if self.gop_trim && self.packets.is_empty() && !p.key {
            return; // never start a video ring on a non-keyframe
        }
        self.bytes += p.data.len();
        self.packets.push_back(p);
        self.trim();
    }

    fn trim(&mut self) {
        let Some(newest) = self.packets.back().map(|p| p.pts) else {
            return;
        };
        if self.gop_trim {
            loop {
                let next_key = self.packets.iter().skip(1).position(|p| p.key).map(|i| i + 1);
                let Some(k) = next_key else { break };
                let old_enough = newest - self.packets[k].pts >= self.max_span;
                if !(old_enough || self.bytes > self.max_bytes) {
                    break;
                }
                for p in self.packets.drain(..k) {
                    self.bytes -= p.data.len();
                }
            }
        } else {
            while let Some(front) = self.packets.front() {
                if newest - front.pts > self.max_span || self.bytes > self.max_bytes {
                    self.bytes -= front.data.len();
                    self.packets.pop_front();
                } else {
                    break;
                }
            }
        }
    }

    /// Video: the last `seconds_hns` of history, starting at the latest keyframe at or
    /// before `newest - seconds_hns` (so the clip is never shorter than asked when history
    /// allows). Falls back to the oldest keyframe.
    pub fn video_snapshot(&self, seconds_hns: i64) -> Vec<Packet> {
        let Some(newest) = self.packets.back().map(|p| p.pts) else {
            return Vec::new();
        };
        let target = newest - seconds_hns;
        let mut start = 0;
        for (i, p) in self.packets.iter().enumerate() {
            if p.pts > target {
                break;
            }
            if p.key {
                start = i;
            }
        }
        self.packets.range(start..).cloned().collect()
    }

    /// Packets whose pts lies in `[from, to)`.
    pub fn range(&self, from: i64, to: i64) -> Vec<Packet> {
        self.packets
            .iter()
            .filter(|p| p.pts >= from && p.pts < to)
            .cloned()
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pkt(pts: i64, key: bool) -> Packet {
        Packet {
            pts,
            duration: 10,
            key,
            data: Arc::from(vec![0u8; 4]),
        }
    }

    #[test]
    fn video_ring_trims_whole_gops_and_keeps_history() {
        let mut r = PacketRing::video(100, usize::MAX);
        for i in 0..100 {
            r.push(pkt(i * 10, i % 5 == 0));
        }
        let snap = r.video_snapshot(100);
        assert!(snap[0].key);
        let newest = 990;
        assert!(newest - snap[0].pts >= 100, "clip must cover the requested span");
        assert!(newest - snap[0].pts < 150, "but not much more than one GOP extra");
    }

    #[test]
    fn video_ring_rejects_leading_non_key() {
        let mut r = PacketRing::video(100, usize::MAX);
        r.push(pkt(0, false));
        assert_eq!(r.len(), 0);
        r.push(pkt(10, true));
        assert_eq!(r.len(), 1);
    }

    #[test]
    fn byte_cap_is_respected_for_audio() {
        let mut r = PacketRing::audio(i64::MAX, 40);
        for i in 0..100 {
            r.push(pkt(i, true));
        }
        assert!(r.bytes() <= 40);
    }
}
