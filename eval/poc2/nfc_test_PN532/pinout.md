# PN532 — wiring (POC2)

Elechouse PN532 V3 on the **I2C header**, via POC2 SDA/SCL/3V3/GND hubs (shared with AW9523 on I2C1):

| **PN532 Pin** | **What it does** | **RPi CM4 Header Pin** | Cable | **RPi GPIO Name** |
| ------------- | ---------------- | ---------------------- | ----- | ----------------- |
| **GND** | Ground | GND hub → Pin 14 | Black | Ground |
| **VCC** | Power: 3.3 V only — never 5 V | 3V3 hub → Pin 1 | White | 3V3 Power |
| **SDA** | I2C data | SDA hub → Pin 3 | Grey | GPIO 2 (I2C1_SDA) |
| **SCL** | I2C clock | SCL hub → Pin 5 | Purple | GPIO 3 (I2C1_SCL) |

### DIP switches (critical)

The Elechouse V3 ships in **HSU/UART mode** by default. For this project:

| Channel 1 | Channel 2 | Mode |
| --------- | --------- | ---- |
| **ON** | **OFF** | **I2C** ← use this |
| OFF | OFF | HSU/UART (default — will not work here) |
| OFF | ON | SPI |

The 7-bit I2C address is **0x24** (handled by the driver).

**Quick self-test** after setting switches and wiring:

```bash
i2ctransfer -y 1 w1@0x24 0x00    # should complete silently (not "Connection timed out")
i2ctransfer -y 1 r1@0x24           # returns e.g. 0x00 or 0x01
```

If the write command times out, the module is still not in I2C mode or SDA/SCL are swapped.

Enable I2C on the Pi and confirm:

```bash
sudo i2cdetect -y 1
```

You should see `24` on bus 1. Device path: `/dev/i2c-1`. Full Pi prep: [`boards/prototypes/plippa-D-poc2/boardSetup.md`](../../../boards/prototypes/plippa-D-poc2/boardSetup.md).

## Troubleshooting

**`SAM configuration failed` / `Errno(5)` / `Errno(110)`**

The Raspberry Pi I2C driver reports “not ready” PN532 status polls as `EIO` or `ETIMEDOUT` instead of a clean NACK. This project uses a Pi-specific I2C layer (`src/rpi_i2c.rs`) to work around that.

If communication fails repeatedly:

1. **Power-cycle the PN532** (unplug VCC briefly) or reboot the Pi — a bad session can hang the I2C bus so even `i2cdetect` stalls.
2. Confirm `sudo i2cdetect -y 1` completes quickly and shows `24` before running the app.
3. Slow the bus if reads still time out — add to `/boot/firmware/config.txt`:
   ```
   dtparam=i2c_arm_baudrate=10000
   ```
   then reboot.

**`sudo cargo run` → `cargo: command not found`**

Build as your user, then run the binary as root:

```bash
cd ~/projects/Plippa_PreRelease
cargo build --release -p poc2_nfc_test_PN532
sudo ./target/release/poc2_nfc_test_PN532
```

Or add your user to the `i2c` group (log out/in after) and run without sudo.
