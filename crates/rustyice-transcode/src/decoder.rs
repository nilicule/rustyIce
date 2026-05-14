use std::collections::VecDeque;
use std::io::{self, Read, Seek, SeekFrom};
use std::sync::{Arc, Mutex};
use rustyice_core::types::CodecId;
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

/// Minimum bytes to accumulate before attempting format probe. Sized so
/// Symphonia's OggReader probe (which reads the full Vorbis header chain plus
/// at least one audio packet to verify the codec) can complete in a single
/// pass against the buffered bytes; below this threshold the probe runs out
/// of data, errors, and destructively consumes the BOS page — leaving us
/// unable to retry. For live sources this adds ~0.5 s of warmup at 1 Mbps.
const INIT_THRESHOLD: usize = 65_536;

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

pub struct StreamDecoder {
    buf: Arc<Mutex<PipeBuf>>,
    /// Source codec.  Selects the Symphonia format hint and how `frame_staging`
    /// finds complete-unit boundaries: MP3 frames vs Ogg pages.
    source_codec: CodecId,
    /// Accumulates bytes before we have enough data to probe the format.
    pending: Vec<u8>,
    /// Accumulates post-init bytes until at least one complete frame (MP3) or
    /// page (Ogg) is available.  Only complete units are forwarded to `buf` so
    /// that Symphonia's format reader never receives a `WouldBlock` mid-unit.
    /// A mid-frame WouldBlock causes the MpegReader to resync from an arbitrary
    /// offset, skipping frames and producing pervasive audio corruption.
    frame_staging: Vec<u8>,
    inner: Option<DecoderInner>,
}

impl StreamDecoder {
    pub fn new(source_codec: CodecId) -> Self {
        Self {
            buf: Arc::new(Mutex::new(PipeBuf { data: VecDeque::new(), eof: false })),
            source_codec,
            pending: Vec::new(),
            frame_staging: Vec::new(),
            inner: None,
        }
    }

    /// Push raw bytes from the source. Returns (interleaved_f32, sample_rate, channels).
    /// Returns empty samples if not enough data has been received to decode yet.
    pub fn push(&mut self, data: &[u8]) -> Result<(Vec<f32>, u32, u8), TranscodeError> {
        if data.is_empty() {
            return Ok((vec![], 0, 0));
        }

        if self.inner.is_none() {
            self.pending.extend_from_slice(data);
            if self.pending.len() < INIT_THRESHOLD {
                return Ok((vec![], 0, 0));
            }
            let pending = std::mem::take(&mut self.pending);
            self.buf.lock().unwrap().data.extend(pending);
            if let Err(e) = self.try_init() {
                tracing::debug!("decoder init failed, discarding buffer: {e}");
                self.buf.lock().unwrap().data.clear();
                return Ok((vec![], 0, 0));
            }
            // try_init() ran a warmup pass that consumed the init buffer.
            // Return empty until fresh data arrives on the next push.
            return Ok((vec![], 0, 0));
        } else {
            self.frame_staging.extend_from_slice(data);
            self.stage_complete_frames_to_pipe();
        }

        self.drain_packets()
    }

    /// Signal end of stream. Flushes any remaining pending data and decodes what's left.
    pub fn flush_eof(&mut self) -> Result<(Vec<f32>, u32, u8), TranscodeError> {
        if self.inner.is_none() && !self.pending.is_empty() {
            let pending = std::mem::take(&mut self.pending);
            self.buf.lock().unwrap().data.extend(pending);
            let _ = self.try_init();
        }

        // Flush any remaining staged bytes unconditionally — partial frames are
        // acceptable at end of stream and Symphonia handles them gracefully with
        // eof=true (it will stop at the last decodable frame).
        if !self.frame_staging.is_empty() {
            let staged = std::mem::take(&mut self.frame_staging);
            self.buf.lock().unwrap().data.extend(staged);
        }

        self.buf.lock().unwrap().eof = true;

        if self.inner.is_none() {
            return Ok((vec![], 0, 0));
        }
        self.drain_packets()
    }

    /// Scan `frame_staging` for complete units (MP3 frames or Ogg pages
    /// depending on `source_codec`) and move all bytes up to (and including)
    /// the last complete unit into `buf`. Any trailing partial unit is left in
    /// `frame_staging` for the next push.
    ///
    /// Any bytes before the first detected boundary are included in the flush —
    /// they are the continuation of a unit that was partially present in the
    /// init buffer and must reach Symphonia in order.
    fn stage_complete_frames_to_pipe(&mut self) {
        match self.source_codec {
            CodecId::VORBIS => self.stage_complete_ogg_pages(),
            // Default to MP3-style sync-word scanning for everything else.
            // Only MP3 and Vorbis are supported source codecs for transcoding;
            // anything else would have been rejected upstream.
            _ => self.stage_complete_mp3_frames(),
        }
    }

    fn stage_complete_mp3_frames(&mut self) {
        let data = &self.frame_staging;
        let mut pos = 0usize;
        let mut last_complete_end = 0usize;

        while pos + 4 <= data.len() {
            if data[pos] != 0xFF || (data[pos + 1] & 0xE0) != 0xE0 {
                pos += 1;
                continue;
            }
            match rustyice_codec::mp3_frame_size(&data[pos..]) {
                Some(size) if pos + size <= data.len() => {
                    last_complete_end = pos + size;
                    pos = last_complete_end;
                }
                Some(_) => break, // frame starts here but is incomplete; wait for more data
                None => { pos += 1; } // false sync word; advance
            }
        }

        if last_complete_end > 0 {
            let frames: Vec<u8> = self.frame_staging.drain(..last_complete_end).collect();
            self.buf.lock().unwrap().data.extend(frames);
        }
    }

    fn stage_complete_ogg_pages(&mut self) {
        let data = &self.frame_staging;
        let mut pos = 0usize;
        let mut last_complete_end = 0usize;

        while pos + 27 <= data.len() {
            if &data[pos..pos + 4] != b"OggS" {
                pos += 1;
                continue;
            }
            match rustyice_codec::ogg_page_size(&data[pos..]) {
                Some(size) if pos + size <= data.len() => {
                    last_complete_end = pos + size;
                    pos = last_complete_end;
                }
                Some(_) => break, // page starts here but is incomplete; wait for more data
                None => { pos += 1; } // false magic; advance
            }
        }

        if last_complete_end > 0 {
            let frames: Vec<u8> = self.frame_staging.drain(..last_complete_end).collect();
            self.buf.lock().unwrap().data.extend(frames);
        }
    }

    fn try_init(&mut self) -> Result<(), TranscodeError> {
        let pipe = PipeSource(Arc::clone(&self.buf));
        let mss = MediaSourceStream::new(Box::new(pipe), Default::default());
        let mut hint = Hint::new();
        hint.mime_type(mime_for(&self.source_codec));

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

fn mime_for(codec: &CodecId) -> &'static str {
    match *codec {
        CodecId::VORBIS => "audio/ogg",
        _ => "audio/mpeg",
    }
}
