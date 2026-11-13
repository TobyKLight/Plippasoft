//! Inter-process message types and Unix domain socket transport (Postcard).

mod frame;
mod paths;
mod transport;

pub use plippa_core::audio_route::{AudioOutput, AudioRoute, AudioRouteWanted};
pub use plippa_core::{
    OrderMode, RepeatMode, ScanDir, ScanState, SourceOrder, VoiceKind, WarpMode,
};
use plippa_core::{
    NowPlayingEntry, NowPlayingListKind, PlayableItemId, SessionState, TransportState, Volume,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub use frame::{encode_frame, try_pop_frame, FrameError, MAX_FRAME_BODY};
pub use paths::{
    clear_shutdown, ensure_ipc_dir, poweroff_requested, remove_stale_sock, request_poweroff,
    request_shutdown, resolve_paths, shutdown_requested, IpcPaths, ENV_ENGINE_SOCK, ENV_IPC_DIR,
};
pub use transport::{IpcConnection, IpcListener, IpcTransportError};

/// Stable `UserNotice.code` when a tap matches nothing in the local library.
pub const NOTICE_LIBRARY_MISS: &str = "library_miss";
/// Stable `UserNotice.code` when `.noudata` matched but a media file is missing (or could not be
/// opened for a reason other than its format; see [`NOTICE_MEDIA_UNPLAYABLE`]).
pub const NOTICE_MEDIA_FILE_MISSING: &str = "media_file_missing";
/// Stable `UserNotice.code` when a file exists but this player cannot play it: its format is
/// not enabled for the board, or no entry of the release is playable here. A different thing
/// from [`NOTICE_MEDIA_FILE_MISSING`]: the library may legitimately hold formats another
/// player decodes.
pub const NOTICE_MEDIA_UNPLAYABLE: &str = "media_unplayable";
/// Stable `UserNotice.code` when NDEF extract (§6.1) fails.
pub const NOTICE_NDEF_INVALID: &str = "ndef_invalid";
/// Stable `UserNotice.code` when no media library storage is present.
pub const NOTICE_NO_MEDIA_STORAGE: &str = "no_media_storage";
/// Stream decode / sink failure after Play started (MVP_13).
pub const NOTICE_DECODE_FAILED: &str = "decode_failed";
/// NB_06: a tap matched several playables sharing one Noubin key (a merge left a
/// duplicate). The engine played the first by `.noudata` path and lists the others.
pub const NOTICE_DUPLICATE_KEY: &str = "duplicate_key";
/// PLAY_02: "normal order" was asked for on a dynamic queue (add-to-queue / play-next): the
/// setting is stored but the order cannot be restored. Message text is the owner's wording.
pub const NOTICE_NO_FIXED_ORDER: &str = "no_fixed_order";
/// AUDIO_01: more outputs were ticked than the engine can route
/// (`MAX_ENABLED_OUTPUTS`); the extra ones were not enabled.
pub const NOTICE_AUDIO_OUTPUT_LIMIT: &str = "audio_output_limit";
/// AUDIO_01: an output id the engine does not know; the route was left unchanged.
pub const NOTICE_AUDIO_OUTPUT_UNKNOWN: &str = "audio_output_unknown";
/// AUDIO_01: the output backend refused the route (or a delay had to be adjusted).
/// The engine keeps the last working route rather than going silent.
pub const NOTICE_AUDIO_ROUTE_FAILED: &str = "audio_route_failed";

/// Wire protocol version for the engine ↔ UI ↔ server bus.
///
/// **Version 2 (BCS_07)**: [`UnmatchedNoubin`] gained an appended
/// [`UnmatchedNoubin::candidate_urls`] field. The append is the only change, and it is
/// deliberately compatible in both directions, which [`UnmatchedNoubin`]'s hand-written
/// `Deserialize` is responsible for:
///
/// * an **old** decoder reads a new frame and ignores the trailing field (Postcard's
///   `from_bytes` leaves trailing bytes unread), so an old UI still gets the notice;
/// * a **new** decoder reads an old frame whose body ends after `seq`: the absent field
///   decodes as an empty list instead of an error, so a new UI still gets the notice.
///
/// Bump this constant for every wire change and keep [`USED_MESSAGE_VARIANTS`] and
/// [`USED_COMMAND_VARIANTS`] in step; the compatibility tests pin both.
pub const PROTOCOL_VERSION: u32 = 2;

/// Number of variants [`IpcMessage`] declares (index `0..this`).
///
/// Postcard encodes the variant index, so a variant may only ever be **appended**. The
/// constant exists so an append cannot be made silently: the compatibility test fails until
/// the new variant has been considered for mixed-version peers.
pub const USED_MESSAGE_VARIANTS: u32 = 12;

/// Number of variants [`PlaybackCommand`] declares (index `0..this`). Appending only.
pub const USED_COMMAND_VARIANTS: u32 = 33;

/// Errors from Postcard encode/decode of IPC messages.
#[derive(Debug, Error)]
pub enum IpcCodecError {
    #[error("postcard encode failed: {0}")]
    Encode(#[from] postcard::Error),
}

/// Encode any serialisable IPC payload to Postcard bytes.
pub fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>, IpcCodecError> {
    Ok(postcard::to_allocvec(value)?)
}

/// Decode Postcard bytes into an IPC payload. Truncated/invalid input returns
/// [`Err`] — never panics.
pub fn decode<T: for<'de> Deserialize<'de>>(bytes: &[u8]) -> Result<T, postcard::Error> {
    postcard::from_bytes(bytes)
}

/// Envelope for messages between engine ↔ UI ↔ server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum IpcMessage {
    /// UI → engine: raw NDEF payload from a completed NFC tap.
    TapNdef(TapNdefPayload),
    /// UI / harness → engine: pre-extracted (or already-normalised) Noubin key.
    TapKey(TapKeyPayload),
    /// UI / server → engine: transport / volume / load commands.
    PlaybackCommand(PlaybackCommand),
    /// Engine → UI / server: current playback snapshot.
    PlaybackStatus(PlaybackStatus),
    /// Server → engine: library index changed; engine may refresh lookups.
    LibraryIndexUpdated(LibraryIndexUpdated),
    /// Any → UI (and optionally server): user-visible notice.
    UserNotice(UserNotice),
    /// Server → UI: write NDEF for associate; UI replies with [`AssociateResponse`].
    AssociateRequest(AssociateRequest),
    /// UI → server: outcome of an NFC associate write (server then write-through `.noudata`).
    AssociateResponse(AssociateResponse),
    /// Engine → UI / server (NB_04): the `nowPlayingList` was loaded, replaced, changed
    /// track, or its cue set changed. Sent to every client, and to each new client on connect.
    NowPlayingListChanged(NowPlayingListChanged),
    /// Engine → server (NB_04): persist (or clear) a long-form resume point in the
    /// playable's `.noudata`. The engine never writes `.noudata` itself.
    ResumeWriteback(ResumeWriteback),
    /// Engine → UI / server (NB_08): a tap carried a URL Noubin Key that matched nothing in
    /// the local library. The engine never contacts the network; the server/UI decide whether
    /// to offer the (consent-gated) web lookup.
    UnmatchedNoubin(UnmatchedNoubin),
    /// Engine → UI / server (AUDIO_01): the routed outputs, delays and diagnostics.
    /// Sent to every client, and to each new client on connect.
    AudioRouteChanged(AudioRoute),
}

// Wire compatibility (Postcard is positional): new `IpcMessage` / `PlaybackCommand`
// variants are only ever **appended**, never inserted or reordered. All three
// processes ship from one workspace build; a mixed-version pair is not supported.

/// NB_08 / BCS_07: a URL Noubin Key with no local match ("unmatched official Noubin").
///
/// # Wire compatibility
///
/// Postcard is positional and `candidate_urls` was **appended** (BCS_07, protocol 2). Both
/// mixed-version directions must keep working, so this type decodes by hand:
///
/// * the field is last, so a decoder that stops after `seq` has read the whole legacy frame;
/// * when the frame ends before the field, it decodes as an empty list
///   ([`Default`](Vec::default)) instead of failing, which is what lets a new UI read an old
///   engine's frame. An empty list is the honest value: the engine did not tell us.
///
/// Do not append anything after `candidate_urls`: the tolerance above depends on this field
/// being the final one in the frame.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct UnmatchedNoubin {
    /// The Noubin Key exactly as read from the tag (after §6.1 extraction), at most 512 bytes.
    /// Untrusted text: consumers must sanitise before display.
    pub noubin_key: String,
    /// §6.2 Normalised Noubin Key (what the local lookup used).
    pub normalised_key: String,
    /// Monotonic per-engine counter so a consumer can tell a repeat tap from an old notice.
    pub seq: u64,
    /// BCS_07: the platform links the tag declared (every URI record, in tag order, at most
    /// [`MAX_UNMATCHED_CANDIDATE_URLS`], each at most [`MAX_CANDIDATE_URL_BYTES`]). These are
    /// untrusted tag text and are never fetched without the caller's own consent gate.
    /// Empty when the engine had none to give (an old engine, or a key tap).
    pub candidate_urls: Vec<String>,
}

