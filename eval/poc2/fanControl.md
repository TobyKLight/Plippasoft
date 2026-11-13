# CM4 IO Board — CPU-temperature fan control

The Raspberry Pi **CM4 IO Board** has an on-board **EMC2301** PWM fan controller (connector **J17**). With no driver loaded, the controller leaves the fan at **full speed**. Kernel thermal control via the `i2c-fan` overlay ties PWM duty cycle to SoC temperature.

This is **not** the `raspi-config` GPIO fan option (and GPIO14 is already used by POC2 for SWE3 A).

## Prerequisites

- Raspberry Pi CM4 on the official CM4 IO Board
- 4-wire PWM fan plugged into **J17** (power + PWM + tach as the header expects)
- Raspberry Pi OS with kernel **≥ 6.1** (Bookworm/Trixie; `emc2305` driver covers EMC2301)
- Edit the live firmware config: `/boot/firmware/config.txt`

## One-time setup (virgin device)

1. Open the firmware config:

   ```bash
   sudo nano /boot/firmware/config.txt
   ```

2. Under the existing `[cm4]` section, add:

   ```ini
   # CM4 IO Board EMC2301 PWM fan (J17) — CPU-temp controlled
   # Without this overlay the hardware defaults to full speed.
   # midtemp/maxtemp are millidegrees C; minpwm=0 keeps fan off when cool.
   dtoverlay=i2c-fan,emc2301,i2c_csi_dsi,minpwm=0,maxpwm=255,midtemp=45000,maxtemp=65000
   ```

   Example context:

   ```ini
   [cm4]
   otg_mode=1

   # CM4 IO Board EMC2301 PWM fan (J17) — CPU-temp controlled
   dtoverlay=i2c-fan,emc2301,i2c_csi_dsi,minpwm=0,maxpwm=255,midtemp=45000,maxtemp=65000
   ```

3. Reboot:

   ```bash
   sudo reboot
   ```

## What the parameters mean

| Parameter        | Value used | Meaning                                      |
| ---------------- | ---------- | -------------------------------------------- |
| `emc2301`        | (flag)     | Use Microchip EMC230x on the IO board        |
| `i2c_csi_dsi`    | (flag)     | Fan controller sits on the CSI/DSI I2C bus   |
| `minpwm`         | `0`        | Fan off / idle PWM below the mid trip        |
| `maxpwm`         | `255`      | Full PWM at / above max trip                 |
| `midtemp`        | `45000`    | Start ramping at **45 °C** (millidegrees C)  |
| `maxtemp`        | `65000`    | Full speed at **65 °C**                      |

Optional hysteresis (defaults are usually fine): `midtemp_hyst`, `maxtemp_hyst` (also millidegrees C).

Defaults if you omit the temps: mid **50 °C**, max **70 °C**, minpwm **0**, maxpwm **255**.

## Verify after reboot

Controller present on the CSI/DSI I2C bus (typically `/dev/i2c-10`) at address **0x2f**:

```bash
sudo apt install -y i2c-tools   # if needed
i2cdetect -y 10
# expect a device at 2f
```

Thermal / cooling devices:

```bash
ls /sys/class/hwmon/
# look for an emc230* / pwm fan hwmon

ls /sys/class/thermal/
cat /sys/class/thermal/thermal_zone0/temp   # millidegrees C
vcgencmd measure_temp
```

At idle (~30–40 °C) the fan should be **stopped or very quiet**. Under load it should ramp toward full speed by ~65 °C.

Overlay loaded:

```bash
dtoverlay -l
# or check dmesg:
dmesg | grep -iE 'emc230|i2c-fan'
```

## Tuning

Edit the same `dtoverlay=` line, then reboot. Examples:

- Quieter earlier: raise `midtemp` (e.g. `55000` = 55 °C)
- Cooler under load: lower `maxtemp` (e.g. `55000`)
- Never fully off: set `minpwm=40` (or similar)

## RTC note

If you also enable the CM4 IO Board RTC on the same bus, use matching bus selection, e.g.:

```ini
dtoverlay=i2c-fan,emc2301,i2c_csi_dsi,minpwm=0,maxpwm=255,midtemp=45000,maxtemp=65000
dtoverlay=i2c-rtc,pcf85063a,i2c_csi_dsi
```

## Shutdown behaviour

On halt/poweroff the EMC2301 may leave the last PWM setting; the fan can keep spinning until power is removed. That is a known limitation of the hardware/driver path, not a misconfiguration of this overlay.
