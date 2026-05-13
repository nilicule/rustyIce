use crate::TranscodeError;
use rubato::{FftFixedInOut, Resampler};

pub struct PcmResampler {
    inner: FftFixedInOut<f32>,
    channels: usize,
}

impl PcmResampler {
    pub fn new(from_rate: u32, to_rate: u32, channels: usize) -> Result<Self, TranscodeError> {
        let inner = FftFixedInOut::<f32>::new(
            from_rate as usize,
            to_rate as usize,
            1024, // chunk size
            channels,
        )
        .map_err(|e| TranscodeError::Resample(e.to_string()))?;
        Ok(Self { inner, channels })
    }

    /// Resample interleaved f32 samples. Returns resampled interleaved f32 samples.
    pub fn process(&mut self, samples: &[f32]) -> Result<Vec<f32>, TranscodeError> {
        if samples.is_empty() {
            return Ok(vec![]);
        }

        // Split interleaved into per-channel vecs
        let mut channels_in: Vec<Vec<f32>> = (0..self.channels)
            .map(|ch| {
                samples
                    .iter()
                    .skip(ch)
                    .step_by(self.channels)
                    .copied()
                    .collect()
            })
            .collect();

        let chunk_size = self.inner.input_frames_next();
        let mut output_interleaved = Vec::new();

        // Process in fixed-size chunks
        while channels_in[0].len() >= chunk_size {
            let chunk_in: Vec<Vec<f32>> = channels_in
                .iter()
                .map(|ch| ch[..chunk_size].to_vec())
                .collect();

            let chunk_out = self
                .inner
                .process(&chunk_in, None)
                .map_err(|e| TranscodeError::Resample(e.to_string()))?;

            // Interleave output channels
            let out_len = chunk_out[0].len();
            for i in 0..out_len {
                for ch in &chunk_out {
                    output_interleaved.push(ch[i]);
                }
            }

            // Drain processed samples
            for ch in channels_in.iter_mut() {
                ch.drain(..chunk_size);
            }
        }

        Ok(output_interleaved)
    }
}
