# Plippa IPC — Unix domain sockets + Postcard

Live transport for `IpcMessage` between `plippa-engine`, `plippa-ui`, and `plippa-server`.

## Topology

**Engine is the hub.** It binds and listens; UI and server connect as clients.

```
plippa-ui  ──connect──►  engine.sock  ◄──connect──  plippa-server
                              ▲
                              │ listen
                         plippa-engine
```

| Direction | Messages |
|-----------|----------|
| UI → engine | `TapNdef`, `TapKey`, `PlaybackCommand` |
| Server → engine | `PlaybackCommand`, `LibraryIndexUpdated` |
| Engine → all clients | `PlaybackStatus`, `UserNotice`, `NowPlayingListChanged`, `ResumeWriteback`, `AudioRouteChanged` (broadcast; only the server acts on `ResumeWriteback`) |
| Server → UI (via hub) | `AssociateRequest` (forwarded to peer clients) |
| UI → server (via hub) | `AssociateResponse` (forwarded to peer clients) |

Engine ignores inbound `PlaybackStatus` / `UserNotice` / associate traffic for its own session state; associate frames are only relayed.

## `nowPlayingList` messages

**Versioning rule.** Postcard is positional: new `IpcMessage` / `PlaybackCommand` variants are only ever *appended* (existing variant indices never change; a proto unit test pins them). The three processes ship from one workspace build, so a mixed-version pair is unsupported; struct fields are added at the end.

