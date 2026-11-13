# poc2_nfc2audio_test_PN532_PCM5102

NFC tag (PN532) → USB MP3 playback on PCM5102 DAC with random Plippa SFX.

## Requirements

- PN532 on I2C (`/dev/i2c-1`, DIP switches: I2C mode)
- PCM5102 on I2S (`dtoverlay=hifiberry-dac` in `config.txt`)
- `alsa-utils` (`aplay`), `mpg123`
- NTAG215 tags with NDEF URI/text like `https://play1`
- USB stick with `play1.mp3`, etc. on the **top level** of the mount

## Install dependencies (Pi)

```bash
sudo apt update
sudo apt install -y alsa-utils mpg123 pipewire pipewire-pulse wireplumber
which mpg123 pw-play
```

**PipeWire (recommended):** mixes SFX + MP3 and routes to whatever sink is default.

### When do I run `wpctl set-default`?

**Only if you use PipeWire** (`--audio pulse`, or the default when a session is running) **and** the wrong device is selected.

1. Run `wpctl status` and look under **Sinks**. The current default is marked with `*`.
2. If the `*` is already on the PCM5102 / HiFiBerry sink (may be named “Built-in Audio Stereo”), **do nothing** — you're set.
3. If the `*` is on HDMI or another device, run **once for this boot session**:
   ```bash
   wpctl set-default <hifiberry_sink_id>
   ```

That choice lasts until **reboot** or a **PipeWire/WirePlumber restart** — not permanently. After reboot, check `wpctl status` again; if it picked the wrong sink, run `set-default` once more.

You do **not** need `wpctl` when using `--audio alsa` (direct to PCM5102, no PipeWire).

Direct ALSA cannot mix two streams on the PCM5102; playback is sequential (SFX finishes, then MP3).

## Build

This crate is part of the shared Cargo workspace at the repo root (one `target/`
directory for all mini-projects).

```bash
cd ~/projects/Plippa_PreRelease
cargo build --release -p poc2_nfc2audio_test_PN532_PCM5102
```

## Run

```bash
cd ~/projects/Plippa_PreRelease

# Verify hardware
sudo i2cdetect -y 1          # PN532 at 24
aplay -l                     # PCM5102 as sndrpihifiberry

# USB media (plug stick in first)
lsblk -o NAME,LABEL,MOUNTPOINT,FSTYPE
export MEDIA_DIR="/media/$USER/PLIPPA"   # adjust label
ls "$MEDIA_DIR"/*.mp3

# Default: PipeWire if a session is running (mixed SFX + MP3)
sudo ./target/release/poc2_nfc2audio_test_PN532_PCM5102 --media-dir "$MEDIA_DIR"

# Force PipeWire
sudo ./target/release/poc2_nfc2audio_test_PN532_PCM5102 --media-dir "$MEDIA_DIR" --audio pulse

# Direct ALSA to PCM5102 (sequential SFX then MP3)
sudo ./target/release/poc2_nfc2audio_test_PN532_PCM5102 --card-index 2 --media-dir "$MEDIA_DIR" --audio alsa
```

On startup you should see `Audio: PipeWire/Pulse (mixed)` when PipeWire is active. The app passes your user's PipeWire session to child processes even under `sudo` (via `SUDO_UID`).

**Test mixing without NFC:**

```bash
pw-play assets/plippaSFX/PlippaSFX_Chime1.wav &
sleep 0.3 && mpg123 -q -o pulse "$MEDIA_DIR/play1.mp3"
```

## Behaviour

1. Polls PN532 every 100 ms.
2. Reads NDEF from Type 2 tags; key = part after `https://` (e.g. `play1`).
3. Looks for `{key}.mp3` on the USB mount (top level only).
4. On match: random `.wav` SFX, then MP3 after 300 ms (overlapping with PipeWire; sequential with `--audio alsa`).
5. Another tag tap stops playback.
6. **Ctrl-C** stops playback and exits.

MIFARE Classic 1K (SAK 0x08) is not supported.

See `pinout.md` for wiring.
