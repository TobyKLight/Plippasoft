# Plippa UI threading — I²C service + display/LED workers

Local device I/O for `plippa-ui` on boards that share **i2c-1** between the AW9523 expander and PN532 (plippa-D-poc2). Complements [`ipc.md`](./ipc.md) (engine/server non-blocking) with the same rule for UI-local buses and device present: the coordinator never blocks on SMBus/I²C, **SPI framebuffer push**, or **WS2811 DMA**.

## Roles

```
┌─────────────────────────────────────────────────────────────┐
│ plippa-ui coordinator (main)                                │
│  drain InputEvent / NfcEvent / IPC → UiApp                  │
│  enqueue frame (latest-wins) / LED cues only                │
└───┬──────────────────┬──────────────────▲───────────────────┘
    │                  │                  │ events
    │ frame            │ cues             │
    ▼                  ▼                  │
┌──────────────┐  ┌────────────┐   ┌────────────────────────────┐
│ Display      │  │ LED worker │   │ I²C service thread         │
│ worker       │  │ owns       │   │ owns i2c-1:                │
│ owns ILI9341 │  │ WS2811     │   │  AW9523 via INT (GPIO23)   │
│ SPI present  │  │ ~30 FPS    │   │    + ≤100 ms force poll    │
└──────────────┘  └────────────┘   │  PN532 poll / read / write │
                                   │  + encoder GPIO poll       │
                                   │  bus schedule: SW > NFC    │
                                   └────────────────────────────┘
```

| Role | Thread | Owns | Must **not** do |
|------|--------|------|-----------------|
| Coordinator | main | `UiApp`, IPC `try_recv`/`send`, enqueue display/LED | `read_ports`, PN532, SPI `present`, WS2811 `render` |
| I²C service | one worker | `/dev/i2c-1` for AW9523 **and** PN532; encoder GPIO; AW9523 `int_gpio` | UI model, IPC, TFT, LEDs |
| Display worker | one worker | ILI9341 SPI present / blank | Input drain, IPC, I²C, LED DMA |
| LED worker | one worker | WS2811 strips (compose + render) | Input drain, IPC, I²C, SPI |
| Mock / desktop | in-process | `MockDisplay`, `MockLedCue`, `MockInput`, `NfcBackend::Mock` | Spawn I²C / display / LED threads |

**Chosen layout:** encoders share the **I²C service thread** (GPIO only — no second i2c-1 client).

**Chosen layout:** **separate** display worker and LED worker (not one combined outputs thread). Play-button / tap cues must not wait behind a full SPI present. Mailbox depth **1** (latest frame wins).

**Do not** give AW9523 and PN532 each a thread that both talk to i2c-1.

## Forbidden ops (coordinator)

- `Aw9523::read_ports` / SMBus on i2c-1
- `NfcSession::poll_once` / `write_first_ndef_record` on a live PN532
- Burst loops that `sleep` while polling the expander
- Waiting on associate NFC write completion except by draining a result channel
- `Ili9341Display::present` / SPI wait (enqueue only via `DisplayProxy`)
- `Poc2LedCue` DMA `render` / `tick` body (enqueue only via `LedProxy`)

## i2c-1 schedule

| Cadence | Rule |
|---------|------|
| Expander sample | **IRQ-driven** on board `[expander].int_gpio` (active-low INTN): wake on falling edge, then `read_ports` (clears INT). Soft debounce still in software (`[inputs].debounce_ms`). Force I²C sample ≥ every 100 ms if INT is quiet. Encoders still polled on a ≤5 ms cadence |
| NFC poll | **Command-driven:** coordinator sends `PollTick` at board `[nfc].poll_hz` (default 5 Hz); service runs one short `InList` (board `[nfc].inlist_timeout_ms`, default 25 ms) only after enough expander turns |
| Associate write | One uninterruptible PN532 write critical section. Before starting: expander samples. After: resume INT-driven expander cadence |
| Priority | Expander always first; defer NFC until ≥~100 ms of expander-only turns since last NFC |

### Slice policy

