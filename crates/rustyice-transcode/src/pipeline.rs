use bytes::Bytes;
use rustyice_core::config::TranscodeConfig;

use crate::{
    TranscodeError,
    decoder::decode_mp3_frames,
    encoder::LameEncoder,
    resampler::PcmResampler,
};

pub struct TranscodePipeline {
    config: TranscodeConfig,
    input_buf: Vec<u8>,
    encoder: LameEncoder,
    // Set after first frame decoded; encoder is rebuilt if these differ from config
    source_sample_rate: Option<u32>,
    source_channels: Option<u8>,
    resampler: Option<PcmResampler>,
}

impl TranscodePipeline {
    #[cfg(test)]
    pub(crate) fn resampler_is_active(&self) -> bool {
        self.resampler.is_some()
    }

    pub fn new(config: TranscodeConfig) -> Result<Self, TranscodeError> {
        // Encoder is built eagerly with config sample rate and a placeholder source rate.
        // It will be rebuilt on the first frame if source rate differs.
        let encoder = LameEncoder::new(
            config.sample_rate,
            config.sample_rate,
            2, // default to stereo; will rebuild on first frame if mono
            config.bitrate_kbps,
        )?;
        Ok(Self {
            config,
            input_buf: Vec::new(),
            encoder,
            source_sample_rate: None,
            source_channels: None,
            resampler: None,
        })
    }

    /// Feed raw bytes from the source. Returns transcoded MP3 bytes.
    /// Returns empty `Bytes` when still buffering (no complete frame yet).
    pub fn push(&mut self, data: &[u8]) -> Result<Bytes, TranscodeError> {
        if data.is_empty() {
            return Ok(Bytes::new());
        }
        self.input_buf.extend_from_slice(data);

        let (frames, consumed) = collect_complete_frames(&self.input_buf);
        if frames.is_empty() {
            return Ok(Bytes::new());
        }
        self.input_buf.drain(..consumed);

        let combined: Vec<u8> = frames.into_iter().flatten().collect();
        self.transcode_pcm(&combined)
    }

    /// Flush resampler tail then encoder's internal buffers at end of stream.
    pub fn flush(&mut self) -> Result<Bytes, TranscodeError> {
        let mut combined = Vec::new();

        if let Some(ref mut resampler) = self.resampler {
            let tail_pcm = resampler.flush()?;
            if !tail_pcm.is_empty() {
                let encoded = self.encoder.encode(&tail_pcm)?;
                combined.extend_from_slice(&encoded);
            }
        }

        let flushed = self.encoder.flush()?;
        combined.extend_from_slice(&flushed);
        Ok(Bytes::from(combined))
    }

    fn transcode_pcm(&mut self, frame_data: &[u8]) -> Result<Bytes, TranscodeError> {
        let (pcm, sample_rate, channels) = decode_mp3_frames(frame_data)?;
        if pcm.is_empty() || sample_rate == 0 {
            return Ok(Bytes::new());
        }

        let mut output: Vec<u8> = Vec::new();

        // Rebuild encoder if source format differs from what encoder expects
        if self.source_sample_rate != Some(sample_rate) || self.source_channels != Some(channels) {
            self.source_sample_rate = Some(sample_rate);
            self.source_channels = Some(channels);
            // Flush the old encoder's internal LAME buffer tail before replacing it.
            if let Ok(tail) = self.encoder.flush() {
                if !tail.is_empty() {
                    output.extend_from_slice(&tail);
                }
            }
            self.encoder = LameEncoder::new(
                self.config.sample_rate, // in_sample_rate = target (we resample before encode)
                self.config.sample_rate,
                channels,
                self.config.bitrate_kbps,
            )?;
            self.resampler = if sample_rate != self.config.sample_rate {
                Some(PcmResampler::new(
                    sample_rate,
                    self.config.sample_rate,
                    channels as usize,
                )?)
            } else {
                None
            };
        }

        let pcm = if let Some(ref mut resampler) = self.resampler {
            resampler.process(&pcm)?
        } else {
            pcm
        };

        if pcm.is_empty() {
            return if output.is_empty() {
                Ok(Bytes::new())
            } else {
                Ok(Bytes::from(output))
            };
        }

        let encoded = self.encoder.encode(&pcm)?;
        output.extend_from_slice(&encoded);
        Ok(Bytes::from(output))
    }
}

