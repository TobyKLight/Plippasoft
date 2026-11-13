# Bring-up — plippa-D-poc2

Short hardware checklist. CI does **not** require this; it is an optional manual check on real hardware.

## Prerequisites

- Media USB **mounted by the OS** (Noubin library root). By default Plippa auto-discovers mounts under `/media/<user>/` — no mandatory `PLIPPA_LIBRARY_ROOT`. Find sticks with:

```bash
lsblk -o NAME,SIZE,TYPE,MOUNTPOINT,FSTYPE,MODEL
ls /media/plip/
```


- Optional override (fixtures / single known path): `--library-root=…` or `PLIPPA_LIBRARY_ROOT` — when set, **only** that path is used (discovery skipped).
- Two sticks: both appear in the portal with volume-label prefixes; Add writes to the first discovered root.
- PipeWire / Pulse, groups, overlays, WS2811 `/dev/mem` + the helper's `setcap`: [`boards/prototypes/plippa-D-poc2/boardSetup.md`](boardSetup.md)

## Start

```bash
export CARGO_BUILD_JOBS=1
export CARGO_TARGET_DIR=~/projects/Plippa_PreRelease/target
cd ~/projects/Plippa_PreRelease

# One package at a time on 1GB RAM (avoid thrash):
cargo build -p plippa-server
cargo build -p plippa-engine --features pulse
cargo build -p plippa-ui --features poc2,accessory-2g4
cargo build -p plippa-leds --features ws281x
# WS2811 needs CAP_SYS_RAWIO, held by the plippa-leds helper only (never plippa-ui) — re-run
# after every plippa-leds rebuild:
sudo setcap cap_sys_rawio+ep "$CARGO_TARGET_DIR/debug/plippa-leds"
cargo build -p plippa

export PLIPPA_ROOT="$PWD"
# Omit PLIPPA_LIBRARY_ROOT when sticks are already mounted.
# Optional override:
# export PLIPPA_LIBRARY_ROOT=/media/plip/YourStickLabel
# PULSE_SERVER is set by plippa.service to /run/user/1000/pulse/native.
# Optional lab poweroff after hold-power:
# export PLIPPA_POWEROFF=1

./target/debug/plippa \
  --board=plippa-D-poc2 \
  --backend=poc2 \
  --sink=pulse
# optional: --library-root=/media/plip/YourStickLabel
```

Sample systemd unit: [`plippa.service`](plippa.service).

## Audio notes

- The audio stack is the `plip` user session (PipeWire + WirePlumber); `plippa-install-root` (root-owned `/usr/lib/plippa`, run by `scripts/deploy.sh install-root`; see `boardSetup.md`) installs the required 256-frame quantum file (the stock 1024 frames is 21 ms at 48 kHz).
- Play of a multi-minute 44.1 kHz MP3 must **not** allocate a full-track f32 buffer or sinc-upsample to 48 kHz in-process (that path swap-thrashed 1 GB POC2). Engine streams at the file’s native rate; PipeWire converts.
- First audible audio target: &lt;1 s after LoadAndPlay / tap-hit (Noubin §3.2.1; ideal &lt;300 ms). Watch engine logs: `decode start`, `stream open`, `first audio …ms`.
- Verify while playing:

```bash
free -h   # swap should not climb to death during Play
# engine stderr should show first-audio well under 1000ms on a warm library
```

## Optional hardware smoke

1. `plippa` launcher starts engine, UI, and server  
2. Portal opens from URL on Network screen (`http://<ip>:8787/`)  
3. Associate a tag from UI or portal (NFC write; `.noudata` keys only on success)  
4. Tap plays over RCA with SFX + LED cue after **full** read (Noubin §3.2); multi-minute MP3 must start within ~1 s without swap thrash  
5. Unknown tag shows clear on-screen error (`library_miss`)  
6. Hold power stops children; with `PLIPPA_POWEROFF=1` host powers off  

## Shutdown notes

| Mode | Behaviour |
|------|-----------|
| Default / CI | Sentinel `$PLIPPA_IPC_DIR/shutdown` → stop children only |
| Lab | `PLIPPA_POWEROFF=1` or `plippa --poweroff` → `systemctl poweroff` (fallback `shutdown -h now`) |

Never enable poweroff in CI or automated tests.
