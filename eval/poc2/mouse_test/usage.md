# poc2_mouse_test — USB mouse event printer

Plug a USB mouse into any port. The tool lists detected USB mice, then prints movement (X/Y) and button changes as events arrive. **Ctrl+C** to exit.

It prefers **hidraw** (raw HID report bytes) when readable; otherwise falls back to **evdev** (same deltas/clicks, no root needed if your user is in the `input` group).

## Build

This crate is part of the shared Cargo workspace at the repo root (one `target/`
directory for all mini-projects).

```bash
cd ~/projects/Plippa_PreRelease
cargo build --release -p poc2_mouse_test
```

## Run

```bash
cd ~/projects/Plippa_PreRelease
./target/release/poc2_mouse_test
```

## Options

| Flag | Description |
|------|-------------|
| `--list` | Enumerate mice only, do not read |
| `--device N` | Read from mouse index `N` shown in the list (default: best match, e.g. a dedicated USB mouse over a macro-pad pointer interface) |

To use hidraw without root, add a udev rule granting your user read access to `/dev/hidraw*`.
