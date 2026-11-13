# Architecture

Plippasoft is the reference software for the [Noubin standard](https://github.com/TobyKLight/Noubin) and follows it exactly. It is written in Rust in a single Cargo workspace.

Target: Raspberry Pi CM4 (1 GB RAM, 8 GB eMMC, wifi, optional 2.4 GHz accessory radio) running Raspberry Pi OS 64-bit. Media lives on external storage supplied by the user. Only the board profile changes between hardware models; the same app runs on all of them.

## Processes

The processes talk over Unix domain sockets using [Postcard](https://docs.rs/postcard) frames. See [`ipc.md`](./ipc.md) for the protocol and threading rules.

**`plippa-engine`** (priority 0: must never stutter)
- Control plane and playback: session state, playlist and transport, decode, mix, output, transitions, SFX.
- The IPC hub: it listens, UI and server connect.
- On a tap it receives the NDEF data from the UI, normalises it to a key and looks it up in the SQLite index. A miss is passed to `plippa-server`.
- Audio runs on its own thread, separate from control-plane work, so lookups and IO can never glitch the buffer.

**`plippa-ui`** (priority 1: may stutter before playback does)
- The local interface: drives the SPI display frame loop, reads buttons, encoders and NFC, drives LEDs.
- Hardware access is behind traits in `plippa-hw`, so boards can differ in how input, LEDs and NFC are wired. The screen is always driven from the Pi's own GPIO.
- Threading is described in [`ui-threading.md`](./ui-threading.md).

**`plippa-server`** (priority 2: can be throttled hard)
- Media management and web portal. Users connect over wifi to upload files and remote-control the device.
- Owns library scanning and all writes to the SQLite index, and tells the engine about changes.
- Owns waveform sidecar extraction: one low-priority worker (`nice +10`), incremental decode through `plippa-audio::StreamDecoder`, throttled while the engine plays.
- Owns the mapping between metadata, NDEF records and normalised keys, including associating Noubins with playable items.

**`plippa`**: launcher and watchdog. Starts the other processes and logs their events and related OS warnings.

**`plippa-leds`**: a small helper holding the only `CAP_SYS_RAWIO` capability (needed for WS281x LED output). The UI talks to it over a socket and holds no capability itself.

## Audio

Code: `crates/plippa-audio` and `bins/plippa-engine/src/audio_thread.rs`.

- **Decode:** Symphonia (MP3, FLAC, WAV/PCM; the compiled set is `COMPILED_EXTENSIONS` in `plippa-audio/src/formats.rs`). Decoding never happens on the audio thread. A decode worker per stream decodes in chunks at the file's native rate and hands blocks over bounded channels, so the full track is never required in RAM before playback starts.
- **Audio thread:** owns a ring buffer (`SourceWindow`, at most 6 MiB, history plus lookahead) that can be read forward and backward, and publishes its range through a seqlock so nothing blocks. Per 256-frame quantum it renders one of three speed voices: bypass (exactly 100%, bit-exact), a pitch-correct stretcher (Signalsmith, with an in-tree WSOLA fallback) or signed varispeed sinc. Voice changes and hard seeks are equal-power crossfades.
- **Seek:** a seek core handles chasing a committed seek, scan hold, far jumps and cross-track joins. The reported position is the heard media position.
- **Sample rates:** streams open at the file's native rate and PipeWire converts for the output device. `max_sample_rate_hz` and `max_bit_depth` in the board profile are a policy ceiling (refuse, or allow and let PipeWire convert).
- **SFX:** a UI/tap sound plays on its own stream (decoded once to PCM). Every Noubin tap plays it when configured.
- **Resume:** tap-only session resume keeps the last 10 tracks in RAM; re-tapping seeks to the bookmark, restarting from 0 if the track finished or is within its last 15 seconds.

### Routing

The graph is PipeWire (with WirePlumber) in the `plip` user session. The engine owns the route: one effective output plays straight into its sink, while more than one plays through the combining sink `plippa.multi`. An output with a manual delay joins through a `plippa.delay.<output id>` sink fed by a loopback with `latency_msec`. Automatic compensation uses the combining sink's `latency_compensate=1`; manual compensation uses `latency_compensate=0` plus the per-output delay. Built-in and wired outputs are normalised to 100% and unmuted when they join a route; wireless sinks keep their own volume. The UI owns the policy (ticked outputs, local-only, delay mode and values) and sends it over IPC; the engine owns the applied route. The quantum is 256 frames (5.3 ms at 48 kHz), set by `51-plippa-latency.conf`; the stock 1024 frames would add 21 ms.

### Accessory radio (2.4 GHz)

`plippa-hw`'s `accessory-2g4` feature (`crates/plippa-hw/src/accessory/two_g4/`) talks to the system radio service over D-Bus with `zbus` (no tokio, no TLS). One radio thread owns the connection; pairing and connecting run on helper threads, and the UI sees state through `AccessoryBackend::poll`. It handles adapters, discovery, pairing and trust/forget. It does not do routing decisions or LE/GATT audio, and the engine does not use it. The app registers as the media player on the selected adapter so a connected accessory's transport buttons can drive the engine (gated by the `Transport commands` setting).

## Storage

**Index:** SQLite in WAL mode at `<library root>/.plippa/`. `plippa-server` does all writes; `plippa-engine` reads directly and prefers stale data over ever blocking on a lock. `.plippa/` is excluded from library scans.

**Waveforms:** one sidecar per playable media file at `<library root>/.plippa/waveforms/<16 hex>.wfm`. A fixed 48-byte little-endian header followed by one `u8` dB-position peak per 20 ms bucket (a 5-minute track is about 15 KB). The header carries the source size, mtime and an FNV-1a checksum, so a changed, truncated or foreign file is re-analysed. Readers validate magic, version, bucket size, a 4 MiB payload cap and exact file length before allocating.

The library lives on removable, untrusted media. Writes refuse the root if `.plippa` or `.plippa/waveforms` is a symlink, and temp files are created with `O_NOFOLLOW` and `create_new` under a random name, then renamed atomically.

## Stack

| Concern | Choice |
|---|---|
| Language | Rust |
| Database | SQLite (rusqlite), kept loosely coupled |
| IPC serialisation | Postcard over Unix domain sockets |
| Decode | Symphonia |
| Output | PipeWire |
| Web portal | Plain HTML/JS served by `plippa-server` |

## Repository layout

```
crates/
  plippa-core/          playable items, transport semantics, capabilities
  plippa-noubin/        NDEF key extract/normalise, .noudata types, library paths, SQLite index
  plippa-ipc-proto/     IPC message types and framing (engine/ui/server)
  plippa-audio/         decode, mix, stretch, sinks
  plippa-hw/            hardware traits and backends: GPIO, NFC, display, accessory
  plippa-board/         board profile -> capabilities and role map
  plippa-image/         cover-art decode, crop, scale, hash, thumbnails
  plippa-store/         store-agnostic download provider abstraction (no HTTP client)
bins/
  plippa/               launcher
  plippa-engine/
  plippa-ui/
  plippa-server/
  plippa-leds/          LED helper
boards/prototypes/      board profiles and setup (plippa-D-poc2)
eval/                   standalone apps for testing specific hardware
scripts/                deploy and backup
assets/                 plippaSFX (reserved branding), testMedia (fetched, git-ignored)
```

## Permissions

Production binaries run without sudo. Group membership and file capabilities provide hardware access; `deploy.sh` applies them (see the README).

## Building on a 1 GB board

Build one package at a time with `CARGO_BUILD_JOBS=1`. `cargo test` also builds. Cross-compiling from a Linux machine with `scripts/deploy.sh` avoids the problem.
