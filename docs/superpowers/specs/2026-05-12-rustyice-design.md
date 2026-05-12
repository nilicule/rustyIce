# RustyIce Architecture Design

**Date:** 2026-05-12
**Status:** Approved

---

## Overview

RustyIce is a single-binary streaming media server inspired by Icecast, written in Rust 2024 edition. The v1 scope is intentionally minimal (MP3 passthrough over Icecast SOURCE/PUT), but the architecture is designed so that additional codecs, ingest protocols, output protocols, and auth backends can be added without restructuring.

---

## 1. Crate Layout

Cargo workspace with seven crates. Dependency graph flows strictly downward: `server` → `{admin, ingest, output, auth, codec}` → `core`. No circular dependencies.

```
rustyice/                        (workspace root)
├── Cargo.toml                   (workspace manifest)
└── crates/
    ├── rustyice-core/           shared traits, types, error types, config schema structs
    ├── rustyice-codec/          CodecId registry + MP3 frame prober (v1)
    ├── rustyice-ingest/         Icecast SOURCE/PUT stream reader (v1)
    ├── rustyice-output/         HTTP passthrough stream writer (v1)
    ├── rustyice-auth/           bcrypt + TOML user table (v1)
    ├── rustyice-admin/          Axum router: admin UI, JSON API, /metrics, embedded assets
    └── rustyice-server/         Binary: runtime wiring, config reload, graceful shutdown
```

Adding a new protocol in v2 (e.g. RTMP ingest): new file in `rustyice-ingest` + one registration line in `rustyice-server`. No other crates change.

---

## 2. Core Traits

All traits live in `rustyice-core`.

### 2.1 Audio Types

```rust
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CodecId(pub &'static str); // "mp3" | "opus" | "vorbis" | "flac" | "aac" | "wav"

pub struct CodecInfo {
    pub id: CodecId,
    pub sample_rate: u32,
    pub channels: u8,
    pub bitrate_kbps: Option<u32>,
}

pub struct PcmFrame {
    pub samples: Vec<f32>,   // interleaved channels, normalized [-1.0, 1.0]
    pub sample_rate: u32,
    pub channels: u8,
}

pub struct EncodedPacket {
    pub codec: CodecId,
    pub data: Bytes,
}

pub enum AudioPayload {
    Encoded(EncodedPacket),  // v1: passthrough, zero-copy fan-out
    Decoded(PcmFrame),       // v2+: transcoding pipeline hook
}

pub struct StreamPacket {
    pub payload: AudioPayload,
    pub pts: Duration,       // presentation timestamp — enables DVR seek buffer
    pub sequence: u64,       // monotonically increasing per mount
}
```

`StreamPacket` is always heap-allocated behind `Arc`. The bus stores `Arc<StreamPacket>` slots; subscribers clone the pointer only. One copy of audio data in memory regardless of listener count.

### 2.2 Codec

```rust
pub trait Codec: Send + Sync + 'static {
    fn codec_id(&self) -> CodecId;

    /// Inspect the first N bytes; return Some if this codec recognises the stream.
    fn probe(&self, header: &[u8]) -> Option<CodecInfo>;

    /// Decode one encoded packet to PCM. Return Err if this impl is probe-only.
    fn decode(&self, packet: &EncodedPacket) -> Result<PcmFrame, CodecError>;

    /// Encode a PCM frame. Return Err if this impl is probe-only.
    fn encode(&self, frame: &PcmFrame, config: &EncodeConfig) -> Result<EncodedPacket, CodecError>;
}
```

V1 MP3 impl: `probe` only (validates sync word, extracts sample rate/bitrate). `decode`/`encode` return `CodecError::Unsupported` until the transcoding pipeline is added.

### 2.3 Fan-Out Bus

```rust
pub trait BroadcastBus: Send + Sync + 'static {
    /// Non-blocking. Advances the write slot; wakes all subscribers.
    fn publish(&self, packet: Arc<StreamPacket>);

    /// Subscribe from this point forward.
    fn subscribe(&self) -> Pin<Box<dyn Stream<Item = Arc<StreamPacket>> + Send + 'static>>;

    /// Instantaneous listener count (admin API + /metrics).
    fn subscriber_count(&self) -> usize;
}
```

V1 implementation: `TokioBroadcastBus` — a thin wrapper around `tokio::sync::broadcast::channel`. The trait boundary means the backing implementation can be swapped for a custom slot-based lock-free ring in v2 (for > ~200 listeners) without touching any consumer.

