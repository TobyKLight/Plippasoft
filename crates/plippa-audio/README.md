# plippa-audio

Decode, quality pick, SFX mix, and output sinks for Plippa.

## Owns

- **Symphonia decode** for the compiled formats: WAV, MP3, FLAC
- **`StreamDecoder`** — chunked pull at the **file native sample rate** (Play/tap path). Hard cap `MAX_STREAM_BUFFER_BYTES` (~2 MiB); first audio uses `START_PREFETCH_BYTES` only, then incremental top-up. PipeWire converts to the device rate; Plippa does **not** whole-file resample on Play.
- **`select_best_local_path`** — Noubin §3.3: play the highest-quality `localFilePaths` entry this model supports
- **Overspec handling** — `Refuse` before stream open with a clear Noubin-style error; `Downsample` on the Play path means *allow* overspec at native rate (PW converts). `prepare_for_playback` still whole-buffer-resamples for offline/tests only — **not** the hot path.
- **Mixer** — optional same-rate main + SFX (legacy/tests). Live engine prefers a **second** Pulse/mock stream for tap SFX at the SFX file’s native rate.
- **Volume** — linear 0.0–1.0 gain
- **`AudioSink` / `AudioSinkFactory`** — open streams per active role at the needed rate; `MockSink` for tests; optional `pulse` feature for PipeWire-Pulse (POC2 RCA)

## Multi-stream overlap

Worst case: UI/tap FX stream + current track stream. PipeWire mixes. Plippa must not keep two **fully decoded** tracks in RAM. Next/prev is a hard cut.

## Thread-safety (for `plippa-engine` audio thread)

Playback must stay off the control plane (architecture: priority-0 audio thread).

| Type | Expectation |
|------|-------------|
| Sinks / stream decoder | Own on the **audio thread**. `Send` so they can be moved onto that thread. |
| `Volume` | `Clone` + interior `AtomicU32` — set from UI/control thread, read each write on the audio thread. |
| Control plane | Extension gate only; hand **paths** (not whole PCM) to the audio thread. Probe+open is single-pass there. |

## Features

- `pulse` — enable `PulseSink` (libpulse-simple; works with PipeWire-Pulse on desktop/RPi).

## Not implemented

AAC, Ogg and Opus decode (only MP3, FLAC and WAV are compiled in), BPM/sync, sample-accurate transitions.
