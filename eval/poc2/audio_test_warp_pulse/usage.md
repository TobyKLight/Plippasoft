# poc2_audio_test_warp_pulse — time-stretch / vinyl / reverse test

In-process Rust audio DSP test: decodes an MP3, runs a scripted 24-second playback
timeline (pitch-preserved warp, vinyl-style speed changes, reverse), and outputs via
Pulse so the routing layer can steer it to any sink.

## Dependencies (Pi)

```bash
sudo apt install -y libpulse-dev pkg-config build-essential libclang-dev
```

- `libclang-dev` — required to build `signalsmith-stretch` (bindgen)
- `build-essential` — provides `g++` for the C++ stretcher
- `libpulse-dev` — Pulse output bindings

No ALSA dev libraries are required — output goes directly to Pulse/PipeWire.

The user-session PipeWire-Pulse server must be running (started by the desktop session on
the dev VM / device). The old system audio stack is retired.

## Build

This crate is part of the shared Cargo workspace at the repo root (one `target/`
directory for all mini-projects).

```bash
cd ~/projects/Plippasoft
cargo build --release -p poc2_audio_test_warp_pulse
```

If you previously set `LIBCLANG_PATH` to a local `.local-pkg/` copy, unset it when
system `libclang-dev` is installed:

```bash
unset LIBCLANG_PATH
cargo build --release -p poc2_audio_test_warp_pulse
```

## Quick test (repo example track)

A suitable test file lives in the repo assets folder (~221 s):

```bash
cd ~/projects/Plippasoft
./target/release/poc2_audio_test_warp_pulse --verbose "assets/testMedia/MP3 V0/Bogan Mongrel - Grooves For Machines To Study - 02 Beat For Machines To Study.mp3"
```

Quotes are required because the filename contains spaces.

Equivalent absolute path:

```bash
./target/release/poc2_audio_test_warp_pulse --verbose \
  "/home/plip/projects/Plippasoft/assets/testMedia/MP3 V0/Bogan Mongrel - Grooves For Machines To Study - 02 Beat For Machines To Study.mp3"
```

Expected console output (abbreviated):

```
Pulse session: user session
Decoding ../assets/testMedia/MP3 V0/Bogan Mongrel - Grooves For Machines To Study - 02 Beat For Machines To Study.mp3…
Decoded ~220.6s @ 44100 Hz, 2 channel(s)
Output: Pulse default sink @ 48000 Hz (F32)
Playing scripted timeline (24.0s)…
```

With `--verbose`, phase transitions appear on stderr, e.g.:

```
[4.00s] complex warp: ramp to 65%
[5.00s] complex warp: hold 65%
…
[20.00s] reverse playback
```

Playback finishes automatically after 24 seconds. Press **Ctrl+C** to stop early.

## General usage

```bash
./target/release/poc2_audio_test_warp_pulse [OPTIONS] <path/to/track.mp3>
```

### Options

| Flag | Description |
|------|-------------|
| `--verbose`, `-v` | Log phase transitions on stderr |
| `--pulse-server ADDR` | Override Pulse socket (see below) |
| `--help`, `-h` | Print usage and exit |

### Examples

```bash
# Any MP3 (quote paths with spaces)
./target/release/poc2_audio_test_warp_pulse /path/to/my/track.mp3

# Verbose phase log
./target/release/poc2_audio_test_warp_pulse --verbose /path/to/my/track.mp3

# Point at the user-session PipeWire socket explicitly
./target/release/poc2_audio_test_warp_pulse \
  --pulse-server "unix:/run/user/$UID/pulse/native" \
  "assets/testMedia/MP3 V0/Bogan Mongrel - Grooves For Machines To Study - 02 Beat For Machines To Study.mp3"
```

## MP3 requirements

Use a track **≥ 30 seconds** long. The scripted output is 24 seconds; source
consumption varies by phase (~20 s of forward material plus 4 s reverse from the
stopping position). Shorter tracks may trigger an underrun warning.

## Audio stack

The system audio stack is retired; the eval app uses the user-session PipeWire stack.
`pulse_env.rs` connects to the running user session at `/run/user/{uid}/pulse/native`.

Confirm a Pulse socket exists:

```bash
ls /run/user/$UID/pulse/native
```

## Verify Pulse routing

While playing, in another terminal:

```bash
pactl list sink-inputs
```

Look for a stream named `poc2_audio_test_warp_pulse` / `warp test`. The eval app and
`pactl` both use the user-session server:

```bash
pactl list sink-inputs
```

Move it to a specific sink (example sink name may differ):

```bash
pactl move-sink-input <index> <sink-name>
```

## Timeline (24 s output)

| Time | Effect |
|------|--------|
| 0–4 s | Normal playback @ 100% |
| 4–12 s | Complex warp: 65% / 135% ramps and holds, **pitch preserved** |
| 12–20 s | Same speed curves, **vinyl** (pitch follows speed) |
| 20–24 s | Reverse @ 100% |

### What to listen for

- **0–4 s** — intelligible, normal tempo and pitch
- **4–12 s** — tempo changes; pitch should stay stable (compare to vinyl section)
- **12–20 s** — same tempo curves; pitch should drop/rise with speed
- **20–24 s** — clearly backwards; no harsh clicks

## Troubleshooting

| Symptom | Likely cause | Fix |
|---------|--------------|-----|
| `connect to PulseAudio` error | PipeWire-Pulse not running | Start the user-session PipeWire server (log into the desktop session) |
| `warning: no Pulse session found` | No user-session socket | The system audio stack is retired; start the user-session PipeWire server |
| `Unable to find libclang` at build | Missing or broken `LIBCLANG_PATH` | `sudo apt install libclang-dev` and `unset LIBCLANG_PATH` |
| `approaching end of source audio` | MP3 shorter than ~25 s | Use a longer track (e.g. the repo example above) |
| Dropouts during warp sections | Heavy DSP on main thread | Expected on slow boards; re-run; consider smaller system load |
