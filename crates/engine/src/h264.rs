//! Minimal H.264 bitstream helpers: Annex B → AVCC (length-prefixed) and parameter sets.

pub const NAL_IDR: u8 = 5;
pub const NAL_SPS: u8 = 7;
pub const NAL_PPS: u8 = 8;
pub const NAL_AUD: u8 = 9;

/// Splits an Annex B byte stream into NAL units (start codes removed).
pub fn split_annexb(data: &[u8]) -> Vec<&[u8]> {
    let mut nals = Vec::new();
    let mut i = 0;
    let mut start: Option<usize> = None;
    while i + 3 <= data.len() {
        let three = data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1;
        if three {
            if let Some(s) = start {
                // Trailing zero of a 4-byte start code belongs to the separator.
                let mut end = i;
                while end > s && data[end - 1] == 0 {
                    end -= 1;
                }
                if end > s {
                    nals.push(&data[s..end]);
                }
            }
            i += 3;
            start = Some(i);
        } else {
            i += 1;
        }
    }
    match start {
        Some(s) if s < data.len() => nals.push(&data[s..]),
        None if !data.is_empty() => nals.push(data), // no start codes: single NAL
        _ => {}
    }
    nals
}

pub fn nal_type(nal: &[u8]) -> u8 {
    nal.first().map(|b| b & 0x1f).unwrap_or(0)
}

pub struct AccessUnit {
    /// Length-prefixed (4-byte big endian) NAL units, without SPS/PPS/AUD.
    pub avcc: Vec<u8>,
    pub sps: Option<Vec<u8>>,
    pub pps: Option<Vec<u8>>,
    pub idr: bool,
}

pub fn annexb_to_avcc(data: &[u8]) -> AccessUnit {
    let mut au = AccessUnit {
        avcc: Vec::with_capacity(data.len() + 16),
        sps: None,
        pps: None,
        idr: false,
    };
    for nal in split_annexb(data) {
        match nal_type(nal) {
            NAL_SPS => au.sps = Some(nal.to_vec()),
            NAL_PPS => au.pps = Some(nal.to_vec()),
            NAL_AUD => {}
            t => {
                if t == NAL_IDR {
                    au.idr = true;
                }
                au.avcc.extend_from_slice(&(nal.len() as u32).to_be_bytes());
                au.avcc.extend_from_slice(nal);
            }
        }
    }
    au
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_mixed_start_codes() {
        let data = [0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3, 0, 0, 0, 1, 0x65, 9, 9];
        let nals = split_annexb(&data);
        assert_eq!(nals, vec![&[0x67, 1, 2][..], &[0x68, 3][..], &[0x65, 9, 9][..]]);
    }

    #[test]
    fn converts_to_avcc_and_extracts_parameter_sets() {
        let data = [
            0, 0, 0, 1, 0x09, 0xf0, 0, 0, 0, 1, 0x67, 1, 0, 0, 1, 0x68, 2, 0, 0, 1, 0x65, 7, 7, 7,
        ];
        let au = annexb_to_avcc(&data);
        assert_eq!(au.sps.as_deref(), Some(&[0x67, 1][..]));
        assert_eq!(au.pps.as_deref(), Some(&[0x68, 2][..]));
        assert!(au.idr);
        assert_eq!(au.avcc, vec![0, 0, 0, 4, 0x65, 7, 7, 7]);
    }
}