### 2.4 Ingest Protocol

```rust
#[async_trait]
pub trait IngestProtocol: Send + Sync + 'static {
    fn name(&self) -> &'static str;

    /// Called after HTTP auth and mount negotiation completes.
    /// Reads packets from the source and publishes them to the bus
    /// until the source disconnects.
    async fn run(
        &self,
        reader: Pin<Box<dyn AsyncRead + Send + Unpin>>,
        bus: Arc<dyn BroadcastBus>,
        codec: CodecId,
    ) -> Result<SourceStats, IngestError>;
}
```

Takes `AsyncRead` — RTMP, SRT, and WebRTC all wrap their transport as `AsyncRead`. Neither this trait nor its implementations import axum.

### 2.5 Output Protocol

```rust
#[async_trait]
pub trait OutputProtocol: Send + Sync + 'static {
    fn name(&self) -> &'static str;

    /// Called after HTTP negotiation. Streams packets from the bus to the
    /// listener connection until disconnected or kicked.
    async fn run(
        &self,
        writer: Pin<Box<dyn AsyncWrite + Send + Unpin>>,
        subscription: Pin<Box<dyn Stream<Item = Arc<StreamPacket>> + Send>>,
        mount: &MountInfo,
    ) -> Result<ListenerStats, OutputError>;
}
```

Passes `MountInfo` so the Icecast output impl can inject optional ICY metadata frames.

### 2.6 Auth Backend

```rust
#[async_trait]
pub trait AuthBackend: Send + Sync + 'static {
    async fn verify_admin(&self, username: &str, password: &str) -> Result<bool, AuthError>;
    async fn verify_source(&self, mount: &str, password: &str) -> Result<bool, AuthError>;

    /// Called on SIGHUP. Re-read credentials from backing store.
    async fn reload(&self) -> Result<(), AuthError>;
}
```

V1 impl: reads `[[auth.users]]` from the loaded config. bcrypt comparison runs in `spawn_blocking`. Future impls: OIDC, WebAuthn, bearer tokens — all satisfy this interface.

### 2.7 Mount Registry

```rust
pub struct MountInfo {
    pub path: String,
    pub codec: CodecId,
    pub source_password: String,     // hot-reloadable via ArcSwap
    pub max_listeners: Option<u32>,  // hot-reloadable
    pub metadata: MountMetadata,     // name, description, genre, url — hot-reloadable
}

pub struct ActiveMount {
    pub info: Arc<ArcSwap<MountInfo>>, // lock-free hot swap on SIGHUP
    pub bus: Arc<dyn BroadcastBus>,
    pub source_connected: AtomicBool,
    pub connected_at: Option<Instant>,
    pub stats: Arc<MountStats>,
}

// MountRegistry: Arc<RwLock<HashMap<String, Arc<ActiveMount>>>>
// Write lock only on mount add/remove (rare).
// Read lock on every ingest/listener request (fast path, no contention).
```

---

## 3. Data Flow

```
Source client (IceS, BUTT, ffmpeg, …)
  │
  │  HTTP PUT /stream HTTP/1.1          ← or SOURCE /stream HTTP/1.0
  │  Authorization: Basic base64(pw)
  │  Content-Type: audio/mpeg
  │  [raw MP3 bytes, indefinitely]
  ▼
axum router (rustyice-server, port 8000)
  ├─ tower middleware: intercept HTTP/1.0 SOURCE method
  ├─ AuthBackend::verify_source(mount, password)
  ├─ MountRegistry lookup / ActiveMount creation
  └─ extract AsyncRead from request body
       │
       ▼
  IngestProtocol::run(reader, bus, CodecId("mp3"))
       │
       │  loop:
       │    1. read chunk from reader → Bytes
       │    2. wrap: Arc::new(StreamPacket {
       │         payload: AudioPayload::Encoded(EncodedPacket { codec, data }),
       │         pts: source_start.elapsed(),
       │         sequence: seq.fetch_add(1, Ordering::Relaxed),
       │       })
       │    3. bus.publish(packet)   ← single Arc allocation per chunk
       │
       ▼
  TokioBroadcastBus (tokio::sync::broadcast, capacity = ring_size config value)
       │
       │  each subscriber: tokio broadcast receiver
       │  RecvError::Lagged → slow-listener grace period starts
       │  if still lagging after grace → disconnect
       │
       ├──► Listener task A → OutputProtocol::run(writer, subscription, info)
       │      reads Arc<StreamPacket>.payload.data
       │      writes bytes to TCP socket (HTTP/1.1 200 chunked, no copy)
       │
       ├──► Listener task B  (same)
       │
       └──► [future] DVR buffer task: VecDeque<Arc<StreamPacket>>,
                  capped by pts to 60s, indexed for seek
```

