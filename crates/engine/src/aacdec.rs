//! AAC-LC decoding with the Media Foundation decoder (for the editor: waveforms, remixing).

use anyhow::{Context, Result, anyhow, bail};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::CoTaskMemFree;

pub struct AacDecoder {
    transform: IMFTransform,
    channels: usize,
    /// Output is f32 (else 16-bit PCM, converted).
    float: bool,
    provides_samples: bool,
    out_size: u32,
}

fn find_decoder() -> Result<IMFTransform> {
    let input = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Audio,
        guidSubtype: MFAudioFormat_AAC,
    };
    let mut ptr: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count = 0u32;
    unsafe {
        MFTEnumEx(
            MFT_CATEGORY_AUDIO_DECODER,
            MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_SORTANDFILTER,
            Some(&input),
            None,
            &mut ptr,
            &mut count,
        )?;
        if ptr.is_null() || count == 0 {
            bail!("no AAC decoder installed (Windows N edition without the Media Feature Pack?)");
        }
        let list = std::slice::from_raw_parts_mut(ptr, count as usize);
        let first = list[0].take();
        for s in list.iter_mut() {
            s.take();
        }
        CoTaskMemFree(Some(ptr as *const _));
        Ok(first.ok_or_else(|| anyhow!("AAC decoder activate missing"))?.ActivateObject()?)
    }
}

impl AacDecoder {
    /// `asc` is the AudioSpecificConfig from the file's esds.
    pub fn new(asc: &[u8], sample_rate: u32, channels: u32) -> Result<Self> {
        let transform = find_decoder()?;
        unsafe {
            let inp = MFCreateMediaType()?;
            inp.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
            inp.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_AAC)?;
            inp.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, sample_rate)?;
            inp.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, channels)?;
            inp.SetUINT32(&MF_MT_AAC_PAYLOAD_TYPE, 0)?; // raw access units
            inp.SetUINT32(&MF_MT_AAC_AUDIO_PROFILE_LEVEL_INDICATION, 0x29)?;
            // HEAACWAVEINFO tail (payload type, profile, struct type, reserved) + ASC.
            let mut user = vec![0u8; 12];
            user[2..4].copy_from_slice(&0x29u16.to_le_bytes());
            user.extend_from_slice(asc);
            inp.SetBlob(&MF_MT_USER_DATA, &user)?;
            transform.SetInputType(0, &inp, 0).context("AAC decoder SetInputType")?;

            // Take one of the formats the decoder offers: float if it has it, else 16-bit PCM.
            let mut chosen: Option<(IMFMediaType, bool)> = None;
            let mut i = 0;
            while let Ok(t) = transform.GetOutputAvailableType(0, i) {
                i += 1;
                let sub = t.GetGUID(&MF_MT_SUBTYPE)?;
                let bits = t.GetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE).unwrap_or(0);
                if sub == MFAudioFormat_Float {
                    chosen = Some((t, true));
                    break;
                }
                if sub == MFAudioFormat_PCM && bits == 16 && chosen.is_none() {
                    chosen = Some((t, false));
                }
            }
            let (out, float) = chosen.context("AAC decoder offers no PCM output")?;
            let channels = out.GetUINT32(&MF_MT_AUDIO_NUM_CHANNELS).unwrap_or(channels);
            transform
                .SetOutputType(0, &out, 0)
                .with_context(|| format!("AAC decoder SetOutputType (asc {asc:02x?}, {sample_rate} Hz, {channels} ch)"))?;

            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
            let info = transform.GetOutputStreamInfo(0)?;
            Ok(Self {
                transform,
                channels: channels as usize,
                float,
                provides_samples: info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 != 0,
                out_size: info.cbSize.max(1024 * 8 * channels),
            })
        }
    }

    pub fn channels(&self) -> usize {
        self.channels
    }

    /// Decodes one access unit; appends interleaved f32 to `out`.
    pub fn decode(&mut self, au: &[u8], out: &mut Vec<f32>) -> Result<()> {
        unsafe {
            let buffer = MFCreateMemoryBuffer(au.len() as u32)?;
            let mut p = std::ptr::null_mut();
            buffer.Lock(&mut p, None, None)?;
            std::ptr::copy_nonoverlapping(au.as_ptr(), p, au.len());
            buffer.Unlock()?;
            buffer.SetCurrentLength(au.len() as u32)?;
            let sample = MFCreateSample()?;
            sample.AddBuffer(&buffer)?;
            self.transform.ProcessInput(0, &sample, 0).context("AAC decode input")?;
        }
        self.drain_output(out)
    }

    /// Flushes anything the decoder still holds.
    pub fn finish(&mut self, out: &mut Vec<f32>) -> Result<()> {
        unsafe { self.transform.ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0)? };
        self.drain_output(out)
    }

    fn drain_output(&mut self, out: &mut Vec<f32>) -> Result<()> {
        loop {
            unsafe {
                let provided = if self.provides_samples {
                    None
                } else {
                    let s = MFCreateSample()?;
                    s.AddBuffer(&MFCreateMemoryBuffer(self.out_size)?)?;
                    Some(s)
                };
                let mut buf = [MFT_OUTPUT_DATA_BUFFER {
                    dwStreamID: 0,
                    pSample: std::mem::ManuallyDrop::new(provided),
                    dwStatus: 0,
                    pEvents: std::mem::ManuallyDrop::new(None),
                }];
                let mut status = 0u32;
                let r = self.transform.ProcessOutput(0, &mut buf, &mut status);
                let s = std::mem::ManuallyDrop::take(&mut buf[0].pSample);
                std::mem::ManuallyDrop::drop(&mut buf[0].pEvents);
                match r {
                    Ok(()) => {}
                    Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(()),
                    Err(e) => return Err(e).context("AAC decode output"),
                }
                let Some(s) = s else { return Ok(()) };
                let b = s.ConvertToContiguousBuffer()?;
                let mut p = std::ptr::null_mut();
                let mut len = 0u32;
                b.Lock(&mut p, None, Some(&mut len))?;
                if self.float {
                    out.extend_from_slice(std::slice::from_raw_parts(p as *const f32, len as usize / 4));
                } else {
                    let s16 = std::slice::from_raw_parts(p as *const i16, len as usize / 2);
                    out.extend(s16.iter().map(|&x| x as f32 / 32768.0));
                }
                b.Unlock()?;
            }
        }
    }
}
