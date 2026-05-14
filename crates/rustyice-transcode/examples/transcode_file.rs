/// Diagnostic: feed a local MP3 file through the EXACT same TranscodePipeline
/// path used by production (4096-byte chunks, incremental push, same LAME
/// multi-call pattern). Output is written to `transcoded.mp3` in the current
/// directory.
///
/// This exercises what decode_to_wav does NOT: StreamDecoder incremental push
/// and repeated small LAME calls.  If the output sounds clean here, the bug is
/// in the HTTP/async ingest layer.  If artifacts appear, the bug is inside
/// TranscodePipeline itself.
///
/// Enable debug logging to see per-chunk LAME boundary data:
///
///   RUST_LOG=rustyice_transcode=debug \
///   cargo run --example transcode_file --manifest-path crates/rustyice-transcode/Cargo.toml \
///       -- /path/to/source.mp3 [output_kbps]
///
/// Default output bitrate: 128 kbps.  Set tracing level to debug to see the
/// chunk counter and first/last-4 samples handed to LAME on each call.
use rustyice_core::config::{TranscodeConfig, TranscodeFormat};
use rustyice_core::types::CodecId;
use rustyice_transcode::TranscodePipeline;
use std::{env, fs};
use tracing_subscriber::EnvFilter;

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    let mut args = env::args().skip(1);
    let path = args.next().expect("Usage: transcode_file <input.mp3> [kbps]");
    let kbps: u32 = args.next().and_then(|s| s.parse().ok()).unwrap_or(128);

    let mp3_bytes = fs::read(&path).expect("cannot read input file");
    println!("Input: {} ({} bytes, {} kbps output target)", path, mp3_bytes.len(), kbps);

    let config = TranscodeConfig {
        format: TranscodeFormat::Mp3,
        sample_rate: 44100,
        bitrate_kbps: kbps,
    };

    let mut pipeline = TranscodePipeline::new(config, CodecId::MP3, vec![])
        .expect("pipeline init failed");

    let chunk_size = 4096; // production chunk size
    let mut output: Vec<u8> = Vec::new();
    let mut push_count = 0usize;
    let mut output_chunks = 0usize;

    for chunk in mp3_bytes.chunks(chunk_size) {
        push_count += 1;
        match pipeline.push(chunk) {
            Ok(b) if b.is_empty() => {}
            Ok(b) => {
                output_chunks += 1;
                output.extend_from_slice(&b);
            }
            Err(e) => eprintln!("push {push_count} error: {e}"),
        }
    }

    match pipeline.flush() {
        Ok(tail) if !tail.is_empty() => {
            output.extend_from_slice(&tail);
        }
        Ok(_) => {}
        Err(e) => eprintln!("flush error: {e}"),
    }

    println!(
        "Pushed {} chunks, got {} non-empty outputs, {} total output bytes",
        push_count, output_chunks, output.len()
    );

    if output.is_empty() {
        eprintln!("ERROR: no output produced — pipeline produced nothing");
        std::process::exit(1);
    }

    let sync_word_count = output.windows(2).filter(|w| w[0] == 0xFF && (w[1] & 0xE0) == 0xE0).count();
    println!("MP3 sync words in output: {sync_word_count}");

    fs::write("transcoded.mp3", &output).expect("cannot write transcoded.mp3");
    println!("Wrote transcoded.mp3");
    println!();
    println!("Compare:");
    println!("  Listen: transcoded.mp3                     (production pipeline path)");
    println!("  Listen: decoded_lame.mp3 from decode_to_wav  (one-shot encode path)");
    println!("  lame -b {kbps} decoded_raw.wav reference_lame.mp3  (reference)");
    println!();
    println!("If transcoded.mp3 has artifacts but decoded_lame.mp3 does not,");
    println!("  the bug is in the incremental TranscodePipeline path.");
    println!("If transcoded.mp3 sounds clean, the bug is in the HTTP/ingest layer.");
}
