# warp_bench — WARP eval app for the CM4 (measure it, listen to it, turn it into constants)

`warp_bench` is the standalone POC2 eval app of the WARP series (tempo, pitch-correct speed, cassette FF/RW scan, jog jump, reverse). **You run it on the CM4.** It measures what a cloud box cannot (decode speed, backward-seek cost on your real USB stick, Pulse latency, stretcher CPU, underruns) and lets you **listen to every warp artefact the product will have**, with the production constants as command-line parameters. Then a table maps each result to a named tuning constant.

- **It is not production code.** The voices inside (windowed-sinc varispeed, boxcar+cubic for scan speeds, Signalsmith Stretch, a Pulse sink) are small reference implementations that sound representative of the WARP design. Production lives in `plippa-audio` and does not depend on this crate; this crate depends on no Plippa crate.
- **Conservative defaults ship.** The `Default` impls of the config structs hold conservative values. After running this app you can change them according to section 6 below.
- **Nothing here is a verdict from the cloud.** The cloud only built the app and unit-tested its pure logic (argument parsing, signal generators, the scan/chase rate profiles by simulation, the `suggest` arithmetic, the byte-counting `MediaSource`, report formatting). Numbers printed by a debug build or on x86 are meaningless and must not be quoted.

Contents: 1 What it is · 2 Build and deploy · 3 Preparing the CM4 · 4 Sub-commands and the numbers to read · 5 What to listen for · 6 From results to constants · 7 Results sheet, run order, time budget · 8 Limits of the eval app

## 1. What it is

| Sub-command | Measures / plays | Needs |
|---|---|---|
| `info` | CPU model, governor, current/max MHz, temperature, `vcgencmd get_throttled`, kernel, memory, Pulse server/sink, release or debug build | - |
| `decode <file...>` | Symphonia decode speed (x realtime, median and min of 5 runs) per format / rate / channel count | media files |
| `usbread <file>` | sequential MB/s, random 4 KiB read latency; derives the estimated MP3 index build time and the estimated indexed-seek cost | a large file on the stick |
| `seek <file...>` | forward and backward `Accurate` seek at 10/50/90 %: wall time, bytes read, landing offset | the 1-hour MP3, a FLAC, a WAV |
| `latency` | `Simple::get_latency` and underruns for tlength 60/80/120/160/240 ms | audio output, `--features pulse` |
| `cpu` | CPU % of one core for varispeed, boxcar+cubic, stretcher (speed voice and seek voice, 0.5x-4x) and the p99 time of one 256-frame block | - |
| `all <file...>` | `info`, `decode`, `usbread`, `seek`, `latency`, `cpu`, then `suggest` | all of the above |
| `suggest [results.txt]` | turns the measured numbers into suggested constants (pure arithmetic) | a results file |
| `listen speed` / `scan` / `reverse` / `jump` / `xfade` | plays each warp artefact through Pulse and prints what to listen for | audio output |
| `gen tone\|chirp\|clicks\|sweep\|music <out.wav>` | synthetic test signals where artefacts are obvious | - |
| `sheet [--append]` | prints / appends the blank listening sheet | - |

Every sub-command has `--help`. Measurement sub-commands print a table and append a section (`== info ==`, `== decode ==`, ...) to `warp_bench_<date>.txt` in the current directory (or `--report FILE`); every section also holds machine-readable `key = value` lines that `suggest` reads. The file ends with an empty `== listening ==` sheet (section 7) that you fill in.

## 2. Build and deploy

### Primary: cross-build on the dev machine, push to the Pi

On the dev machine that already has the deploy setup (`scripts/deploy.sh setup` done once; see the README Build and deploy section). One-time packages (the stretcher is C++):

```bash
rustup target add aarch64-unknown-linux-gnu
sudo apt install gcc-aarch64-linux-gnu g++-aarch64-linux-gnu libclang-dev python3 rsync openssh-client
```

Then, from the repo root:

```bash
cd ~/Plippasoft && git pull origin main
PROFILE=release scripts/deploy.sh eval warp_bench
```