/// Most platform links [`UnmatchedNoubin::candidate_urls`] may carry.
pub const MAX_UNMATCHED_CANDIDATE_URLS: usize = 8;

/// Longest platform link [`UnmatchedNoubin::candidate_urls`] may carry, in UTF-8 bytes.
pub const MAX_CANDIDATE_URL_BYTES: usize = 512;

impl<'de> Deserialize<'de> for UnmatchedNoubin {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;

        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = UnmatchedNoubin;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("an unmatched-Noubin notice")
            }

            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> Result<Self::Value, A::Error> {
                use serde::de::Error as _;
                let noubin_key = seq
                    .next_element::<String>()?
                    .ok_or_else(|| A::Error::invalid_length(0, &self))?;
                let normalised_key = seq
                    .next_element::<String>()?
                    .ok_or_else(|| A::Error::invalid_length(1, &self))?;
                let seq_no = seq
                    .next_element::<u64>()?
                    .ok_or_else(|| A::Error::invalid_length(2, &self))?;
                // Protocol 1 frames end here. A frame that stops early is an old engine, not
                // a corrupt frame, so the absent field is an empty list.
                let candidate_urls = seq
                    .next_element::<Vec<String>>()
                    .ok()
                    .flatten()
                    .unwrap_or_default();
                Ok(UnmatchedNoubin {
                    noubin_key,
                    normalised_key,
                    seq: seq_no,
                    candidate_urls,
                })
            }
        }

        deserializer.deserialize_tuple(4, Visitor)
    }
}

/// Raw NDEF bytes handed from `plippa-ui` to `plippa-engine` after a full tag read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TapNdefPayload {
    /// Complete NDEF message bytes (not yet normalised to a Noubin key).
    pub ndef_message: Vec<u8>,
    /// Monotonic tap id from UI (for dedupe / ignore-while-playing).
    pub tap_id: u64,
}

/// Pre-extracted Noubin key (tests / remote control). Engine still applies §6.2
/// unless [`TapKeyPayload::already_normalised`] is set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TapKeyPayload {
    pub noubin_key: String,
    pub tap_id: u64,
    /// When true, `noubin_key` is already a `noubinKeyNormalised` value.
    pub already_normalised: bool,
}

/// Playback / session control commands.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PlaybackCommand {
    Play,
    Pause,
    Stop,
    SetVolume(Volume),
    /// Load a library item (does not imply Play by itself).
    Load(PlayableItemId),
    /// Load a library item and start playback (portal Play button).
    LoadAndPlay(PlayableItemId),
    /// Seek within the current item; milliseconds from start.
    SeekMs(u64),
    /// Advance within the current playable's `mediaList`.
    NextTrack,
    /// Go back within the current playable's `mediaList`.
    PreviousTrack,
    /// Stop playback and clear all session resume bookmarks (next tap starts at 0).
    Unload,
    /// Load a collection (album / playlist / mix / audiobook / podcast / single track)
    /// into the `nowPlayingList` (replacing it) and start playing. Honours the
    /// playable's `resumeAtItem` / `resumeFromTime`. The playback order comes from the
    /// player's play-mode setting (`SetOrderMode` / `SetRepeatMode` / `SetPauseAfterEach`),
    /// never from the file; a load always clears `RepeatMode::Item` back to `Off`.
    LoadCollection { playable_id: PlayableItemId },
    /// Next cue start (any enabled navigable cue list), else next track.
    /// (`NextTrack` is kept as an alias with identical behaviour.)
    Next,
    /// FIX_01C: a first press restarts the current entry (cue, else track) only once the
    /// playhead is more than 1 s past that entry's start; at the entry start it steps back
    /// one entry. A *subsequent* press within `plippa_core::PREVIOUS_DOUBLE_PRESS_WINDOW`
    /// (3 s) always goes to the previous entry.
    /// (`PreviousTrack` is kept as an alias.)
    Previous,
    /// Jump to an entry of the flat `NowPlayingListChanged.entries` list: a track
    /// (loads it) or a cue of the current track (seeks, never reloads).
    JumpToEntry { index: u32 },
    /// Enable/disable one cue list of the current track for navigation + display.
    /// Session-only (not persisted).
    SetCueListEnabled { list_index: u32, enabled: bool },
    // ---- WARP_01 (indices 15-21, pinned by a test; WARP_19 appends 22-23). No floats. ----
    /// Speed family: the session-wide set speed in permille, signed. Accepts `100..=2000` or
    /// `-2000..=-100`; anything else is rejected (logged, no state change). Never cancels a
    /// seek and does not break the `Previous` double-press window.
    SetRate { permille: i32 },
    /// Speed family: set speed back to 1000 (touches nothing else: not scan, not the seek
    /// target, not either warp mode).
    ResetRate,
    /// Warp mode of the speed family (stored by the engine; the UI persists and re-sends it).
    SetSpeedWarpMode(WarpMode),
    /// Warp mode of the seek family (stored by the engine; the UI persists and re-sends it).
    SetSeekWarpMode(WarpMode),
    /// Seek family: FF/RW *hold* started (velocity-controlled scan, no destination).
    ScanStart { dir: ScanDir },
    /// Seek family: FF/RW released; the playhead stays wherever the audio got to.
    ScanStop,
    /// Seek family: the UI-owned jog target, sent **once on commit**, absolute media time.
    /// Clamped by the engine to `[0, duration - SEEK_END_GUARD_MS]`. Ignored while a scan
    /// runs. Reported in `SessionState::seek_target_ms` until the playhead has arrived.
    SeekTo { target_ms: u64 },
    // ---- WARP_19 (indices 22-23, pinned by a test) ----
    /// Play-mode setting "Seek outside track" (default off, engine memory only; the UI
    /// persists and re-sends it). On: scan and jog may cross into the neighbouring item.
    SetSeekOutsideTrack(bool),
    /// The jog commit when the live target crossed a boundary: `item_delta` -1 = previous
    /// item, 0 = current (identical to `SeekTo`), +1 = next item; `target_ms` is absolute
    /// media time inside that item. Without the setting (or without such a neighbour) the
    /// engine treats it as a `SeekTo` in the current item.
    SeekToItem { item_delta: i8, target_ms: u64 },
    // ---- UI_14 (appended; Postcard is positional, earlier indices keep their meaning) ----
    /// The FF/RW-hold + Play gesture: back to 100 % **immediately** (no reset ramp) and drop
    /// any scan / seek chase where it stands. Ordinary `ResetRate` (long-press Play) keeps its
    /// slower ramp.
    ResetRateInstant,
    // ---- UI_15 (appended) ----
    /// The **running** FF/RW hold's scan speed, in permille of the usual cassette scan speed
    /// (`plippa_core::SCAN_SPEED_LADDER_PERMILLE`; `1000` = the usual 4x/8x, `10` = 1 % of it,
    /// which is below 1x playback). Sent while the encoder steps the speed with the button
    /// held; it re-targets the live scan and **never** touches the session's set speed
    /// (`target_rate_permille`), which is what the release restores. Ignored without a running
    /// scan; values outside the ladder are rejected (logged, no change).
    ScanRate { permille: i32 },
    // ---- PLAY_02 (appended; the player's play-mode settings, engine memory only) ----
    /// Playback order setting. Same value is a **no-op** (a UI reconcile must not reshuffle);
    /// `Shuffle` on a different value reshuffles, `Normal` restores a fixed list's source order.
    SetOrderMode(OrderMode),
    /// Repeat mode. `Item` is cleared to `Off` by any new load.
    SetRepeatMode(RepeatMode),
    /// Pause between items instead of auto-advancing: the transport goes `Stopped` with the
    /// next item current at 0:00 and ready for Play.
    SetPauseAfterEach(bool),
    /// Repeat-queue wrapping a shuffled list starts a fresh shuffle (default on).
    SetReshuffleOnRepeat(bool),
    // ---- AUDIO_01 (appended) -------------------------------------------------------------
    /// Replace the enabled set and the local-only mode in one atomic step.
    SetAudioOutputs { enabled: Vec<String>, local_only: bool },
    /// Automatic per-output latency compensation on/off (manual map ignored while on).
    SetAudioDelayAuto(bool),
    /// One output's manual delay in ms (only used while automatic compensation is off).
    SetOutputDelay { output: String, delay_ms: u32 },
}