**Zero-copy guarantee:** `Arc<StreamPacket>` is created once per audio chunk at ingest. Every listener clones the pointer only. `Bytes` inside `EncodedPacket` is itself a reference-counted view. No audio data is ever copied after it leaves the ingest reader.

---

## 4. Async Runtime + Concurrency Model

| Concern | Choice | Rationale |
|---------|--------|-----------|
| Runtime | Tokio multi-thread | Standard; `spawn_blocking` available for bcrypt |
| HTTP server | `axum` on both ports | One framework, two `TcpListener`s, composable `tower` middleware |
| Fan-out v1 | `TokioBroadcastBus` wrapping `tokio::sync::broadcast` | Ships fast; swap to custom slot-ring behind the trait in v2 when scale demands it |
| Slow listener | Disconnect after `slow_listener_grace_s` of lag | No unbounded memory growth; configurable |
| Config reload | SIGHUP → re-parse TOML → `ArcSwap::store()` | No restart; sockets stay bound |
| bcrypt on auth | `spawn_blocking` | CPU-bound; must not block the async executor |
| Graceful shutdown | SIGTERM/Ctrl-C → `CancellationToken` → tasks drain → exit | `tokio-util::CancellationToken` propagated to all tasks |
| Mount registry writes | `tokio::sync::RwLock` | Write lock on add/remove only; read lock per request |

**Admin port:** `127.0.0.1:8001` by default — loopback-only, no public exposure without an explicit config change. Stream/ingest port: `0.0.0.0:8000`.

**Slow listener detail:** each subscriber task selects on `{next_packet, kick_signal, shutdown_token}`. If `next_packet` isn't ready within the grace window, the task logs a warning, closes the connection cleanly, and exits. The source task is completely unaffected.

**Hot-reloadable fields** (atomic swap on SIGHUP, no restart): log level, max listeners, slow-listener grace period, user password table, per-mount source passwords, mount metadata (name, description, genre, url).

**Non-hot-reloadable fields** (logged and ignored on SIGHUP, require restart): `stream_bind`, `admin_bind`, `ring_size`.

---

## 5. Config Schema (TOML)

```toml
[server]
stream_bind = "0.0.0.0:8000"       # ingest + listener port
admin_bind  = "127.0.0.1:8001"     # admin UI + /metrics (loopback-only by default)
hostname    = "localhost"           # reported in Icecast compatibility headers

[logging]
level  = "info"                     # hot-reloadable: trace|debug|info|warn|error
format = "json"                     # "json" | "pretty"

[auth]
[[auth.users]]
username        = "admin"
password_bcrypt = "$2b$12$..."      # hot-reloadable

[limits]
max_listeners_global  = 500         # hot-reloadable
ring_size             = 64          # slots per mount bus (restart required to change)
slow_listener_grace_s = 2           # hot-reloadable

[[mounts]]
path            = "/stream"
source_password = "hackme"          # hot-reloadable
max_listeners   = 100               # hot-reloadable
name            = "My Radio"        # hot-reloadable
description     = "The best radio"  # hot-reloadable
genre           = "Electronic"      # hot-reloadable
url             = "https://example.com"  # hot-reloadable

# ── Extension points (parsed but unused in v1) ──────────────────────────────

# [tls]
# acme_domain   = "radio.example.com"
# cert_cache_dir = "/var/lib/rustyice/acme"

# [[mounts.relay]]
# url               = "http://upstream:8000/stream"
# reconnect_delay_s = 5

# [auth.oidc]
# issuer        = "https://accounts.google.com"
# client_id     = "..."
# client_secret = "..."
```

---

## 6. Dependency Choices

