//! Generate small audio fixtures with embedded metadata for tests.
//!
//! Compiled only when the `test-fixtures` feature is on. Lets unit tests
//! (this crate) and integration tests (downstream crates, e.g. the server's
//! e2e suite) produce real MP3 + Ogg Vorbis files with known tags into a
//! tempdir, without committing binaries or depending on system `ffmpeg`.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

const TEST_TITLE: &str = "Test Track";
const TEST_ARTIST: &str = "Test Artist";
const SAMPLE_RATE: u32 = 44_100;

/// Write `short_tagged.mp3` and `short_tagged.ogg` into `dir`, both 1 second
/// of stereo 440 Hz sine with `Test Artist` / `Test Track` metadata.
///
/// Returns paths to the two written files: `(mp3, ogg)`.
///
/// # Errors
/// Returns an error if file I/O or audio encoding fails.
pub fn write_test_fixtures(dir: &Path) -> std::io::Result<(PathBuf, PathBuf)> {
    let mp3 = dir.join("short_tagged.mp3");
    let ogg = dir.join("short_tagged.ogg");
    write_mp3_fixture(&mp3)?;
    write_ogg_fixture(&ogg)?;
    Ok((mp3, ogg))
}

fn sine_pcm_stereo(seconds: u32) -> Vec<f32> {
    let total = (SAMPLE_RATE * seconds) as usize;
    let mut out = Vec::with_capacity(total * 2);
    let amplitude = 0.2_f32;
    for i in 0..total {
        let t = i as f32 / SAMPLE_RATE as f32;
        let s = (t * 440.0 * std::f32::consts::TAU).sin() * amplitude;
        out.push(s); // L
        out.push(s); // R
    }
    out
}

fn write_mp3_fixture(path: &Path) -> std::io::Result<()> {
    let pcm = sine_pcm_stereo(1);
    let mp3 = encode_mp3(&pcm).map_err(|e| {
        std::io::Error::new(std::io::ErrorKind::Other, format!("mp3 encode: {e}"))
    })?;

    // Build an ID3v2 tag and write [TAG][AUDIO] in that order.
    use id3::{Tag, TagLike, Version};
    let mut tag = Tag::new();
    tag.set_title(TEST_TITLE);
    tag.set_artist(TEST_ARTIST);

    let mut file = std::fs::File::create(path)?;
    tag.write_to(&mut file, Version::Id3v23).map_err(|e| {
        std::io::Error::new(std::io::ErrorKind::Other, format!("id3 write: {e}"))
    })?;
    file.write_all(&mp3)?;
    Ok(())
}

fn encode_mp3(pcm_stereo_interleaved: &[f32]) -> Result<Vec<u8>, String> {
    use mp3lame_sys::*;
    if pcm_stereo_interleaved.len() % 2 != 0 {
        return Err("PCM length not divisible by 2 channels".into());
    }
    let frames = pcm_stereo_interleaved.len() / 2;
    let mut left = Vec::with_capacity(frames);
    let mut right = Vec::with_capacity(frames);
    for w in pcm_stereo_interleaved.chunks_exact(2) {
        left.push(w[0]);
        right.push(w[1]);
    }

    unsafe {
        let gfp = lame_init();
        if gfp.is_null() {
            return Err("lame_init failed".into());
        }
        lame_set_in_samplerate(gfp, SAMPLE_RATE as i32);
        lame_set_out_samplerate(gfp, SAMPLE_RATE as i32);
        lame_set_num_channels(gfp, 2);
        lame_set_brate(gfp, 128);
        if lame_init_params(gfp) < 0 {
            lame_close(gfp);
            return Err("lame_init_params failed".into());
        }
        let mut out = vec![0u8; frames * 5 / 4 + 7200];
        let n = lame_encode_buffer_ieee_float(
            gfp,
            left.as_ptr(),
            right.as_ptr(),
            frames as i32,
            out.as_mut_ptr(),
            out.len() as i32,
        );
        let mut flush = vec![0u8; 7200];
        let f = lame_encode_flush(gfp, flush.as_mut_ptr(), 7200);
        lame_close(gfp);
        if n < 0 || f < 0 {
            return Err(format!("lame encode/flush failed: n={n} f={f}"));
        }
        let mut buf = Vec::with_capacity((n + f) as usize);
        buf.extend_from_slice(&out[..n as usize]);
        buf.extend_from_slice(&flush[..f as usize]);
        Ok(buf)
    }
}

/// A `Write` sink backed by a shared `Vec<u8>`.
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

fn write_ogg_fixture(path: &Path) -> std::io::Result<()> {
    let pcm = sine_pcm_stereo(1);

    let sink = Arc::new(Mutex::new(Vec::new()));
    let mut builder = vorbis_rs::VorbisEncoderBuilder::new(
        std::num::NonZeroU32::new(SAMPLE_RATE).unwrap(),
        std::num::NonZeroU8::new(2).unwrap(),
        SharedSink(sink.clone()),
    )
    .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, format!("vorbis builder: {e}")))?;

    builder.bitrate_management_strategy(vorbis_rs::VorbisBitrateManagementStrategy::Abr {
        average_bitrate: std::num::NonZeroU32::new(96_000).unwrap(),
    });

    builder
        .comment_tags([("TITLE", TEST_TITLE), ("ARTIST", TEST_ARTIST)])
        .map_err(|e| {
            std::io::Error::new(std::io::ErrorKind::Other, format!("vorbis comments: {e}"))
        })?;

    let mut enc = builder
        .build()
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, format!("vorbis build: {e}")))?;

    let frames = pcm.len() / 2;
    let mut left = Vec::with_capacity(frames);
    let mut right = Vec::with_capacity(frames);
    for w in pcm.chunks_exact(2) {
        left.push(w[0]);
        right.push(w[1]);
    }

    enc.encode_audio_block(&[&left[..], &right[..]])
        .map_err(|e| {
            std::io::Error::new(std::io::ErrorKind::Other, format!("vorbis encode: {e}"))
        })?;

    enc.finish()
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, format!("vorbis finish: {e}")))?;

    let bytes = std::mem::take(&mut *sink.lock().unwrap());
    std::fs::write(path, &bytes)?;
    Ok(())
}
