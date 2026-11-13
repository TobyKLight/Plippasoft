# poc2_led_test — WS2811 addressable LED hardware test

Steps through each LED on the **front** and **top** WS2811 strips (GRB colour order). One LED is lit at a time; colours cycle **red → green → blue** by position (not white). Stream 1 (front, GPIO 13) runs first, then stream 2 (top, GPIO 12). After that, **all** LEDs show a sequential rainbow (full saturation, scrolling hue) for **3 seconds**.

## Requirements

- Strips wired per `pinout.md` (5 V power, common GND with Pi)
- Pi prepared per [`boards/prototypes/plippa-D-poc2/boardSetup.md`](../../../boards/prototypes/plippa-D-poc2/boardSetup.md)
- Rust (`cargo`)
- **sudo** — the driver needs PWM/DMA access

## Build

```bash
cd ~/projects/Plippa_PreRelease
cargo build --release -p poc2_led_test
```

On a 1 GiB Pi, limit parallel jobs if needed:

```bash
export CARGO_BUILD_JOBS=1
cargo build --release -p poc2_led_test
```

## Run

Defaults: **33** front LEDs, **30** top LEDs, **30%** brightness, **30 fps** (~33 ms per step).

```bash
cd ~/projects/Plippa_PreRelease
sudo ./target/release/poc2_led_test
```

```bash
sudo ./target/release/poc2_led_test --fps 10     # slower
sudo ./target/release/poc2_led_test --step-ms 100  # explicit override step speed
```


Example output:

```text
Stream 1 — front panel (GPIO 13)
  front LED  0/33  red    (R=255 G=0 B=0)
  front LED  1/33  green  (R=0 G=255 B=0)
  ...
Stream 2 — top panel (GPIO 12)
  top LED  0/30  red    (R=255 G=0 B=0)
  ...

Rainbow — all LEDs, full saturation, 3s
```

**Ctrl-C** clears both strips and exits.

## Options

| Flag | Default | Description |
| ---- | ------- | ----------- |
| `--front-leds N` | 33 | Front panel LED count (GPIO 13) |
| `--top-leds N` | 30 | Top panel LED count (GPIO 12) |
| `--brightness P` | 30 | Brightness 0–100 (percent) |
| `--fps N` | 30 | Step rate (LEDs per second) |
| `--step-ms N` | — | Override `--fps` with explicit hold time (ms) |

```bash
sudo ./target/release/poc2_led_test --brightness 50 --fps 15
sudo ./target/release/poc2_led_test --front-leds 40 --step-ms 100
```

## Troubleshooting

| Symptom | Things to try |
| ------- | ------------- |
| `ws2811 init failed` | Run with `sudo`; confirm GPIO 12/13 are free |
| Wrong colours | Strips are GRB — this test sets `StripType::Ws2811Grb` |
| No LEDs | Check 5 V supply and GND common; verify DATA on pin 32/33 |
| Flicker on both strips | Unplug one strip and test each stream alone |
