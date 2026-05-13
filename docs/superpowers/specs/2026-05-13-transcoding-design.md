# Transcoding Design: Decode-and-Reencode Pipeline (MP3 v1)

## Date: 2026-05-13

## Context

RustyIce currently relays MP3 streams as raw passthrough — bytes from the source go straight to listeners without inspection or conversion. Sources can push any bitrate or VBR content; operators want to guarantee listeners receive a specific format and sample rate (for consistent bandwidth, player compatibility, or relay quality).

This feature adds a decode → resample → reencode pipeline that activates per-mount based on config, with a global fallback.

## Approach

Inline pipeline in ingest. A new `rustyice-transcode` crate provides a stateful `TranscodePipeline`. `IcecastIngest` holds an `Option<TranscodePipeline>`; when present, raw bytes from the source are fed into it and transcoded bytes are published to the broadcast bus. Listeners are unaffected — they still see `Encoded(MP3)` packets on one bus.

## Libraries

- `symphonia` (mp3 feature) — pure Rust MP3 decode
- `rubato` — pure Rust resampling (only when source rate ≠ target rate)
- `mp3lame-sys` — LAME C bindings for encode (requires C toolchain)

## Config

```toml
# Global fallback — applies to any mount without its own transcode block
[transcode]
format       = "mp3"
sample_rate  = 44100
bitrate_kbps = 128

# Per-mount override (takes precedence over global)
[[mounts]]
path = "/stream"
[mounts.transcode]
format       = "mp3"
sample_rate  = 44100
bitrate_kbps = 192
```

No `[transcode]` block anywhere = passthrough (current behaviour unchanged).

## Components

### TranscodeConfig (rustyice-core::config)

```rust
pub enum TranscodeFormat { Mp3 }

pub struct TranscodeConfig {
    pub format: TranscodeFormat,
    pub sample_rate: u32,
    pub bitrate_kbps: u32,
}
```

Resolved per-mount via `Config::effective_transcode(mount) -> Option<&TranscodeConfig>`: mount-level takes precedence, falls back to global, falls back to `None` (passthrough).

### TranscodePipeline (rustyice-transcode)

Stateful struct exposing:
- `TranscodePipeline::new(config: TranscodeConfig) -> Result<Self, TranscodeError>` — constructs and initialises LAME encoder; hard error on bad config
- `push(&mut self, data: &[u8]) -> Result<Bytes, TranscodeError>` — feeds raw bytes, returns transcoded bytes (empty = still buffering)
- `flush(&mut self) -> Result<Bytes, TranscodeError>` — drains encoder tail on source disconnect

Internal flow in `push`:
1. Append bytes to internal `input_buf`
2. Scan for complete MP3 frames using sync-word + frame-size logic (reusing `rustyice-codec::mp3`)
3. Decode each complete frame with symphonia → collect `f32` PCM samples
4. Resample with rubato if source sample rate ≠ target sample rate (`FftFixedInOut`)
5. Encode PCM with mp3lame-sys at configured bitrate
6. Return accumulated output bytes; shift consumed bytes out of `input_buf`

Sub-components:
- `decoder.rs` — symphonia wrapper, lazy-init on first valid frame, detects source sample rate
- `resampler.rs` — rubato `FftFixedInOut` wrapper, created only when rates differ
- `encoder.rs` — mp3lame-sys wrapper, initialised at construction

### IcecastIngest changes (rustyice-ingest)

Gains `transcode: Option<TranscodePipeline>`. In `run()`, after raw bytes are collected and before publishing to bus:

```rust
let data = match self.transcode {
    Some(ref mut p) => match p.push(&raw_chunk) {
        Ok(b) if b.is_empty() => continue,
        Ok(b) => b,
        Err(e) => { warn!("transcode error: {e}"); continue; }
    },
    None => Bytes::copy_from_slice(&raw_chunk),
};
```

On source disconnect, `flush()` is called and any remaining bytes published as a final packet.

### rustyice-server wiring

In the PUT/SOURCE ingest handler, after mount resolution:
```rust
let pipeline = config.effective_transcode(mount_cfg)
    .map(|cfg| TranscodePipeline::new(cfg.clone()))
    .transpose()?;  // hard error at connect time if LAME init fails
let ingest = IcecastIngest::new(/* existing args */, pipeline);
```

## Error Handling

- Decode/encode errors during streaming: log `warn!`, drop packet, continue
- Construction errors (bad config, LAME init failure): hard error surfaced at source connect

## Future Work

- Ogg Opus support: add `OggOpus` variant to `TranscodeFormat`, implement `OpusEncoder` in `rustyice-transcode`
- The pipeline structure is codec-agnostic beyond the format enum

## Verification

1. `cargo build --workspace` — all crates compile including LAME C toolchain
2. `cargo test -p rustyice-transcode` — unit tests pass
3. `cargo test --workspace` — no regressions
4. Manual: push 320 kbps CBR source, confirm listener receives configured bitrate
5. Manual: push VBR source, confirm no Xing header in output, no player buffering issues
