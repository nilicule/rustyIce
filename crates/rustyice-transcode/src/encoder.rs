use crate::TranscodeError;

pub struct LameEncoder {
    gfp: *mut mp3lame_sys::lame_global_flags,
    channels: u8,
    chunk_counter: u64,
}

// SAFETY: lame_global_flags is not thread-safe by itself; we never share it.
unsafe impl Send for LameEncoder {}

impl LameEncoder {
    pub fn new(
        in_sample_rate: u32,
        out_sample_rate: u32,
        channels: u8,
        bitrate_kbps: u32,
    ) -> Result<Self, TranscodeError> {
        unsafe {
            let gfp = mp3lame_sys::lame_init();
            if gfp.is_null() {
                return Err(TranscodeError::EncoderInit("lame_init returned null".into()));
            }
            mp3lame_sys::lame_set_in_samplerate(gfp, in_sample_rate as i32);
            mp3lame_sys::lame_set_out_samplerate(gfp, out_sample_rate as i32);
            mp3lame_sys::lame_set_num_channels(gfp, channels as i32);
            mp3lame_sys::lame_set_brate(gfp, bitrate_kbps as i32);
            mp3lame_sys::lame_set_quality(gfp, 5);
            let ret = mp3lame_sys::lame_init_params(gfp);
            if ret < 0 {
                mp3lame_sys::lame_close(gfp);
                return Err(TranscodeError::EncoderInit(format!(
                    "lame_init_params returned {ret}"
                )));
            }
            Ok(Self { gfp, channels, chunk_counter: 0 })
        }
    }

    /// Encode interleaved f32 PCM samples. Returns MP3 bytes (may be empty — LAME buffers internally).
    pub fn encode(&mut self, samples: &[f32]) -> Result<Vec<u8>, TranscodeError> {
        if samples.is_empty() {
            return Ok(vec![]);
        }
        let num_samples_per_channel = samples.len() / self.channels as usize;
        // LAME output buffer: 1.25 * num_samples + 7200 bytes per channel
        let buf_size = (num_samples_per_channel * 5 / 4) + 7200;
        let mut output = vec![0u8; buf_size];

        let written = unsafe {
            if self.channels == 2 {
                // Deinterleave
                let left: Vec<f32> = samples.iter().step_by(2).copied().collect();
                let right: Vec<f32> = samples.iter().skip(1).step_by(2).copied().collect();

                let n = left.len();
                let head: [f32; 4] = std::array::from_fn(|i| *left.get(i).unwrap_or(&0.0));
                let tail: [f32; 4] = std::array::from_fn(|i| {
                    *left.get(n.saturating_sub(4) + i).unwrap_or(&0.0)
                });
                tracing::debug!(
                    chunk = self.chunk_counter,
                    total_samples = samples.len(),
                    samples_per_ch = n,
                    head_L = ?head,
                    tail_L = ?tail,
                    "lame encode chunk"
                );
                self.chunk_counter += 1;

                mp3lame_sys::lame_encode_buffer_ieee_float(
                    self.gfp,
                    left.as_ptr(),
                    right.as_ptr(),
                    num_samples_per_channel as i32,
                    output.as_mut_ptr(),
                    buf_size as i32,
                )
            } else {
                // Mono: use same pointer for both channels
                mp3lame_sys::lame_encode_buffer_ieee_float(
                    self.gfp,
                    samples.as_ptr(),
                    samples.as_ptr(),
                    num_samples_per_channel as i32,
                    output.as_mut_ptr(),
                    buf_size as i32,
                )
            }
        };
        if written < 0 {
            return Err(TranscodeError::Encode(format!(
                "lame encode returned {written}"
            )));
        }
        output.truncate(written as usize);
        Ok(output)
    }

    /// Flush LAME's internal buffers. Call when the source stream ends.
    pub fn flush(&mut self) -> Result<Vec<u8>, TranscodeError> {
        let mut output = vec![0u8; 7200];
        let written =
            unsafe { mp3lame_sys::lame_encode_flush(self.gfp, output.as_mut_ptr(), 7200) };
        if written < 0 {
            return Err(TranscodeError::Encode(format!(
                "lame flush returned {written}"
            )));
        }
        output.truncate(written as usize);
        Ok(output)
    }
}

impl Drop for LameEncoder {
    fn drop(&mut self) {
        unsafe {
            mp3lame_sys::lame_close(self.gfp);
        }
    }
}
