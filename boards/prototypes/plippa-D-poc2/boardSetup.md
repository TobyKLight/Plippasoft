# plippa-D-poc2 — board software setup

Software / OS configuration for the **Model D** POC2 prototype so Plippa runs without sudo (NFC, display, audio, LEDs).

**Hardware pinout** (do not duplicate here):

| File | Use |
|------|-----|
| [`POC2 Circuits Parts - WIRING - Actual 20.07.26.csv`](POC2%20Circuits%20Parts%20-%20WIRING%20-%20Actual%2020.07.26.csv) | Full harness / pin source of truth |
| [`POC2 Circuits Parts - LEDPATCH.csv`](POC2%20Circuits%20Parts%20-%20LEDPATCH.csv) | WS2811 LED index map |
| [`config/board.toml`](config/board.toml) | Capabilities, roles, pin map for `plippa-board` |
| [`plippa.service`](plippa.service) | systemd unit — see **Autostart (systemd)** |

Console after UART is disabled: **SSH or USB serial only** (GPIO14/15 used by SWE3 A and Sync).

---

## Why these settings

| Setting | Reason (from wiring CSV) |
|---------|--------------------------|
| `enable_uart=0` + no `console=serial0` | SWE3 A on GPIO14; Sync on GPIO15 |
| `dtoverlay=spi0-1cs,no_miso` | TFT CS = CE0/GPIO8; SWE1 B = GPIO7 (default CE1); SWE5 A = GPIO9 (default MISO) |
| `dtoverlay=hifiberry-dac` | PCM5102 I2S: BCK=GPIO18, LRCK=GPIO19, DATA=GPIO21 |
| `dtparam=i2c_arm=on` | PN532 + AW9523 on I2C1 (pins 3/5) |
| ILI9341 SPI | DC=GPIO24, RST=GPIO25, BL=GPIO16 |
| LED data | Top/NFC ring = GPIO12, front = GPIO13 (PWM) |

Do **not** use plain `dtparam=spi=on` alone — that claims CE1 and MISO and fights the front encoders.

---

## Packages

```bash
sudo apt update
sudo apt install -y \
  alsa-utils i2c-tools \
  build-essential pkg-config libssl-dev \
  pipewire pipewire-pulse wireplumber pulseaudio-utils dbus \
  libcap2-bin
```

Install Rust (`rustup`) if you will build the workspace crates.

---

## User groups (buses)

For NFC / I2C / SPI / encoders / audio **without sudo**:

```bash
sudo usermod -aG gpio,i2c,spi,audio,input "$USER"
# log out / in (or reboot)
```

WS2811 still needs extra steps below (the `plippa-leds` helper holds the capability).

---

## Boot firmware — `config.txt`

Edit `/boot/firmware/config.txt` (or `/boot/config.txt` on older images).

Ensure these lines exist (and that a lone `dtparam=spi=on` is **removed** or replaced):

```ini
dtparam=i2c_arm=on
dtoverlay=spi0-1cs,no_miso
dtoverlay=hifiberry-dac
enable_uart=0
```

Keep the rest of a stock Raspberry Pi OS image (`otg_mode`, `vc4-kms-v3d`, etc.).

Optional if PN532 I2C is flaky:

```ini
dtparam=i2c_arm_baudrate=10000
```

Fan (CM4 IO board J17): enable the usual `i2c-fan` / EMC2301 overlay if your image uses it; see board wiring CSV for connector notes.

---

## Boot cmdline — free serial pins

Edit `/boot/firmware/cmdline.txt` (single line). **Remove** `console=serial0,115200`. Keep `console=tty1` and the rest.

Before:

```text
console=serial0,115200 console=tty1 root=...
```

After:

```text
console=tty1 root=...
```

---

## Audio (user session)

The audio stack runs in the `plip` user session: PipeWire + WirePlumber, with
`PULSE_SERVER=unix:/run/user/1000/pulse/native` set by `plippa.service`. The
PCM5102 I2S DAC is the local sink `alsa_output.platform-soc_sound.stereo-fallback`,
named by `[audio]` in `config/board.toml`.

Install the device audio files (quantum + radio identity/power) and retire any legacy
system audio target with the root installer (**Root-owned install tree** below installs
everything root needs, audio files included):

```bash
scripts/deploy.sh install-root        # on the dev machine; or on the device:
sudo /usr/lib/plippa/bin/plippa-install-root
sudo reboot
```

