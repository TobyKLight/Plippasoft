# WS2811 addressable LEDs — wiring (POC2)

Two independent data streams into onboard level shifters. LEDs are **GRB** order (green, red, blue on the wire). Power is **5 V** from the barrel jack; data is **3.3 V** from the Pi.

### Front panel (stream 1)

| **Connector** | **Signal** | **RPi CM4 header pin** | **Cable** | **RPi GPIO** |
| ------------- | ---------- | ---------------------- | --------- | ------------ |
| F_LED_CN1 pin 1 | DATA | Pin 33 | Black | GPIO 13 (PWM1) |
| F_LED_CN1 pin 5/6 | 5 V | 5V hub | Orange / Green | 5 V |
| F_LED_CN1 pin 7/8 | GND | GND hub | Blue / Purple | GND |

Default **33** LEDs (`--front-leds`).

### Top / basetop panel (stream 2)

| **Connector** | **Signal** | **RPi CM4 header pin** | **Cable** | **RPi GPIO** |
| ------------- | ---------- | ---------------------- | --------- | ------------ |
| BT_LED pin 1 | DATA | Pin 32 | Black | GPIO 12 (PWM0) |
| BT_LED pin 5/6 | 5 V | 5V hub | Orange / Green | 5 V |
| BT_LED pin 7/8 | GND | GND hub | Blue / Purple | GND |

Default **30** LEDs (`--top-leds`).

GPIO 12/13 are used instead of GPIO 18 (I2S BCK) so LED timing does not conflict with the PCM5102 DAC. See [`boards/prototypes/plippa-D-poc2/boardSetup.md`](../../../boards/prototypes/plippa-D-poc2/boardSetup.md).

## Run

```bash
cd ~/projects/Plippasoft
cargo build --release -p poc2_led_test
sudo ./target/release/poc2_led_test
```

The test walks stream 1 (front) LED-by-LED, then stream 2 (top), then lights **all** LEDs in a scrolling rainbow (full saturation) for 3 seconds. Each lit LED in the walk phase is pure **red**, **green**, or **blue** (cycling by index) at **30%** brightness by default. **Ctrl-C** clears both strips and exits.

## Options

```bash
sudo ./target/release/poc2_led_test --front-leds 33 --top-leds 30 --brightness 30 --fps 30
```

See `./target/release/poc2_led_test --help` for the full list.
