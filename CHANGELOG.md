# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.2.0] - Unreleased

### Added

#### Server
- Show clickable admin link under banner when starting up server.

#### AutoDJ 
- New `[[autodjs]]` config block — each entry registers its own mount and plays a folder of MP3 / Ogg Vorbis files automatically
- Recursive folder scan, rescanned + reshuffled at the end of each loop pass so files added between passes are picked up.
- Live Icecast sources connecting to an AutoDJ-owned mount preempt the rotation: the AutoDJ releases the slot, parks, and resumes from the next track once the live source disconnects.
- Admin kick-source on an AutoDJ mount cancels the current track (skip-track semantics).

### Fixed

- Classic Icecast-2 source clients (libshout 2.x via Mixxx, EdCast, …) now receive an upfront `HTTP/1.0 200 OK` so they actually start streaming audio instead of hanging in `SHOUT_STATE_RESP` and disconnecting with zero bytes.
- Source-protocol handler now decodes HTTP/1.1 `Transfer-Encoding: chunked` request bodies. Previously chunked PUTs would hang indefinitely as the handler couldn't see end-of-body.

## [0.1.0] - 2026-05-15

First public release.

### Added

#### Server
- Single static binary, async Tokio runtime, seven-crate workspace.
- Two-port layout: public stream/source ingest on `8000`, admin API + metrics on `8001`.
- TOML configuration with `--config` flag, auto-detection of `./config.toml`, and a built-in default that boots on random credentials when no config is supplied.
- `--print-config` flag to emit a config template.
- `SIGHUP` hot-reload of mount metadata and auth credentials without dropping listeners.
- Graceful shutdown on `SIGINT` / `SIGTERM`.

#### Ingest
- Icecast2-compatible source ingest accepting both `SOURCE` and `PUT` methods (Tower layer rewrites `SOURCE` → `PUT`).
- Per-mount source passwords plus an optional global source password that lets authenticated sources create dynamic mounts at runtime (auto-removed on disconnect).
- Parsing of incoming `Ice-*` / `Icy-*` headers into a source overlay, merged with config metadata via an effective-identity helper that drives both listener response headers and ICY metaint behavior.
- Automatic MP3 bitrate detection from frame headers and real-time source pacing.
- Optional `source_max_kbps` rate limiter for file-based sources.

#### Codecs
- MP3 codec with frame prober.
- Ogg Vorbis codec and encoder.

#### Output
- HTTP passthrough output with optional ICY metadata injection and correct `icy-metaint` advertisement.
- Burst-on-connect — new listeners receive a prefill of recent audio (Icecast-compatible `burst_size`, default 64 KiB, per-mount override).
- Rolling history buffer so subscribers joining mid-stream get continuous audio.
- Vorbis listeners joining mid-stream are seamlessly primed with the three Vorbis header pages (identification / comment / setup).
- Vorbis output advertises `Content-Type: application/ogg`; ICY `icy-metaint` is never advertised for Vorbis streams.
- Runtime stream title overlay used in ICY metadata with the mount `name` as fallback.

#### Transcoding (`rustyice-transcode`)
- Decode/resample/re-encode pipeline supporting any combination of MP3 and Ogg Vorbis as source and target.
- Stateful MP3 `StreamDecoder` that preserves the bit reservoir across frames, eliminating bit-reservoir artifacts.
- VBR-aware frame scanner; complete MP3 frames are staged to avoid mid-frame `WouldBlock`.
- Output paced on the configured bitrate.
- Per-mount `[mounts.transcode]` config with a global `[transcode]` fallback; absence of both means transparent passthrough at zero overhead.
- Sources with codecs other than MP3 or Ogg Vorbis connecting to a transcode-enabled mount are rejected with `415 Unsupported Media Type`.

#### Auth
- bcrypt/TOML auth backend with hot-reloadable credentials.
- Session-based authentication for the admin dashboard.

#### Admin
- JSON REST API: list mounts, list listeners (including peer addresses), kick-source, kick-listener, per-mount listener detail, server stats.
- `PUT` / `DELETE /api/mounts/{path}/title` endpoints to set or clear the runtime stream title.
- `MountStatus` JSON exposes the merged effective identity (including current title) and live inbound/outbound bandwidth.
- Prometheus `/metrics` endpoint.
- Embedded admin UI with per-mount title editing.

#### Landing page
- Dynamic streams listing with responsive layout, clickable stream links, and one-click playback.
- Current stream title rendered under each active mount.

#### Testing
- End-to-end integration tests covering stream playback, admin API, auth, shutdown, source `Ice-*`/`Icy-*` header handling, transcoding, runtime title behavior, and `SIGHUP` reload semantics.
- Unit-level transcode pipeline tests covering rate mismatch and encoder rebuild.

#### Tooling
- `rustyice-loadtest` crate for concurrent listener benchmarking against a running server, excluded from `default-members` so it's never linked into release builds.

#### Distribution
- GitHub Actions release workflow producing prebuilt single-file binaries for `linux-amd64`, `linux-arm64`, `darwin-amd64`, `darwin-arm64`, and `windows-amd64`.
- `.deb` and `.rpm` packages for `linux-amd64` and `linux-arm64`.

[0.1.0]: https://github.com/nilicule/rustyice/releases/tag/v0.1.0