| Crate | Justification |
|-------|---------------|
| `tokio` (full) | Async runtime, signal handling, sync primitives |
| `axum` 0.8 | Type-safe HTTP; tower composability for future auth/rate-limit layers |
| `tower` + `tower-http` | Timeout, trace, CORS middleware on both axum routers |
| `serde` + `toml` | TOML config deserialization |
| `tracing` + `tracing-subscriber` | Structured logging with JSON formatter |
| `metrics` + `metrics-exporter-prometheus` | Pure-Rust Prometheus /metrics endpoint |
| `bcrypt` | Pure-Rust bcrypt; run in `spawn_blocking` |
| `rust-embed` | Embed admin UI assets at compile time; disk reads in debug builds via `debug-embed` feature |
| `arc-swap` | Lock-free atomic swap of `Arc` for hot-reload; no `RwLock` on read path |
| `bytes` | Reference-counted byte buffers; zero-copy fan-out |
| `thiserror` | Typed error enums per crate |
| `tokio-util` | `CancellationToken` for graceful shutdown propagation |
| `async-trait` | Async fn in traits (until native AFIT is stable enough in Rust 2024) |
| `futures` | Stream and sink combinators |
| `rustls` + `tokio-rustls` | Pure-Rust TLS; wired up but unused in v1, ready for ACME |

No `openssl`. No `ffmpeg`. No C dependencies. On Linux with `RUSTFLAGS="-C target-feature=+crt-static"`, `cargo build --release` produces a fully static binary.

---

## 7. Risks and Lock-in Points

| Risk | Impact | Mitigation |
|------|--------|-----------|
| **MP3 frame boundary alignment** | V1 reads arbitrary TCP chunks; frame boundaries are not guaranteed. DVR timestamps and HLS segmentation require a proper frame parser. | `Codec::probe()` returns `CodecInfo`. A future `Codec::parse_frames(&mut buf) -> Vec<EncodedPacket>` method handles this without touching the bus or fan-out. |
| **HTTP/1.0 SOURCE method** | Axum does not natively route custom HTTP methods. | A `tower` middleware layer intercepts the raw request before axum's router; well-documented pattern. |
| **`async-trait` overhead** | Virtual dispatch + heap allocation per async call. | Non-issue by design: `IngestProtocol::run` and `OutputProtocol::run` are called once per connection. The inner packet loop is inside `run`, not across the trait boundary. |
| **RTMP requires its own TCP listener** | RTMP is not HTTP; cannot share the axum router. | `IngestProtocol::run` takes `AsyncRead`. The RTMP crate wraps its session transport as `AsyncRead` and spawns its own `TcpListener` task in `rustyice-server`. Zero axum dependency in `rustyice-ingest`. |
| **`ring_size` not hot-reloadable** | The ring buffer is allocated once per mount. Resizing requires recreating the bus, which drops all current subscribers. | Documented in config schema. Logged as "ignored until restart" on SIGHUP if changed. A future admin API "restart mount" action can handle this gracefully. |
| **`AppState` growth** | Axum's `State<T>` permeates handler signatures. | `AppState` holds `Arc<dyn Trait>` for each subsystem from day one. Handlers extract only the subsystem they need. |
| **ICY metadata injection** | Icecast listeners that send `Icy-MetaData: 1` expect interleaved ICY frames, which is non-standard HTTP. | `OutputProtocol::run` receives `MountInfo` including current track metadata. The v1 Icecast output impl can inject ICY frames inline. The interface already passes what's needed. |

---

## Future Features — Architecture Readiness

| Feature | How it fits |
|---------|-------------|
| Ogg Opus / Vorbis / FLAC / AAC | New `Codec` impl in `rustyice-codec`; `CodecId` registered; no other changes |
| RTMP / SRT ingest | New `IngestProtocol` impl in `rustyice-ingest`; own TCP listener; no axum dependency |
| WebRTC WHIP ingest | Same as RTMP; wraps WebRTC track as `AsyncRead` |
| HLS output | New `OutputProtocol` impl in `rustyice-output`; segments stored to disk or memory |
| WHEP / WebRTC playback | New `OutputProtocol`; WebRTC session negotiated, then `AsyncWrite` wraps the data channel |
| Icecast relay pull | New `IngestProtocol` that is itself an HTTP client; pushes to `BroadcastBus` |
| In-process transcoding | `AudioPayload::Decoded` variant consumed; `Codec::decode` → resample → `Codec::encode` inserted between ingest and bus publish |
| WebAuthn / OIDC / bearer tokens | New `AuthBackend` impls in `rustyice-auth` |
| ACME / Let's Encrypt | `rustls` already present; `[tls]` config section already reserved |
| DVR seek buffer | Subscriber task storing `Arc<StreamPacket>` in a `VecDeque` capped by `pts`; `sequence` and `pts` on `StreamPacket` already support this |
| Zero-downtime hot-swap | Listening sockets can be passed via `SO_REUSEPORT` or file descriptor inheritance; `CancellationToken` tree is already the shutdown mechanism |
