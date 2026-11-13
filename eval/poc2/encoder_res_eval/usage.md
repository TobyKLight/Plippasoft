# poc2_encoder_res_eval — 18-detent encoder feel

Interactive eval: is SWE1’s **18-detent** encoder fine enough for subtle 0–100% control, and what step grid / velocity smoothing makes it feel good?

## What it does

- **SWE1** moves a value from **0.00%–100.00%** (starts at **50%**).
- **Detent filter** (default **2** edges/click): one mechanical click → one step. Use `--detent-edges 1` for raw quadrature.
- **Screen** (ILI9341 landscape): **FPS** badge, large %, clock-style pie fill, left→right bar, live param text.
- Continuous full-screen present at default **SPI 62.5 MHz** (~**47 FPS** when core stays ~500 MHz).
- **SWE5** sets how many steps the range is divided into: default **100**; **10…100** in steps of **10**, then **150…2000** in steps of **50**.
- **SWE3** (default **accel OFF**): push the encoder to toggle **accel** ↔ **turns**.
  - **accel** — raw gain (0.01).
  - **turns** — target full-scale revolutions at a reference fast spin (**0.2** steps; **3.0** default when leaving accel OFF). Slow click still always 1 step.
- **SWE4** **EMA** ms (default **OFF**): optional display / LED lag toward the target.
- **SWE3 + SWE4 LED bars** (5 each) show the same %, with **fractional brightness** on the edge LED so the full range is visible beyond 5 hard steps.
- LED strip max brightness via `--brightness` (default **2%**).

## Requirements

- Wired per `pinout.md` and POC2 wiring / LEDPATCH CSVs
- Pi prep per [`boards/prototypes/plippa-D-poc2/boardSetup.md`](../../../boards/prototypes/plippa-D-poc2/boardSetup.md)
- Rust (`cargo`)
- **sudo** — GPIO + SPI + PWM/DMA LEDs

## Build / run (on this 1 GB board)

Prefer a single-job build (`CARGO_BUILD_JOBS=1`) on a 1 GB board.

```bash
cd ~/projects/Plippasoft
export CARGO_BUILD_JOBS=1
cargo build --release -p poc2_encoder_res_eval

# Optional: pin core for fair ~47 FPS
echo performance | sudo tee /sys/devices/system/cpu/cpu*/cpufreq/scaling_governor

sudo ./target/release/poc2_encoder_res_eval
```

Steady **~11 FPS** usually means core clock sag, not “the app is wrong” — pin governor or set `core_freq_min=500`.

## Suggested feel test

1. Leave SWE3/SWE4 at OFF. Turn SWE1 slowly and watch the pie / bar / %.
2. Raise **SWE5 steps** (finer grid) and see if slow turns feel usable.
3. Add a little **SWE3 accel**: slow = detail, fast = coarse.
4. Optionally add **SWE4 ema** if the jumpiness wants a bit of glide.
5. Watch the hardware LED bars next to SWE3/SWE4 for the same curve.
6. Glance at **FPS** — if it collapses while turning, the feel test is contaminated.

## Options

| Flag | Default | Description |
| ---- | ------- | ----------- |
| `--brightness P` | 2 | LED max brightness 0–100 (%) |
| `--steps N` | 100 | Initial step count (snapped to 50s) |
| `--detent-edges N` | 2 | Quadrature edges per mechanical click (`1` = raw) |
| `--spi-speed HZ` | 62500000 | SPI clock (~47 FPS lab default) |
| `--rotation D` | 90 | 0 / 90 / 180 / 270 |
| `--mirrored` / `--no-mirror` | mirrored | Match board.toml |

```bash
sudo ./target/release/poc2_encoder_res_eval --brightness 2 --steps 400
sudo ./target/release/poc2_encoder_res_eval --detent-edges 1    # raw quadrature
sudo ./target/release/poc2_encoder_res_eval --detent-edges 4    # 4-edge parts
sudo ./target/release/poc2_encoder_res_eval --spi-speed 40000000
```

**Ctrl-C** clears LEDs, blanks the panel, and turns the backlight off.