The quantum file is required: stock 1024 frames is 21 ms at 48 kHz, while the
local-only target needs 256 frames (5.3 ms). Verify after the reboot:

```bash
pw-metadata -n settings | grep quantum     # clock.quantum = 256
pw-top                                     # QUANT 256, ERR 0 while playing
pactl list short sinks
# default sink: alsa_output.platform-soc_sound.stereo-fallback (PCM5102 I2S DAC)
```

For a manual run, use `PULSE_SERVER=unix:/run/user/1000/pulse/native`; `plippa.service`
sets it on boot.

---

## WS2811 LEDs (NFC ring + front strip)

`rs_ws281x` drives GPIO **12** (top / NFC ring) and **13** (front) via PWM + DMA and mmap’s
`/dev/mem`. It runs **only** inside `plippa-leds`, a tiny helper that holds the file capability
`cap_sys_rawio+ep`. `plippa-ui` holds no capability and never opens `/dev/mem`: it sends
fixed-size colour frames over `$PLIPPA_IPC_DIR/leds.sock`, and the launcher supervises the
helper as a non-essential child (it may crash and cool down; the UI just shows no LED cue).
Code execution inside the UI can therefore no longer program the DMA engine **while the deployed
helper is the one running**. The helper binary, its unit file and the launcher all live in the
plip-writable checkout, so an attacker who can rewrite them gets the capability back on the next
launcher restart: that is a known residual, not something this split closes by itself.

Two device prerequisites, both installed for you:

- `/etc/udev/rules.d/99-ws281x-mem.rules` (`SUBSYSTEM=="mem", GROUP="gpio", MODE="0660"`) —
  shipped in the root payload and installed by `plippa-install-root`. It lets the helper *open*
  `/dev/mem`; the kernel still refuses read/write without `CAP_SYS_RAWIO`.
- the capability itself, set on **`plippa-leds` only** by `scripts/deploy.sh push`.

```bash
scripts/deploy.sh            # build + push + setcap the helper + install-root + restart
```

After a push (rsync rewrites the binary, which drops the capability) check:

```bash
getcap ~/projects/Plippa_PreRelease/target/release/plippa-leds   # cap_sys_rawio=ep
getcap ~/projects/Plippa_PreRelease/target/release/plippa-ui     # (nothing)
grep CapEff /proc/$(pidof plippa-leds)/status                    # bit 17 set
grep CapEff /proc/$(pidof plippa-ui)/status                      # bit 17 clear
stat -c '%a %U:%G %n' /dev/mem                                   # 660 root:gpio
```

`deploy.sh` re-applies and verifies all of that itself and fails loudly otherwise. A manual
build on the device needs the same two steps by hand:

```bash
sudo setcap cap_sys_rawio+ep "$CARGO_TARGET_DIR/debug/plippa-leds"
sudo setcap -r "$CARGO_TARGET_DIR/debug/plippa-ui"      # strip any capability from an old deploy
```

On next `plippa` start the helper logs `plippa-leds: serving <socket> (N streams, M LEDs)` and the
UI's NFC tap cue, front strip and play-button LED work as before. If the capability is missing the
helper logs one line naming the cause and exits; LED output stays dark (audio, UI and everything
else are unaffected). On a clean exit and 2 s after the UI disappears the helper blanks the strips; a
`SIGKILL`ed helper leaves the strips latching their last frame until it is restarted.

---

## Reboot and verify buses

```bash
sudo reboot
```

After reboot:

```bash
sudo i2cdetect -y 1          # PN532 @ 0x24 when wired / I2C DIP
ls -l /dev/spidev0.0         # should NOT need spidev0.1
aplay -l                     # expect sndrpihifiberry / pcm5102
pinctrl get 7,9,14,15        # UART off; pins free for encoders / sync
stat -c '%a %U:%G %n' /dev/mem
```

---

## Control map (switch / encoder purposes)

**Source of truth:** [`POC2 Circuits Parts - SWPATCH.csv`](POC2%20Circuits%20Parts%20-%20SWPATCH.csv).  
Wiring: [`POC2 Circuits Parts - WIRING - Actual 20.07.26.csv`](POC2%20Circuits%20Parts%20-%20WIRING%20-%20Actual%2020.07.26.csv).  
LED indices: [`POC2 Circuits Parts - LEDPATCH.csv`](POC2%20Circuits%20Parts%20-%20LEDPATCH.csv).  
Runtime roles: [`config/board.toml`](config/board.toml) `[roles]`.

