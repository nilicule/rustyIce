# rustyIce

A single-binary Icecast-compatible MP3 streaming server written in Rust.

## Features

- Icecast2-compatible source ingest — both `SOURCE` and `PUT` methods
- MP3 streaming with automatic bitrate detection and real-time playback pacing
- Per-mount source passwords **and** an optional global default password that lets sources create dynamic mounts on demand (removed automatically when the source disconnects)
- Strips ID3v2 tags and Xing/Info/VBRI metadata frames from incoming streams so players stay in live-stream mode instead of finite-file mode
- Rolling history buffer so new listeners start playing without waiting a full audio cycle
- ICY metadata headers (`icy-name`, `icy-br`, `icy-metaint`) for compatibility with Icecast clients
- Admin REST API + Prometheus metrics endpoint
- bcrypt-hashed admin credentials, plaintext per-stream source passwords
- Hot-reload of config (mount metadata, auth credentials) via `SIGHUP` with no listener drops
- Single static binary, async (Tokio), single config file

## Quickstart

**Prerequisites:** Rust 1.85+ (2024 edition), Cargo.

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
stream_bind = "0.0.0.0:8000"
admin_bind  = "127.0.0.1:8001"
hostname    = "localhost"

[limits]
max_listeners_global  = 500
ring_size             = 64        # broadcast ring buffer slots
slow_listener_grace_s = 2

[auth]
[[auth.users]]
username        = "admin"
password_bcrypt = "$2b$12$..."    # bcrypt hash; generate with htpasswd

[[mounts]]
path            = "/stream"
source_password = "hackme"
name            = "My Radio"
description     = "Optional description"
genre           = "Music"
max_listeners   = 100             # omit for unlimited
```

Send `SIGHUP` to hot-reload the config (mount metadata and auth credentials update without dropping listeners).

## Development

```sh
cargo test --workspace              # unit + integration tests
cargo test -p rustyice-server --test e2e_test -- --test-threads=1   # e2e tests
cargo clippy --workspace -- -D warnings
```
