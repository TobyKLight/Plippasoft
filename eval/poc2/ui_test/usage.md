# poc2_ui_test — encoders, switches, LEDs

Interactive hardware test for front rotary encoders (SWE1/3/4/5), front/top pushbuttons, and both WS2811 strips. Maps inputs to LEDs per [`../POC2 Circuits Parts - LEDPATCH.csv`](../../../boards/prototypes/plippa-D-poc2/POC2%20Circuits%20Parts%20-%20LEDPATCH.csv).

## Requirements

- Wired per `pinout.md` and the POC2 wiring CSV
- Pi prepared per [`boards/prototypes/plippa-D-poc2/boardSetup.md`](../../../boards/prototypes/plippa-D-poc2/boardSetup.md) (`i2c_arm=on`, UART off, `spi0-1cs,no_miso`)
- Rust (`cargo`)
- **sudo** — PWM/DMA for LEDs + GPIO

## Build

```bash
cd ~/projects/Plippa_PreRelease
cargo build --release -p poc2_ui_test
```

## Run

Defaults: **30%** brightness, **20 ms** debounce, AW9523 on `/dev/i2c-1` @ **0x58**.

```bash
cd ~/projects/Plippa_PreRelease
sudo ./target/release/poc2_ui_test
```

On start, **all LEDs light red**. Then:

| Input | Effect |
| ----- | ------ |
| Front SW2 / SW6, top BT_SW1–6 | Toggle that switch’s LED base ↔ blue + NFC chase |
| Any SWE push | Blue chase once around the NFC ring |
| SWE1 push | Also toggles power LED base ↔ blue |
| Rotate SWE1 | Fill left volume meter (base → blue) |
| Rotate SWE5 | Fill right volume meter |
| Rotate SWE3 | Fill SWE3 meter LEDs |
| Rotate SWE4 | Fill SWE4 meter LEDs |
| SW7 (toggle) | Reports **high/low**; **low → green** base, **high → red** base |

**Ctrl-C** clears LEDs and exits.

## Options

| Flag | Default | Description |
| ---- | ------- | ----------- |
| `--brightness P` | 30 | Brightness 0–100 (%) |
| `--debounce-ms N` | 20 | Switch debounce (ms) |
| `--i2c-bus N` | 1 | I2C bus number |
| `--i2c-addr 0xHH` | 0x58 | AW9523 address |
| `--front-leds N` | 33 | Front strip length |
| `--top-leds N` | 30 | Top strip length |

```bash
sudo ./target/release/poc2_ui_test --debounce-ms 5 --brightness 40
```