| PCB | Control | Purpose (SWPATCH) | Software role |
|-----|---------|-------------------|-------------------|
| Front | SWE1 encoder | Volume | `volume_encoder` |
| Front | SWE1 push | *(blank in CSV)* | `power_button` (hold-to-power) |
| Front | SW2 | Play Mode | `play_mode_button` (mapped; no behaviour yet) |
| Front | SWE3 encoder / push | Assignable A | `assignable_encoder_a` |
| Front | SWE4 encoder / push | Assignable B | — (out of scope) |
| Front | SWE5 encoder | Navigate Menu | `menu_encoder` |
| Front | SWE5 push | Menu Enter | menu enter (via SWE5 push) |
| Front | SW6 | Menu Back | `back_button` |
| Front | SW7 | Mic Enable | — |
| Base top | BT_SW1 | Previous Track | `prev_track_button` |
| Base top | BT_SW2 | Rewind | `rewind_button` |
| Base top | BT_SW3 | Play | `play_stop_button` (play/pause) |
| Base top | BT_SW4 | Fast Forward | `fast_forward_button` |
| Base top | BT_SW5 | Next Track | `next_track_button` |
| Base top | BT_SW6 | Record | `record_button` |

Play button LED (`BT_LED_SW3`): blue = Playing, orange = loaded standby, off = nothing loaded.

### Roles: how software sees this board

Software never names `BT_SW1`, `BT_LED_SW5`, LED index 24 or `AW9523_PA0`. It asks for a **logical role**;
`board.toml` says whether the board has it and where it is patched. `LEDPATCH.csv` / `SWPATCH.csv` stay the
human source of truth, `board.toml` is the machine-readable copy.

| Section | Keys | Meaning |
|---------|------|---------|
| `[roles]` (required) | `volume_encoder`, `menu_encoder`, `back_button`, `power_button` | Must exist; each names an `[encoders.*]` / `[buttons.*]` entry. `menu_encoder` must have a `push` (= menu enter). |
| `[roles]` (optional) | `play_stop_button`, `assignable_encoder_a`, `prev_track_button`, `next_track_button`, `rewind_button`, `fast_forward_button`, `record_button`, `play_mode_button` | Missing = the board has no such control; features degrade silently (no error, no notice). |
| `[led_streams.<NAME>]` | `gpio`, `channel`, `count` | One WS2811 data line (`F_LED_DATA`, `BT_LED_DATA`): Pi GPIO, rs_ws281x channel, LED count. |
| `[leds.<role>]` | `stream`, `start`, `count` (default 1), optional `max_brightness_pct` (1-100), `color_order` | LED role → `count` consecutive LEDs from `start` on `stream`. A ring/meter is one role with `count > 1`; set-colour applies to the whole span. |

LED roles (`[leds.*]` keys): `play_button`, `prev_track_button`, `next_track_button`, `rewind_button`,
`fast_forward_button`, `record_button`, `play_mode_button`, `menu_back_button`, `nfc_ring`, `power_led`,
`vu_left`, `vu_right`, `assignable_a_meter`, `assignable_b_meter`. An unknown `[leds.*]` key is a load error.
Today the software drives `power_led` (steady blue), `play_button` (transport colour) and `nfc_ring` (tap animation);
the rest are declared but not driven yet (for example the prev/next backlights).

Load-time validation (`BoardError::Invalid`, readable message): every role must point at a defined button/encoder,
every LED span at a defined stream and inside it, spans must not overlap, brightness/colour-order values must be valid.

**Adding a board:** copy `config/board.toml` to `boards/<id>/config/board.toml`; keep the four required `[roles]`,
`[encoders]`/`[buttons]` they reference and `[capabilities]`; then add only the optional roles/LED spans the hardware
really has. Boards with fewer controls are first-class - see `crates/plippa-board/fixtures/minimal.board.toml`
(menu encoder + back + power, no LEDs) which the tests load. The expander driver in `plippa-ui` (`SwitchId`) still
resolves `AW9523_*` pin ids; a non-AW9523 button type needs a new resolver in `control_roles.rs`.

---

## Build and run Plippa

On 1 GiB RAM, one package at a time:

