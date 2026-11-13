# ILI9341 bouncing-balls anim — wiring (POC2)

Same module and pins as [`../screen_test_ILI9341_240x320/pinout.md`](../screen_test_ILI9341_240x320/pinout.md).

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

Pi system prep: [`boards/prototypes/plippa-D-poc2/boardSetup.md`](../../../boards/prototypes/plippa-D-poc2/boardSetup.md) — `dtoverlay=spi0-1cs,no_miso`.

## Run

```bash
cd ~/projects/Plippasoft
cargo build --release -p poc2_screen_anim_ILI9341_bouncy
sudo ./target/release/poc2_screen_anim_ILI9341_bouncy
```

Default SPI clock is **40 MHz**. If glitchy: `--spi-speed 20000000` or `10000000`.
