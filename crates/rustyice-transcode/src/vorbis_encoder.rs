use std::io::Write;
use std::num::{NonZeroU32, NonZeroU8};
use std::sync::{Arc, Mutex};

use vorbis_rs::{VorbisBitrateManagementStrategy, VorbisEncoderBuilder};

use crate::TranscodeError;

/// `vorbis_rs` consumes a `W: Write` sink and writes Ogg pages to it as they
/// are produced. We hand it this `Arc<Mutex<Vec<u8>>>` wrapper so we can drain
/// pages after each `encode_audio_block` call without giving up ownership.
struct SharedSink(Arc<Mutex<Vec<u8>>>);

impl Write for SharedSink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

pub struct VorbisEncoder {
    /// `Option` because `vorbis_rs::VorbisEncoder::finish` takes `self`.  On
    /// `flush()` we take the encoder out and finalise it, emitting the EOS page
    /// into `sink`.
    inner: Option<vorbis_rs::VorbisEncoder<SharedSink>>,
    sink: Arc<Mutex<Vec<u8>>>,
    channels: u8,
}

// SAFETY: `vorbis_rs::VorbisEncoder` wraps libvorbis FFI state via raw pointers
// and is not `Send` automatically. The contract is the same as `LameEncoder`:
// only one owner at a time accesses the encoder (through `&mut self`), and
// libvorbis itself has no thread-local state, so moving between tokio worker
// threads is safe as long as we never share concurrently.
unsafe impl Send for VorbisEncoder {}

impl VorbisEncoder {
    pub fn new(
        sample_rate: u32,
        channels: u8,
        bitrate_kbps: u32,
        comments: &[(String, String)],
    ) -> Result<Self, TranscodeError> {
        let nz_sample_rate = NonZeroU32::new(sample_rate)
            .ok_or_else(|| TranscodeError::EncoderInit("sample_rate must be non-zero".into()))?;
        let nz_channels = NonZeroU8::new(channels)
            .ok_or_else(|| TranscodeError::EncoderInit("channels must be non-zero".into()))?;
        let nz_bitrate = NonZeroU32::new(bitrate_kbps.saturating_mul(1000))
            .ok_or_else(|| TranscodeError::EncoderInit("bitrate must be non-zero".into()))?;

        let sink = Arc::new(Mutex::new(Vec::new()));
        let mut builder =
            VorbisEncoderBuilder::new(nz_sample_rate, nz_channels, SharedSink(sink.clone()))
                .map_err(|e| TranscodeError::EncoderInit(format!("vorbis builder: {e}")))?;
        builder.bitrate_management_strategy(VorbisBitrateManagementStrategy::Abr {
            average_bitrate: nz_bitrate,
        });
        if !comments.is_empty() {
            builder
                .comment_tags(comments.iter().map(|(k, v)| (k.as_str(), v.as_str())))
                .map_err(|e| TranscodeError::EncoderInit(format!("vorbis comments: {e}")))?;
        }
        let encoder = builder
            .build()
            .map_err(|e| TranscodeError::EncoderInit(format!("vorbis build: {e}")))?;

        Ok(Self {
            inner: Some(encoder),
            sink,
            channels,
        })
    }

    /// Encode interleaved f32 PCM samples. Returns any Ogg bytes the encoder
    /// emitted, which on the first call includes the three header pages.
    pub fn encode(&mut self, samples: &[f32]) -> Result<Vec<u8>, TranscodeError> {
        if !samples.is_empty() {
            let enc = self.inner.as_mut().ok_or_else(|| {
                TranscodeError::Encode("vorbis encoder already finished".into())
            })?;
            let channels = self.channels as usize;
            if channels == 0 || samples.len() % channels != 0 {
                return Err(TranscodeError::Encode(format!(
                    "interleaved samples len {} not divisible by channels {}",
                    samples.len(),
                    channels,
                )));
            }
            let frames = samples.len() / channels;
            let mut planes: Vec<Vec<f32>> =
                (0..channels).map(|_| Vec::with_capacity(frames)).collect();
            for frame in samples.chunks_exact(channels) {
                for (c, &sample) in frame.iter().enumerate() {
                    planes[c].push(sample);
                }
            }
            enc.encode_audio_block(&planes)
                .map_err(|e| TranscodeError::Encode(format!("vorbis encode: {e}")))?;
        }
        Ok(self.drain())
    }