```bash
export CARGO_BUILD_JOBS=1
export CARGO_TARGET_DIR=~/projects/Plippa_PreRelease/target
cd ~/projects/Plippa_PreRelease

cargo build -p plippa-server
cargo build -p plippa-engine --features pulse
cargo build -p plippa-ui --features poc2,accessory-2g4
cargo build -p plippa-leds --features ws281x
cargo build -p plippa
# after the helper rebuild (capabilities are dropped by every rewrite):
sudo setcap cap_sys_rawio+ep "$CARGO_TARGET_DIR/debug/plippa-leds"
sudo setcap -r "$CARGO_TARGET_DIR/debug/plippa-ui" 2>/dev/null || true

export PLIPPA_ROOT="$PWD"
export PLIPPA_LIBRARY_ROOT=/media/plip/<label>   # your media mount
# optional: export PULSE_SERVER=unix:/run/user/1000/pulse/native

./target/debug/plippa \
  --board=plippa-D-poc2 \
  --backend=poc2 \
  --sink=pulse \
  --library-root="$PLIPPA_LIBRARY_ROOT"
```

Hardware smoke checklist: [`docs/bringup-poc2.md`](bringup-poc2.md).

---

## Root-owned install tree

Root never executes or installs anything from the plip-owned checkout. Everything root runs
or installs on the device lives in the root-owned tree `/usr/lib/plippa` (`root:root`, nothing
group/world-writable): the polkit rules, the audio files and scripts, the `/dev/mem` udev rule,
the unit, and the one installer `bin/plippa-install-root`. A polkit rule is system-wide authorisation, so a rule
planted by a compromised `plip` process would otherwise be installed with root.

From the dev machine (needs the SSH access and sudo password that `scripts/deploy.sh setup`
stores; `push` runs it too):

```bash
scripts/deploy.sh install-root
```

It streams the payload over SSH straight into root-run `tar` (extracted to
`/usr/lib/plippa.new`, never through a plip-writable directory), verifies every file with
`sha256sum` against the developer copy, swaps it into place and runs
`sudo /usr/lib/plippa/bin/plippa-install-root`. On the device alone, re-apply the already
installed tree with `sudo /usr/lib/plippa/bin/plippa-install-root`.

The installer audits `/usr/lib/plippa` first (no symlinked path component, everything owned by
root, nothing group/world-writable, no setuid, no special files) and installs nothing if the
audit fails. Then it installs `/etc/polkit-1/rules.d/50-plippa-{power,network}.rules`, the
PipeWire quantum and radio files, retires any legacy system audio target, installs
`/etc/udev/rules.d/99-ws281x-mem.rules` (and reloads udev), installs
`/etc/systemd/system/plippa.service` and runs `daemon-reload`. It sets no capability: that stays
with `deploy.sh push`. Check the result:

```bash
stat -c '%U:%G %a %n' /usr/lib/plippa/bin/plippa-install-root /etc/polkit-1/rules.d/50-plippa-*.rules
```

---

## Autostart (systemd)

Boots `plippa` (engine + UI + server) in the user session. Unit file:
[`plippa.service`](plippa.service).

### Prerequisites

