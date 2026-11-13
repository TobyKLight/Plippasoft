# ILI9341 240×320 display — wiring (POC2)

SPI TFT, **240×320**, ILI9341. Hardware CS on SPI0 CE0. Backlight on GPIO 16.

### Module → Pi (from wiring CSV)

| **Module pin** | **What it does** | **RPi CM4 header pin** | **Cable** | **RPi GPIO / function** |
| -------------- | ---------------- | ---------------------- | --------- | ----------------------- |
| **VCC** | Power (3.3 V) | 3V3 hub → Pin 1 | Purple | 3V3 power |
| **GND** | Ground | GND hub | White | Ground |
| **DIN** | SPI MOSI | Pin 19 | Green | GPIO 10 (SPI0 MOSI) |
| **CLK** | SPI clock | Pin 23 | Orange | GPIO 11 (SPI0 SCLK) |
| **CS** | Chip select | Pin 24 | Yellow | GPIO 8 (SPI0 CE0) |
| **DC** | Data/command | Pin 18 | Blue | GPIO 24 |
| **RST** | Display reset | Pin 22 | Brown | GPIO 25 |
| **BL** | Backlight | Pin 36 | Grey | GPIO 16 |

Pi system prep (SPI overlay that frees GPIO7/GPIO9 for encoders): see [`boards/prototypes/plippa-D-poc2/boardSetup.md`](../../../boards/prototypes/plippa-D-poc2/boardSetup.md) — `dtoverlay=spi0-1cs,no_miso`.

## SPI device

Default: **`/dev/spidev0.0`** with kernel **CE0** (app default — do not use `SPI_NO_CS`).

```bash
ls -l /dev/spidev0.0
```

## Default software config

| Setting | Value |
|---------|--------|
| Size | 240×320 |
| Rotation | 0° |
| Offset | (0, 0) |
| Colour | RGB, inversion **off** |
| SPI | `/dev/spidev0.0` + CE0 |
| SPI mode | **0** |
| BL | GPIO 16 high |

```bash
cd ~/projects/Plippasoft
cargo build --release -p poc2_screen_test_ILI9341_240x320
sudo ./target/release/poc2_screen_test_ILI9341_240x320
```

## Troubleshooting

### Backlight ON, no pixels

Check SPI path continuity (DIN/CLK/CS/DC/RST). Swap DC/RST if colours never appear.

```bash
sudo ./target/release/poc2_screen_test_ILI9341_240x320 --smoke
```

### Other commands

```bash
sudo ./target/release/poc2_screen_test_ILI9341_240x320 --rotation 90
sudo ./target/release/poc2_screen_test_ILI9341_240x320 --color-order bgr
sudo ./target/release/poc2_screen_test_ILI9341_240x320 --spi-mode 3
gpioset -c gpiochip0 16=1    # backlight on
```

**Ctrl-C** exits and turns off BL if the app was driving it.