fn mpeg_frame_size(header: &[u8], bitrate_kbps: u32, sample_rate: u32) -> usize {
    let mpeg1 = header.len() >= 2 && (header[1] >> 3) & 0x03 == 0b11;
    let padding = if header.len() >= 3 { ((header[2] >> 1) & 0x01) as usize } else { 0 };
    let bitrate_bps = bitrate_kbps as usize * 1000;
    if mpeg1 {
        144 * bitrate_bps / sample_rate as usize + padding
    } else {
        72 * bitrate_bps / sample_rate as usize + padding
    }
}

/// Scan `data` for complete MP3 frames. Returns (list_of_frame_byte_slices_owned, total_bytes_consumed).
fn collect_complete_frames(data: &[u8]) -> (Vec<Vec<u8>>, usize) {
    use rustyice_codec::mp3::Mp3Codec;
    use rustyice_core::traits::Codec as _;

    let codec = Mp3Codec;
    let mut pos = 0;
    let mut frames: Vec<Vec<u8>> = Vec::new();

    while pos + 4 <= data.len() {
        if data[pos] != 0xFF || (data[pos + 1] & 0xE0) != 0xE0 {
            pos += 1;
            continue;
        }

        let header = &data[pos..];
        let Some(info) = codec.probe(header) else {
            pos += 1;
            continue;
        };

        let frame_end = if let Some(bitrate_kbps) = info.bitrate_kbps {
            pos + mpeg_frame_size(header, bitrate_kbps, info.sample_rate)
        } else {
            // Free-format: scan for the next sync word to determine frame end.
            let mut search = pos + 4;
            let found = loop {
                if search + 1 >= data.len() {
                    break None;
                }
                if data[search] == 0xFF && (data[search + 1] & 0xE0) == 0xE0 {
                    break Some(search);
                }
                search += 1;
            };
            match found {
                Some(end) => end,
                None => break,
            }
        };

        if frame_end > data.len() {
            break;
        }

        frames.push(data[pos..frame_end].to_vec());
        pos = frame_end;
    }

    (frames, pos)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustyice_core::config::TranscodeFormat;

    fn test_config() -> TranscodeConfig {
        TranscodeConfig {
            format: TranscodeFormat::Mp3,
            sample_rate: 44100,
            bitrate_kbps: 64,
        }
    }

    #[test]
    fn push_empty_returns_empty() {
        let mut p = TranscodePipeline::new(test_config()).unwrap();
        let out = p.push(&[]).unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn push_garbage_returns_empty() {
        let mut p = TranscodePipeline::new(test_config()).unwrap();
        let out = p.push(&[0u8; 1024]).unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn collect_frames_finds_no_frames_in_garbage() {
        let data = vec![0u8; 200];
        let (frames, consumed) = collect_complete_frames(&data);
        assert!(frames.is_empty());
        // consumed may be > 0: scanner advances past garbage that cannot
        // contain a valid sync word, preventing unbounded buffer growth.
        assert!(consumed <= data.len());
    }

    #[test]
    fn roundtrip_silence_through_pipeline() {
        // Encode 5 seconds of silence with LAME to ensure LAME flushes output,
        // then decode+reencode through pipeline.
        use crate::encoder::LameEncoder;

        let mut enc = LameEncoder::new(44100, 44100, 2, 128).unwrap();
        let silence = vec![0.0f32; 44100 * 2 * 5]; // 5 seconds stereo silence
        let mut mp3_data = enc.encode(&silence).unwrap();
        mp3_data.extend_from_slice(&enc.flush().unwrap());

        assert!(!mp3_data.is_empty(), "LAME must produce output for silence");

        // Verify it starts with a valid MP3 sync word
        assert!(
            mp3_data.iter().enumerate().any(|(i, &b)| {
                i + 1 < mp3_data.len() && b == 0xFF && (mp3_data[i + 1] & 0xE0) == 0xE0
            }),
            "LAME output must contain MP3 sync word"
        );

        // Now feed through pipeline in small chunks
        let mut pipeline = TranscodePipeline::new(test_config()).unwrap();
        let mut all_output = Vec::new();
        let mut error_count = 0usize;
        for chunk in mp3_data.chunks(4096) {
            match pipeline.push(chunk) {
                Ok(out) => all_output.extend_from_slice(&out),
                Err(_) => error_count += 1,
            }
        }
        if let Ok(tail) = pipeline.flush() {
            all_output.extend_from_slice(&tail);
        }

        assert_eq!(error_count, 0, "pipeline produced unexpected errors");

        // Output must be non-empty and contain valid MP3 sync words.
        assert!(!all_output.is_empty(), "pipeline must produce output from 5 seconds of audio");
        assert!(
            all_output.windows(2).any(|w| w[0] == 0xFF && (w[1] & 0xE0) == 0xE0),
            "pipeline output must contain MP3 sync words"
        );
    }

    #[test]
    fn rate_mismatch_produces_output() {
        use crate::encoder::LameEncoder;
        use rustyice_core::config::TranscodeFormat;

        // Generate MP3 at 48000 Hz
        let mut enc = LameEncoder::new(48000, 48000, 2, 128).unwrap();
        let silence = vec![0.0f32; 48000 * 2]; // 1 second stereo
        let mut mp3_48k: Vec<u8> = enc.encode(&silence).unwrap();
        mp3_48k.extend_from_slice(&enc.flush().unwrap());

        assert!(!mp3_48k.is_empty(), "LAME produced no output — encoder broken or environment misconfigured");

        // Pipeline targeting 44100 Hz — resampler should activate
        let mut pipeline = TranscodePipeline::new(TranscodeConfig {
            format: TranscodeFormat::Mp3,
            sample_rate: 44100,
            bitrate_kbps: 64,
        })
        .unwrap();

        let mut any_output = false;
        for chunk in mp3_48k.chunks(4096) {
            match pipeline.push(chunk) {
                Ok(out) if !out.is_empty() => any_output = true,
                Ok(_) => {}
                Err(e) => eprintln!("transcode error (may be ok for silence): {e}"),
            }
        }
        if let Ok(tail) = pipeline.flush() {
            if !tail.is_empty() {
                any_output = true;
            }
        }

        assert!(any_output, "pipeline must produce output when resampling 48000 Hz to 44100 Hz");
        assert!(
            pipeline.resampler_is_active(),
            "resampler must be initialized when source rate differs from target rate"
        );
    }

    fn generate_vbr_mp3() -> Vec<u8> {
        use mp3lame_sys::*;
        unsafe {
            let gfp = lame_init();
            assert!(!gfp.is_null(), "lame_init failed");
            lame_set_in_samplerate(gfp, 44100);
            lame_set_out_samplerate(gfp, 44100);
            lame_set_num_channels(gfp, 2);
            lame_set_VBR(gfp, vbr_mode::vbr_default);
            lame_set_VBR_quality(gfp, 5.0);
            let ret = lame_init_params(gfp);
            assert!(ret >= 0, "lame_init_params failed: {ret}");

            let num_samples = 44100; // 1 second per channel
            let silence_l = vec![0.0f32; num_samples];
            let silence_r = vec![0.0f32; num_samples];
            let mut out = vec![0u8; num_samples * 5 / 4 + 7200];
            let n = lame_encode_buffer_ieee_float(
                gfp, silence_l.as_ptr(), silence_r.as_ptr(),
                num_samples as i32, out.as_mut_ptr(), out.len() as i32,
            );
            let mut flush = vec![0u8; 7200];
            let f = lame_encode_flush(gfp, flush.as_mut_ptr(), 7200);
            lame_close(gfp);

            let mut result = Vec::new();
            if n > 0 { result.extend_from_slice(&out[..n as usize]); }
            if f > 0 { result.extend_from_slice(&flush[..f as usize]); }
            result
        }
    }

    #[test]
    fn vbr_stream_processes_without_error() {
        use rustyice_core::config::TranscodeFormat;

        // Generate a real VBR MP3 with a Xing header
        let mp3_data = generate_vbr_mp3();

        assert!(!mp3_data.is_empty(), "LAME produced no output — encoder broken or environment misconfigured");

        let mut pipeline = TranscodePipeline::new(TranscodeConfig {
            format: TranscodeFormat::Mp3,
            sample_rate: 44100,
            bitrate_kbps: 64,
        })
        .unwrap();

        // Feed in small chunks simulating streaming delivery
        let mut all_output = Vec::new();
        let mut error_count = 0usize;
        for chunk in mp3_data.chunks(512) {
            match pipeline.push(chunk) {
                Ok(out) => all_output.extend_from_slice(&out),
                Err(_) => error_count += 1,
            }
        }
        if let Ok(tail) = pipeline.flush() {
            all_output.extend_from_slice(&tail);
        }

        assert_eq!(error_count, 0, "pipeline produced errors on VBR input");

        assert!(!all_output.is_empty(), "transcoded VBR stream produced no output");

        // The transcoded output must NOT contain a Xing/Info VBR header,
        // since re-encoding via LAME for streaming does not insert one.
        let xing_present = all_output.windows(4).any(|w| w == b"Xing" || w == b"Info");
        assert!(
            !xing_present,
            "transcoded output must not contain a Xing/Info VBR header"
        );
    }
}
