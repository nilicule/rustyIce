use std::fs::File;
use std::path::Path;
use symphonia::core::audio::AudioBufferRef;
use symphonia::core::codecs::DecoderOptions;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

/// One decoded frame of interleaved f32 PCM samples, plus its rate / channel
/// count. Channels in `[-1.0, 1.0]`.
#[derive(Debug, Clone)]
pub struct PcmChunk {
    pub samples: Vec<f32>,
    pub sample_rate: u32,
    pub channels: u8,
}

/// Streaming decoder over a single file. Iterate `next()` until `Ok(None)`.
pub struct FileDecoder {
    format: Box<dyn symphonia::core::formats::FormatReader>,
    decoder: Box<dyn symphonia::core::codecs::Decoder>,
    track_id: u32,
    sample_rate: u32,
    channels: u8,
}

impl FileDecoder {
    /// Open `path` and prepare its first audio track for streaming decode.
    ///
    /// # Errors
    /// Returns an error if the file cannot be opened, the format probed, or
    /// no decodable audio track is found.
    pub fn open(path: &Path) -> Result<Self, symphonia::core::errors::Error> {
        let file = File::open(path).map_err(symphonia::core::errors::Error::IoError)?;
        let mss = MediaSourceStream::new(Box::new(file), Default::default());
        let mut hint = Hint::new();
        if let Some(ext) = path.extension().and_then(|s| s.to_str()) {
            hint.with_extension(ext);
        }
        let probed = symphonia::default::get_probe().format(
            &hint,
            mss,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )?;
        let format = probed.format;
        let track = format
            .default_track()
            .ok_or_else(|| symphonia::core::errors::Error::DecodeError("no default track"))?;
        let codec_params = track.codec_params.clone();
        let decoder =
            symphonia::default::get_codecs().make(&codec_params, &DecoderOptions::default())?;
        let track_id = track.id;
        let sample_rate = codec_params
            .sample_rate
            .ok_or_else(|| symphonia::core::errors::Error::DecodeError("missing sample_rate"))?;
        let channels = codec_params
            .channels
            .map(|c| c.count() as u8)
            .ok_or_else(|| symphonia::core::errors::Error::DecodeError("missing channels"))?;
        Ok(Self {
            format,
            decoder,
            track_id,
            sample_rate,
            channels,
        })
    }

    #[must_use]
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    #[must_use]
    pub fn channels(&self) -> u8 {
        self.channels
    }

    /// Return the next decoded chunk, or `Ok(None)` at end-of-file.
    ///
    /// # Errors
    /// Decode errors bubble up. Recoverable decode errors (`DecodeError`,
    /// `IoError` with `UnexpectedEof`) return `Ok(None)` to allow the caller
    /// to advance to the next track.
    pub fn next(&mut self) -> Result<Option<PcmChunk>, symphonia::core::errors::Error> {
        loop {
            let packet = match self.format.next_packet() {
                Ok(p) => p,
                Err(symphonia::core::errors::Error::IoError(e))
                    if e.kind() == std::io::ErrorKind::UnexpectedEof =>
                {
                    return Ok(None)
                }
                Err(symphonia::core::errors::Error::ResetRequired) => return Ok(None),
                Err(e) => return Err(e),
            };
            if packet.track_id() != self.track_id {
                continue;
            }
            match self.decoder.decode(&packet) {
                Ok(buf) => {
                    let chunk = audio_buffer_to_pcm(buf);
                    if chunk.samples.is_empty() {
                        continue;
                    }
                    return Ok(Some(chunk));
                }
                Err(symphonia::core::errors::Error::DecodeError(_)) => continue,
                Err(e) => return Err(e),
            }
        }
    }
}

fn audio_buffer_to_pcm(buf: AudioBufferRef<'_>) -> PcmChunk {
    let spec = *buf.spec();
    let sample_rate = spec.rate;
    let channels = spec.channels.count() as u8;
    // Use capacity (not frames) so the SampleBuffer is large enough to hold the
    // data even when symphonia pre-allocates more than the current frame count.
    let capacity = buf.capacity();
    let mut tmp =
        symphonia::core::audio::SampleBuffer::<f32>::new(capacity as u64, spec);
    tmp.copy_interleaved_ref(buf);
    PcmChunk {
        samples: tmp.samples().to_vec(),
        sample_rate,
        channels,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opens_and_decodes_mp3_fixture() {
        let dir = tempfile::tempdir().unwrap();
        let (mp3, _) = crate::test_fixtures::write_test_fixtures(dir.path()).unwrap();
        let mut d = FileDecoder::open(&mp3).unwrap();
        assert_eq!(d.channels(), 2);
        assert_eq!(d.sample_rate(), 44_100);
        let mut total = 0usize;
        while let Some(chunk) = d.next().unwrap() {
            assert_eq!(chunk.channels, 2);
            assert_eq!(chunk.sample_rate, 44_100);
            total += chunk.samples.len();
        }
        // 1 second stereo at 44100 Hz ≈ 88_200 samples (LAME pads slightly).
        assert!(total >= 80_000, "too few samples decoded: {total}");
    }

    #[test]
    fn opens_and_decodes_ogg_fixture() {
        let dir = tempfile::tempdir().unwrap();
        let (_, ogg) = crate::test_fixtures::write_test_fixtures(dir.path()).unwrap();
        let mut d = FileDecoder::open(&ogg).unwrap();
        assert_eq!(d.channels(), 2);
        let mut total = 0usize;
        while let Some(chunk) = d.next().unwrap() {
            total += chunk.samples.len();
        }
        assert!(total >= 80_000, "too few samples decoded: {total}");
    }
}