1. Binaries built under `~/projects/Plippa_PreRelease/target/release/` (the unit's `ExecStart`; a debug build needs the drop-in described in the README "Build profile"; see **Build and run** above).
2. After every `plippa-leds` rebuild: `sudo setcap cap_sys_rawio+ep …/plippa-leds` (and
   `sudo setcap -r …/plippa-ui`); `scripts/deploy.sh push` does both and verifies them
3. Device audio files installed (see **Audio (user session)** above).
4. User linger so `/run/user/1000` exists before login (already typical on this desk):

```bash
loginctl show-user plip -p Linger   # expect Linger=yes
# if no:
sudo loginctl enable-linger plip
```

5. No manual `./target/debug/plippa …` still running (would fight the service for IPC / hardware).

### Install and enable

The unit is installed by the root installer (see **Root-owned install tree** below), never
copied from the plip-owned checkout:

```bash
scripts/deploy.sh install-root        # on the dev machine; or on the device:
sudo /usr/lib/plippa/bin/plippa-install-root   # installs /etc/systemd/system/plippa.service + daemon-reload
sudo systemctl enable --now plippa.service
```

### Supervision, limits and the watchdog

`plippa.service` is `Type=notify` with `WatchdogSec=30`, `Restart=on-failure`, `RestartSec=3`,
`StartLimitIntervalSec=300` / `StartLimitBurst=5`, `MemoryHigh=600M`, `MemoryMax=700M` and
`KillMode=mixed`; the comments in the unit explain each number.

- The launcher restarts a crashed child after 1, 2, 4, 8, 16, then 30 s (back to 1 s after 60 s
  of stable running) and gives a child up after 8 crashes in 5 minutes. `plippa-server` then
  cools down for 10 minutes while engine and UI keep running (indexed items still play); engine
  or UI exhaustion stops everything and the launcher exits 70.
- Exit 0 is only an owner-requested stop (shutdown/power-off sentinel, SIGINT, SIGTERM) and does
  not restart. Every other exit is non-zero (1 could not start, 70 crash budget, 71 internal)
  and restarts the unit, until the start limit trips. After that the unit stays failed until
  `sudo systemctl reset-failed plippa` or a power cycle; the journal
  (`journalctl -u plippa -b`) is the only place it shows.
- The launcher sends `WATCHDOG=1` every 10 s from its supervision loop whatever the children
  are doing, so the watchdog only fires when the launcher itself is hung (systemd then restarts
  the unit). A crashing child is handled by the backoff, the crash budget and exit 70.
- The memory limits are starting values. Measure steady-state use on the device (1,000-item
  library, playback, waveform worker, a 500 MB upload: `systemd-cgtop`, `systemctl status
  plippa`) and never ship a limit below measured use plus headroom.

Optional: pin a single USB library (skips discovery) by editing the installed unit —
uncomment `Environment=PLIPPA_LIBRARY_ROOT=…` — then:

```bash
sudo systemctl daemon-reload
sudo systemctl restart plippa.service
```

### Check if it is up

```bash
systemctl is-enabled plippa.service    # expect: enabled
systemctl is-active plippa.service     # expect: active
systemctl status plippa.service --no-pager
pgrep -a plippa                        # launcher + plippa-engine / plippa-ui / plippa-server
```

### Listen to CLI / journal output

```bash
# follow live (Ctrl-C to stop following; does not stop the app)
journalctl -u plippa.service -f

# last boot / recent lines
journalctl -u plippa.service -b --no-pager
journalctl -u plippa.service -n 100 --no-pager
```

Look for: `plippa: started 3 processes`, `plippa-engine ready`, `plippa-ui: backend=poc2`, portal listen URLs.

### Stop (leave enabled for next boot)

```bash
sudo systemctl stop plippa.service
systemctl is-active plippa.service     # expect: inactive
```

### Start again (without reinstall)

```bash
sudo systemctl start plippa.service
```

### Remove from autostart

Stops now and disables boot start; leaves the unit file in place:

```bash
sudo systemctl disable --now plippa.service
systemctl is-enabled plippa.service    # expect: disabled
```

Fully remove the unit (optional):

```bash
sudo systemctl disable --now plippa.service
sudo rm -f /etc/systemd/system/plippa.service
sudo systemctl daemon-reload
sudo systemctl reset-failed plippa.service 2>/dev/null || true
```

### After rebuilding binaries

```bash
# rebuild as usual (CARGO_BUILD_JOBS=1), then:
sudo setcap cap_sys_rawio+ep ~/projects/Plippa_PreRelease/target/debug/plippa-leds
sudo setcap -r ~/projects/Plippa_PreRelease/target/debug/plippa-ui 2>/dev/null || true
sudo systemctl restart plippa.service
journalctl -u plippa.service -f
```

### Power off / hold-to-power

Holding the **volume encoder push** turns the unit off; a tap does nothing:

| Hold | Result |
|---|---|
| < 1 s | nothing |
| ≥ 1 s | status popup **“Keep holding to power off”**, power LED amber |
| release before 4 s | popup clears, LED back to blue |
| ≥ 4 s | audio pauses, popup **“Powering off…”**, app stops, host powers off |

The 4 s hold writes `$PLIPPA_IPC_DIR/poweroff`; the launcher stops engine / UI / server,
syncs and runs `systemctl --no-block poweroff`. The server checkpoints the SQLite WAL and
closes the library before it exits, so the USB stick is safe to remove once the board has
halted. A plain `sudo systemctl stop plippa.service` writes only `shutdown` and **never**
powers the host off — that is the deploy / restart path. Any sentinel left behind by a
previous run is cleared when the launcher starts, so a crash cannot replay a power-off.

The polkit rules (power-off and the Wi-Fi join from the UI; the service runs as `plip`
without a logind session) are installed into `/etc/polkit-1/rules.d/` by the root installer.
polkitd picks them up immediately; check for syntax errors:

```bash
journalctl -u polkit --since "5 min ago" | grep -i plippa
```

Verify the rule **before** relying on the button (a process running as `plip`):

```bash
runuser -u plip -- pkcheck --action-id org.freedesktop.login1.power-off --process $$
# expect: no output, exit 0
runuser -u plip -- pkcheck --action-id org.freedesktop.NetworkManager.settings.modify.system --process $$
# expect: no output, exit 0 (Wi-Fi rule)
```

The unit ships `Environment=PLIPPA_POWEROFF=1` and `TimeoutStopSec=30`. After pulling
changes to the unit, run `scripts/deploy.sh install-root`, then `restart`.

Machine safety checks:

```bash
# deploy/restart must never power off — stop must leave the board running
sudo systemctl restart plippa.service && echo "still running"

# armed?
systemctl show plippa.service -p Environment | tr ' ' '\n' | grep PLIPPA_POWEROFF
```

**Powering back on with the same button is not implemented on POC2.** After the hold the
CM4 halts, and in halt only GPIO3 / GLOBAL_EN wake it; the volume push sits on the AW9523
I²C expander and GPIO3 is the I²C clock on this board, so the same button cannot wake it in
software. Until a hardware wake route is chosen, power off means halt
(≈1 W, screen/LEDs off) and power on is a power-cycle or a wake button on GPIO3. Do **not**
change `WAKE_ON_GPIO` / `POWER_OFF_ON_HALT` on POC2.

### Troubleshooting: library notice / no sound after autostart

**`Library index not found at /media/plip/<label>/…`**

Often a **stale empty mountpoint** left by udisks (root-owned, not a real mount). Real media is e.g. `/media/plip/<label>`. Check:

```bash
findmnt /media/plip/* 2>/dev/null
ls -la /media/plip/
```

Remove leftovers that are **not** mounted (only empty dirs owned by root):

```bash
# example — only if findmnt shows they are NOT mounted:
sudo rmdir /media/plip/<stale-dir>
```

Or pin the live stick in the unit (`Environment=PLIPPA_LIBRARY_ROOT=/media/plip/<label>`), then `daemon-reload` + `restart`.

Current discovery skips unreadable `/media/…` dirs and paths that are not in `/proc/mounts`.

**Plays but no RCA / `Pulse open failed … using silent mock sink`**

The unit must use a **live** Pulse socket. Session Pulse on this desk:

```bash
PULSE_SERVER=unix:/run/user/1000/pulse/native pactl info
```

If that fails, log in as `plip` or enable linger (below), then check the session services: `systemctl --user status pipewire wireplumber` as `plip`.

Re-install the unit from this repo after pulling the Pulse fix
(`scripts/deploy.sh install-root`), then:

```bash
sudo systemctl restart plippa.service
journalctl -u plippa.service -f
# expect: plippa-engine: PULSE_SERVER=unix:/run/user/1000/pulse/native
# expect: NO "using silent mock sink" on Play/tap
```

---

## Re-apply checklist (existing Pi)

1. Patch `config.txt` / `cmdline.txt` as above
2. Groups; the WS2811 udev rule and the `plippa-leds` capability (both handled by the scripts below)
3. `scripts/deploy.sh install-root` (or `sudo /usr/lib/plippa/bin/plippa-install-root` on the device) → `sudo reboot` → bus verify + `pw-metadata -n settings | grep quantum` (256)
4. Build with `--features pulse` / `--features poc2` / `--features ws281x`, re-`setcap` `plippa-leds`
5. Enable `plippa.service` (installed by step 3; see **Autostart** above) *or* run `plippa` manually for a one-off

---

## This folder

| Path | Purpose |
|------|---------|
| `config/board.toml` | Runtime board profile |
| `plippa.service` | systemd unit (boot autostart), installed by `plippa-install-root` |
| `50-plippa-*.rules` | polkit rules, installed by `plippa-install-root` |
| `audio/pipewire`, `audio/radio` | audio config files, installed by `plippa-install-root` |
| `firmware/` | MCU firmware (none yet — Linux drives GPIO/I2C) |
| `*.csv` | Hardware wiring / LED map |
