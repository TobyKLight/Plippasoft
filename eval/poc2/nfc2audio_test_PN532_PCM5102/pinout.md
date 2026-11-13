# poc2_nfc2audio_test_PN532_PCM5102 — hardware

NFC tag tap → random Plippa SFX → MP3 from USB, played on the PCM5102 DAC. Wiring matches POC2 CSV (hubs + I2C1 + HiFiBerry I2S).

## PN532 (I2C)

Elechouse PN532 V3 on the **I2C header**, via SDA/SCL/3V3/GND hubs:

| **PN532 pin** | **What it does** | **RPi CM4 header pin** | **Cable** | **RPi GPIO / function** |
| ------------- | ---------------- | ---------------------- | --------- | ----------------------- |
| **GND** | Ground | GND hub → Pin 14 | Black | Ground |
| **VCC** | Power (3.3 V only) | 3V3 hub → Pin 1 | White | 3V3 power |
| **SDA** | I2C data | SDA hub → Pin 3 | Grey | GPIO 2 (I2C1_SDA) |
| **SCL** | I2C clock | SCL hub → Pin 5 | Purple | GPIO 3 (I2C1_SCL) |

Shares I2C1 with the AW9523 expander.

### DIP switches (critical)

| Channel 1 | Channel 2 | Mode |
| --------- | --------- | ---- |
| **ON** | **OFF** | **I2C** ← use this |
| OFF | OFF | HSU/UART (default — will not work here) |
| OFF | ON | SPI |

I2C address: **0x24**. Device path: `/dev/i2c-1`.

Enable I2C, then:

```bash
sudo i2cdetect -y 1    # expect 24
```

## PCM5102 DAC (I2S)

| **PCM5102 signal** | **What it does** | **RPi CM4 header pin** | **Cable** | **RPi GPIO / function** |
| ------------------ | ---------------- | ---------------------- | --------- | ----------------------- |
| **BCK** | I2S bit clock | Pin 12 | Green | GPIO 18 (PCM_CLK) |
| **LRCK** | I2S word select | Pin 35 | Purple | GPIO 19 (PCM_FS) |
| **DATA** | I2S serial data | Pin 40 | Blue | GPIO 21 (PCM_DOUT) |
| **GND** | Ground | Pin 39 | Grey | Ground |
| **VCC** | Power (5 V) | Pin 2 | Black | 5V power |

### Device tree overlay

```
dtoverlay=hifiberry-dac
```

See [`boards/prototypes/plippa-D-poc2/boardSetup.md`](../../../boards/prototypes/plippa-D-poc2/boardSetup.md). Reboot after changing `config.txt`.

### Verify audio

```bash
aplay -l
# card N: sndrpihifiberry [snd_rpi_hifiberry_dac] … pcm5102a-hifi-0
```

## USB media

MP3 files (`play1.mp3`, etc.) live on a USB stick, **top level of the mount** (e.g. `/media/plip/PLIPPA/play1.mp3`). NTAG215 tags carry NDEF like `https://play1`; the app looks up `play1.mp3`.

## Troubleshooting

**PN532 `SAM configuration failed`**

Power-cycle the PN532 or reboot the Pi. Confirm `sudo i2cdetect -y 1` shows `24`. See `nfc_test_PN532/pinout.md` for I2C bus notes.

**No audio / wrong card**

Confirm `aplay -l` lists `sndrpihifiberry`. Override with `--card-index N` if auto-detect picks the wrong card.

**I2C group / permissions**

Add your user to the `i2c` group (log out/in) to avoid sudo, or run with sudo:

```bash
cd ~/projects/Plippasoft
cargo build --release -p poc2_nfc2audio_test_PN532_PCM5102
sudo ./target/release/poc2_nfc2audio_test_PN532_PCM5102 --media-dir "$MEDIA_DIR"
```