- **Real PN532 field check:** empty-field wait capped by `[nfc].inlist_timeout_ms` (board default 40 ms). On miss: **expander sample, then WUPA+InList** as a second slice in the same PollTick (not one stacked critical section, and not waiting for the next 200 ms tick). Full NDEF read only after a UID is seen (longer critical section, infrequent).
- **Injected / test NFC:** long ops run as ≤2 ms slices so expander samples continue.
- **Rate limit:** coordinator `PollTick` at `poll_hz` only — `Poc2Nfc` disables the session’s extra ≥200 ms gate so field checks are not skipped.
- **Post-NFC:** service force-samples expander after each NFC slice.

## Display worker

| Item | Policy |
|------|--------|
| Thread | `plippa-ui-display` — sole owner of `Ili9341Display` (or test double) |
| Submit | Coordinator `DisplayBackend::present` / `present_rect` on `DisplayProxy` **replaces** the pending frame (depth-1 mailbox) and returns immediately |
| Backpressure | Latest-frame-wins: a newer present drops any unconsumed pending frame. Never unbounded framebuffer queues |
| Partial present | `UiApp::present` diffs against its previous frame and submits only the changed bounding box, except on the first frame, when nothing changed (no present at all), or when the change reaches ≥ 60 % of the pixels **outside** the waveform band (full frame). The band is `CONTENT_BOTTOM − CONTENT_TOP` = 183 of 240 rows ≈ 76 % today, so a screen-relative guard would send a full frame for every scrolling band frame; measuring outside the band means a band-only change can never trip it |
| Blank / Stop | `Blank` then `Stop` on shutdown; worker blanks (backlight off) before exit |
| Mock | `MockDisplay` stays synchronous in-process — no display thread in CI |

### Partial presents

The Now Playing waveform scrolls at ~30 FPS, so most frames change only a band of the screen.

- `Rect { x, y, w, h }` (`union`, `area`, `is_empty`, `clamped`) travels with each submit.
- A **full** submit carries `None` (whole frame); a windowed submit carries `Some(rect)`.
- The mailbox keeps `(FrameBuffer, Option<Rect>)`. When a submit replaces a frame that has
  not been consumed yet, `MailboxState::enqueue` takes the pending rect, **unions** it with the
  new one and only then replaces the pending frame (a full frame clears it to `None`). The
  newest frame is a complete composition, so painting the union from it covers every region
  the worker will ever paint from the queued frames — the dropped frame cannot leave stale
  pixels on the panel.
- `DisplayBackend::present_rect` defaults to `present` (software/mock backends stay correct).
  `MockDisplay` composites the rect into its accumulated `screen` and records each rect, so a
  frame sequence can be compared against the app's own framebuffer.
- `hw/ili9341.rs` sets a CASET/RASET window for the rect and streams only those rows through
  a second pixel buffer (`rect_scratch`, allocated once at `width × height` so any rect can be
  staged without reallocating); the full-frame path uses the separate `scratch` buffer.

### Memory note (1 GB)

RGB565 240×320 ≈ **150 KiB** per framebuffer. Budget:

| Buffer | Count | ≈ |
|--------|-------|---|
| `UiApp` compose frame | 1 | 150 KiB |
| `UiApp` `prev_frame` (diff baseline) | 1 | 150 KiB |
| Display mailbox pending | ≤1 | 150 KiB |
| ILI9341 full-frame scratch (worker) | 1 | 150 KiB |
| ILI9341 rect scratch (worker) | 1 | 150 KiB |

≈ **750 KiB** peak for the TFT path — acceptable. Do **not** deepen the mailbox, and do not
drop `prev_frame`: without it every frame is a full present.

## LED worker

| Item | Policy |
|------|--------|
| Thread | `plippa-ui-led` — sole owner of `Poc2LedCue` / WS2811 controller |
| API | Coordinator sends `LedCmd`: `OnTapFeedback`, `SetPlayButton`, `ClearAll`, `Stop` via bounded `sync_channel` (cap 32). `LedProxy::tick` is a no-op — worker self-paces |
| Rate | Event-driven coalesce + max **~30 FPS** (`FRAME_INTERVAL` ≈ 33 ms) |
| Optimistic play | `SetPlayButton` may be sent on `PlayStop` **before** `PlaybackStatus` arrives |
| Clear | Worker runs `clear_all` on `Stop` / `ClearAll` (and on `Drop` of owned cue) so SIGINT/SIGTERM leave strips dark |
| Mock | `MockLedCue` synchronous — no LED thread in CI |