/// Status broadcast from the engine control plane.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlaybackStatus {
    pub session: SessionState,
    /// Convenience mirror of `session.transport` for UI bindings.
    pub transport: TransportState,
    /// Monotonic revision; UI drops snapshots older than the last applied.
    /// Bumped when transport or loaded item changes; periodic hub refresh
    /// reuses the current revision.
    pub revision: u64,
}

impl PlaybackStatus {
    /// Build status at revision 0 (tests / callers that ignore revision).
    pub fn from_session(session: SessionState) -> Self {
        Self::from_session_rev(session, 0)
    }

    pub fn from_session_rev(session: SessionState, revision: u64) -> Self {
        Self {
            transport: session.transport,
            session,
            revision,
        }
    }
}

/// The engine's `nowPlayingList` as UIs should display it.
///
/// `entries` is flat and nested by position: every track in play order and, directly
/// after the **current** track, that track's navigable cue entries (see
/// `plippa_core::NowPlayingList`). `current_index` is the flat index of the current
/// *track* entry; the playing cue is `current_index + 1 + status.current_cue_index`.
/// Under cue-level shuffle (`PLAY_03`) `current_cue_index` is the **permutation** position
/// among the published cue rows, which are themselves in play order, so that formula still
/// names the row being heard.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NowPlayingListChanged {
    pub entries: Vec<NowPlayingEntry>,
    pub current_index: u32,
    pub list_kind: NowPlayingListKind,
    /// PLAY_02: identity of the **loaded list**, assigned once per load (replacement).
    /// A reorder or a track change keeps the same `list_id`; a new load gets a new one. That
    /// is how a UI tells "the collection was replaced" from "the same list was reordered".
    pub list_id: u64,
}

/// Request to persist resume state for a long-form playable (`.noudata` write-through).
///
/// `resume_at_item` is 1-indexed (`localPlaybackData.resumeAtItem`);
/// `resume_from_ms` is written as `itemLocalPlaybackData.resumeFromTime` of the item at
/// `resume_at_item`. Both `None` ⇒ clear resume state (finished, or no longer eligible).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResumeWriteback {
    /// Global playable id (as in `SessionState::current_item`).
    pub playable_id: PlayableItemId,
    pub resume_at_item: Option<u32>,
    pub resume_from_ms: Option<u64>,
}

/// Notification that `plippa-server` finished (re)building the SQLite index.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LibraryIndexUpdated {
    /// Generation / scan counter; engine can ignore stale generations.
    pub generation: u64,
    /// Approximate number of indexed playable items (informational).
    pub item_count: u64,
}

/// Severity for user-visible notices.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum NoticeSeverity {
    Info,
    Warning,
    Error,
}

/// Something the local UI (and optionally web portal) should show the user.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserNotice {
    pub severity: NoticeSeverity,
    /// Stable machine code (e.g. `no_media_storage`).
    pub code: String,
    /// Human-readable message.
    pub message: String,
}

/// Server asks UI to associate a playable with the tag currently in the field.
///
/// Transactional contract (MVP_07): server writes `.noudata` keys **only** after a
/// successful [`AssociateResponse`]. NFC failure leaves `.noudata` unchanged.
///
/// When [`Self::keep_existing_key`] is false, the UI writes [`Self::noubin_key`] to
/// the tag. When true, the UI reads the key already on the tag (no overwrite) and
/// returns it; a blank key after normalisation is treated as failure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssociateRequest {
    pub request_id: u64,
    pub playable_id: PlayableItemId,
    /// Relative path of the playable's `.noudata` (informational for UI).
    pub noudata_relpath: String,
    pub playable_item_title: String,
    /// Pre-generated key the UI should write (Text NDEF for local keys).
    /// Unused when [`Self::keep_existing_key`] is true (may be empty).
    pub noubin_key: String,
    /// If true, read the existing tag key instead of writing a new one.
    pub keep_existing_key: bool,
}

