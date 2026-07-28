# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [1.0.4] - 2026-07-28

### Security
- Bumped the transitive `quinn-proto` dependency from 0.11.14 to 0.11.16 to clear a Dependabot advisory. The crate reaches us through `reqwest`'s optional HTTP/3 support, which this project does not enable, so no shipped code path was affected — the update only removes the vulnerable version from `Cargo.lock`.

[1.0.4]: https://github.com/nilicule/rustyice/releases/tag/v1.0.4

## [1.0.3] - 2026-05-21

### Fixed
- Linux release binaries and `.deb` / `.rpm` packages are now built with `cargo-zigbuild` against glibc 2.17, so they run on older distributions such as CentOS/RHEL 7 — previously they failed at startup with `GLIBC_2.xx not found`.

### Removed
- Dropped the Intel macOS (`darwin-amd64`) prebuilt binary; GitHub retired the `macos-13` build runner. Apple Silicon (`darwin-arm64`) builds remain.

[1.0.3]: https://github.com/nilicule/rustyice/releases/tag/v1.0.3

## [1.0.0] - Unreleased

### Added

#### Admin console — full config editor
- New `#admin/config` view with per-section editors for server, transcode, mounts, relays, autodjs, and users. Saves go through `PUT /api/config/<section>`, round-trip via `toml_edit` to preserve comments and key order, write atomically, and apply through the same diff pipeline SIGHUP uses (sharing a write lock to avoid races).
- Mounts / relays / autodjs / users use a collapsed list with one-at-a-time inline editing and an in-row confirm prompt for removal. Blank password fields resolve to the existing values server-side, so editing one entry doesn't require re-entering everyone else's secrets.
- AutoDJ folders are picked via a server-side filesystem browser (`BROWSE…`); a browser file picker would point at the operator's machine, not the server.
- `apply_config` now adds and removes mounts at runtime to match `[[mounts]]` — previously only metadata updates worked without a restart.

#### Role-based access
- `[[auth.users]]` entries gain `role = "admin" | "operator"` (missing role defaults to `admin`, so existing configs upgrade cleanly). Operators can edit mounts, autodjs, and relays; admin-only sections are hidden from their sidebar, write attempts return `403`, and `GET /api/config` strips those fields server-side.
- The logged-in admin can't remove or demote themselves, and the user list is rejected if it contains no admins at all.

### Fixed
- Listener disconnects (broken pipe / connection reset) now log at `debug` instead of `warn` — that's normal player behavior, not a server problem.

[1.0.0]: https://github.com/nilicule/rustyice/releases/tag/v1.0.0

## [0.3.0] - 2026-05-15

### Added

#### Relay
- New `[[relays]]` config block — each entry pulls from a remote Icecast-compatible URL and re-broadcasts on a local mount. Optional HTTP Basic auth, optional per-relay `[relays.transcode]` (falls back to global `[transcode]`, passthrough when unset).
- Reconnect-on-failure with exponential backoff (1 s → 30 s cap). Resets on successful connect.
- Live Icecast sources connecting to a relay-owned mount preempt the relay until they disconnect, then the relay reconnects automatically. `/api/mounts` exposes `source_kind = "relay"` to disambiguate from live and AutoDJ sources.

#### Stream detail page
- Public per-stream detail page, reached by clicking a stream on the landing page (landing entries now open this page instead of the raw stream URL).
- Built-in in-browser player — a custom play/stop control driving an HTML5 `<audio>` element, with an offline state for mounts with no live source.
- Real-time audio visualizer with a bars (frequency spectrum) / line (oscilloscope waveform) toggle, driven by the Web Audio API; synthetic-motion fallback when Web Audio is unavailable.
- Now-playing card showing the live title, description, genre, listener count, uptime, and a compact codec / bitrate / sample-rate / channels spec line.
- Stream responses now send `Access-Control-Allow-Origin: *` so the browser can analyse the audio cross-origin for the visualizer.

### Fixed

- Suppress `symphonia_metadata` INFO noise from the default logger so AutoDJ folders with tagged audio no longer flood the console with `unsupported frame GEOB` (and similar) lines.
- Live sources can now preempt an AutoDJ or Relay on a mount that has no `[[mounts]]` entry of its own. `verify_source` falls back to the global `[auth].source_password` when no per-mount password is configured — previously such PUTs were always rejected with `401`, so the live-source preemption promised by the AutoDJ feature never actually worked.

[0.3.0]: https://github.com/nilicule/rustyice/releases/tag/v0.3.0

## [0.2.0] - 2026-05-15

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

[0.2.0]: https://github.com/nilicule/rustyice/releases/tag/v0.2.0

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
