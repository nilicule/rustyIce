/// Diagnostic: feed an MP3 file through StreamDecoder in production-equivalent
/// 4096-byte chunks (the same incremental push path used by TranscodePipeline)
/// and write the PCM to `stream_decoded.wav`.
///
/// Compare this with `decoded_raw.wav` from the `decode_to_wav` example:
///   - If both sound clean → StreamDecoder PCM is correct; bug is in LAME multi-call
///   - If stream_decoded.wav has artifacts → bug is inside StreamDecoder itself
///
/// Usage:
///   cargo run --example stream_decode_to_wav --manifest-path crates/rustyice-transcode/Cargo.toml \
///       -- /path/to/source.mp3
use rustyice_transcode::StreamDecoder;
use std::{env, fs::File, io::Write};

fn write_wav_header(out: &mut impl Write, sample_rate: u32, channels: u16, n_samples: u32) {
    let bits_per_sample: u16 = 32;
    let block_align = channels * bits_per_sample / 8;
    let byte_rate = sample_rate * block_align as u32;
    let data_size = n_samples * block_align as u32;
    let file_size = 36 + data_size;

    out.write_all(b"RIFF").unwrap();
    out.write_all(&file_size.to_le_bytes()).unwrap();
    out.write_all(b"WAVE").unwrap();
    out.write_all(b"fmt ").unwrap();
    out.write_all(&16u32.to_le_bytes()).unwrap();
    out.write_all(&3u16.to_le_bytes()).unwrap();  // IEEE float
    out.write_all(&channels.to_le_bytes()).unwrap();
    out.write_all(&sample_rate.to_le_bytes()).unwrap();
    out.write_all(&byte_rate.to_le_bytes()).unwrap();
    out.write_all(&block_align.to_le_bytes()).unwrap();
    out.write_all(&bits_per_sample.to_le_bytes()).unwrap();
    out.write_all(b"data").unwrap();
    out.write_all(&data_size.to_le_bytes()).unwrap();
}

fn main() {
    let path = env::args().nth(1).expect("Usage: stream_decode_to_wav <input.mp3>");
    let mp3_bytes = std::fs::read(&path).expect("cannot read input file");
    println!("Input: {} ({} bytes)", path, mp3_bytes.len());

    let mut decoder = StreamDecoder::new();
    let mut all_pcm: Vec<f32> = Vec::new();
    let mut detected_rate = 0u32;
    let mut detected_channels = 0u8;
    let mut push_count = 0usize;
    let mut producing = false;

    let chunk_size = 4096; // production chunk size

    for chunk in mp3_bytes.chunks(chunk_size) {
        push_count += 1;
        match decoder.push(chunk) {
            Ok((pcm, rate, ch)) if !pcm.is_empty() => {
                if !producing {
                    println!("First PCM output at push #{push_count}: {}Hz {}ch, {} samples",
                             rate, ch, pcm.len());
                    let head: Vec<_> = pcm.iter().take(8).copied().collect();
                    println!("  First 8 samples: {head:?}");
                    producing = true;
                }
                if detected_rate == 0 {
                    detected_rate = rate;
                    detected_channels = ch;
                }
                all_pcm.extend_from_slice(&pcm);
            }
            Ok(_) => {}
            Err(e) => eprintln!("push {push_count} error: {e}"),
        }
    }

    match decoder.flush_eof() {
        Ok((pcm, rate, ch)) if !pcm.is_empty() => {
            if detected_rate == 0 { detected_rate = rate; detected_channels = ch; }
            println!("flush_eof produced {} samples", pcm.len());
            all_pcm.extend_from_slice(&pcm);
        }
        Ok(_) => {}
        Err(e) => eprintln!("flush_eof error: {e}"),
    }

    println!("Total pushes: {push_count}, total samples: {}, rate: {}Hz, channels: {}",
             all_pcm.len(), detected_rate, detected_channels);

    if all_pcm.is_empty() {
        eprintln!("ERROR: no PCM produced");
        std::process::exit(1);
    }

    let peak = all_pcm.iter().copied().fold(0.0f32, |m, s| m.max(s.abs()));
    let clipped = all_pcm.iter().filter(|&&s| s.abs() > 1.0).count();
    println!("PCM peak: {peak:.6}, clipped: {clipped}/{}", all_pcm.len());

    let n_samples = all_pcm.len() as u32;
    let mut f = File::create("stream_decoded.wav").expect("cannot create stream_decoded.wav");
    write_wav_header(&mut f, detected_rate, detected_channels as u16, n_samples);
    for s in &all_pcm {
        f.write_all(&s.to_le_bytes()).unwrap();
    }
    println!("Wrote stream_decoded.wav ({n_samples} samples)");
    println!();
    println!("Compare:");
    println!("  stream_decoded.wav  (StreamDecoder incremental push — production path)");
    println!("  decoded_raw.wav     (bare Symphonia all-at-once — from decode_to_wav)");
    println!();
    println!("If both sound the same → StreamDecoder PCM is correct; bug is in LAME multi-call");
    println!("If stream_decoded.wav has artifacts → bug is in StreamDecoder");
}
