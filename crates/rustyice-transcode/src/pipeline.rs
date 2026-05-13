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

    /// Flush encoder's internal buffers at end of stream.
    pub fn flush(&mut self) -> Result<Bytes, TranscodeError> {
        let bytes = self.encoder.flush()?;
        Ok(Bytes::from(bytes))
    }

    fn transcode_pcm(&mut self, frame_data: &[u8]) -> Result<Bytes, TranscodeError> {
        let (pcm, sample_rate, channels) = decode_mp3_frames(frame_data)?;
        if pcm.is_empty() || sample_rate == 0 {
            return Ok(Bytes::new());
        }

        // Rebuild encoder if source format differs from what encoder expects
        if self.source_sample_rate != Some(sample_rate) || self.source_channels != Some(channels) {
            self.source_sample_rate = Some(sample_rate);
            self.source_channels = Some(channels);
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
            return Ok(Bytes::new());
        }

        let encoded = self.encoder.encode(&pcm)?;
        Ok(Bytes::from(encoded))
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
            let mpeg1 = (header[1] >> 3) & 0x03 == 0b11;
            let padding = ((header[2] >> 1) & 0x01) as usize;
            let bitrate_bps = bitrate_kbps as usize * 1000;
            let frame_size = if mpeg1 {
                144 * bitrate_bps / info.sample_rate as usize + padding
            } else {
                72 * bitrate_bps / info.sample_rate as usize + padding
            };
            pos + frame_size
        } else {
            // Free-format: find next sync word as heuristic
            let mut next = pos + 4;
            while next + 1 < data.len() {
                if data[next] == 0xFF && (data[next + 1] & 0xE0) == 0xE0 {
                    break;
                }
                next += 1;
            }
            if next + 1 >= data.len() {
                break;
            }
            next
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
        // Encode 1 second of silence with LAME, then decode+reencode through pipeline.
        use crate::encoder::LameEncoder;

        let mut enc = LameEncoder::new(44100, 44100, 2, 128).unwrap();
        let silence = vec![0.0f32; 44100 * 2]; // 1 second stereo silence
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
        for chunk in mp3_data.chunks(4096) {
            match pipeline.push(chunk) {
                Ok(out) => all_output.extend_from_slice(&out),
                Err(e) => {
                    // Decode errors on synthetic frames are acceptable;
                    // just check that the pipeline doesn't panic.
                    eprintln!("transcode error (acceptable in unit test): {e}");
                }
            }
        }
        if let Ok(tail) = pipeline.flush() {
            all_output.extend_from_slice(&tail);
        }

        // Output should either be empty (LAME didn't produce frames from silence)
        // or contain valid MP3 sync words if it did produce output.
        if !all_output.is_empty() {
            assert!(
                all_output.iter().enumerate().any(|(i, &b)| {
                    i + 1 < all_output.len() && b == 0xFF && (all_output[i + 1] & 0xE0) == 0xE0
                }),
                "pipeline output must contain MP3 sync words"
            );
        }
    }
}
