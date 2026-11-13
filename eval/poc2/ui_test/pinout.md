# UI inputs + LEDs — wiring (POC2)

From [`../POC2 Circuits Parts - WIRING - Actual 20.07.26.csv`](../../../boards/prototypes/plippa-D-poc2/POC2%20Circuits%20Parts%20-%20WIRING%20-%20Actual%2020.07.26.csv) and [`../POC2 Circuits Parts - LEDPATCH.csv`](../../../boards/prototypes/plippa-D-poc2/POC2%20Circuits%20Parts%20-%20LEDPATCH.csv).

## Encoders (RPi GPIO, quadrature)

| Encoder | A | B | Push (AW9523) |
| ------- | - | - | ------------- |
| SWE1 | GPIO17 | GPIO7 | PB1 |
| SWE3 | GPIO14 | GPIO5 | PB2 |
| SWE4 | GPIO6 | GPIO27 | PB3 |
| SWE5 | GPIO9 | GPIO26 | PB4 |

Internal pull-ups enabled via `pinctrl` at startup. Requires UART console off (SWE3 A on GPIO14) and `dtoverlay=spi0-1cs,no_miso` (frees GPIO7 / GPIO9).

## Switches (AW9523B on I2C1)

Expander: SDA/SCL hubs, **RST = GPIO22**, default address **0x58**. Contacts are active-low.

| Switch | Port |
| ------ | ---- |
| BT_SW1–6 | PA0–PA5 |
| Front SW2 | PA6 |
| Front SW6 | PA7 |
| Front SW7 | PB0 (toggle: high/low → base red/green) |
| SWE1/3/4/5 push | PB1–PB4 |
| Jack detect | PB5 (ignored) |

## LEDs (WS2811 GRB)

| Stream | GPIO | PWM channel |
| ------ | ---- | ----------- |
| Front (F_LED_DATA) | GPIO13 | channel 1 |
| Top / basetop (BT_LED_DATA) | GPIO12 | channel 0 |

| Index | Function |
| ----- | -------- |
| F 0 | SW6 backlight |
| F 1–10 | Left volume (SWE1) |
| F 11–20 | Right volume (SWE5) |
| F 21 | Power (SWE1 push) |
| F 22 | SW2 backlight |
| F 23–27 | SWE4 meter |
| F 28–32 | SWE3 meter |
| BT 0–23 | NFC ring (SWE chase) |
| BT 24–29 | BT_SW1–6 backlights |

## Run

```bash
cd ~/projects/Plippa_PreRelease
cargo build --release -p poc2_ui_test
sudo ./target/release/poc2_ui_test
```
