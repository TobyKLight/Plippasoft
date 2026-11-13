# poc2_screen_test_ILI9341_240x320

ILI9341 **240×320** SPI display hardware test for POC2 (hardware CS on CE0, backlight GPIO 16).

## Requirements

- Module wired per `pinout.md`
- Pi prepared per [`boards/prototypes/plippa-D-poc2/boardSetup.md`](../../../boards/prototypes/plippa-D-poc2/boardSetup.md) (`dtoverlay=spi0-1cs,no_miso`)
- Rust (`cargo`)

## Build

```bash
cd ~/projects/Plippa_PreRelease
cargo build --release -p poc2_screen_test_ILI9341_240x320
```

## Run

Defaults: **240×320, rot 0, offset (0,0), RGB, inversion off, SPI mode 0 + CE0**.

```bash
cd ~/projects/Plippa_PreRelease
sudo ./target/release/poc2_screen_test_ILI9341_240x320
```

Modes cycle automatically every 8 seconds:

| Mode | What to look for |
| ---- | ---------------- |
| **Calibration** | mipidsi test pattern — white border, RGB bars |
| **Pixel walk** | 16×16 coloured block scans the screen |
| **Color bars** | Six horizontal bands (R G B Y M C) |
| **Gradient sweep** | Animated hue columns |
| **Checker scroll** | Moving checkerboard |

**Ctrl-C** clears the screen and turns off the backlight.

## Blank screen?

```bash
sudo ./target/release/poc2_screen_test_ILI9341_240x320 --smoke
sudo ./target/release/poc2_screen_test_ILI9341_240x320 --color-order bgr
sudo ./target/release/poc2_screen_test_ILI9341_240x320 --invert
sudo ./target/release/poc2_screen_test_ILI9341_240x320 --spi-mode 3
```

## Options

```bash
sudo ./target/release/poc2_screen_test_ILI9341_240x320 --rotation 90
sudo ./target/release/poc2_screen_test_ILI9341_240x320 --mode-secs 5
sudo ./target/release/poc2_screen_test_ILI9341_240x320 --blk-float
```

See `./target/release/poc2_screen_test_ILI9341_240x320 --help` for the full list.
