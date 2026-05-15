# rustyIce

![rustyice logo](https://raw.githubusercontent.com/nilicule/rustyIce/main/docs/assets/rustyice_logo.png)

A single-binary Icecast-compatible streaming server written in Rust, supporting MP3 and Ogg Vorbis.

## Features

| Component       | What it does |
|-----------------|--------------|
| **Ingest**      | Icecast2-compatible source protocol (`SOURCE` / `PUT`), MP3 + Ogg Vorbis, automatic bitrate detection, real-time pacing |
| **Streaming**   | HTTP output with burst-on-connect prefill, ICY metadata, Vorbis header priming for mid-stream joiners |
| **Transcoding** | Decode/re-encode between MP3 and Ogg Vorbis, per-mount or global, passthrough when unset |
| **AutoDJ**      | Auto-rotate a folder of MP3 / Ogg Vorbis on a mount, shuffle or sequential, tag-derived ICY titles, live-source preemption |
| **Relay**       | Pull from an upstream Icecast-compatible URL on a mount; optional Basic auth, optional transcode, exponential-backoff reconnect, live-source preemption |
| **Auth**        | Per-mount source passwords + optional global password for dynamic mounts; bcrypt-hashed admin users |
| **Admin**       | Web dashboard + REST API (kick-source / kick-listener, listener detail) and Prometheus `/metrics` |
| **Ops**         | Single static binary (async Tokio); `SIGHUP` hot-reload with no listener drops; optional config file with random-credential fallback |
| **Landing**     | Public stream listing with one-click playback |

## Quickstart

### Install

Download the latest prebuilt binary for your OS/arch:

```sh
curl -LJO "https://github.com/nilicule/rustyice/releases/latest/download/rustyice-$(uname -s | tr '[:upper:]' '[:lower:]')-$(uname -m | sed -e 's/x86_64/amd64/' -e 's/aarch64/arm64/')"
chmod +x rustyice-*
mv rustyice-* rustyice
```

Or build from source — **prerequisites:** Rust 1.85+ (2024 edition), Cargo. For transcoding: a C toolchain and `libmp3lame` (`brew install lame` / `apt install libmp3lame-dev`). libvorbis and libogg are vendored by `vorbis_rs` and compiled during the build — no system package needed.

```sh
git clone https://github.com/nilicule/rustyice.git
cd rustyice
cargo build --release
# The binary is at ./target/release/rustyice
```

### Run

The simplest way — no config file, fresh random admin and source passwords printed to stdout on every start:

```sh
./target/release/rustyice
```

To pin credentials and other settings, generate a config template, edit it, and pass it in:

```sh
./target/release/rustyice --print-config > config.toml
# edit config.toml
./target/release/rustyice --config config.toml
```

If `config.toml` exists in the current directory, it's picked up automatically — no flag needed. Configuration precedence: `--config <path>` > `./config.toml` > built-in defaults with random credentials.

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
burst_size            = 65536       # burst-on-connect bytes (0 to disable)
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
# burst_size    = 32768             # optional per-mount override of [limits].burst_size

# Optional: per-mount transcode config.
# When set, all source audio is decoded and re-encoded before delivery.
# Overrides the global [transcode] block if both are set.
# [mounts.transcode]
# format       = "mp3"      # "mp3" or "vorbis"
# sample_rate  = 44100
# bitrate_kbps = 128
```

### Transcoding

rustyIce can decode incoming audio and re-encode it to a consistent format, so listeners always receive a predictable bitrate regardless of what the source is pushing (CBR, VBR, 320 kbps, etc.).

Supported source/target combinations:

| Source             | Target             | Notes                                       |
|--------------------|--------------------|---------------------------------------------|
| MP3                | MP3                | Re-encode at a different bitrate/sample rate. |
| MP3                | Ogg Vorbis         | Decode MP3 → encode Vorbis (ABR).            |
| Ogg Vorbis         | MP3                | Decode Vorbis → encode MP3.                  |
| Ogg Vorbis         | Ogg Vorbis         | Re-encode at a different bitrate.            |
| MP3 or Ogg Vorbis  | (none)             | Passthrough — source bytes broadcast as-is.  |

Vorbis output advertises `Content-Type: application/ogg`. Listener-facing metadata is set once at stream start via Vorbis comments (derived from `name`, `description`, `genre`, `url` on the mount); ICY `icy-metaint` is never advertised for Vorbis streams. Vorbis listeners joining a live broadcast mid-stream are seamlessly primed with the three Vorbis header pages (identification / comment / setup) captured from the encoder's output.

Add a global fallback that applies to all mounts without their own transcode config:

```toml
[transcode]
format       = "mp3"        # or "vorbis"
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
format       = "vorbis"     # Vorbis output for bandwidth-constrained listeners
sample_rate  = 22050
bitrate_kbps = 48
```

No `[transcode]` section anywhere = transparent passthrough (default behaviour, zero overhead).

Connecting a source whose codec is neither MP3 nor Ogg Vorbis (AAC, FLAC, …) to a transcode-enabled mount returns `415 Unsupported Media Type`.

**Requirements:** transcoding uses LAME (MP3 encode) and libvorbis + libogg (Vorbis encode) via C bindings. A C toolchain is required at build time; `libmp3lame` must be present on the system, while libvorbis + libogg are vendored by the `vorbis_rs` crate and compiled automatically during `cargo build`.

Send `SIGHUP` to hot-reload the config (mount metadata and auth credentials update without dropping listeners).

### AutoDJ — local-folder rotation

Stream a folder of MP3 / Ogg Vorbis files automatically. Each `[[autodjs]]` entry registers its own mount; live Icecast sources that connect to the same path preempt the rotation for the duration of the broadcast, then the AutoDJ resumes from the next track.

```toml
[[autodjs]]
mount         = "/lofi"
name          = "Lo-Fi Beats"
description   = "24/7 study channel"
genre         = "Lo-Fi"
folder        = "/var/lib/rustyice/lofi"
enabled       = true
loop          = true                # restart playlist when it ends
order         = "shuffle"           # or "sequential"

[autodjs.transcode]
format       = "mp3"
sample_rate  = 44100
bitrate_kbps = 128
```

The transcode block is required: all files are decoded and re-encoded to a uniform output so listeners get a clean continuous stream regardless of per-file codec, sample rate, or bitrate differences. Per-track ICY title is derived from each file's tags (`artist - title`, or `title` alone, or the filename stem when the file is untagged). MP3 and Ogg Vorbis input files are supported; other extensions in the folder are skipped with a warning.

When `loop = false`, the AutoDJ disconnects after the playlist exhausts (listeners drop). When `loop = true`, the folder is rescanned and re-shuffled at the end of each pass, so files added between passes are picked up automatically. SIGHUP picks up additions, removals, and field changes; metadata-only changes apply without restarting the stream.

### Relay — re-broadcast an upstream stream

Pull from a remote Icecast-compatible URL and re-broadcast on a local mount. Each `[[relays]]` entry registers its own mount; live Icecast sources that connect to the same path preempt the relay for the duration of the broadcast, then the relay reconnects.

```toml
[[relays]]
mount         = "/relay-jazz"
upstream      = "http://upstream.example.com:8000/jazz"
name          = "Jazz Relay"
genre         = "Jazz"
enabled       = true
# username    = "relay"           # optional HTTP Basic
# password    = "secret"

# Optional: re-encode the upstream into a uniform output. Falls back to the
# global [transcode] block when unset. Passthrough when neither is set.
[relays.transcode]
format       = "mp3"
sample_rate  = 44100
bitrate_kbps = 128
```

Connection failures (DNS, TCP, TLS, non-2xx, or mid-stream errors) trigger an exponential backoff and retry forever: 1 s, 2 s, 4 s, …, capped at 30 s. The backoff resets on a successful connect. Admin kick-source on a relay mount drops the upstream connection and immediately reconnects — useful for forcing a fresh handshake without restarting the server.

## Load testing

A separate `rustyice-loadtest` crate opens N concurrent listeners against a running server and reports per-second throughput and dropped connections. It is excluded from `default-members`, so `cargo build --release` never compiles or links it — invoke it explicitly:

```sh
ulimit -n 65535          # raise fd limit before stressing past a few hundred listeners
cargo run --release -p rustyice-loadtest -- http://localhost:8000/stream -n 1000 -r 10 -d 60
```

Flags:

| Flag | Default | Meaning |
|------|---------|---------|
| `-n, --listeners`     | `100` | Concurrent listeners held open |
| `-r, --ramp-secs`     | `5`   | Ramp window — listeners dialed evenly over this period |
| `-d, --duration-secs` | `60`  | Hold duration after ramp completes |

Output is one line per second showing connected count, RX KiB/s, and cumulative drop counts (`drop_eof`, `drop_err`, `connect_err`), followed by a final tally. While the test runs, watch `top`, `lsof -p <pid> | wc -l`, and the server's `/metrics` endpoint to spot the real bottleneck (typically fd limits → CPU on transcode → uplink saturation).

## Development

```sh
cargo test --workspace              # unit + integration tests
cargo test -p rustyice-server --test e2e_test -- --test-threads=1   # e2e tests
cargo clippy --workspace -- -D warnings
```
