# poc2_screen_anim_ILI9341_bouncy

ILI9341 **240×320** full-screen bouncing-balls animation for POC2 (same wiring as the static screen test).

One vertical-strip bounce pattern is defined once, then stamped across the width as **6 strips** with different hues and phase / speed offsets. Software framebuffer + one `fill_contiguous` present per frame. Prints measured **FPS** each second.

With `--leds` it also runs the [`led_test`](../led_test/src/main.rs) WS2811 animation on a second thread, reproducing the `plippa-ui` display-worker + LED-worker split when chasing screen flicker. See [LED flicker diagnosis](#led-flicker-diagnosis).

## Requirements

- Module wired per `pinout.md`
- Pi prepared per [`boards/prototypes/plippa-D-poc2/boardSetup.md`](../../../boards/prototypes/plippa-D-poc2/boardSetup.md) (`dtoverlay=spi0-1cs,no_miso`)
- Rust (`cargo`)

## Build

```bash
cd ~/projects/Plippasoft
cargo build --release -p poc2_screen_anim_ILI9341_bouncy
```

`--leds` pulls in `rs_ws281x` (same as `led_test`), so the build now also needs
`libclang`/`libclang-dev` for bindgen — see [`README.md`](../../../README.md).

## Run

Defaults: **240×320 native → 320×240 landscape, rot 90 + mirrored** (same as `plippa-ui` / `board.toml`), SPI mode 0 + CE0, SPI 40 MHz.

```bash
cd ~/projects/Plippasoft
sudo ./target/release/poc2_screen_anim_ILI9341_bouncy
```

**Ctrl-C** clears the screen and turns off the backlight.

## SPI speed

Default is **40 MHz** so full-screen refresh can approach ~30 FPS. If you see sparkles / corruption:

```bash
sudo ./target/release/poc2_screen_anim_ILI9341_bouncy --spi-speed 20000000
sudo ./target/release/poc2_screen_anim_ILI9341_bouncy --spi-speed 10000000
```

## Options

```bash
sudo ./target/release/poc2_screen_anim_ILI9341_bouncy --rotation 0
sudo ./target/release/poc2_screen_anim_ILI9341_bouncy --no-mirror
sudo ./target/release/poc2_screen_anim_ILI9341_bouncy --color-order bgr
sudo ./target/release/poc2_screen_anim_ILI9341_bouncy --invert
```

See `./target/release/poc2_screen_anim_ILI9341_bouncy --help` for the full list.

## LED flicker diagnosis

`plippa-ui` runs the ILI9341 SPI present and the WS2811 render on separate
threads. This harness can do the same: `--leds` starts the `led_test`
animation (walk front, walk top, 3 s rainbow, repeat) on its own thread while
the bounce loop keeps presenting full frames.

LEDs need root for PWM/DMA (same as `poc2_led_test`):

```bash
sudo ./target/release/poc2_screen_anim_ILI9341_bouncy --leds
sudo ./target/release/poc2_screen_anim_ILI9341_bouncy --leds --led-quiet   # less stdout noise
```

Strips default to **10%** brightness (front 33 / top 30 LEDs, 30 fps) — close to
the UI's dim steady state. Raise it with `--led-brightness` when testing current draw.

The on-screen badge adds `L<n>` (LED renders since the last 5 Hz update) and
stderr gains `STATLED` lines:

```text
STATLED t=12.4s led_renders=30 led_max=2.1ms led_overlap=1 led_errors=0
```

`led_max` is the slowest `render()` since start (it includes waiting for the
DMA burst); `led_overlap` counts renders that started while an SPI present was
in flight.

### Isolating the cause

Run each case against the same bounce loop and watch the panel:

| Command | DMA renders | LED current | If this flickers |
|---------|-------------|-------------|------------------|
| (no `--leds`) | none | none | Baseline: display/SPI only |
| `--leds --led-mode off` | every frame | none | **DMA / timing / CPU**, not light |
| `--leds --led-mode solid --led-brightness 100` | once | max | **Supply / ground / current**, not DMA |
| `--leds --led-mode steady --led-brightness 100` | every frame | max | Both load paths |
| `--leds --led-mode cycle --led-brightness 2` | every frame | low | DMA with little current |

Useful knobs:

```bash
# Lower the LED refresh rate (fewer DMA bursts overlapping SPI)
sudo ./target/release/poc2_screen_anim_ILI9341_bouncy --leds --led-fps 5

# Move WS2811 off its default DMA channel 10
sudo ./target/release/poc2_screen_anim_ILI9341_bouncy --leds --led-dma 9

# Fewer LEDs / no top strip (lower current and shorter DMA bursts)
sudo ./target/release/poc2_screen_anim_ILI9341_bouncy --leds --led-top 0
```

If `off` flickers but no-`--leds` does not, the strips' light/current is not
the cause — look at DMA channel choice, thread scheduling, or SPI clock.
If only `solid`/`steady` at high brightness flickers, the panel is being
disturbed by the strip current (supply sag or shared ground return).

These modes are deliberately harsher than `plippa-ui`'s normal LED frame
(mostly static, 20% brightness), so the harness is for proving that the two
subsystems interact — not for reproducing the exact UI load.

`--led-mode solid` drives all 63 LEDs white at full brightness — check the 5 V
supply rating before running it for long.

See [`../led_test/pinout.md`](../led_test/pinout.md) for LED wiring.

## Paint-pattern diagnosis (why `plippa-ui` flickers and this does not)

`plippa-ui` pushes the same 153.6 KB full frame over the same SPI driver as this
eval, so a slow present cannot by itself be the difference. What differs is
*what* is on the panel and *when* it is pushed:

| | this eval (`anim`) | `plippa-ui` |
|---|---|---|
| Content between frames | balls move a few px | mostly identical text/borders |
| Contrast | soft blobs on dark bg | 1 px white lines, white text |
| Present cadence | as fast as SPI allows | event driven (1 Hz clock, input, status) |
| Changed area per present | a few hundred px | a few px, or a whole list while scrolling |

A full-frame push takes ~35 ms at 40 MHz and ~81 ms once the core clock sags,
while the panel scans its own GRAM at ~70 Hz. So each
present leaves ~2–6 horizontal tear lines on screen for one 14 ms field.
Tearing is only *visible* where the new and old frames differ:

* bouncy balls: old and new frames differ by a few pixels of ball edge, so the
  tear line has nothing to show — the animation looks smooth at 12 FPS;
* static UI text: also nothing to show, so slow present alone is not "flicker";
* but while scrolling a list / turning an encoder, every row changes, and 2–6
  bands of vertically-shifted content sweep the screen — that reads as shimmer.

Use `--paint` to test the UI's paint pattern on this harness:

```bash
# UI-shaped screen (borders, text list, clock, progress bar) presented at 30 Hz
sudo ./target/release/poc2_screen_anim_ILI9341_bouncy --paint ui

# Same screen repainted at 1 Hz, matching plippa-ui's idle tick
sudo ./target/release/poc2_screen_anim_ILI9341_bouncy --paint ui --paint-hz 1

# Same screen, whole list shifted every present (encoder spin)
sudo ./target/release/poc2_screen_anim_ILI9341_bouncy --paint ui-scroll --paint-hz 30
```

Reading the result:

| Result | Conclusion |
|---|---|
| `--paint ui` flickers, `--paint anim` does not | tearing over high-contrast/static content; fix by raising SPI clock or dropping the `ondemand` governor |
| `--paint ui-scroll` flickers but `ui` is clean | only large-area changes tear; UI redraws are fine, the visible artefact is scroll shear |
| nothing flickers even at `--paint ui --paint-hz 1` | the present path is not the cause — look for duplicate `plippa-ui` instances or a restart loop blanking the panel |