/// UI reports NFC write outcome for an [`AssociateRequest`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct AssociateResponse {
    pub request_id: u64,
    pub ok: bool,
    /// Echo of the key written when `ok` is true.
    pub noubin_key: Option<String>,
    /// Normalised form when `ok` is true.
    pub noubin_key_normalised: Option<String>,
    /// User-readable error when `ok` is false.
    pub error: Option<String>,
    /// NB_06: the tag could not be rewritten (locked / read-only), so `noubin_key` is the
    /// key it already carried and nothing was written. The server then applies the
    /// spec §3.4 step-3 *move* semantics (the key is cleared from every other item).
    /// Always true for a `keep_existing_key` request that succeeded.
    pub tag_locked: bool,
    /// NB_06: the (raw) Noubin key the tag carried **before** this associate wrote a new
    /// one, when the UI could read one. The server reports which items still match it.
    pub previous_key: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use plippa_core::{PlayableItemId, TransportState, Volume};

    fn round_trip(msg: &IpcMessage) -> IpcMessage {
        let bytes = encode(msg).expect("encode");
        decode(&bytes).expect("decode")
    }

    #[test]
    fn tap_ndef_round_trips() {
        let msg = IpcMessage::TapNdef(TapNdefPayload {
            ndef_message: vec![0xd1, 0x01, 0x0a, 0x54],
            tap_id: 42,
        });
        assert_eq!(round_trip(&msg), msg);
    }

    #[test]
    fn playback_commands_round_trip() {
        let commands = [
            IpcMessage::PlaybackCommand(PlaybackCommand::Play),
            IpcMessage::PlaybackCommand(PlaybackCommand::Pause),
            IpcMessage::PlaybackCommand(PlaybackCommand::Stop),
            IpcMessage::PlaybackCommand(PlaybackCommand::SetVolume(Volume::new(75))),
            IpcMessage::PlaybackCommand(PlaybackCommand::Load(PlayableItemId::new(9))),
            IpcMessage::PlaybackCommand(PlaybackCommand::LoadAndPlay(PlayableItemId::new(9))),
            IpcMessage::PlaybackCommand(PlaybackCommand::SeekMs(1_234)),
            IpcMessage::PlaybackCommand(PlaybackCommand::NextTrack),
            IpcMessage::PlaybackCommand(PlaybackCommand::PreviousTrack),
            IpcMessage::PlaybackCommand(PlaybackCommand::Unload),
            IpcMessage::PlaybackCommand(PlaybackCommand::LoadCollection {
                playable_id: PlayableItemId::new(77),
            }),
            IpcMessage::PlaybackCommand(PlaybackCommand::Next),
            IpcMessage::PlaybackCommand(PlaybackCommand::Previous),
            IpcMessage::PlaybackCommand(PlaybackCommand::JumpToEntry { index: 4 }),
            IpcMessage::PlaybackCommand(PlaybackCommand::SetCueListEnabled {
                list_index: 1,
                enabled: false,
            }),
            IpcMessage::PlaybackCommand(PlaybackCommand::SetRate { permille: 1500 }),
            IpcMessage::PlaybackCommand(PlaybackCommand::ResetRate),
            IpcMessage::PlaybackCommand(PlaybackCommand::SetSpeedWarpMode(WarpMode::PitchCorrect)),
            IpcMessage::PlaybackCommand(PlaybackCommand::SetSeekWarpMode(WarpMode::PitchShift)),
            IpcMessage::PlaybackCommand(PlaybackCommand::ScanStart { dir: ScanDir::Forward }),
            IpcMessage::PlaybackCommand(PlaybackCommand::ScanStop),
            IpcMessage::PlaybackCommand(PlaybackCommand::SeekTo { target_ms: 99 }),
            IpcMessage::PlaybackCommand(PlaybackCommand::SetSeekOutsideTrack(true)),
            IpcMessage::PlaybackCommand(PlaybackCommand::SeekToItem {
                item_delta: -1,
                target_ms: 4_000,
            }),
            IpcMessage::PlaybackCommand(PlaybackCommand::ResetRateInstant),
            IpcMessage::PlaybackCommand(PlaybackCommand::ScanRate { permille: 1500 }),
            IpcMessage::PlaybackCommand(PlaybackCommand::SetOrderMode(OrderMode::Shuffle)),
            IpcMessage::PlaybackCommand(PlaybackCommand::SetRepeatMode(RepeatMode::Queue)),
            IpcMessage::PlaybackCommand(PlaybackCommand::SetPauseAfterEach(true)),
            IpcMessage::PlaybackCommand(PlaybackCommand::SetReshuffleOnRepeat(false)),
            IpcMessage::PlaybackCommand(PlaybackCommand::SetAudioOutputs {
                enabled: vec!["local".into(), "usb.headset".into()],
                local_only: false,
            }),
            IpcMessage::PlaybackCommand(PlaybackCommand::SetAudioDelayAuto(false)),
            IpcMessage::PlaybackCommand(PlaybackCommand::SetOutputDelay {
                output: "usb.headset".into(),
                delay_ms: 120,
            }),
        ];
        for msg in &commands {
            assert_eq!(&round_trip(msg), msg);
        }
    }

    #[test]
    fn warp_commands_round_trip_and_pin_wire_positions() {
        let pinned = [
            (PlaybackCommand::SetRate { permille: 1500 }, 15u8),
            (PlaybackCommand::SetRate { permille: -750 }, 15),
            (PlaybackCommand::ResetRate, 16),
            (PlaybackCommand::SetSpeedWarpMode(WarpMode::PitchCorrect), 17),
            (PlaybackCommand::SetSeekWarpMode(WarpMode::PitchShift), 18),
            (PlaybackCommand::ScanStart { dir: ScanDir::Backward }, 19),
            (PlaybackCommand::ScanStop, 20),
            (PlaybackCommand::SeekTo { target_ms: 123_456 }, 21),
            (PlaybackCommand::SetSeekOutsideTrack(true), 22),
            (PlaybackCommand::SeekToItem { item_delta: 1, target_ms: 5 }, 23),
            (PlaybackCommand::ResetRateInstant, 24),
            (PlaybackCommand::ScanRate { permille: 1000 }, 25),
            (PlaybackCommand::ScanRate { permille: 10 }, 25),
        ];
        for (c, idx) in pinned {
            let bytes = encode(&c).unwrap();
            assert_eq!(bytes[0], idx, "{c:?} wire index");
            assert_eq!(decode::<PlaybackCommand>(&bytes).unwrap(), c);
            let msg = IpcMessage::PlaybackCommand(c);
            assert_eq!(round_trip(&msg), msg);
        }
        // Earlier commands keep their positions.
        assert_eq!(
            encode(&PlaybackCommand::SetCueListEnabled { list_index: 0, enabled: true }).unwrap()[0],
            14
        );
    }

    #[test]
    fn play_mode_commands_keep_their_wire_indices() {
        // PLAY_02 appended four commands after `ScanRate` (25); Postcard puts the variant
        // index first, so these bytes are the wire contract PLAY_04 is written against.
        let pinned = [
            (PlaybackCommand::SetOrderMode(OrderMode::Normal), 26u8),
            (PlaybackCommand::SetOrderMode(OrderMode::Shuffle), 26),
            (PlaybackCommand::SetRepeatMode(RepeatMode::Off), 27),
            (PlaybackCommand::SetRepeatMode(RepeatMode::Queue), 27),
            (PlaybackCommand::SetRepeatMode(RepeatMode::Item), 27),
            (PlaybackCommand::SetPauseAfterEach(false), 28),
            (PlaybackCommand::SetPauseAfterEach(true), 28),
            (PlaybackCommand::SetReshuffleOnRepeat(true), 29),
            (PlaybackCommand::SetReshuffleOnRepeat(false), 29),
        ];
        for (c, idx) in pinned {
            let bytes = encode(&c).unwrap();
            assert_eq!(bytes[0], idx, "{c:?} wire index");
            assert_eq!(decode::<PlaybackCommand>(&bytes).unwrap(), c);
            let msg = IpcMessage::PlaybackCommand(c);
            assert_eq!(round_trip(&msg), msg);
        }
        // The command before them keeps its position.
        assert_eq!(encode(&PlaybackCommand::ScanRate { permille: 1000 }).unwrap()[0], 25);
    }

    #[test]
    fn audio_commands_keep_their_pinned_wire_positions() {
        // AUDIO_01 appended three commands after `SetReshuffleOnRepeat` (29).
        let pinned = [
            (
                PlaybackCommand::SetAudioOutputs {
                    enabled: vec!["local".into()],
                    local_only: true,
                },
                30u8,
            ),
            (PlaybackCommand::SetAudioDelayAuto(true), 31),
            (
                PlaybackCommand::SetOutputDelay {
                    output: "local".into(),
                    delay_ms: 50,
                },
                32,
            ),
        ];
        for (c, idx) in pinned {
            let bytes = encode(&c).unwrap();
            assert_eq!(bytes[0], idx, "{c:?} wire index");
            assert_eq!(decode::<PlaybackCommand>(&bytes).unwrap(), c);
            let msg = IpcMessage::PlaybackCommand(c);
            assert_eq!(round_trip(&msg), msg);
        }
        // The command before them keeps its position.
        assert_eq!(
            encode(&PlaybackCommand::SetReshuffleOnRepeat(true)).unwrap()[0],
            29
        );
        // And so does the last message appended before AUDIO_01.
        let un = IpcMessage::UnmatchedNoubin(UnmatchedNoubin {
            noubin_key: "a".into(),
            normalised_key: "a".into(),
            seq: 0,
            candidate_urls: Vec::new(),
        });
        assert_eq!(encode(&un).unwrap()[0], 10, "UnmatchedNoubin stays message 10");
    }

    #[test]
    fn audio_route_changed_round_trips_with_a_full_route() {
        use plippa_core::audio_route::AudioOutput;
        use std::collections::BTreeMap;

        let mut props = BTreeMap::new();
        props.insert("device.bus".into(), "usb".into());
        let route = AudioRoute {
            outputs: vec![
                AudioOutput {
                    id: "local".into(),
                    description: "Built-in output".into(),
                    available: true,
                    is_local: true,
                    delay_ms: 20,
                    props: BTreeMap::new(),
                },
                AudioOutput {
                    id: "usb.headset".into(),
                    description: "USB headset".into(),
                    available: false,
                    is_local: false,
                    delay_ms: 0,
                    props,
                },
            ],
            enabled: vec!["local".into(), "usb.headset".into()],
            paused: vec!["usb.headset".into()],
            local_only: true,
            delay_auto: false,
            manual_delay_ms: BTreeMap::from([("usb.headset".into(), 120)]),
            problem: Some("USB headset is not connected".into()),
            revision: 7,
        };
        let msg = IpcMessage::AudioRouteChanged(route);
        assert_eq!(encode(&msg).unwrap()[0], 11, "AudioRouteChanged is message 11");
        assert_eq!(round_trip(&msg), msg);
    }

    #[test]
    fn play_mode_wire_enums_have_pinned_encodings() {
        assert_eq!(encode(&OrderMode::Normal).unwrap(), [0]);
        assert_eq!(encode(&OrderMode::Shuffle).unwrap(), [1]);
        assert_eq!(encode(&RepeatMode::Off).unwrap(), [0]);
        assert_eq!(encode(&RepeatMode::Queue).unwrap(), [1]);
        assert_eq!(encode(&RepeatMode::Item).unwrap(), [2]);
        assert_eq!(encode(&SourceOrder::Fixed).unwrap(), [0]);
        assert_eq!(encode(&SourceOrder::Dynamic).unwrap(), [1]);
    }

    #[test]
    fn warp_wire_enums_have_pinned_encodings() {
        assert_eq!(encode(&WarpMode::PitchShift).unwrap(), [0]);
        assert_eq!(encode(&WarpMode::PitchCorrect).unwrap(), [1]);
        assert_eq!(encode(&ScanDir::Forward).unwrap(), [0]);
        assert_eq!(encode(&ScanDir::Backward).unwrap(), [1]);
        assert_eq!(encode(&ScanState::None).unwrap(), [0]);
        assert_eq!(encode(&ScanState::Forward).unwrap(), [1]);
        assert_eq!(encode(&ScanState::Backward).unwrap(), [2]);
        assert_eq!(encode(&VoiceKind::Bypass).unwrap(), [0]);
        assert_eq!(encode(&VoiceKind::Varispeed).unwrap(), [1]);
        assert_eq!(encode(&VoiceKind::Stretch).unwrap(), [2]);
    }

    #[test]
    fn session_state_warp_fields_round_trip_and_default() {
        let d = plippa_core::SessionState::default();
        assert_eq!(
            (d.rate_permille, d.target_rate_permille, d.speed_warp_mode, d.seek_warp_mode),
            (1000, 1000, plippa_core::SPEED_WARP_DEFAULT, WarpMode::PitchShift)
        );
        assert_eq!((d.scan, d.seek_target_ms, d.speed_voice), (ScanState::None, None, VoiceKind::Bypass));
        assert!(!d.warp_active && !d.can_ff && !d.can_rw);
        let session = plippa_core::SessionState {
            rate_permille: -750,
            target_rate_permille: 1500,
            speed_warp_mode: WarpMode::PitchCorrect,
            seek_warp_mode: WarpMode::PitchCorrect,
            scan: ScanState::Backward,
            seek_target_ms: Some(42_000),
            speed_voice: VoiceKind::Stretch,
            warp_active: true,
            can_ff: true,
            can_rw: true,
            ..d
        };
        let msg = IpcMessage::PlaybackStatus(PlaybackStatus::from_session_rev(session, 9));
        assert_eq!(round_trip(&msg), msg);
    }

    /// WARP_01 appended ten fields to `SessionState`. Postcard is positional, so the old
    /// field encodings are byte-identical (a prefix of the new encoding) but an **old** frame
    /// no longer decodes into the new struct: all three processes ship from one workspace
    /// build and a mixed-version pair is unsupported (same stance as NB_14).
    #[test]
    fn session_state_old_fields_keep_their_encoding_and_old_frames_do_not_decode() {
        #[derive(Serialize)]
        struct PreWarpSessionState {
            transport: TransportState,
            volume: Volume,
            current_item: Option<PlayableItemId>,
            position_ms: u64,
            duration_ms: Option<u64>,
            title: String,
            artist: String,
            current_entry_index: u32,
            current_cue_index: Option<u32>,
            can_prev: bool,
            can_next: bool,
        }
        let new = plippa_core::SessionState {
            transport: TransportState::Playing,
            volume: Volume::new(30),
            current_item: Some(PlayableItemId::new(5)),
            position_ms: 61_000,
            duration_ms: Some(200_000),
            title: "T".into(),
            artist: "A".into(),
            current_entry_index: 2,
            current_cue_index: Some(1),
            can_prev: true,
            can_next: true,
            ..Default::default()
        };
        let old = PreWarpSessionState {
            transport: new.transport,
            volume: new.volume,
            current_item: new.current_item,
            position_ms: new.position_ms,
            duration_ms: new.duration_ms,
            title: new.title.clone(),
            artist: new.artist.clone(),
            current_entry_index: new.current_entry_index,
            current_cue_index: new.current_cue_index,
            can_prev: new.can_prev,
            can_next: new.can_next,
        };
        let old_bytes = encode(&old).unwrap();
        let new_bytes = encode(&new).unwrap();
        assert!(new_bytes.starts_with(&old_bytes), "old fields unchanged and first");
        assert!(decode::<plippa_core::SessionState>(&old_bytes).is_err());
    }

    /// WAVE_01 appended exactly one `Option<String>` after the WARP_19 fields. Same stance as
    /// WARP_01: the old bytes stay a prefix; an old frame does not decode into the new struct.
    #[test]
    fn session_state_waveform_path_is_appended_and_old_frames_do_not_decode() {
        #[derive(Serialize)]
        struct PreWaveSessionState {
            transport: TransportState,
            volume: Volume,
            current_item: Option<PlayableItemId>,
            position_ms: u64,
            duration_ms: Option<u64>,
            title: String,
            artist: String,
            current_entry_index: u32,
            current_cue_index: Option<u32>,
            can_prev: bool,
            can_next: bool,
            rate_permille: i32,
            target_rate_permille: i32,
            speed_warp_mode: WarpMode,
            seek_warp_mode: WarpMode,
            scan: ScanState,
            seek_target_ms: Option<u64>,
            speed_voice: VoiceKind,
            warp_active: bool,
            can_ff: bool,
            can_rw: bool,
            seek_outside_track: bool,
            prev_duration_ms: Option<u64>,
            next_duration_ms: Option<u64>,
        }
        let new = plippa_core::SessionState {
            transport: TransportState::Playing,
            volume: Volume::new(70),
            current_item: Some(PlayableItemId::new(7)),
            position_ms: 1_000,
            duration_ms: Some(90_000),
            title: "T".into(),
            artist: "A".into(),
            current_entry_index: 1,
            current_cue_index: Some(0),
            can_prev: true,
            can_next: true,
            rate_permille: 1000,
            target_rate_permille: 1000,
            seek_outside_track: true,
            prev_duration_ms: Some(10),
            next_duration_ms: Some(20),
            waveform_path: Some("/media/stick/.plippa/waveforms/0123456789abcdef.wfm".into()),
            ..Default::default()
        };
        let old = PreWaveSessionState {
            transport: new.transport,
            volume: new.volume,
            current_item: new.current_item,
            position_ms: new.position_ms,
            duration_ms: new.duration_ms,
            title: new.title.clone(),
            artist: new.artist.clone(),
            current_entry_index: new.current_entry_index,
            current_cue_index: new.current_cue_index,
            can_prev: new.can_prev,
            can_next: new.can_next,
            rate_permille: new.rate_permille,
            target_rate_permille: new.target_rate_permille,
            speed_warp_mode: new.speed_warp_mode,
            seek_warp_mode: new.seek_warp_mode,
            scan: new.scan,
            seek_target_ms: new.seek_target_ms,
            speed_voice: new.speed_voice,
            warp_active: new.warp_active,
            can_ff: new.can_ff,
            can_rw: new.can_rw,
            seek_outside_track: new.seek_outside_track,
            prev_duration_ms: new.prev_duration_ms,
            next_duration_ms: new.next_duration_ms,
        };
        let old_bytes = encode(&old).unwrap();
        let new_bytes = encode(&new).unwrap();
        assert!(new_bytes.starts_with(&old_bytes), "old fields unchanged and first");
        let extra = &new_bytes[old_bytes.len()..];
        assert_eq!(extra[0], 1, "Some tag");
        // Path is shorter than 128 bytes, so postcard uses a one-byte length prefix.
        // UI_13 appended an empty `collection_title`; PLAY_02 appended the five play-mode
        // fields after it (Normal / Off / false / true / Fixed).
        assert_eq!(
            &extra[2..extra.len() - 6],
            new.waveform_path.as_ref().unwrap().as_bytes()
        );
        assert_eq!(&extra[extra.len() - 6..], &[0u8, 0, 0, 0, 1, 0][..],
            "empty collection_title, then Normal / Off / false / true / Fixed");
        assert!(decode::<plippa_core::SessionState>(&old_bytes).is_err());

        // `None` is the default and still one appended byte.
        let none = plippa_core::SessionState {
            waveform_path: None,
            ..new
        };
        assert_eq!(none.waveform_path, None);
        assert_eq!(decode::<plippa_core::SessionState>(&encode(&none).unwrap()).unwrap(), none);
    }

    /// UI_13 appended exactly one `String` after `waveform_path`. Same stance as WARP_01 /
    /// WAVE_01: the old bytes stay a prefix and an old frame does not decode into the new struct.
    #[test]
    fn session_state_collection_title_is_appended_and_old_frames_do_not_decode() {
        #[derive(Serialize)]
        struct PreCollectionSessionState {
            transport: TransportState,
            volume: Volume,
            current_item: Option<PlayableItemId>,
            position_ms: u64,
            duration_ms: Option<u64>,
            title: String,
            artist: String,
            current_entry_index: u32,
            current_cue_index: Option<u32>,
            can_prev: bool,
            can_next: bool,
            rate_permille: i32,
            target_rate_permille: i32,
            speed_warp_mode: WarpMode,
            seek_warp_mode: WarpMode,
            scan: ScanState,
            seek_target_ms: Option<u64>,
            speed_voice: VoiceKind,
            warp_active: bool,
            can_ff: bool,
            can_rw: bool,
            seek_outside_track: bool,
            prev_duration_ms: Option<u64>,
            next_duration_ms: Option<u64>,
            waveform_path: Option<String>,
        }
        let new = plippa_core::SessionState {
            transport: TransportState::Playing,
            volume: Volume::new(70),
            current_item: Some(PlayableItemId::new(7)),
            position_ms: 1_000,
            duration_ms: Some(90_000),
            title: "T".into(),
            artist: "A".into(),
            current_entry_index: 1,
            current_cue_index: Some(0),
            can_prev: true,
            can_next: true,
            rate_permille: 1000,
            target_rate_permille: 1000,
            seek_outside_track: true,
            prev_duration_ms: Some(10),
            next_duration_ms: Some(20),
            waveform_path: Some("/media/stick/.plippa/waveforms/0123456789abcdef.wfm".into()),
            collection_title: "The Long Album".into(),
            ..Default::default()
        };
        let old = PreCollectionSessionState {
            transport: new.transport,
            volume: new.volume,
            current_item: new.current_item,
            position_ms: new.position_ms,
            duration_ms: new.duration_ms,
            title: new.title.clone(),
            artist: new.artist.clone(),
            current_entry_index: new.current_entry_index,
            current_cue_index: new.current_cue_index,
            can_prev: new.can_prev,
            can_next: new.can_next,
            rate_permille: new.rate_permille,
            target_rate_permille: new.target_rate_permille,
            speed_warp_mode: new.speed_warp_mode,
            seek_warp_mode: new.seek_warp_mode,
            scan: new.scan,
            seek_target_ms: new.seek_target_ms,
            speed_voice: new.speed_voice,
            warp_active: new.warp_active,
            can_ff: new.can_ff,
            can_rw: new.can_rw,
            seek_outside_track: new.seek_outside_track,
            prev_duration_ms: new.prev_duration_ms,
            next_duration_ms: new.next_duration_ms,
            waveform_path: new.waveform_path.clone(),
        };
        let old_bytes = encode(&old).unwrap();
        let new_bytes = encode(&new).unwrap();
        assert!(new_bytes.starts_with(&old_bytes), "old fields unchanged and first");
        let extra = &new_bytes[old_bytes.len()..];
        // Title is shorter than 128 bytes, so postcard uses a one-byte length prefix.
        // PLAY_02 appended the five play-mode fields after the title.
        assert_eq!(usize::from(extra[0]), new.collection_title.len());
        assert_eq!(
            &extra[1..extra.len() - 5],
            new.collection_title.as_bytes()
        );
        assert_eq!(
            &extra[extra.len() - 5..],
            &[0u8, 0, 0, 1, 0][..],
            "Normal / Off / false / true / Fixed"
        );
        assert!(decode::<plippa_core::SessionState>(&old_bytes).is_err());

        // The empty default is a zero-length title, then the default play-mode fields.
        let empty = plippa_core::SessionState {
            collection_title: String::new(),
            ..new
        };
        let bytes = encode(&empty).unwrap();
        assert_eq!(bytes[..old_bytes.len()], old_bytes[..]);
        assert_eq!(&bytes[old_bytes.len()..], &[0u8, 0, 0, 0, 1, 0][..]);
        assert_eq!(
            decode::<plippa_core::SessionState>(&bytes).unwrap(),
            empty
        );
    }

    #[test]
    fn now_playing_list_changed_round_trips() {
        use plippa_core::NowPlayingEntryKind;
        let entry = |kind, title: &str, start| NowPlayingEntry {
            kind,
            title: title.into(),
            artist: "Artist".into(),
            start_ms: start,
            end_ms: (kind == NowPlayingEntryKind::Cue).then_some(start + 1_000),
            duration_ms: (kind == NowPlayingEntryKind::Track).then_some(180_000),
            track_index: 0,
            playable_id: (kind == NowPlayingEntryKind::Track).then(|| PlayableItemId::new(5)),
            available: true,
            item_position: 0,
        };
        let msg = IpcMessage::NowPlayingListChanged(NowPlayingListChanged {
            entries: vec![
                entry(NowPlayingEntryKind::Track, "One", 0),
                entry(NowPlayingEntryKind::Cue, "Intro", 0),
                entry(NowPlayingEntryKind::Cue, "Verse", 1_000),
            ],
            current_index: 0,
            list_kind: NowPlayingListKind::Mix,
            list_id: 1,
        });
        assert_eq!(round_trip(&msg), msg);

        let empty = IpcMessage::NowPlayingListChanged(NowPlayingListChanged {
            entries: vec![],
            current_index: 0,
            list_kind: NowPlayingListKind::Single,
            list_id: 0,
        });
        assert_eq!(round_trip(&empty), empty);
    }

    #[test]
    fn resume_writeback_round_trips() {
        for (item, ms) in [(Some(3), Some(61_500)), (None, None)] {
            let msg = IpcMessage::ResumeWriteback(ResumeWriteback {
                playable_id: PlayableItemId::new(1 << 40 | 9),
                resume_at_item: item,
                resume_from_ms: ms,
            });
            assert_eq!(round_trip(&msg), msg);
        }
    }

    #[test]
    fn playback_status_carries_entry_and_cue_indices() {
        let session = plippa_core::SessionState {
            current_entry_index: 2,
            current_cue_index: Some(4),
            ..Default::default()
        };
        let msg = IpcMessage::PlaybackStatus(PlaybackStatus::from_session_rev(session, 3));
        let IpcMessage::PlaybackStatus(s) = round_trip(&msg) else {
            panic!("status");
        };
        assert_eq!(s.session.current_entry_index, 2);
        assert_eq!(s.session.current_cue_index, Some(4));
    }

    #[test]
    fn new_variants_are_appended_so_old_wire_bytes_still_decode() {
        // Postcard encodes the variant index first: existing variants keep their index.
        let bytes = encode(&IpcMessage::AssociateResponse(AssociateResponse {
            request_id: 1,
            ok: false,
            noubin_key: None,
            noubin_key_normalised: None,
            error: Some("x".into()),
            ..Default::default()
        }))
        .unwrap();
        assert_eq!(bytes[0], 7, "AssociateResponse stays variant 7");
        let np = encode(&IpcMessage::ResumeWriteback(ResumeWriteback {
            playable_id: PlayableItemId::new(1),
            resume_at_item: None,
            resume_from_ms: None,
        }))
        .unwrap();
        assert_eq!(np[0], 9);
        let un = encode(&IpcMessage::UnmatchedNoubin(UnmatchedNoubin {
            noubin_key: "https://a.example/x".into(),
            normalised_key: "a.example/x/".into(),
            seq: 1,
            candidate_urls: Vec::new(),
        }))
        .unwrap();
        assert_eq!(un[0], 10, "UnmatchedNoubin is appended after ResumeWriteback");
        let unload = encode(&PlaybackCommand::Unload).unwrap();
        assert_eq!(unload[0], 9, "Unload stays command 9; new commands follow");
        assert_eq!(encode(&PlaybackCommand::Next).unwrap()[0], 11);
    }

    #[test]
    fn nb04_variants_keep_their_pinned_wire_positions() {
        // Appended in this order by NB_04 (postcard = variant index first byte). Reordering
        // or inserting breaks a UI/engine/server built from different commits.
        let pid = PlayableItemId::new(1);
        let cmd = |c: PlaybackCommand| encode(&c).unwrap()[0];
        assert_eq!(cmd(PlaybackCommand::LoadCollection { playable_id: pid }), 10);
        assert_eq!(cmd(PlaybackCommand::Next), 11);
        assert_eq!(cmd(PlaybackCommand::Previous), 12);
        assert_eq!(cmd(PlaybackCommand::JumpToEntry { index: 0 }), 13);
        assert_eq!(
            cmd(PlaybackCommand::SetCueListEnabled { list_index: 0, enabled: true }),
            14
        );
        let list = IpcMessage::NowPlayingListChanged(NowPlayingListChanged {
            entries: vec![],
            current_index: 0,
            list_kind: NowPlayingListKind::Single,
            list_id: 0,
        });
        assert_eq!(encode(&list).unwrap()[0], 8, "NowPlayingListChanged is message 8");
    }

    #[test]
    fn unmatched_noubin_round_trips() {
        let msg = IpcMessage::UnmatchedNoubin(UnmatchedNoubin {
            noubin_key: "https://artist.com/noubin/album?ref=nfc".into(),
            normalised_key: "artist.com/noubin/album/".into(),
            seq: 3,
            candidate_urls: vec!["https://artist.com/noubin/album?ref=nfc".into()],
        });
        assert_eq!(round_trip(&msg), msg);
    }

    #[test]
    fn tap_key_round_trips() {
        let msg = IpcMessage::TapKey(TapKeyPayload {
            noubin_key: "artist.com/album/".into(),
            tap_id: 7,
            already_normalised: true,
        });
        assert_eq!(round_trip(&msg), msg);
    }

    #[test]
    fn playback_status_round_trips() {
        let mut session = plippa_core::SessionState::default();
        session.current_item = Some(PlayableItemId::new(3));
        session.transport = TransportState::Playing;
        session.volume = Volume::new(40);
        session.position_ms = 12_345;
        session.duration_ms = Some(180_000);
        session.title = "Tone".into();
        session.artist = "Test Artist".into();
        let msg = IpcMessage::PlaybackStatus(PlaybackStatus::from_session_rev(session, 9));
        let decoded = round_trip(&msg);
        assert_eq!(decoded, msg);
        if let IpcMessage::PlaybackStatus(s) = decoded {
            assert_eq!(s.revision, 9);
            assert_eq!(s.session.position_ms, 12_345);
            assert_eq!(s.session.duration_ms, Some(180_000));
            assert_eq!(s.session.title, "Tone");
            assert_eq!(s.session.artist, "Test Artist");
        }
    }

    #[test]
    fn playback_status_snapshot_includes_now_playing_fields() {
        let mut session = plippa_core::SessionState::default();
        session.position_ms = 1_000;
        session.duration_ms = Some(60_000);
        session.title = "Song".into();
        session.artist = String::new(); // empty artist is valid
        let status = PlaybackStatus::from_session(session);
        assert_eq!(status.session.position_ms, 1_000);
        assert_eq!(status.session.duration_ms, Some(60_000));
        assert_eq!(status.session.title, "Song");
        assert!(status.session.artist.is_empty());
    }

    #[test]
    fn library_index_updated_round_trips() {
        let msg = IpcMessage::LibraryIndexUpdated(LibraryIndexUpdated {
            generation: 7,
            item_count: 128,
        });
        assert_eq!(round_trip(&msg), msg);
    }

    #[test]
    fn user_notice_round_trips() {
        let msg = IpcMessage::UserNotice(UserNotice {
            severity: NoticeSeverity::Warning,
            code: "no_media_storage".into(),
            message: "No media storage found".into(),
        });
        assert_eq!(round_trip(&msg), msg);
    }

    #[test]
    fn associate_request_response_round_trip() {
        let req = IpcMessage::AssociateRequest(AssociateRequest {
            request_id: 3,
            playable_id: PlayableItemId::new(11),
            noudata_relpath: "Music/a.noudata".into(),
            playable_item_title: "Smooth Jazz".into(),
            noubin_key: "Smooth Jazz.noubin.ABC123".into(),
            keep_existing_key: false,
        });
        assert_eq!(round_trip(&req), req);

        let keep = IpcMessage::AssociateRequest(AssociateRequest {
            request_id: 4,
            playable_id: PlayableItemId::new(12),
            noudata_relpath: "Music/b.noudata".into(),
            playable_item_title: "Reuse".into(),
            noubin_key: String::new(),
            keep_existing_key: true,
        });
        assert_eq!(round_trip(&keep), keep);

        let resp = IpcMessage::AssociateResponse(AssociateResponse {
            request_id: 3,
            ok: true,
            noubin_key: Some("Smooth Jazz.noubin.ABC123".into()),
            noubin_key_normalised: Some("smooth jazz.noubin.abc123".into()),
            error: None,
            ..Default::default()
        });
        assert_eq!(round_trip(&resp), resp);
    }

    #[test]
    fn decode_truncated_bytes_returns_error() {
        let msg = IpcMessage::PlaybackCommand(PlaybackCommand::Play);
        let bytes = encode(&msg).unwrap();
        assert!(bytes.len() > 1);
        let truncated = &bytes[..bytes.len() - 1];
        let err = decode::<IpcMessage>(truncated);
        assert!(err.is_err(), "truncated Postcard must not decode successfully");
    }

    #[test]
    fn decode_invalid_bytes_returns_error_no_panic() {
        let garbage = [0xff, 0xfe, 0xfd, 0x00, 0x01];
        let err = decode::<IpcMessage>(&garbage);
        assert!(err.is_err());
    }

    #[test]
    fn decode_empty_returns_error() {
        assert!(decode::<IpcMessage>(&[]).is_err());
    }

    /// PLAY_02 appended the five play-mode fields after `collection_title`. Same stance as
    /// WARP_01 / WAVE_01 / UI_13: the old bytes stay a prefix and an old frame does not decode.
    #[test]
    fn session_state_play_mode_fields_are_appended_and_old_frames_do_not_decode() {
        #[derive(Serialize)]
        struct PrePlayModeSessionState {
            transport: TransportState,
            volume: Volume,
            current_item: Option<PlayableItemId>,
            position_ms: u64,
            duration_ms: Option<u64>,
            title: String,
            artist: String,
            current_entry_index: u32,
            current_cue_index: Option<u32>,
            can_prev: bool,
            can_next: bool,
            rate_permille: i32,
            target_rate_permille: i32,
            speed_warp_mode: WarpMode,
            seek_warp_mode: WarpMode,
            scan: ScanState,
            seek_target_ms: Option<u64>,
            speed_voice: VoiceKind,
            warp_active: bool,
            can_ff: bool,
            can_rw: bool,
            seek_outside_track: bool,
            prev_duration_ms: Option<u64>,
            next_duration_ms: Option<u64>,
            waveform_path: Option<String>,
            collection_title: String,
        }
        let new = plippa_core::SessionState {
            transport: TransportState::Paused,
            volume: Volume::new(40),
            current_item: Some(PlayableItemId::new(8)),
            position_ms: 2_000,
            duration_ms: Some(60_000),
            title: "T".into(),
            artist: "A".into(),
            current_entry_index: 1,
            current_cue_index: Some(2),
            can_prev: true,
            can_next: true,
            waveform_path: Some("/media/w.wfm".into()),
            collection_title: "The Album".into(),
            order_mode: OrderMode::Shuffle,
            repeat_mode: RepeatMode::Queue,
            pause_after_each: true,
            reshuffle_on_repeat: false,
            source_order: SourceOrder::Dynamic,
            ..Default::default()
        };
        let old = PrePlayModeSessionState {
            transport: new.transport,
            volume: new.volume,
            current_item: new.current_item,
            position_ms: new.position_ms,
            duration_ms: new.duration_ms,
            title: new.title.clone(),
            artist: new.artist.clone(),
            current_entry_index: new.current_entry_index,
            current_cue_index: new.current_cue_index,
            can_prev: new.can_prev,
            can_next: new.can_next,
            rate_permille: new.rate_permille,
            target_rate_permille: new.target_rate_permille,
            speed_warp_mode: new.speed_warp_mode,
            seek_warp_mode: new.seek_warp_mode,
            scan: new.scan,
            seek_target_ms: new.seek_target_ms,
            speed_voice: new.speed_voice,
            warp_active: new.warp_active,
            can_ff: new.can_ff,
            can_rw: new.can_rw,
            seek_outside_track: new.seek_outside_track,
            prev_duration_ms: new.prev_duration_ms,
            next_duration_ms: new.next_duration_ms,
            waveform_path: new.waveform_path.clone(),
            collection_title: new.collection_title.clone(),
        };
        let old_bytes = encode(&old).unwrap();
        let new_bytes = encode(&new).unwrap();
        assert!(new_bytes.starts_with(&old_bytes), "old fields unchanged and first");
        assert_eq!(
            &new_bytes[old_bytes.len()..],
            &[1u8, 1, 1, 0, 1][..],
            "Shuffle, Queue, stop yes, reshuffle no, Dynamic — in this order"
        );
        assert!(decode::<plippa_core::SessionState>(&old_bytes).is_err());
        assert_eq!(
            decode::<plippa_core::SessionState>(&new_bytes).unwrap(),
            new
        );

        // Nothing loaded: the documented defaults.
        let d = plippa_core::SessionState::default();
        assert_eq!(
            (
                d.order_mode,
                d.repeat_mode,
                d.pause_after_each,
                d.reshuffle_on_repeat,
                d.source_order
            ),
            (
                OrderMode::Normal,
                RepeatMode::Off,
                false,
                true,
                SourceOrder::Fixed
            )
        );
    }

    /// PLAY_02 appended `item_position` to `NowPlayingEntry`.
    #[test]
    fn now_playing_entry_item_position_is_appended() {
        use plippa_core::NowPlayingEntryKind;

        #[derive(Serialize)]
        struct PreItemPositionEntry {
            kind: NowPlayingEntryKind,
            title: String,
            artist: String,
            start_ms: u64,
            end_ms: Option<u64>,
            duration_ms: Option<u64>,
            track_index: u32,
            playable_id: Option<PlayableItemId>,
            available: bool,
        }
        let new = NowPlayingEntry {
            kind: NowPlayingEntryKind::Track,
            title: "T".into(),
            artist: "A".into(),
            start_ms: 0,
            end_ms: None,
            duration_ms: Some(1_000),
            track_index: 3,
            playable_id: Some(PlayableItemId::new(4)),
            available: true,
            item_position: 7,
        };
        let old = PreItemPositionEntry {
            kind: new.kind,
            title: new.title.clone(),
            artist: new.artist.clone(),
            start_ms: new.start_ms,
            end_ms: new.end_ms,
            duration_ms: new.duration_ms,
            track_index: new.track_index,
            playable_id: new.playable_id,
            available: new.available,
        };
        let old_bytes = encode(&old).unwrap();
        let new_bytes = encode(&new).unwrap();
        assert!(new_bytes.starts_with(&old_bytes));
        assert_eq!(
            &new_bytes[old_bytes.len()..],
            &[7u8][..],
            "u32 source position, one varint byte"
        );
        assert!(decode::<NowPlayingEntry>(&old_bytes).is_err());
        assert_eq!(decode::<NowPlayingEntry>(&new_bytes).unwrap(), new);

        // Cue rows carry 0: cues are identified by `start_ms`.
        let cue = NowPlayingEntry {
            kind: NowPlayingEntryKind::Cue,
            item_position: 0,
            ..new
        };
        assert_eq!(cue.item_position, 0);
        assert_eq!(decode::<NowPlayingEntry>(&encode(&cue).unwrap()).unwrap(), cue);
    }

    /// PLAY_02 appended `list_id` to `NowPlayingListChanged` (list identity across a reorder).
    #[test]
    fn now_playing_list_changed_carries_a_list_id() {
        use plippa_core::NowPlayingEntryKind;

        #[derive(Serialize)]
        struct PreListIdChanged {
            entries: Vec<NowPlayingEntry>,
            current_index: u32,
            list_kind: NowPlayingListKind,
        }
        let entry = NowPlayingEntry {
            kind: NowPlayingEntryKind::Track,
            title: "One".into(),
            artist: String::new(),
            start_ms: 0,
            end_ms: None,
            duration_ms: Some(1_000),
            track_index: 0,
            playable_id: None,
            available: true,
            item_position: 0,
        };
        let new = NowPlayingListChanged {
            entries: vec![entry.clone()],
            current_index: 0,
            list_kind: NowPlayingListKind::Album,
            list_id: 7,
        };
        let old = PreListIdChanged {
            entries: vec![entry],
            current_index: new.current_index,
            list_kind: new.list_kind,
        };
        let old_bytes = encode(&old).unwrap();
        let new_bytes = encode(&new).unwrap();
        assert!(new_bytes.starts_with(&old_bytes));
        assert_eq!(&new_bytes[old_bytes.len()..], &[7u8][..]);
        assert!(decode::<NowPlayingListChanged>(&old_bytes).is_err());
        assert_eq!(decode::<NowPlayingListChanged>(&new_bytes).unwrap(), new);
    }

    // ---------------------------------------------------------------------------------------
    // BCS_07: `UnmatchedNoubin` gained `candidate_urls` and mixed-version peers must both work.
    // ---------------------------------------------------------------------------------------

    /// The protocol-1 shape, kept here so the "old peer" is a real encoder, not a mock.
    #[derive(Serialize, Deserialize, PartialEq, Eq, Debug)]
    struct PreUnmatched {
        noubin_key: String,
        normalised_key: String,
        seq: u64,
    }

    fn legacy_notice() -> PreUnmatched {
        PreUnmatched {
            noubin_key: "https://sable-music.bandcamp.com/track/were-getting-older-baby".into(),
            normalised_key: "https://sable-music.bandcamp.com/track/were-getting-older-baby".into(),
            seq: 4,
        }
    }

    /// New engine → old UI: the new frame is the old frame plus one trailing field, and
    /// Postcard leaves trailing bytes unread, so the old decoder still sees its notice.
    #[test]
    fn unmatched_noubin_new_frame_still_decodes_in_an_old_ui() {
        let legacy = legacy_notice();
        let new = UnmatchedNoubin {
            noubin_key: legacy.noubin_key.clone(),
            normalised_key: legacy.normalised_key.clone(),
            seq: legacy.seq,
            candidate_urls: vec!["https://sable-music.bandcamp.com/track/x".into()],
        };
        let legacy_bytes = encode(&legacy).unwrap();
        let new_bytes = encode(&IpcMessage::UnmatchedNoubin(new.clone())).unwrap();
        // Variant tag, then the same three fields, then the appended list.
        assert_eq!(new_bytes[0], 10);
        assert!(
            new_bytes[1..].starts_with(&legacy_bytes[..]),
            "the legacy fields keep their bytes; the new field is only appended"
        );
        // Postcard ignores the trailing bytes an old build does not know about.
        let seen_by_old_ui: PreUnmatched = decode(&new_bytes[1..]).unwrap();
        assert_eq!(seen_by_old_ui, legacy);
    }

    /// Old engine → new UI: a protocol-1 body ends after `seq`; the absent field is an empty
    /// list, not a decode error, so the notice still arrives.
    #[test]
    fn unmatched_noubin_old_frame_still_decodes_in_a_new_ui() {
        let legacy = legacy_notice();
        // Hand-build the protocol-1 frame: variant tag + the three legacy fields, no list.
        let mut body = vec![10u8];
        body.extend_from_slice(&encode(&legacy).unwrap());
        let decoded: IpcMessage = decode(&body).unwrap();
        let IpcMessage::UnmatchedNoubin(got) = decoded else {
            panic!("wrong variant");
        };
        assert_eq!(got.noubin_key, legacy.noubin_key);
        assert_eq!(got.normalised_key, legacy.normalised_key);
        assert_eq!(got.seq, legacy.seq);
        assert!(got.candidate_urls.is_empty(), "an absent list is empty, never a guess");
        // And the current round trip keeps the list.
        let with_list = UnmatchedNoubin {
            candidate_urls: vec![
                "https://sable-music.bandcamp.com/track/x".into(),
                "https://example.test/b".into(),
            ],
            ..got
        };
        assert_eq!(
            round_trip(&IpcMessage::UnmatchedNoubin(with_list.clone())),
            IpcMessage::UnmatchedNoubin(with_list)
        );
    }

    /// Appending is the only wire change allowed: a variant index must never move. These
    /// numbers are the protocol; changing one is a breaking change for every peer.
    #[test]
    fn protocol_version_and_variant_indices_are_pinned() {
        assert_eq!(PROTOCOL_VERSION, 2, "BCS_07 appended a notice field");
        assert_eq!(USED_MESSAGE_VARIANTS, 12);
        assert_eq!(USED_COMMAND_VARIANTS, 33);

        let first = encode(&IpcMessage::TapNdef(TapNdefPayload {
            ndef_message: Vec::new(),
            tap_id: 0,
        }))
        .unwrap();
        assert_eq!(first[0], 0, "TapNdef is variant 0");
        let unmatched = encode(&IpcMessage::UnmatchedNoubin(UnmatchedNoubin::default())).unwrap();
        assert_eq!(unmatched[0], 10, "UnmatchedNoubin is variant 10");
        let route = encode(&IpcMessage::AudioRouteChanged(AudioRoute::default())).unwrap();
        assert_eq!(route[0], 11, "AudioRouteChanged is the last variant");
        let last_cmd = encode(&IpcMessage::PlaybackCommand(
            PlaybackCommand::SetOutputDelay {
                output: String::new(),
                delay_ms: 0,
            },
        ))
        .unwrap();
        assert_eq!(last_cmd[1], 32, "PlaybackCommand index rides after variant 2");
    }
}