| Message | Direction | Meaning |
|---------|-----------|---------|
| `PlaybackCommand::LoadCollection { playable_id }` | UI/server → engine | Load an album / playlist / mix / audiobook / podcast / single track into the list (**replaces** it) and play; honours `resumeAtItem` / `resumeFromTime`. The order, repeat mode and pause-after-each come from the **player's** play-mode settings (`SetOrderMode` / `SetRepeatMode` / `SetPauseAfterEach`), never from the file, and a load always clears `RepeatMode::Item` to `Off`. An audiobook or podcast ignores `Shuffle` for that list only (chapters stay in order; the setting is kept). A load allocates a new `list_id` and advances the shuffle seed. `LoadAndPlay` uses the same machinery but always starts at the beginning. |
| `PlaybackCommand::Next` / `Previous` | → engine | Next cue start (all enabled navigable cue lists) else next track. With cue-level shuffle on (below) the cue branch walks the **permutation** instead of the clock, and the last segment of the pass behaves like "last cue": Next moves to the next track, Previous restarts the segment it is inside of. Previous: a first press restarts the current entry (cue, else track) once the playhead is more than 1 s past that entry's start; at the entry start, or within the first second, it steps back one entry instead, so one press per track matches Next. A *subsequent* `Previous` arriving within `PREVIOUS_DOUBLE_PRESS_WINDOW` (3 s, monotonic clock between command arrivals, not media time) always goes to the previous entry (then the state is cleared); the 3 s tracker only pairs presses. Any other transport command (Next, JumpToEntry, Load*/tap, Stop, Unload, SeekMs, SetCueListEnabled) clears the state; Play/Pause do not. Previous is available (`NowPlayingList::can_prev`) whenever the list is non-empty. `NextTrack` / `PreviousTrack` are aliases. |
| `PlaybackCommand::JumpToEntry { index }` | → engine | `index` is a flat index of `NowPlayingListChanged.entries`: a track (loads it) or a cue of the current track (seek, no reload). With cue-level shuffle on, the cue rows are in **play order**, so a cue index is a permutation position and is mapped back to a segment by the engine (`NowPlayingList::jump_to_entry`). |
| `PlaybackCommand::SetCueListEnabled { list_index, enabled }` | → engine | Session-only cue list switch; triggers a new `NowPlayingListChanged`. |
| `IpcMessage::NowPlayingListChanged { entries, current_index, list_kind, list_id }` | engine → all | Sent on load/replace, track change, cue-set change, an order change, and to every newly connected client. `entries` = every track in play order (shuffle already applied) with the **current** track's navigable cue entries directly after it, in the order they will be played (with cue-level shuffle on that is the segment permutation); each `NowPlayingEntry` carries `item_position`, its source `mediaList` position (`0` on cue entries, which are identified by `start_ms`). `current_index` = flat index of the current track entry. `list_id` = identity of the loaded list, assigned once per **load**: reorders and track changes keep it, a replacement allocates a new one (that is how a UI tells "same list, new order" from "new collection"). Capped at 1500 entries. |
| `PlaybackStatus.session.current_entry_index` / `current_cue_index` | engine → all | Current track (index among track entries) and playing cue (index among the current track's cue rows, from the *media* position — with cue-level shuffle on it is the **permutation position**). The highlighted cue row is `current_index + 1 + current_cue_index`. |
| `PlaybackStatus.session.can_prev` / `can_next` | engine → all | Whether Previous / Next are usable now: `can_prev` = a list is loaded (`NowPlayingList::can_prev`); `can_next` = `NowPlayingList::can_next(media position)` (a cue start after the playhead or another available track, or `RepeatMode::Queue`). Computed only by the engine; the device LEDs and the portal render them. |
| `IpcMessage::ResumeWriteback { playable_id, resume_at_item, resume_from_ms }` | engine → server | Persist (or, with both `None`, clear) a long-form resume point. The server writes `.noudata` (`localPlaybackData.resumeAtItem` 1-indexed + item `resumeFromTime`); the engine never writes `.noudata`. |

## Play mode

Playback **order**, **repeat** and **pause after each** are player settings held by the engine (session state, engine memory), not `.noudata` data: Noubin v0.4.1 file keys (`shuffle`, `repeatList`, `stopAfterEach`) are stripped at parse time and are never honoured. The five-rung UI cycle and its wording live in `plippa-core::play_mode`.

| Command | Idx | Meaning |
|---------|-----|---------|
| `SetOrderMode(OrderMode)` | 26 | `Normal` / `Shuffle`. A **reorder is a list operation only**: it never prepares a stream, seeks or changes the playhead. `Shuffle` keeps the playing track first and shuffles the rest; `Normal` restores a fixed list's `mediaList` order around the playing track (the current index is re-derived from `item_position`). The **same value is a no-op** (no reshuffle, no status), so a UI reconcile cannot reshuffle by accident; a fresh order comes from the value changing. On a `SourceOrder::Dynamic` queue, `Normal` stores the setting but cannot reorder and answers with one `NOTICE_NO_FIXED_ORDER`. |
| `SetRepeatMode(RepeatMode)` | 27 | `Off` / `Queue` (wrap at the end of the list) / `Item` (restart the current item at EOF). Any load clears `Item` back to `Off`. Status only (no list change). |
| `SetPauseAfterEach(bool)` | 28 | Pause between items instead of auto-advancing. At the end of an item the transport goes to `Stopped` with the **next item already current at 0:00** (its cues loaded, no stream opened), so one Play starts it. Status only. |
| `SetReshuffleOnRepeat(bool)` | 29 | Default **on**: repeating a shuffled queue starts a fresh pass (never the track that just finished first) — also when the wrap is taken by a manual `Next`, and the wrap is resolved *before* a pause-after-each stop, so the queued item belongs to the new pass. Off: the identical order every pass. Status only. |

`SessionState` appended fields (after `collection_title`, in this order; the four setters and a reshuffle-on-wrap bump `status_revision`):

| Field | Default | Meaning |
|-------|---------|---------|
| `order_mode: OrderMode` | `Normal` | The player's order setting (what the UI shows, even when this list cannot use it). |
| `repeat_mode: RepeatMode` | `Off` | |
| `pause_after_each: bool` | `false` | Pause between items; the next item is current at 0:00 and Play starts it. |
| `reshuffle_on_repeat: bool` | `true` | Owner default On. |
| `source_order: SourceOrder` | `Fixed` | `Dynamic` = a queue built by add-to-queue / play-next: there is no source order to restore, so the Play mode menu says so. |

`NOTICE_NO_FIXED_ORDER` (`Warning`) carries exactly `Dynamic playlist, no fixed order`.

Shell driver: `plippa-np order normal|shuffle`, `repeat off|queue|item`, `pauseafter on|off`, `reshuffle on|off`; its output has a `mode ...` line with the fields above.

## Audio routing

The UI sends the wanted output set; the engine validates it against the current sinks and owns the applied route. The three commands are appended after `SetReshuffleOnRepeat` (29); `AudioRouteChanged` is appended after `UnmatchedNoubin` (10).

| Message | Idx | Direction | Meaning |
|---------|-----|-----------|---------|
| `SetAudioOutputs { enabled, local_only }` | 30 | UI → engine | Replace the enabled output ids and the local-only mode in one atomic step. The engine keeps the local output first, drops unknown ids, caps the set at `MAX_ENABLED_OUTPUTS` and never leaves the set empty while a sink exists. |
| `SetAudioDelayAuto(bool)` | 31 | UI → engine | Automatic per-output latency compensation on/off. While on, the manual delay map is ignored. |
| `SetOutputDelay { output, delay_ms }` | 32 | UI → engine | One output's manual delay in ms; used only while automatic compensation is off. The engine clamps to `DELAY_MAX_MS` and rounds to `DELAY_STEP_MS`. |
| `IpcMessage::AudioRouteChanged(AudioRoute)` | 11 | engine → all | The routed outputs, effective/paused ids, local-only and delay state, diagnostics and revision. Sent on every route change and to each new client on connect. |

## Cue-level shuffle

With `OrderMode::Shuffle` on a **mix or a lone file** that has at least two enabled navigable cues
(albums, playlists, audiobooks and podcasts always reorder whole tracks instead), the cues of the
current item are played as shuffled **segments** in one open stream.

- **Tiling.** Segment *i* runs from its start to the next distinct start; the last runs to EOF.
  Explicit cue `endTime` does **not** bound a segment in this mode, and a playhead inside an
  `endTime` gap belongs to the preceding segment. This is the only visible difference for a file
  with explicit gaps, and it is why a curator's `endTime` appears to be ignored here.
- **`current_cue_index`** is the segment's **permutation position**, and `entries` publishes the
  current item's cue rows in that same order — one order, two readers. With the shuffle off the
  permutation is the identity.
- **Transitions are seeks inside the open stream.** No stream is ever prepared for a segment
  change (`NowPlayingListChanged` is list-only). The engine polls at the fast status cadence, sees
  a boundary inside `CUE_BOUNDARY_LEAD_MS` (60 ms) and seeks to the next permuted segment, so at
  most that much of a tail is cut and none of the next *chronological* segment is heard. Boundaries
  closer than `CUE_BOUNDARY_MIN_SEGMENT_MS` (200 ms) are left alone.
- **The end of the last permuted segment ends the item**, even when minutes of audio remain in the
  file: the hand-over is the end-of-item path (advance, wrap, pause-after-each hold, end of list),
  identical to EOF. `RepeatMode::Item` loops the segment being heard, and pause-after-each stops
  *before* the next segment — it becomes current at its start, as for items.
- A boundary is ignored while a scan/chase, a pending start or an unlanded seek is in flight, so a
  slow jog through a boundary does not jump without warning.

## Warp / tempo / seek messages

Two control families, one state each, both held by the engine control plane (`plippa-engine`). `rate_permille`, `warp_active` and `speed_voice` report the real audio state. Commands are appended in this pinned order (a proto test pins the indices); no floats on the wire; 22 `SetSeekOutsideTrack(bool)` and 23 `SeekToItem{item_delta,target_ms}`. There is no `SeekRelative` and no `SeekCancel`: the UI owns the live jog target and sends the absolute target once, on commit.

| Command | Idx | Meaning |
|---------|-----|---------|
| `SetRate { permille: i32 }` | 15 | Speed family: session-wide set speed, signed. Accepts `100..=2000` or `-2000..=-100`; `0` and out-of-range are rejected (logged, no state change). Negative values are accepted by the engine. |
| `ResetRate` | 16 | Set speed back to `1000` **only** (does not touch scan, `seek_target_ms` or either warp mode). |
| `SetSpeedWarpMode(WarpMode)` | 17 | Warp mode of the speed family. Held in engine memory only (the UI persists and re-sends it). |
| `SetSeekWarpMode(WarpMode)` | 18 | Warp mode of the seek family (same persistence rule). |
| `ScanStart { dir: ScanDir }` | 19 | FF/RW hold started. Sets `scan`; clears a pending `seek_target_ms`. Ignored when nothing is loaded. |
| `ScanStop` | 20 | FF/RW released; `scan` back to `None`. |
| `SetSeekOutsideTrack(bool)` | 22 | Play-mode setting "Seek outside track" (default off, engine memory only, kept across `Unload`; the UI re-sends it on connect and on a status mismatch). On: scan and jog may cross into the neighbouring item (what `Next`/`Previous` would select; first/last item of a non-repeating list stays a clamp). |
| `SeekToItem { item_delta: i8, target_ms: u64 }` | 23 | Jog commit that crossed a boundary: -1 previous item, 0 current (= `SeekTo`), +1 next item; `target_ms` absolute inside that item. Setting off, no such neighbour or an unopenable neighbour: handled as `SeekTo` in the current item (clamped). Otherwise a crossfaded jump with the `Next`/`Previous` state changes (cue, now-playing, resume write-back of the item left at the boundary, `Previous` tracker cleared). |
| `SeekTo { target_ms: u64 }` | 21 | The committed jog target (absolute media ms). Clamped to `[0, duration - SEEK_END_GUARD_MS (250)]` (unknown duration: lower clamp only). **Ignored while `scan != None`.** Publishes `seek_target_ms = Some(clamped)` at once and performs a hard seek (the `SeekMs` machinery); the target clears when that seek has landed. A newer `SeekTo` replaces the pending target. |

Wire enums (declared in `plippa-core`, re-exported by `plippa-ipc-proto`): `WarpMode { PitchShift, PitchCorrect }` (used for both settings), `ScanDir { Forward, Backward }`, `ScanState { None, Forward, Backward }`, `VoiceKind { Bypass, Varispeed, Stretch }`.

`SessionState` appended fields (after `can_next`, in this order; Postcard is positional, so the old fields keep their encoding and an *old* frame no longer decodes: one workspace build, no mixed versions):

| Field | Default | Meaning |
|-------|---------|---------|
| `rate_permille: i32` | `1000` | Instantaneous effective rate, signed, includes seek. Honest `1000` until the DSP lands. |
| `target_rate_permille: i32` | `1000` | The set speed (`SetRate`/`ResetRate`). Session-wide: kept across tracks, NFC taps, pause/resume; reset by `Unload` and engine start. |
| `speed_warp_mode: WarpMode` | `SPEED_WARP_DEFAULT` | `PitchCorrect` (one named constant in `plippa-core`). |
| `seek_warp_mode: WarpMode` | `PitchShift` | |
| `scan: ScanState` | `None` | Hold scan (velocity-controlled, no destination). |
| `seek_target_ms: Option<u64>` | `None` | Target of a **committed** `SeekTo` in progress; `None` while a scan runs and when idle. |
| `speed_voice: VoiceKind` | `Bypass` | Diagnostic only (tests, `plippa-np`); never a UI indicator. |
| `warp_active: bool` | `false` | |
| `seek_outside_track: bool` | `false` | the setting as stored by the engine. |
| `prev_duration_ms` / `next_duration_ms: Option<u64>` | `None` | durations of the neighbour items (only while the setting is on and the neighbour exists and is openable) for the UI's live jog target. |
| `can_ff` / `can_rw: bool` | `false` | Something is loaded (computed next to `can_prev`); independent of transport, position and tempo. |
| `waveform_path: Option<String>` | `None` | absolute path of the loaded item's `.wfm` sidecar (set whenever a media file is loaded, even before the file exists; cleared by `Unload`, `None` when no media is resolved or the path is outside the library root). |

**Position semantics.** `position_ms` is the **actual playhead** (media time). The legacy rule "report the pending target until the seek is applied" stays only for `SeekMs` (cue / resume / list seeks) and stream starts. For `SeekTo` the target is in `seek_target_ms` and `position_ms` stays actual.

**Clearing rules.** `Unload` and engine start reset the set speed to `1000` and clear scan and `seek_target_ms`. `Next` / `Previous` / `JumpToEntry` / any new load (tap, `LoadCollection`, `Load*`) and any track change clear scan and `seek_target_ms` but keep the set speed and both warp modes; a `SeekMs` (cue seek) drops a pending `seek_target_ms`. `SeekTo` clears the `Previous` double-press tracker; `SetRate`, `ResetRate`, the two mode commands and `ScanStart/Stop` do not. `SetRate`, `ResetRate` and mode changes never cancel a seek. A command that changes nothing (same rate, same mode) publishes nothing.

**Status cadence.** Any change of the set speed, either warp mode, `scan` or `seek_target_ms` publishes a status immediately (revision bump). The hub's periodic refresh runs every **100 ms** while `target_rate_permille != 1000`, `scan != None` or `seek_target_ms` is `Some` (`Engine::wants_fast_status`, `ipc_hub::StatusPacer`), else every 500 ms. Unit-tested with injected `Instant`s.

Shell driver: `plippa-np rate <permille>|reset`, `speedmode shift|correct`, `seekmode shift|correct`, `scan fwd|back|stop`, `seekto <ms>`; its output has a `warp ...` line with the fields above.

## Socket paths

Resolved by `plippa_ipc_proto::resolve_paths()`:

1. `PLIPPA_IPC_DIR` if set (launcher always sets this)
2. Else `$XDG_RUNTIME_DIR/plippa/` when `XDG_RUNTIME_DIR` is set
3. Else `/run/plippa/` if it exists and is writable
4. Else a process-local fallback under `/tmp/plippa-<pid>/` (tests / desktop)

Socket file: `{dir}/engine.sock`

Optional overrides:

- `PLIPPA_IPC_ENGINE_SOCK` — absolute path to the engine listen socket
- `PLIPPA_IPC_CONNECT_SECS` — UI/server first-connect wait (default 20; `0` = NullIpc / skip)

## Framing

Never assume one `write` = one message.

```
[u32 little-endian body length][Postcard body]
```

- Max body length: 1 MiB (`MAX_FRAME_BODY`)
- Truncated reads buffer until a full frame arrives
- Invalid Postcard body → error, **frame consumed**, connection stays usable
- Length prefix over max → error, **reconnect required** (buffer cleared by caller)

## Threading & non-blocking rules

```
┌─────────────┐   UDS    ┌──────────────────┐  cmd channel  ┌─────────────────┐
│ plippa-ui   │◄────────►│ plippa-engine    │──────────────►│ audio thread    │
│ frame loop  │          │ hub / control    │◄──────────────│ StreamDecoder   │
└─────────────┘          │ (accept/dispatch)│   stats/err   │ + Pulse streams │
┌─────────────┐          └──────────────────┘               └─────────────────┘
│plippa-server│◄──UDS────────────┘
│ HTTP threads│
└─────────────┘
```

| Loop / role | May do | Must **not** do |
|-------------|--------|-----------------|
| Engine hub (accept/dispatch) | Enqueue `StartMainStream` / Play / Stop; light SQLite **read**; broadcast status; relay associate | Whole-file decode; multi-second sleep; wait for first audio; large resample |
| Engine audio thread | Chunked decode, Pulse open/write, SFX second stream | SQLite / IPC accept |
| Server HTTP worker | Short library lock; enqueue `LoadAndPlay`; poll status | Block until `Playing`; hold `ServerService` mutex across NFC wait |
| UI frame loop | `try_recv` / fire-and-forget `TapNdef`; reconnect attempt ~1 Hz; drain input/NFC **channels** only | Wait on media prepare; block on I²C / PN532 (see [`ui-threading.md`](./ui-threading.md)) |

### Startup order

1. Launcher creates `PLIPPA_IPC_DIR`, spawns children, logs when `engine.sock` appears (does not wait for Pulse).
2. Engine: construct process + audio thread with **lazy** Pulse factory → **bind `engine.sock`** → accept → then SFX decode.
3. Server/UI: connect with timeout; on miss, **background reconnect** (server) or frame-loop retry (UI) — not permanent sticky NullIpc for the process lifetime when the sock appears later.

### Play / tap while decode in flight

1. Hub receives `LoadAndPlay` / tap → resolve path + `validate_for_stream` (probe only) → `StartMainStream` + `Play` on the audio channel → emit `Playing` immediately (first-audio contract).
2. Hub returns to accept/poll; timer still broadcasts `PlaybackStatus`.
3. Audio thread opens native-rate stream; errors → `NOTICE_DECODE_FAILED` via `poll_stream_lifecycle`.
4. Portal `/api/.../play` returns after IPC send; client polls `/api/now-playing`.

### Failure modes

| Mode | Behaviour |
|------|-----------|
| Peer connects before ready | Sock listen-first; backlog accepts; SFX may still be loading |
| Worker / decode error | Notice + Stop; no panic |
| Audio cmd channel | Unbounded `mpsc`; `Stop` / new `StartMainStream` supersede by FIFO (hard cut). Disconnect drops commands |
| Associate NFC wait | Service mutex **released**; IPC lock held only in ≤100 ms slices; HTTP is multi-threaded |

### Race audit

| Item | Resolution |
|------|------------|
| Engine listen vs Pulse/SFX | Listen before SFX; Pulse lazy on audio thread; SFX deferred until after first accept drain |
| Hub vs decode | Chunk decode off hub; no wait-after-Start on hub |
| Server NullIpc after early miss | `ReconnectingIpc` background retry |
| HTTP mutex × associate | `associate_playable_unlocked` + per-request threads |
| UI sticky offline | Reconnect ~1 Hz when `conn` is `None` |
| Multi-client during load | Hub continues accept + status broadcast |
| Cmd channel full | N/A (unbounded); document supersede policy above |

## UI local I/O

The UI **coordinator** must not perform AW9523/PN532 transactions on the shared i2c-1 bus. A dedicated I²C service thread owns that bus; see [`ui-threading.md`](./ui-threading.md).

## Launcher

`plippa` creates the IPC directory, exports `PLIPPA_IPC_DIR`, then spawns children so they inherit the same bus path. After start it logs when `engine.sock` exists (≤5 s wait). Children must not assume SFX finished before the first IPC message.

Hold-power: UI writes `{PLIPPA_IPC_DIR}/shutdown`; launcher stops children. Host poweroff only when `PLIPPA_POWEROFF=1` / `--poweroff` (lab).

## Smoke (no hardware)

```bash
export CARGO_BUILD_JOBS=1
cargo build -p plippa-engine   # also builds plippa-ipc-smoke-client
cargo build -p plippa-ui
cargo build -p plippa-server
./scripts/mvp09_ipc_smoke.sh
```
