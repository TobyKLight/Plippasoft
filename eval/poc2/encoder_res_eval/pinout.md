# Encoder resolution eval — wiring (POC2)

Combines ILI9341 screen wiring (same as [`../screen_anim_ILI9341_bouncy/pinout.md`](../screen_anim_ILI9341_bouncy/pinout.md)), front encoders, and SWE3/SWE4 meter LEDs from the wiring / LEDPATCH CSVs.

## Encoders (RPi GPIO, quadrature)

| Encoder | A | B | Role in this eval |
| ------- | - | - | ----------------- |
| SWE1 | GPIO17 | GPIO7 | Value 0–100% |
| SWE3 | GPIO14 | GPIO5 | Velocity accel (smoothing) |
| SWE4 | GPIO6 | GPIO27 | EMA time constant (ms) |
| SWE5 | GPIO9 | GPIO26 | Step count 100…2000 / 50 |

Internal pull-ups via `pinctrl` at startup. Needs UART console off (SWE3 A on GPIO14) and `dtoverlay=spi0-1cs,no_miso` (frees GPIO7 / GPIO9 for SWE1 B / SWE5 A).

## Screen (ILI9341 SPI0)

| Module | RPi |
| ------ | --- |
| DIN / CLK / CS | GPIO10 / GPIO11 / GPIO8 (SPI0) |
| DC / RST / BL | GPIO24 / GPIO25 / GPIO16 |

## LEDs (WS2811 GRB)

| Stream | GPIO | Use here |
| ------ | ---- | -------- |
| Front F_LED_DATA | GPIO13 | SWE4 meters idx 23–27 + SWE3 meters idx 28–32 (same fill) |
| Top BT_LED_DATA | GPIO12 | Held dark |

## Run

```bash
cd ~/projects/Plippa_PreRelease
cargo build --release -p poc2_encoder_res_eval
sudo ./target/release/poc2_encoder_res_eval
```