This runs `cargo build --release --target aarch64-unknown-linux-gnu -p poc2_warp_bench --features pulse` with the same cross environment as `scripts/deploy.sh build` (sysroot of the Pi's libraries, `CXX_aarch64_unknown_linux_gnu=aarch64-linux-gnu-g++`), then rsyncs **only that binary** to the Pi. It does **not** restart `plippa.service` and does not run `setcap`. The binary lands at:

```
~/projects/Plippasoft/target/release/warp_bench      # on the Pi (PI_DIR/target/release)
```

**Use a release build for every number.** The default profile of `scripts/deploy.sh` is `debug`; a debug warp_bench is 10-50x slower, `info` prints a DEBUG warning and `suggest` refuses to suggest anything from a debug run.

### Alternative: native build on the CM4 (slow; prefer the cross-build)

The C++ stretcher compiles on a 1 GB board, so build one job at a time and expect many minutes:

```bash
sudo apt install -y libpulse-dev pkg-config build-essential libclang-dev
cd ~/projects/Plippasoft && git pull origin main
CARGO_BUILD_JOBS=1 cargo build --release -p poc2_warp_bench --features pulse
# binary: ~/projects/Plippasoft/target/release/warp_bench
```

Without `--features pulse` the app still builds and runs every measurement except `latency`, and the listening tests can only render to a file (`--out file.wav`).

## 3. Preparing the CM4 for honest numbers

1. **Stop the product** so nothing else uses the CPU or the sink (restart it afterwards):
   ```bash
   sudo systemctl stop plippa.service
   systemctl is-active plippa.service      # expect: inactive
   ```
   Nothing else heavy should be running (no `cargo`, no browser, no `apt`). Keep the SSH session itself light.
2. **Governor and clocks: do not change them.** The product runs with the board default; `warp_bench info` records the governor and the current/max frequency so the numbers can be interpreted.
3. **Temperature and throttling.** Run `warp_bench info` (or `vcgencmd get_throttled` and `vcgencmd measure_temp`). `throttled=0x0` is good. **Anything else invalidates the run** (under-voltage or thermal limit was seen since boot): fix the supply/cooling, cool down, `sudo reboot` to clear the flags, and repeat. `suggest` refuses to suggest constants from a run whose `info` shows a non-zero throttled value. Wait until the temperature is below about 65 C before the `cpu` run; run it twice if it climbs.
4. **Audio server for the listening tests and `latency`.** The product service talks to the desktop-session PipeWire-Pulse (`/run/user/1000/pulse/native`). From an SSH shell set the same environment first, and check it:
   ```bash
   export XDG_RUNTIME_DIR=/run/user/1000
   export PULSE_SERVER=unix:/run/user/1000/pulse/native
   pactl info | grep -E "Server Name|Default Sink"     # must answer; the default sink is the RCA output
   ```
   (If you changed the service's `PULSE_SERVER`, use the same value. A stale `/run/plippa-audio/...` socket gives `Connection refused`.)
5. **Where to run.** Run the binary from the SD card (it lives there) in a results directory, but keep the **test media on the USB stick**:
   ```bash
   mkdir -p ~/warp_results && cd ~/warp_results
   ```
6. **Test media on the stick** (the sets under `assets/testMedia` cover the formats): a **3-minute MP3**, a **FLAC**, a **WAV**, and a **1-hour MP3** (mix or audiobook, ideally VBR; this is the worst case for backward seeks). Use files at the sample rates the product plays (44.1 and/or 48 kHz); `decode` reports each file's rate, and the worst result over all files you pass counts. Find the stick:
   ```bash
   findmnt -rno TARGET,SOURCE,FSTYPE | grep -E "/media|/mnt"
   lsblk -o NAME,SIZE,TYPE,MOUNTPOINT,FSTYPE,MODEL
   ls /media/plip/                     # e.g. /media/plip/<label>
   ```
   In the examples below `$STICK=/media/plip/<label>`.
7. **Drop the caches before `usbread` and `seek`** (the app also asks the kernel to drop each file's cached pages, but do this too):
   ```bash
   sync; echo 3 | sudo tee /proc/sys/vm/drop_caches
   ```
8. The report file for the day is `warp_bench_<date>.txt` in `~/warp_results`. Re-running a sub-command appends a new section; `suggest` always uses the **last** value of each number.

## 4. Sub-commands: what each does, how long, which numbers to read

The "good / worrying" columns are rules of thumb taken from the mapping table of section 6 (for example: 8x scan needs `D x 0.5 >= 8`). The authoritative step is `suggest`. All example outputs below show **format only**; `NN` marks a number you will read off.

Options are listed with `--help`. Common ones: `--report FILE` (where to append), `--seconds`, `--runs`, `--reps`.

### 4.1 `info` (a few seconds)

```bash
~/projects/Plippasoft/target/release/warp_bench info
```

```
item           value
-------------  -----------------------------------
cpu_model      Raspberry Pi Compute Module 4 ...
governor       NN
cur_mhz        NN
max_mhz        NN
temp_c         NN.N
throttled      0x0
build_profile  release
build_pulse    yes
pulse_server   PulseAudio (on PipeWire ...)
pulse_default_sink  <your RCA sink>
```

| Read | Good | Worrying |
|---|---|---|
| `throttled` | `0x0` | anything else: **run invalid**, cool down/fix supply, reboot, repeat |
| `build_profile` | `release` | `debug`: **run invalid**, rebuild with `PROFILE=release` |
| `build_pulse` | `yes` (needed for `latency` and listening) | `no` |
| `temp_c` | below 65 | above 70 (the app prints a warning) |
| `pulse_server` / `pulse_default_sink` | answers, default sink is the RCA output | `n/a`: fix `PULSE_SERVER` (section 3) |

### 4.2 `decode <file...>` (about 1 minute per file)

```bash
warp_bench decode "$STICK/test3min.mp3" "$STICK/test.flac" "$STICK/test.wav"
```

Decodes the whole file or its first 120 s (`--seconds`), 5 runs (`--runs`), through Symphonia including the f32 conversion, and prints x realtime (audio seconds per wall second): median and minimum.

```
file             codec  rate   ch  audio s  median x  min x  runs
---------------  -----  -----  --  -------  --------  -----  --------
test3min.mp3     mp3    44100  2   120      NN.N      NN.N   NN/NN/...
test.flac        flac   44100  2   120      NN.N      NN.N   ...
test.wav         ...
D (worst median) = NN.Nx realtime.
```

The **worst median over all files is `D`** in the mapping table. Run 1 may be slower (cold file); the median ignores it. Good: `D >= 16` (allows the 8x scan with the 0.5 safety factor); worrying: `D < 8` (scan capped at 4x or lower). Also read `min x`: a big gap to the median means jitter (USB, CPU frequency).

### 4.3 `usbread <file>` (under a minute; use the 1-hour MP3)

```bash
sync; echo 3 | sudo tee /proc/sys/vm/drop_caches
warp_bench usbread "$STICK/onehour.mp3"
```

Reads up to `--mb` (default 512) MB sequentially, then `--reads` (default 400) random 4 KiB blocks, and derives:

- **est. MP3 index build time `B`** = size / (MB/s) x 1.5
- **est. indexed-seek cost `S_i`** = p95 random read + decode time of 3 frames

| Read | Good | Worrying |
|---|---|---|
| sequential read (MB/s) | above 30 | below 10 (a 1-hour MP3 index then takes over 20 s) |
| random 4 KiB p95 (ms) | below 10 | above 50 |
| `B` (s) | below 20 | above 20: the first rewind within `B` seconds of loading an MP3 uses the slow hold-and-fade path |
| `S_i` (ms) | below 100 | above 300: reverse above about 4x will stall on MP3 |

### 4.4 `seek <file...>` (up to about 10 minutes with a 1-hour MP3)

```bash
sync; echo 3 | sudo tee /proc/sys/vm/drop_caches
warp_bench seek "$STICK/onehour.mp3" "$STICK/test.flac" "$STICK/test.wav"      # --reps 3 to cut the time
```

For each file: forward and backward `Accurate` seek at 10 %, 50 %, 90 %, 5 reps each. Before each timed seek the reader is parked 5 s before (forward) or after (backward) the target; the timed part ends when the first decoded frame at the target is available. The app drops the file's page cache before each rep.

```
file        dir   at   median (ms)  max (ms)  MiB read  seeks  landing offset (frames)
----------  ----  ---  -----------  --------  --------  -----  -----------------------
onehour...  fwd   10%  NN           NN        NN.N      N      NNNN
onehour...  back  90%  NN           NN        NN.N      N      NNNN
S_a (worst backward median) = N.NN s
```

This is the **worst case before the MP3 index exists** and the cost of any WAV/FLAC seek. The backward rows on a long MP3 are the interesting ones: `MiB read` growing with the target position shows the demuxer restarting from byte 0 and decoding forward. `landing offset` is how many frames the decoder lands before the requested frame (it must be trimmed); it is a packet size, not an error. `S_a` is the largest backward **median**. Good: `S_a < 0.3 s`. Worrying: `S_a > 2 s` (typical for an index-less long MP3; it only matters before the index is built).

### 4.5 `latency` (about 1 minute; pulse build)

```bash
warp_bench latency            # --load 2 runs two busy threads meanwhile: "the product under load"
```

For each `tlength` in 60/80/120/160/240 ms (`minreq = tlength / 4`, the same `BufferAttr` the product will use) writes 10 s (1 s silence, 8 s quiet tone, 1 s silence) to the real sink and reports `Simple::get_latency` min/avg/max and the **underrun count**.

```
tlength (ms)  minreq (ms)  latency min  avg   max (ms)  underruns  samples
------------  -----------  -----------  ----  --------  ---------  -------
60            15           NN.N         NN.N  NN.N      N          NNN
80            20           ...
```

The simple Pulse API has no underflow callback, so underruns are **detected from the latency**: the app learns the steady latency from the first samples and counts one underrun each time it collapses below 35 % of that (and re-arms above 70 %). Treat a non-zero count as "the buffer ran dry" and listen to confirm (a dropout or tick). Good: the smallest `tlength` with 0 underruns, and its `avg` is the sink latency. Worrying: every `tlength` underruns (CPU load, PipeWire quantum). Run it once idle and once with `--load 2`.

### 4.6 `cpu` (2-5 minutes)

```bash
warp_bench cpu               # --seconds 20 (default); --only stretch_seek to repeat a subset
```

Renders 20 s of audio offline per case and reports the process CPU time as **% of one core** plus the **p99 and max time of one 256-frame block** and the p99 as a percentage of the block deadline (256 / rate = 5.8 ms at 44.1 kHz):

| Cases | Meaning |
|---|---|
| varispeed sinc at 0.25 0.5 1.5 2.0 -1.0 | the speed voice in pitch-shift mode, and reverse |
| boxcar+cubic at 4 and 8 | the scan voice above 2x |
| stretcher "speed voice" at 0.5 1.0 1.5 2.0 3.0 4.0 (default preset) | pitch-correct tempo and what the stretcher costs at input rate r |
| stretcher "seek voice" at the same rates | the rate wobbles every block and the stretcher is re-primed every 2 s (jump/scan with `seek_warp_mode` = pitch correct) |
| stretcher "speed voice (cheaper)" at 0.5 1.0 2.0 | the cheaper preset, for the fallback rule |

```
voice               rate  CPU % of one core  p99 block (us)  p99 % of deadline  max block (us)
------------------  ----  -----------------  --------------  -----------------  --------------
stretch seek voice  2.0   NN.N               NNNN            NN                 NNNN
```

Budget: the **stretcher alone <= 25 % of one core**; audio thread total below 50 % (the app measures the voices, you judge the total with the product running). A `p99 % of deadline` near 100 means audible dropouts. Good: stretcher at 2.0 / 3.0 / 4.0 all below 25 %. Worrying: 2.0 above 25 % (pitch-correct seek is then capped at 2x; see the flags `suggest` prints). Let the board cool between runs if `info` shows the temperature climbing; a hot board throttles and lies.

### 4.7 `all` (about 30 minutes; the measurement run in one go)

```bash
cd ~/warp_results
warp_bench all "$STICK/test3min.mp3" "$STICK/test.flac" "$STICK/test.wav" "$STICK/onehour.mp3"
```

Runs `info`, `decode` (all files), `usbread` (the largest file), `seek` (all files), `latency` (skipped with a note in a non-pulse build), `cpu`, then `suggest`, and appends the blank listening sheet. `--quick` shortens everything (smoke test only, not for numbers). Remember `sync; echo 3 | sudo tee /proc/sys/vm/drop_caches` first.

### 4.8 `suggest [results.txt]` (instant)

```bash
warp_bench suggest                       # newest warp_bench_*.txt in the current directory
warp_bench suggest warp_bench_2026-10-05.txt
```

Prints the suggested constants with the reason for each, **FLAGS** (things that need a decision) and NOTES (missing measurements), and appends `== suggest ==` to the file. Flags you may see: `THROTTLED` / `DEBUG BUILD` (invalid run, no constants suggested), `DECODE TOO SLOW` (D x 0.5 below 2, no scan rung is fast enough), `STRETCH OVER BUDGET AT 2x`, `SPEED STRETCH OVER BUDGET` (set `SPEED_WARP_DEFAULT` back to `PitchShift`), `NO BACK WINDOW FITS` (MP3 reverse above N x will stall), `HISTORY CLIPPED`, `INDEX BUILD SLOW`, `LATENCY` (every tlength underran). You can edit a `key = value` line in the file by hand and run `suggest` again to see the effect.

### 4.9 `gen` and `sheet`

```bash
warp_bench gen chirp chirp.wav --seconds 10     # also tone, clicks, sweep, music; --rate 44100 --channels 2
warp_bench sheet                                 # print the blank listening sheet
```

`tone` = 1 kHz sine; `chirp` = log chirp 100 Hz-18 kHz (repeats every 10 s); `clicks` = click train every 100 ms; `sweep` = stepped sine sweep 100 Hz-16 kHz; `music` = deterministic music-like material. All listening tests use `--signal chirp|tone|clicks|sweep|music` when no file is given (default `chirp`).

## 5. What to listen for

Run the listening tests **with the product stopped, from the SSH shell with `PULSE_SERVER` set (section 3)**, over the same output chain you use for the product. Use a real music file with drums and sustained notes **and** a synthetic signal (`--signal chirp`, `--signal clicks`) where artefacts are obvious: run each test twice. Each test prints which case is playing and a **LISTEN FOR** hint, separates cases with a short beep (`--no-beep` to drop), and prints the **number of underruns in that run and the total at the end**. `--out file.wav` renders to a file instead of the audio output (for smoke tests, or to copy a clip off the board and listen elsewhere); a file render has no real-time sink, so it counts no underruns and cannot show stalls.

Common options of all listening tests: `[file]` (a media file; default is a generated signal), `--signal`, `--start S` (position in the source), `--max-load-s 180` (how much of the file is decoded into memory), `--preset default|cheaper` (stretcher preset), `--out file.wav`, `--tlength-ms 80 --minreq-ms 20` (Pulse buffer, the shipped `PulseConfig` values), `--no-beep`.

Fill the **verdict** columns with `ok`, `minor` or `bad` plus a few words (what, when) and paste the sheet back (section 7).

### 5.1 `listen speed` (varispeed and pitch-correct at constant speeds)

```bash
warp_bench listen speed "$STICK/test3min.mp3" --mode shift
warp_bench listen speed "$STICK/test3min.mp3" --mode correct
warp_bench listen speed --signal chirp --mode shift       # aliasing is easiest to hear on the chirp
warp_bench listen speed --signal clicks --mode correct    # transient smear is easiest to hear on clicks
```

Plays `--rates 0.1,0.25,0.75,1.25,1.5,2.0` (10, 25, 75, 125, 150, 200 %) for `--seconds 10` each; the tempo moves from one rate to the next with a first-order lag `--tau-tempo-ms 120` (`TAU_TEMPO_MS`) so you also hear the ramps. 6 cases x 10 s, about 70 s per mode.

**`--mode shift` (varispeed): pitch follows speed (expected, tape-like).**

| Rate | Listen for | Verdict |
|---|---|---|
| 10 % | muddy low rumble; steppy / zipper pitch glide on the way down; ticks | |
| 25 % | same; dull sound | |
| 75 % | dull treble loss; ticks | |
| 125 % | metallic fizz or "birdie" tones on the chirp/sweep (aliasing); dull treble loss | |
| 150 % | same, stronger | |
| 200 % | same, strongest | |

| Likely cause | Constant |
|---|---|
| alias / fizz at 125-200 % | the sinc cutoff table (`RateConfig`) |
| zipper / steppy glide on rate ramps | `TAU_TEMPO_MS` smaller steps (`RateConfig`; try `--tau-tempo-ms` to hear other values) |

**`--mode correct` (stretch): pitch must stay constant (expected).**

| Rate | Listen for | Verdict |
|---|---|---|
| 10 % | "underwater" sound; repeating "stutter" grains; bass wobble; extra delay before the sound starts | |
| 25 % | same | |
| 75 % | phasiness / flanging / chorus on sustained notes; stereo image narrowing or drifting | |
| 125 % | transient smear (drums lose their attack, pre-echo); warble or metallic voice | |
| 150 % | same, stronger | |
| 200 % | same, strongest; "machine-gun" repeats | |

| Likely cause | Constant |
|---|---|
| extremes (10 %, 200 %) are poor | narrow `TEMPO_MIN_PERMILLE` / `TEMPO_MAX_PERMILLE` for pitch correct (`StretchConfig`, `tempo_range`) |
| smear at fast rates | lower `MAX_SEEK_STRETCH_*` (`StretchConfig`) |
| phasiness | `STRETCH_PRESET`, `STRETCH_BLOCK_FRAMES` (`StretchConfig`); try `--preset cheaper` to hear the cheaper preset |

### 5.2 `listen scan` (FF/RW hold, the cassette sound)

```bash
warp_bench listen scan "$STICK/test3min.mp3" --mode shift --dir fwd
warp_bench listen scan "$STICK/test3min.mp3" --mode shift --dir rev
warp_bench listen scan "$STICK/test3min.mp3" --mode correct --base 2 --boost 2          # correct, 2x
warp_bench listen scan "$STICK/test3min.mp3" --mode correct --base 3 --boost 3          # 3x
warp_bench listen scan "$STICK/test3min.mp3" --mode correct --base 4 --boost 4 --max-scan-stretch 4     # 4x
```

The program: 1 s of normal playback, **press**, spin up to `--base` (4x) with lag `--tau-up`, after `--boost-after` seconds (2) towards `--boost` (8x) with lag `--tau-boost`, **release** after `--hold` seconds (6), brake back to 1.0x with lag `--tau-down`, hand over to normal playback, 2 s tail. The scan speed is absolute source speed. The text output gives the press/release positions, the peak rate and what the formulas predict for the travel distance and brake time (rendered and predicted agree; if you change a constant you hear the new value). In `--mode correct` the voice is the stretcher, forward only and capped at `--max-scan-stretch` (3); **backward scans are always varispeed whatever the mode** (the app says so). The scan is capped at `--max-scan-vari 8` otherwise.

| Test | Listen for | Verdict |
|---|---|---|
| shift, fwd | smooth rising whine (the expected cassette sound); steps or zipper in pitch (ramps too coarse); a **click** at the press or at the release hand-over; hiss / aliasing at 8x; **dropouts or stutter** (underruns count); brake too abrupt or too lazy; a position jump at the release | |
| shift, rev | the same, backwards; a glide through zero speed at the release (the brake goes from -8x through 0 to +1x) | |
| correct, 2x | constant pitch (expected); smeared / "machine-gun" repeats; swishing | |
| correct, 3x | same | |
| correct, 4x | same | |

| Likely cause | Constant |
|---|---|
| steps / zipper | `TAU_UP_S` (`ScanProfile`; try `--tau-up`) |
| click at start or release hand-over | `XFADE_VOICE_MS` (and `XFADE_BYPASS_MS`, `XfadeConfig`; try `--xfade-voice-ms`, `--xfade-bypass-ms`) |
| dropouts at 8x | lower `MAX_SCAN_VARI` (`ScanProfile`; try `--max-scan-vari 4`); check the `decode` numbers |
| brake too abrupt / too lazy | `TAU_DOWN_S` (`ScanProfile`; try `--tau-down`) |
| spin-up too slow/fast, 4x/8x feel | `TAU_UP_S`, `TAU_BOOST_S`, `SCAN_BASE`, `SCAN_BOOST`, `BOOST_AFTER_S` |
| correct-mode smear | `MAX_SEEK_STRETCH_SCAN` (`StretchConfig`) |

### 5.3 `listen reverse` (backwards from the file, no index)

```bash
warp_bench listen reverse "$STICK/onehour.mp3" --start 60       # early in the file: cheap seeks
warp_bench listen reverse "$STICK/onehour.mp3" --start 1800     # mid-file: the worst case for an index-less MP3
warp_bench listen reverse "$STICK/test3min.mp3" --back-window-s 4
```

Plays backwards at `--rates -0.25,-0.5,-1,-2` for `--seconds 10` each. The audio comes from **back-windows**: blocks of `--back-window-s` seconds (default 2, `BACK_WINDOW_S`) read by an **`Accurate` seek per block on a worker thread, no index**: the worst case before the MP3 index exists. The app times every block fetch, prints the median/p95/max and counts **stalls** (the next block was not ready, the output goes silent until it is). Reverse is always varispeed (pitch follows speed; no pitch-corrected reverse exists).

| Rate | Listen for | Verdict |
|---|---|---|
| -0.25 | speech/music backwards, pitch follows speed (expected); **clicks at every block seam** (every `BACK_WINDOW_S` of source); stutter or stall at block boundaries; silence gaps | |
| -0.5 | same | |
| -1 | same | |
| -2 | same; stalls appear first here | |

Read off the table printed at the end: **fetch median / p95 / max (ms)** against the budget `BACK_WINDOW_S / rate` (a 2 s block at 2x must be fetched in well under 1 s), the **stall count** and the **silent seconds**.

| Likely cause | Constant |
|---|---|
| seam clicks | `XFADE_*` (`XfadeConfig`) |
| stalls / stutter at block boundaries (slow backward seek, no index) | `BACK_WINDOW_S` up, `HISTORY_S` up (`WindowConfig`; see `seek` and `usbread`); or lower `MAX_SCAN_VARI` for MP3 |
| silence gaps | same |

### 5.4 `listen jump` (the jog commit swoop)

```bash
warp_bench listen jump "$STICK/test3min.mp3"                       # shift, 0.1/1/5/10/60 s forward and back
warp_bench listen jump "$STICK/test3min.mp3" --mode correct        # forward jumps use the stretcher, backward stay varispeed
warp_bench listen jump "$STICK/test3min.mp3" --dist 1,10 --t-catch 0.3 --r-max-vari 6
```

For each distance and direction: 1.5 s of normal playback, **one** smoothed jump to the target with the chase controller, 1.5 s of normal playback at the target. Near targets are a short varispeed swoop (a chirp that ends exactly at the target). Targets beyond the far-jump threshold (`R_MAX x CHASE_MAX_TIME_S` = 6.8 s with the defaults) do a **crossfaded waypoint jump** to `target - dir x WAYPOINT_LEAD_S` and then chase the last 3 s audibly. With a file, the waypoint audio is **really decoded from the file** on a worker thread with its own file handle, so a slow USB seek shows up as a late waypoint ("TOO LATE" if the chase arrived first). The table at the end lists per jump: path (chase / waypoint), voice, **swoop (ms)** (the audible chirp length), **what the formulas predict (ms)**, peak rate, landing error in frames, and **waypoint ready (ms)** (how long the file took).

| Case | Listen for | Verdict |
|---|---|---|
| 0.1 s fwd / back | a very short chirp ending exactly at the target; too violent? | |
| 1 s fwd / back | a short chirp; chirp too long / too short | |
| 5 s fwd / back | a quick tape-wind feel; overshoot / wobble at the end | |
| 10 s fwd / back | waypoint jump: **double-audio blend or click at the waypoint**, or a silence gap while the file seeks | |
| 60 s fwd / back | same | |
| correct, 1 / 5 / 60 s fwd | the same swoop with constant pitch (smear at high rates?) | |

| Likely cause | Constant |
|---|---|
| chirp too long / too short / too violent | `T_CATCH_S`, `R_MAX_VARI`, `R_MAX_STRETCH`, `A_DECEL`, `A_SLEW` (`ChaseConfig`) |
| overshoot / wobble at the end | `A_DECEL` up |
| far-jump threshold | `CHASE_MAX_TIME_S` (`ChaseConfig`; or `--far-jump-s` to try a value directly) |
| click at the waypoint | `XFADE_WAYPOINT_MS` (`XfadeConfig`) |
| silence gap / late waypoint | `WAYPOINT_LEAD_S` up (`ChaseConfig`) |

### 5.5 `listen xfade` (crossfade lengths)

```bash
warp_bench listen xfade "$STICK/test3min.mp3"
warp_bench listen xfade --lengths-ms 5,10,20,40,60 --reps 3
```

A hard jump (`--jump-s 30` away) with an equal-power crossfade of 5/10/20/40/60 ms, first over a 1 kHz tone, then over music (the file, else the generated `music` signal). Each length is announced and marked by a beep and makes `--reps` joins, one second apart.

| Length | Over tone | Over music |
|---|---|---|
| 5 ms | click (too short)? | |
| 10 ms | | |
| 20 ms | | |
| 40 ms | audible double / flutter (too long)? | |
| 60 ms | | |

Pick the shortest length without a click. Constants: `XFADE_SEEK_MS`, `XFADE_BYPASS_MS`, `XFADE_VOICE_MS`, `XFADE_WAYPOINT_MS` (`XfadeConfig`).

## 6. From results to constants

`UPPER_CASE` names are the **fields of the named struct's `Default` impl in snake_case** (for example `R_MAX_VARI` is the field `r_max_vari` of `ChaseConfig`).

**To change a constant:** edit that field in the `impl Default` of the named struct (file column), rebuild and deploy (`scripts/deploy.sh`), then re-run the affected checks on the device.

| Struct | File |
|---|---|
| `WindowConfig` | `crates/plippa-audio/src/window.rs` |
| `RateConfig` | `crates/plippa-audio/src/ratestage.rs` |
| `XfadeConfig` | `crates/plippa-audio/src/xfade.rs` |
| `StretchConfig` | `crates/plippa-audio/src/stretch.rs` |
| `PulseConfig` | `crates/plippa-audio/src/sink/pulse.rs` |
| `ChaseConfig` | `crates/plippa-audio/src/seek.rs` |
| `ScanProfile` | `crates/plippa-audio/src/scan.rs` |
| `SPEED_WARP_DEFAULT` (a constant, not a struct) | `crates/plippa-core/src/warp.rs` |

### Result -> constant mapping (`suggest` implements the measurable rows)

| Result (read from) | Rule | Constant (struct, file) |
|---|---|---|
| `decode`: worst median x realtime over MP3/FLAC/WAV at the product's sample rates (`D`) | largest of {2, 4, 6, 8} that is <= `D x 0.5` | `MAX_SCAN_VARI` (`ScanProfile`, `scan.rs`); `R_MAX_VARI` (`ChaseConfig`, `seek.rs`) = min(6, that); `DECODE_SAFETY_FACTOR` (`WindowConfig`) stays 0.5 unless underruns persist, then 0.4 |
| `cpu`: stretcher % of one core at input 2.0 / 3.0 / 4.0 (budget: <= 25% of a core for the stretcher alone, audio thread total < 50%) | largest of {2, 3, 4} within budget for scan, largest of {2, 3} for chase; if even 2.0 is over budget set both to 2.0 and flag | `MAX_SEEK_STRETCH_SCAN` (`StretchConfig`, `stretch.rs`), `MAX_SEEK_STRETCH_CHASE` / `R_MAX_STRETCH` (`ChaseConfig`) |
| `cpu`: stretcher % at 0.5 / 1.0 / 2.0 (speed voice) over budget | switch to a cheaper preset; if still over, set `SPEED_WARP_DEFAULT` back to `PitchShift` (one-line change, WARP README risk 2) | `STRETCH_PRESET`, `STRETCH_BLOCK_FRAMES` (`StretchConfig`); `SPEED_WARP_DEFAULT` (`plippa-core`) |
| `cpu`: varispeed/boxcar at the scan rates over budget | lower the scan caps | `MAX_SCAN_VARI` |
| `usbread` + `seek`: indexed-seek estimate `S_i` (production path once the index is built), `D` | smallest `W` in {1, 2, 4, 8} s with `S_i + W/D <= 0.7 x W / MAX_SCAN_VARI`; if none, use 8 and flag "MP3 reverse above N x will stall: lower `MAX_SCAN_VARI` for MP3" | `BACK_WINDOW_S` (`WindowConfig`, `window.rs`) |
| `seek`: worst `Accurate` backward cost `S_a` (before the index is ready) | `HISTORY_S = max(8, ceil(S_a x MAX_SCAN_VARI))`, keeping `HISTORY_S + LOOKAHEAD_S <= WINDOW_CAP_BYTES / (rate x ch x 4)` (17 s at 44.1 kHz stereo f32): trim `LOOKAHEAD_S` if needed | `HISTORY_S`, `LOOKAHEAD_S` (`WindowConfig`) |
| `usbread`: estimated index build time `B` | informational: first rewind within `B` s of loading an MP3 uses the slow path (hold-and-fade); if `B > 20 s`, raise the index worker priority or stride | `MP3_INDEX_STRIDE_FRAMES` (`WindowConfig`) |
| `latency`: smallest `tlength` with 0 underruns in 10 s, and its average `get_latency` | use it; add 40 ms if the product still underruns under load | `PULSE_TLENGTH_MS`, `PULSE_MINREQ_MS` (`PulseConfig`, `sink/pulse.rs`); `SINK_LATENCY_MS` = measured average |
| `info`: throttled flag not 0 | the run is invalid: cool down, repeat | - |
| listening: `speed --mode correct` extremes poor | narrow the pitch-correct range, e.g. 25%-200% | `TEMPO_MIN_PERMILLE` / `TEMPO_MAX_PERMILLE` (`StretchConfig`, `tempo_range`) |
| listening: clicks or doubling at joins | click -> longer, doubling -> shorter | `XFADE_SEEK_MS`, `XFADE_BYPASS_MS`, `XFADE_VOICE_MS`, `XFADE_WAYPOINT_MS` (`XfadeConfig`, `xfade.rs`) |
| listening: jump swoop too long/violent/short, overshoot | longer chirp -> lower `R_MAX_*`, raise `T_CATCH_S`; overshoot -> raise `A_DECEL` | `ChaseConfig` fields above |
| listening: scan feel | acceleration slower/faster; brake | `TAU_UP_S`, `TAU_BOOST_S`, `TAU_DOWN_S`, `SCAN_BASE`, `SCAN_BOOST`, `BOOST_AFTER_S` (`ScanProfile`) |

Notes on how `suggest` applies the rows: `MAX_SCAN_VARI` also takes the varispeed/boxcar CPU rows into account (the smaller wins); `MAX_SCAN_STRETCH` (`ScanProfile`) is suggested with the same value as `MAX_SEEK_STRETCH_SCAN` (the effective cap is the smaller of the two); `suggest` uses the **seek-voice** stretcher CPU for the scan/chase rows and the **speed-voice** numbers for the speed-voice row; if the cheaper preset fits it suggests `STRETCH_PRESET = cheaper`, otherwise it flags `SPEED_WARP_DEFAULT = PitchShift`; `S_a` is the largest backward median over all files and positions; the window size in seconds is 6 MiB divided by the largest measured `rate x ch x 4` bytes per second.

### Trying a value before it is shipped: eval flag <-> constant

Every constant of `ScanProfile` and `ChaseConfig` can be tried on the command line first, so you hear a candidate before anyone edits a `Default` impl:

| Constant | Flag | Test |
|---|---|---|
| `SCAN_BASE`, `SCAN_BOOST`, `BOOST_AFTER_S` | `--base`, `--boost`, `--boost-after` | `listen scan` |
| `TAU_UP_S`, `TAU_BOOST_S`, `TAU_DOWN_S` | `--tau-up`, `--tau-boost`, `--tau-down` | `listen scan` |
| `MAX_SCAN_VARI`, `MAX_SCAN_STRETCH` | `--max-scan-vari`, `--max-scan-stretch` | `listen scan` |
| `XFADE_BYPASS_MS`, `XFADE_VOICE_MS` | `--xfade-bypass-ms`, `--xfade-voice-ms` | `listen scan`, `listen jump` |
| `T_CATCH_S`, `R_FLOOR`, `A_DECEL`, `A_SLEW` | `--t-catch`, `--r-floor`, `--a-decel`, `--a-slew` | `listen jump` |
| `R_MAX_VARI`, `R_MAX_STRETCH` | `--r-max-vari`, `--r-max-stretch` | `listen jump` |
| `CHASE_MAX_TIME_S` (or the far-jump threshold directly) | `--chase-max-time`, `--far-jump-s` | `listen jump` |
| `WAYPOINT_LEAD_S`, `LAND_EPS_FRAMES`, `XFADE_WAYPOINT_MS` | `--waypoint-lead`, `--land-eps`, `--xfade-ms` | `listen jump` |
| `BACK_WINDOW_S` | `--back-window-s` | `listen reverse` |
| `TAU_TEMPO_MS` | `--tau-tempo-ms` | `listen speed` |
| `STRETCH_PRESET` | `--preset default\|cheaper` | all listening tests |
| `PULSE_TLENGTH_MS`, `PULSE_MINREQ_MS` | `--tlength-ms`, `--minreq-ms` | all listening tests |
| crossfade lengths | `--lengths-ms` | `listen xfade` |

### Handing the results back

1. Copy `~/warp_results/warp_bench_<date>.txt` off the Pi (`scp plip@<pi>:warp_results/warp_bench_*.txt .`), with the **listening sheet filled in** (the `== listening ==` section at the end; fill the `verdict:` lines), plus any constants you tried on the command line and liked.
2. Edit the `Default` impls named by the mapping table in section 6 to match what you measured and heard, and note the date and the board state (`info`: governor, throttled, release build).
3. Redeploy with `scripts/deploy.sh` and re-run the on-device checks of the affected packages.

## 7. Results sheet, run order and time budget

### Run order (about 30 minutes of measurements, 30-45 minutes of listening)

| # | Step | Command | Time |
|---|---|---|---|
| 0 | Prepare (section 3): stop the service, set `PULSE_SERVER`, check `info`, drop caches | | 5 min |
| 1 | Measurements, all in one go | `warp_bench all "$STICK/test3min.mp3" "$STICK/test.flac" "$STICK/test.wav" "$STICK/onehour.mp3"` | about 30 min (of which `seek` on the 1-hour MP3 can be 10 min) |
| 1b | Optional: latency under load | `warp_bench latency --load 2` | 1 min |
| 2 | Listening: speed shift, correct | `listen speed` x2 (music, then chirp/clicks) | about 5 min |
| 3 | Listening: scan | `listen scan` fwd, rev, correct 2x/3x/4x | about 5 min |
| 4 | Listening: reverse | `listen reverse` (early and mid-file) | about 5 min |
| 5 | Listening: jump | `listen jump` shift, then `--mode correct` | about 5 min |
| 6 | Listening: crossfades | `listen xfade` | about 3 min |
| 7 | Fill the sheet, copy the report off, restart the service: `sudo systemctl start plippa.service` | | 5 min |

If a `listen` run shows underruns, run `warp_bench info` again (throttled? hot?) before blaming the code.

### Results sheet template

The same sheet is appended to the report as `== listening ==` (`warp_bench sheet` prints it). Paste the filled version:

```
== listening ==
# Fill the verdict column: ok / minor / bad plus a few words (what you heard, at what time).
# Then paste this whole file back. Artefact names are in the README.
speed shift (varispeed)            10%                    verdict:
speed shift (varispeed)            25%                    verdict:
speed shift (varispeed)            75%                    verdict:
speed shift (varispeed)            125%                   verdict:
speed shift (varispeed)            150%                   verdict:
speed shift (varispeed)            200%                   verdict:
speed correct (stretch)            10%                    verdict:
speed correct (stretch)            25%                    verdict:
speed correct (stretch)            75%                    verdict:
speed correct (stretch)            125%                   verdict:
speed correct (stretch)            150%                   verdict:
speed correct (stretch)            200%                   verdict:
scan shift fwd                     4x then 8x, release    verdict:
scan shift rev                     4x then 8x, release    verdict:
scan correct fwd                   2x                     verdict:
scan correct fwd                   3x                     verdict:
scan correct fwd                   4x                     verdict:
reverse                            -0.25x                 verdict:
reverse                            -0.5x                  verdict:
reverse                            -1x                    verdict:
reverse                            -2x                    verdict:
jump shift fwd                     0.1 s                  verdict:
jump shift fwd                     1 s                    verdict:
jump shift fwd                     5 s                    verdict:
jump shift fwd                     10 s                   verdict:
jump shift fwd                     60 s                   verdict:
jump shift back                    0.1 s                  verdict:
jump shift back                    1 s                    verdict:
jump shift back                    5 s                    verdict:
jump shift back                    10 s                   verdict:
jump shift back                    60 s                   verdict:
jump correct fwd                   1 s                    verdict:
jump correct fwd                   5 s                    verdict:
jump correct fwd                   60 s                   verdict:
xfade over tone                    5 ms                   verdict:
xfade over tone                    10 ms                  verdict:
xfade over tone                    20 ms                  verdict:
xfade over tone                    40 ms                  verdict:
xfade over tone                    60 ms                  verdict:
xfade over music                   5 ms                   verdict:
xfade over music                   10 ms                  verdict:
xfade over music                   20 ms                  verdict:
xfade over music                   40 ms                  verdict:
xfade over music                   60 ms                  verdict:
```

Add below it (free text): the files used (format, sample rate, length), anything that dropped out or clicked that the sheet has no row for, the constants you tried with flags (for example `--t-catch 0.3`), and the underrun count the listening runs printed.

## 8. Limits of the eval app (so you read the results correctly)

- **Reference voices, not production.** Same design (64-tap Kaiser sinc with cutoff tables `0.97 / max(1, rate)`, boxcar+cubic above 2x, Signalsmith Stretch `preset_default`/`preset_cheaper`, block 256), not the same code. CPU numbers are for these voices; production adds the window, decode worker, UI and IPC, which is why the budget leaves headroom (stretcher alone 25 %, audio thread 50 %).
- **Voice hand-overs are aligned exactly.** The stretcher's own latency (about 60 ms in and 60 ms out) is compensated when the eval crossfades between voices; production does this by bookkeeping. A doubling you hear at a hand-over in production but not here is an alignment bug, not a crossfade-length problem.
- **The set speed is 1.0x in `listen scan` and `listen jump`** (the scan is absolute source speed anyway; use `listen speed` for tempo).
- **Underruns are detected from the latency** (section 4.5): a heuristic for the blocking Pulse API. Confirm by ear.
- **The page cache.** `usbread` and `seek` ask the kernel to drop the file's pages (`posix_fadvise`), you drop all caches; a file already read minutes ago from the same stick can still be faster from the stick's own cache.
- **`listen reverse` and the far-jump waypoint decode from the file** (a real Accurate seek per block / per waypoint), so they show the real USB behaviour; on generated signals they run from memory and the seek cost is not represented. The jump's main audio comes from the first `--max-load-s` seconds of the file held in memory.
- **x86 / cloud numbers are meaningless.** The app builds and runs on x86 (every sub-command except `latency` works without Pulse; the listening tests need `--out`), which the cloud used as a smoke test only.