## Channels

| Channel | Bound | Producer → consumer | Backpressure |
|---------|-------|---------------------|--------------|
| `I2cCmd` | 16 (`sync_channel`) | coordinator → service | `try_send`; on full drop redundant `PollTick`, keep `Stop` / write / set-loaded |
| `InputEvent` | 256 | service → coordinator | On full: **drop encoder deltas**; if a button edge cannot enqueue, drop one queued rotate then retry; log once per overflow burst |
| `NfcServiceEvent` | 32 | service → coordinator | On full: drop oldest event (log); prefer delivering `WriteResult` |
| Display mailbox | depth **1** | coordinator → display worker | Replace pending frame; union the new rect with the pending one (full frame clears it); drop stale |
| `LedCmd` | 32 | coordinator → LED worker | `try_send`; on full drop redundant ticks/cues, prefer delivering `Stop` / `ClearAll` / latest play state via replace-on-drain |

### Command / event types (logical)

```text
I2cCmd:
  Stop
  PollTick
  WriteNdef { ndef: Vec<u8> }
  SetLoadedUid(Option<Vec<u8>>)
  MarkFieldLoaded
  // test-only: InjectSlowNfc { duration_ms }

NfcServiceEvent:
  Events(Vec<NfcEvent>)
  WriteResult { ok: bool, error: Option<String> }
  WorkerGone

DisplayCmd / mailbox:
  Present(FrameBuffer, Option<Rect>)   // latest wins; None = whole frame
  Blank
  Stop

LedCmd:
  OnTapFeedback(tap_id)
  SetPlayButton(PlayButtonLed)
  ClearAll
  Stop
```

Debounce stays **inside the I²C service** (edge + cooldown from `board.toml` `[inputs].debounce_ms`). The coordinator never samples raw ports.

## Shutdown

Three+ worker join order (no cross-waits that can deadlock):

1. SIGINT / SIGTERM / launcher stop → coordinator `stop_flag` → leave UI loop.
2. **Display + LED first:** send `Stop` (LED also `ClearAll`); join display worker (~500 ms); join LED worker (~500 ms). LEDs must be black before process exit.
3. **I²C service:** `I2cCmd::Stop`; join (~500 ms); drops AW9523 + PN532 (releases i2c-1).
4. No post-prompt stdout after parent tear-down.

Coordinator **never** blocks waiting for a present/render to finish during the hot loop — only timed joins at shutdown. Workers **never** wait on the coordinator (no reply RPCs).

## Failure modes

| Mode | Behaviour |
|------|-----------|
| I²C NACK / AW9523 read fail | Log; skip edges that sample; keep looping |
| PN532 busy / poll error | `NfcServiceEvent` with empty/err log; expander cadence unchanged |
| I²C worker panic | `catch_unwind`; `WorkerGone` if possible; coordinator → safe exit |
| Display / LED worker panic | Log; join returns; hardware may need process restart |
| Flooded input queue | Drop rotates first; see channel table |
| Flooded display presents | Drop intermediate frames (latest wins) |
| Associate vs switches | Write may stall bus; max stall = write duration. Poll ticks deferred while write in flight |

## Race audit

| Item | Resolution |
|------|------------|
| Expander vs PN532 same bus | Single I²C service thread |
| Associate write vs switch sampling | Expander sampled before write; sampling pauses during real write |
| Input overflow under encoder spin | Bounded queue; drop rotates |
| Coordinator vs SPI present | Depth-1 mailbox; coordinator never awaits SPI |
| Coordinator vs LED DMA | Cue channel; worker owns render |
| Display vs LED | Independent threads; no shared locks between them |
| Ctrl-C / SIGTERM | `stop_flag` → leave loop → stop/join display + LED (clear) → stop/join I²C |

## Interaction with IPC

Coordinator still follows [`ipc.md`](./ipc.md): `try_recv` only, fire-and-forget `TapNdef` / PlayStop. Device I/O duration must not delay those sends — inputs arrive pre-debounced on a channel so PlayStop can flush IPC in the same frame without waiting on PN532, SPI, or LED DMA.
