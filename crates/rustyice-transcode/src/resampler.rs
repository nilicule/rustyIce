use crate::TranscodeError;
use rubato::{FftFixedInOut, Resampler};

pub struct PcmResampler {
    inner: FftFixedInOut<f32>,
    channels: usize,
    leftover: Vec<Vec<f32>>,
}

impl PcmResampler {
    pub fn new(from_rate: u32, to_rate: u32, channels: usize) -> Result<Self, TranscodeError> {
        let inner = FftFixedInOut::<f32>::new(
            from_rate as usize,
            to_rate as usize,
            1024,
            channels,
        )
        .map_err(|e| TranscodeError::Resample(e.to_string()))?;
        let leftover = vec![Vec::new(); channels];
        Ok(Self { inner, channels, leftover })
    }

    /// Resample interleaved f32 samples. Returns resampled interleaved f32 samples.
    /// Leftover samples (less than one chunk) are carried to the next call.
    pub fn process(&mut self, samples: &[f32]) -> Result<Vec<f32>, TranscodeError> {
        if samples.is_empty() {
            return Ok(vec![]);
        }

        // Deinterleave into per-channel buffers, appending to any leftover
        for (ch, buf) in self.leftover.iter_mut().enumerate() {
            buf.extend(samples.iter().skip(ch).step_by(self.channels).copied());
        }

        let chunk_size = self.inner.input_frames_next();
        let mut output_interleaved = Vec::new();

        while self.leftover[0].len() >= chunk_size {
            let chunk_in: Vec<&[f32]> = self.leftover
                .iter()
                .map(|ch| &ch[..chunk_size])
                .collect();

            let chunk_out = self
                .inner
                .process(&chunk_in, None)
                .map_err(|e| TranscodeError::Resample(e.to_string()))?;

            let out_len = chunk_out[0].len();
            for i in 0..out_len {
                for ch in &chunk_out {
                    output_interleaved.push(ch[i]);
                }
            }

            for ch in self.leftover.iter_mut() {
                ch.drain(..chunk_size);
            }
        }

        Ok(output_interleaved)
    }

    /// Flush rubato's internal FIR delay at end of stream.
    pub fn flush(&mut self) -> Result<Vec<f32>, TranscodeError> {
        let mut output_interleaved = Vec::new();

        // Process any sub-chunk leftover samples first
        if !self.leftover.is_empty() && !self.leftover[0].is_empty() {
            let partial: Vec<Vec<f32>> = self.leftover.to_vec();
            let chunk_out = self
                .inner
                .process_partial(Some(partial.as_slice()), None)
                .map_err(|e| TranscodeError::Resample(e.to_string()))?;
            let out_len = chunk_out[0].len();
            for i in 0..out_len {
                for ch in &chunk_out {
                    output_interleaved.push(ch[i]);
                }
            }
            for ch in self.leftover.iter_mut() {
                ch.clear();
            }
        }

        // Drain FIR delay
        let chunk_out = self
            .inner
            .process_partial(None::<&[Vec<f32>]>, None)
            .map_err(|e| TranscodeError::Resample(e.to_string()))?;
        if !chunk_out.is_empty() {
            let out_len = chunk_out[0].len();
            for i in 0..out_len {
                for ch in &chunk_out {
                    output_interleaved.push(ch[i]);
                }
            }
        }

        Ok(output_interleaved)
    }
}
