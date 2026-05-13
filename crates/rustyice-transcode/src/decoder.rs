use std::collections::VecDeque;
use std::io::{self, Read, Seek, SeekFrom};
use std::sync::{Arc, Mutex};
use symphonia::core::{
    audio::SampleBuffer,
    codecs::DecoderOptions,
    errors::Error as SymphoniaError,
    formats::FormatOptions,
    io::MediaSourceStream,
    meta::MetadataOptions,
    probe::Hint,
};
use crate::TranscodeError;

/// Minimum bytes to accumulate before attempting format probe.
/// Keeping this at 2× the typical chunk size (4 096 B) so symphonia has at
/// least two chunks of source data in its MSS buffer before the first decode,
/// reducing the chance of a WouldBlock mid-frame on the very first pass.
const INIT_THRESHOLD: usize = 8_192;

struct PipeBuf {
    data: VecDeque<u8>,
    eof: bool,
}

struct PipeSource(Arc<Mutex<PipeBuf>>);

impl Read for PipeSource {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        let mut inner = self.0.lock().unwrap();
        let n = inner.data.len().min(out.len());
        if n == 0 {
            return if inner.eof {
                Ok(0) // real EOF
            } else {
                Err(io::Error::new(io::ErrorKind::WouldBlock, "no data"))
            };
        }
        for (dst, src) in out[..n].iter_mut().zip(inner.data.drain(..n)) {
            *dst = src;
        }
        Ok(n)
    }
}

impl Seek for PipeSource {
    fn seek(&mut self, _: SeekFrom) -> io::Result<u64> {
        Err(io::Error::new(io::ErrorKind::Unsupported, "not seekable"))
    }
}

impl symphonia::core::io::MediaSource for PipeSource {
    fn is_seekable(&self) -> bool { false }
    fn byte_len(&self) -> Option<u64> { None }
}

struct DecoderInner {
    format: Box<dyn symphonia::core::formats::FormatReader>,
    decoder: Box<dyn symphonia::core::codecs::Decoder>,
    track_id: u32,
    sample_rate: u32,
    channels: u8,
}

/// A persistent MP3 decoder. Feed bytes in with `push()`; the internal symphonia
/// decoder is kept alive across calls so the bit reservoir is never lost.
pub struct StreamDecoder {
    buf: Arc<Mutex<PipeBuf>>,
    /// Accumulates bytes before we have enough data to probe the format.
    pending: Vec<u8>,
    inner: Option<DecoderInner>,
}

impl StreamDecoder {
    pub fn new() -> Self {
        Self {
            buf: Arc::new(Mutex::new(PipeBuf { data: VecDeque::new(), eof: false })),
            pending: Vec::new(),
            inner: None,
        }
    }

    /// Push raw bytes from the source. Returns (interleaved_f32, sample_rate, channels).
    /// Returns empty samples if not enough data has been received to probe yet.
    pub fn push(&mut self, data: &[u8]) -> Result<(Vec<f32>, u32, u8), TranscodeError> {
        if data.is_empty() {
            return Ok((vec![], 0, 0));
        }

        if self.inner.is_none() {
            self.pending.extend_from_slice(data);
            if self.pending.len() < INIT_THRESHOLD {
                return Ok((vec![], 0, 0));
            }
            // Drain pending into the shared pipe buffer, then attempt to probe.
            let pending = std::mem::take(&mut self.pending);
            self.buf.lock().unwrap().data.extend(pending);
            if let Err(e) = self.try_init() {
                // Bad data — clear the buffer and wait for valid MP3.
                tracing::debug!("decoder init failed, discarding buffer: {e}");
                self.buf.lock().unwrap().data.clear();
                return Ok((vec![], 0, 0));
            }
            // try_init() ran a warmup pass that consumed the init buffer.
            // Return empty until fresh data arrives on the next push.
            return Ok((vec![], 0, 0));
        } else {
            self.buf.lock().unwrap().data.extend(data.iter().copied());
        }

        self.drain_packets()
    }

    /// Signal end of stream. Flushes any remaining pending data and decodes what's left.
    pub fn flush_eof(&mut self) -> Result<(Vec<f32>, u32, u8), TranscodeError> {
        if self.inner.is_none() && !self.pending.is_empty() {
            let pending = std::mem::take(&mut self.pending);
            self.buf.lock().unwrap().data.extend(pending);
            // Best-effort init with whatever data we have.
            let _ = self.try_init();
        }
        // Tell PipeSource to return 0 (EOF) instead of WouldBlock when empty.
        self.buf.lock().unwrap().eof = true;

        if self.inner.is_none() {
            return Ok((vec![], 0, 0));
        }
        self.drain_packets()
    }

    fn try_init(&mut self) -> Result<(), TranscodeError> {
        let pipe = PipeSource(Arc::clone(&self.buf));
        let mss = MediaSourceStream::new(Box::new(pipe), Default::default());
        let mut hint = Hint::new();
        hint.mime_type("audio/mpeg");

        let probed = symphonia::default::get_probe()
            .format(&hint, mss, &FormatOptions::default(), &MetadataOptions::default())
            .map_err(|e| TranscodeError::DecoderInit(e.to_string()))?;

        let format = probed.format;
        let track = format
            .default_track()
            .ok_or_else(|| TranscodeError::DecoderInit("no default track".into()))?;
        let track_id = track.id;
        let sample_rate = track.codec_params.sample_rate.unwrap_or(44100);
        let channels = track.codec_params.channels.map(|c| c.count() as u8).unwrap_or(2);

        let decoder = symphonia::default::get_codecs()
            .make(&track.codec_params, &DecoderOptions::default())
            .map_err(|e| TranscodeError::DecoderInit(e.to_string()))?;

        self.inner = Some(DecoderInner { format, decoder, track_id, sample_rate, channels });
        // Warmup: decode and discard the init buffer so the bit reservoir fills.
        // Frames decoded here start mid-stream and contain artifacts; discarding
        // them hides the startup glitch from the output.
        let _ = self.drain_packets();
        Ok(())
    }

    fn drain_packets(&mut self) -> Result<(Vec<f32>, u32, u8), TranscodeError> {
        let inner = self.inner.as_mut().unwrap();
        let mut samples: Vec<f32> = Vec::new();
        let mut sample_rate = inner.sample_rate;
        let mut channels = inner.channels;

        loop {
            let packet = match inner.format.next_packet() {
                Ok(p) => p,
                Err(SymphoniaError::IoError(e)) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(SymphoniaError::IoError(_)) => break, // EOF or read error
                Err(SymphoniaError::ResetRequired) => { inner.decoder.reset(); continue; }
                Err(SymphoniaError::DecodeError(_)) => continue,
                Err(e) => { tracing::warn!("symphonia: {e}"); break; }
            };

            if packet.track_id() != inner.track_id {
                continue;
            }

            match inner.decoder.decode(&packet) {
                Ok(audio_buf) => {
                    let spec = *audio_buf.spec();
                    sample_rate = spec.rate;
                    channels = spec.channels.count() as u8;
                    inner.sample_rate = sample_rate;
                    inner.channels = channels;
                    let mut sbuf = SampleBuffer::<f32>::new(audio_buf.capacity() as u64, spec);
                    sbuf.copy_interleaved_ref(audio_buf);
                    samples.extend_from_slice(sbuf.samples());
                }
                Err(SymphoniaError::IoError(_)) => break,
                Err(SymphoniaError::DecodeError(_)) => continue,
                Err(e) => { tracing::warn!("symphonia decode: {e}"); continue; }
            }
        }

        Ok((samples, sample_rate, channels))
    }
}
