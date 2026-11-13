# plippa-engine

Control plane + media playback (architecture priority 0). Audio decode/mix/output runs on a dedicated thread; IPC and SQLite stay on the control plane.

## Run

```bash
# Mock sink (CI / desktop without PipeWire)
plippa-engine --sink=mock --library-root /path/to/media

# Device / Pulse → PCM5102 RCA (`--features pulse` build; local sink `alsa_output.platform-soc_sound.stereo-fallback`)
export PULSE_SERVER=unix:/run/user/1000/pulse/native   # set by plippa.service
plippa-engine --sink=pulse --board=plippa-D-poc2 --library-root /media/usb
```

Build with Pulse:

```bash
export CARGO_BUILD_JOBS=1
cargo build -p plippa-engine --features pulse
```

Without the `pulse` feature, `--sink=pulse` falls back to mock (no panic). With the feature, a failed Pulse connect also falls back to mock.

## CLI / env

| Flag / env | Meaning |
|------------|---------|
| `--library-root PATH` / `PLIPPA_LIBRARY_ROOT` | Noubin media library root (positional argv still accepted) |
| `--board ID` / `PLIPPA_BOARD` | Load capabilities from `plippa-board` |
| `--sink=mock\|pulse` / `PLIPPA_SINK` | Output backend (default: `pulse` if feature enabled, else `mock`) |
| `--sfx PATH` / `PLIPPA_SFX` | Tap-load SFX wav (default: `$PLIPPA_ROOT/assets/plippaSFX/PlippaSFX_Chime1.wav`) |
| `PLIPPA_ROOT` | Monorepo / install root for board profiles + SFX |
| `PLIPPA_IPC_DIR` | UDS directory (see `plippa-ipc-proto`) |
| `PLIPPA_IPC_SMOKE_SECS` | Exit hub after N seconds (smoke tests) |

Missing library root or missing `.plippa` index: engine stays up, emits `UserNotice` (`no_media_storage`), and reopens when `LibraryIndexUpdated` arrives.

## Signals

SIGINT / SIGTERM stop the IPC hub, shut down the audio thread, and exit cleanly.

## PipeWire on plippa-D-poc2

See [`boards/prototypes/plippa-D-poc2/boardSetup.md`](../../boards/prototypes/plippa-D-poc2/boardSetup.md) — the graph runs in the `plip` user session. `audio/install.sh` installs the required 256-frame quantum file and retires the old system target; the local sink comes from `[audio]` in the board profile.
