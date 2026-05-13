# rustyIce

A single-binary Icecast-compatible MP3 streaming server written in Rust.

## Features

- Icecast2-compatible source ingest (`SOURCE` and `PUT`)
- MP3 streaming with automatic bitrate detection and real-time pacing
- **Transcoding** — decode/re-encode to a consistent output; per-mount or global, passthrough when unset
- Per-mount source passwords, plus an optional global password for dynamic mounts
- Public landing page with one-click playback
- Admin dashboard + REST API — kick-source / kick-listener, per-mount listener detail, Prometheus `/metrics`
- Hot-reload via `SIGHUP` — no listener drops
- Single static binary, async (Tokio), single config file

## Quickstart

**Prerequisites:** Rust 1.85+ (2024 edition), Cargo. For transcoding: a C toolchain and `libmp3lame` (`brew install lame` / `apt install libmp3lame-dev`).

```sh
# Clone and build
git clone <repo>
cd rustyIce
cargo build --release

# The binary is at:
./target/release/rustyice
```

### Run

Copy the example config and start the server:

```sh
cp config.toml my-config.toml
./target/release/rustyice --config my-config.toml
```

The server binds two ports:

| Port | Purpose |
|------|---------|
| `8000` | Stream (listener + source ingest) |
| `8001` | Admin API + metrics |

### Connect a source

Use any Icecast-compatible source client (e.g. Liquidsoap, Butt, Darkice) pointed at `http://localhost:8000/stream` with the `source_password` from your config. The server also accepts the raw Icecast `SOURCE` HTTP method.

```sh
# Minimal curl example (PUT method):
curl -u :hackme -T audio.mp3 http://localhost:8000/stream
```

### Listen

```sh
curl http://localhost:8000/stream | mpv -
# or open in any media player / browser
```

### Admin API

```sh
curl http://localhost:8001/api/mounts    # list mounts
curl http://localhost:8001/api/stats     # server stats
curl http://localhost:8001/metrics       # Prometheus metrics
```

## Configuration

`config.toml` supports:

```toml
[server]
stream_bind = "0.0.0.0:8000"        # public stream + source ingest
admin_bind  = "127.0.0.1:8001"      # admin UI + REST API + /metrics
hostname    = "localhost"

[logging]
level  = "info"                     # trace | debug | info | warn | error
format = "pretty"                   # pretty | json

[limits]
max_listeners_global  = 500
ring_size             = 64          # broadcast ring buffer slots
slow_listener_grace_s = 2
# source_max_kbps     = 128         # optional: cap source ingest rate

[auth]
# Optional: any source authenticating with this password may create a
# dynamic mount not listed under [[mounts]]. Removed on disconnect.
# source_password = "letmesource"

[[auth.users]]
username        = "admin"
password_bcrypt = "$2b$12$..."      # bcrypt hash; generate with htpasswd

[[mounts]]
path            = "/stream"
source_password = "hackme"
name            = "My Radio"
description     = "Optional description"
genre           = "Music"
max_listeners   = 100               # omit for unlimited

# Optional: per-mount transcode config.
# When set, all source audio is decoded and re-encoded before delivery.
# Overrides the global [transcode] block if both are set.
# [mounts.transcode]
# format       = "mp3"
# sample_rate  = 44100
# bitrate_kbps = 128
```

### Transcoding

rustyIce can decode incoming audio and re-encode it to a consistent format, so listeners always receive a predictable bitrate regardless of what the source is pushing (CBR, VBR, 320 kbps, etc.).

Add a global fallback that applies to all mounts without their own transcode config:

```toml
[transcode]
format       = "mp3"
sample_rate  = 44100
bitrate_kbps = 128
```

Or configure it per-mount to override (or limit) only specific streams:

```toml
[[mounts]]
path            = "/hifi"
source_password = "hackme"

[mounts.transcode]
format       = "mp3"
sample_rate  = 44100
bitrate_kbps = 192

[[mounts]]
path            = "/mobile"
source_password = "hackme"

[mounts.transcode]
format       = "mp3"
sample_rate  = 22050
bitrate_kbps = 48
```

No `[transcode]` section anywhere = transparent passthrough (default behaviour, zero overhead).

Only MP3 sources are supported for transcoding. Connecting a non-MP3 source (Ogg, AAC) to a transcode-enabled mount returns `415 Unsupported Media Type`.

**Requirements:** transcoding uses LAME via C bindings — a C toolchain (`cc`, `libmp3lame`) must be present at build time.

Send `SIGHUP` to hot-reload the config (mount metadata and auth credentials update without dropping listeners).

## Development

```sh
cargo test --workspace              # unit + integration tests
cargo test -p rustyice-server --test e2e_test -- --test-threads=1   # e2e tests
cargo clippy --workspace -- -D warnings
```
