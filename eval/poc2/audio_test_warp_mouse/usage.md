# poc2_audio_test_warp_mouse — live warp control with USB mouse

Combines [`poc2_audio_test_warp_pulse`](../audio_test_warp_pulse/) (in-process MP3 decode, warp DSP,
Pulse output) with [`poc2_mouse_test`](../mouse_test/) (USB mouse input).

Play an MP3 in a loop and control playback live:

| Input | Action |
|-------|--------|
| **Mouse X** (left/right) | Decrease / increase warp % (playback speed) |
| **Left click** | Toggle **pitch preserved** ↔ **vinyl** mode |
| **Right click** | Reset warp to **0%** (100% speed) |
| **Ctrl+C** | Quit |

Warp is expressed as an offset from 100% speed: `-35` → 65%, `0` → 100%, `+35` → 135%.
Each four mouse counts on the X axis change warp by 1% (clamped to -75% … +300%).

## Dependencies (Pi)

```bash
sudo apt install -y libpulse-dev pkg-config build-essential libclang-dev
```

The user-session PipeWire-Pulse server must be running (started by the desktop session on
the dev VM / device). The old system audio stack is retired.

For mouse input without root, your user should be in the `input` group (evdev fallback), or add
udev rules for hidraw (see [`../mouse_test/usage.md`](../mouse_test/usage.md)).

## Build

This crate is part of the shared Cargo workspace at the repo root (one `target/`
directory for all mini-projects).

```bash
cd ~/projects/Plippasoft
cargo build --release -p poc2_audio_test_warp_mouse
```

## Quick test (repo example track)

```bash
cd ~/projects/Plippasoft
./target/release/poc2_audio_test_warp_mouse --verbose \
  "assets/testMedia/MP3 V0/Bogan Mongrel - Grooves For Machines To Study - 02 Beat For Machines To Study.mp3"
```

Quotes are required because the filename contains spaces.

## General usage

```bash
./target/release/poc2_audio_test_warp_mouse [OPTIONS] <track.mp3>
```

### Options

| Flag | Description |
|------|-------------|
| `--verbose`, `-v` | Log warp % and mode changes on stderr |
| `--list` | List detected USB mice and exit |
| `--device N` | Mouse index from `--list` (default: best match) |
| `--pulse-server ADDR` | Override Pulse socket |
| `--help`, `-h` | Print usage |

### Examples

```bash
# List mice
./target/release/poc2_audio_test_warp_mouse --list

# Choose mouse index 1
./target/release/poc2_audio_test_warp_mouse --device 1 --verbose /path/to/track.mp3

# Point at the user-session PipeWire socket explicitly
./target/release/poc2_audio_test_warp_mouse \
  --pulse-server "unix:/run/user/$UID/pulse/native" \
  "assets/testMedia/MP3 V0/Bogan Mongrel - Grooves For Machines To Study - 02 Beat For Machines To Study.mp3"
```

## What to listen for

- **Pitch preserved mode** — moving X changes tempo; pitch stays stable
- **Vinyl mode** (left click) — same tempo control; pitch rises/falls with speed
- **Right click** — snaps back to normal speed

## Verify Pulse routing

While playing:

```bash
pactl list sink-inputs
```

Look for `poc2_audio_test_warp_mouse`.

## Related crates

- [`poc2_audio_test_warp_pulse`](../audio_test_warp_pulse/) — scripted 24 s warp demo (no mouse)
- [`poc2_mouse_test`](../mouse_test/) — raw mouse event printer
