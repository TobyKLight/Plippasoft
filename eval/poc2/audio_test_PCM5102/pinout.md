# PCM5102 DAC — wiring (POC2 / HiFiBerry overlay)

PCM5102 I2S DAC per POC2 wiring CSV. Audio via kernel `hifiberry-dac` (`dtoverlay=hifiberry-dac` in `config.txt`).

| **PCM5102 signal** | **What it does** | **RPi CM4 header pin** | **Cable** | **RPi GPIO / function** |
| ------------------ | ---------------- | ---------------------- | --------- | ----------------------- |
| **BCK** | **Bit clock:** I2S sample clock. | Pin 12 | Green | GPIO 18 (PCM_CLK) |
| **LRCK** | **Word select:** Left/right channel frame sync. | Pin 35 | Purple | GPIO 19 (PCM_FS) |
| **DATA** | **Serial data:** PCM audio samples from the Pi. | Pin 40 | Blue | GPIO 21 (PCM_DOUT) |
| **GND** | **Ground:** Completes the electrical circuit. | Pin 39 | Grey | Ground |
| **VCC** | **Power:** 5 V to the module. | Pin 2 | Black | 5V power |

A second GND on the module is NC in POC2 — one ground (Pin 39) is enough. GPIO20 is kept free for a future DA7212 PCM_DIN.

## Device tree overlay

Ensure this line is present in `/boot/firmware/config.txt` (see also [`boards/prototypes/plippa-D-poc2/boardSetup.md`](../../../boards/prototypes/plippa-D-poc2/boardSetup.md)):

```
dtoverlay=hifiberry-dac
```

Reboot after changing `config.txt`.

## Verify ALSA sees the DAC

```bash
aplay -l
```

You should see a card named `sndrpihifiberry` / `snd_rpi_hifiberry_dac` with device `pcm5102a-hifi-0`, for example:

```
card 2: sndrpihifiberry [snd_rpi_hifiberry_dac], device 0: HifiBerry DAC HiFi pcm5102a-hifi-0
```

## Quick self-test

```bash
cd ~/projects/Plippa_PreRelease
cargo build --release -p poc2_audio_test_PCM5102
./target/release/poc2_audio_test_PCM5102
```

The app auto-detects the HiFiBerry/PCM5102 card and plays a random `.wav` from `assets/plippaSFX`. Override the card index if needed:

```bash
./target/release/poc2_audio_test_PCM5102 2
```