    /// Finalise the stream: emits the Vorbis EOS page. Subsequent `encode`
    /// calls fail.
    pub fn flush(&mut self) -> Result<Vec<u8>, TranscodeError> {
        if let Some(enc) = self.inner.take() {
            enc.finish()
                .map_err(|e| TranscodeError::Encode(format!("vorbis finish: {e}")))?;
        }
        Ok(self.drain())
    }

    fn drain(&self) -> Vec<u8> {
        std::mem::take(&mut *self.sink.lock().unwrap())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emits_all_three_header_packets_on_construction() {
        let enc = VorbisEncoder::new(44_100, 2, 96, &[]).unwrap();
        let head = enc.drain();
        // First page must be a BOS containing the identification packet alone.
        assert_eq!(&head[..4], b"OggS");
        assert_eq!(head[5] & 0x02, 0x02, "first page must be BOS");
        // libvorbis packs the comment + setup packets together on the second
        // page, so we expect at least 2 OggS pages — but all three header
        // packets must be visible in the payload.
        for marker in [b"\x01vorbis", b"\x03vorbis", b"\x05vorbis"] {
            assert!(
                head.windows(marker.len()).any(|w| w == marker),
                "expected header marker {marker:?} in output",
            );
        }
    }

    #[test]
    fn comments_appear_in_output() {
        let enc = VorbisEncoder::new(
            44_100,
            2,
            96,
            &[("TITLE".to_string(), "Hello".to_string())],
        )
        .unwrap();
        let head = enc.drain();
        // Plaintext Vorbis comment payload appears verbatim in the comment header.
        let needle = b"TITLE=Hello";
        assert!(
            head.windows(needle.len()).any(|w| w == needle),
            "comment header should embed `TITLE=Hello`",
        );
    }

    #[test]
    fn encodes_silence_into_audio_pages() {
        let mut enc = VorbisEncoder::new(44_100, 2, 96, &[]).unwrap();
        // Drain the headers produced at construction so the comparison below
        // measures audio-only output.
        let header_bytes = enc.drain();
        assert!(!header_bytes.is_empty(), "headers should have been produced");

        // 1 second of stereo silence is plenty for libvorbis to emit a full
        // audio page once `flush` finalises the stream.
        let silence = vec![0.0_f32; 44_100 * 2];
        let mut out = enc.encode(&silence).unwrap();
        out.extend_from_slice(&enc.flush().unwrap());
        let pages = out.windows(4).filter(|w| *w == b"OggS").count();
        assert!(pages >= 1, "expected at least one audio page after silence + flush, got {pages}");
        // EOS bit on the final page must be set.
        let last_ogg = out.windows(4).rposition(|w| w == b"OggS").unwrap();
        assert_eq!(out[last_ogg + 5] & 0x04, 0x04, "final page must have EOS bit set");
    }

    #[test]
    fn rejects_misaligned_interleaved_buffer() {
        let mut enc = VorbisEncoder::new(44_100, 2, 96, &[]).unwrap();
        // 3 samples for stereo (channels=2) is not divisible — error.
        let err = enc.encode(&[0.0, 0.0, 0.0]).unwrap_err();
        assert!(format!("{err}").contains("not divisible"));
    }

    #[test]
    fn flush_is_idempotent() {
        let mut enc = VorbisEncoder::new(44_100, 2, 96, &[]).unwrap();
        let _ = enc.flush().unwrap();
        // Second flush should not panic and should return empty bytes (sink already drained).
        let tail = enc.flush().unwrap();
        assert!(tail.is_empty());
    }
}
