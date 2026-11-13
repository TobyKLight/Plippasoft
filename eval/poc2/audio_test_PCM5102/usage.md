# poc2_audio_test_PCM5102 — PCM5102 DAC smoke test

Plays a short test tone through the PCM5102 I2S DAC (`dtoverlay=hifiberry-dac`).

## Build

This crate is part of the shared Cargo workspace at the repo root (one `target/`
directory for all mini-projects).

```bash
cd ~/projects/Plippasoft
cargo build --release -p poc2_audio_test_PCM5102
```

## Run

```bash
cd ~/projects/Plippasoft
./target/release/poc2_audio_test_PCM5102

# optional: force a specific ALSA card index (from `aplay -l`)
./target/release/poc2_audio_test_PCM5102 2
```
