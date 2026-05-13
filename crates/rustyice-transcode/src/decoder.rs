use crate::TranscodeError;
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::DecoderOptions;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

/// Decode a buffer containing one or more complete MP3 frames.
/// Returns (interleaved_f32_samples, sample_rate, channels).
pub fn decode_mp3_frames(data: &[u8]) -> Result<(Vec<f32>, u32, u8), TranscodeError> {
    if data.is_empty() {
        return Ok((vec![], 0, 0));
    }

    let cursor = std::io::Cursor::new(data.to_vec());
    let mss = MediaSourceStream::new(Box::new(cursor), Default::default());

    let mut hint = Hint::new();
    hint.mime_type("audio/mpeg");

    let probed = symphonia::default::get_probe()
        .format(
            &hint,
            mss,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )
        .map_err(|e| TranscodeError::Decode(e.to_string()))?;

    let mut format = probed.format;
    let track = format
        .default_track()
        .ok_or_else(|| TranscodeError::Decode("no track found".into()))?;

    let track_id = track.id;
    let mut decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())
        .map_err(|e| TranscodeError::DecoderInit(e.to_string()))?;

    let mut all_samples: Vec<f32> = Vec::new();
    let mut sample_rate = 0u32;
    let mut channels = 0u8;

    loop {
        let packet = match format.next_packet() {
            Ok(p) => p,
            Err(symphonia::core::errors::Error::IoError(_)) => break,
            Err(symphonia::core::errors::Error::ResetRequired) => {
                decoder.reset();
                continue;
            }
            Err(e) => return Err(TranscodeError::Decode(e.to_string())),
        };

        if packet.track_id() != track_id {
            continue;
        }

        match decoder.decode(&packet) {
            Ok(audio_buf) => {
                let spec = *audio_buf.spec();
                sample_rate = spec.rate;
                channels = spec.channels.count() as u8;

                let mut sample_buf =
                    SampleBuffer::<f32>::new(audio_buf.capacity() as u64, spec);
                sample_buf.copy_interleaved_ref(audio_buf);
                all_samples.extend_from_slice(sample_buf.samples());
            }
            Err(symphonia::core::errors::Error::IoError(_)) => break,
            Err(symphonia::core::errors::Error::DecodeError(_)) => continue,
            Err(e) => return Err(TranscodeError::Decode(e.to_string())),
        }
    }

    Ok((all_samples, sample_rate, channels))
}
