//! Screen models and text-frame rendering for MVP routes.
//!
//! Painting is layered by [`crate::compose::Compositor`]: screens paint the
//! full-frame **screen** layer; the main menu is a separate top overlay.
//!
//! Every paint call goes through the design system in [`crate::ui`]: colours and spacing from
//! [`crate::ui::tokens`], type roles and measurement from [`crate::ui::text`], and the
//! standard chrome from [`crate::ui::widgets`]. There are no colour literals in this module.

use crate::accessories::{kind_text, prompt_peer_lines, AccessoriesState, PairStage};
use crate::analysis::AnalysisView;
use crate::audio::{delay_blocked, AudioRow, AudioView, AUDIO_ROWS};
use crate::keyboard::KeyboardField;
use crate::compose::MAIN_MENU_H;
use crate::build_info;
use crate::control::{AdjustParam, NowPlayingFocus, NpMode, NpTarget};
use crate::diag;
use crate::display::{FrameBuffer, Rgb565};
use crate::nav::{NavStack, Route};
use crate::network::{NetworkRow, NetworkView, PIN_DIGITS};
use crate::pinpad::PinPad;
use crate::store_tap::{CoverState, StoreTapPhase, StoreTapView};
use crate::store_browse::{
    ConnectAction, ConnectPhase, ConnectView, DeleteAction, DeletePhase, DeleteView, DownloadPhase,
    ReleaseAction, ReleasePhase, ReleaseView, ReleasesPhase, ReleasesRow, ReleasesView,
    StoresAction, StoresView, CONNECT_INSTRUCTION,
};
use crate::play_mode::{
    PlayModeValues, PlayModeView, TimecodeUnit, DEBUG_ROWS, PLAYBACK_SETTINGS_ROWS,
};
use crate::art::paint_art_square;
use crate::ui::primitives::{
    blend_toward, border, chevron_left, chevron_right, fill, hairline, scrollbar, triangle_down,
};
use crate::ui::text::{self, cell_height, measure, Align, TypeRole};
use crate::ui::tokens::*;
use crate::ui::widgets::{self, DataRow, Header};
use crate::warp_ui::JogView;
use crate::wifi::WifiForm;
use plippa_core::{
    NowPlayingEntry, NowPlayingEntryKind, NowPlayingListKind, PlayableItemId, TransportState,
    Volume,
};
use plippa_hw::PairPrompt;
use plippa_ipc_proto::{NoticeSeverity, NowPlayingListChanged, PlaybackStatus, UserNotice};
use plippa_noubin::covers::CoverRef;
use plippa_noubin::{encode_playable_id, sanitise_line, LibraryKind};

/// Gap between the rule under the Playback settings rows and their description. The window is
/// sized for two `Small` description lines, which is what the longest row needs.
pub const PLAY_MODE_DESC_GAP: u16 = 2;
/// Row pitch of the merged Playback settings list. [`widgets::setting_row`] always fills a
/// whole [`ROW_H`], so anything less would overlap the rows (the focus ring is drawn inside
/// each row).
pub const PLAY_MODE_ROW_PITCH: u16 = ROW_H;
/// Rows the merged Playback settings screen shows at once. Eleven rows do not fit the content
/// box with a description, so the list windows on the cursor (with a scrollbar) instead of
/// dropping a row or the description.
pub const PLAY_MODE_VISIBLE_ROWS: usize = 8;
/// Waveform band (UI_13): the Now Playing content box, so the band is full width and the
/// 30 px footer sits exactly on the Now Playing token boundary.
pub const WAVE_BAND_Y: u16 = NP_CONTENT_TOP;
pub const WAVE_BAND_BOTTOM: u16 = NP_CONTENT_BOTTOM;
pub const WAVE_BAND_H: u16 = WAVE_BAND_BOTTOM - WAVE_BAND_Y;
/// Centre line the mirrored bars grow from.
pub const WAVE_CENTRE_Y: u16 = WAVE_BAND_Y + WAVE_BAND_H / 2;
/// Display curve applied to the stored dB peaks.
pub const WAVE_GAMMA: f32 = 1.5;
/// ~30 FPS animation tick while the picture moves.
pub const WAVE_FRAME_MS: u64 = 33;
/// Refresh tick when the band is static and the footer clock shows whole seconds only
/// (sidecar pending or failed, timecode unit `Off`): one repaint per displayed second instead
/// of 30 static compositions. Sub-second units tick faster (see `tick_waveform`).
pub const WAVE_CLOCK_MS: u64 = 1_000;
/// The fixed playhead column (the waveform scrolls under it).
pub const WAVE_PLAYHEAD_X: u16 = SCREEN_W / 2;

/// Now Playing cover art (ART_08, D10): a square at the left edge of the waveform band,
/// vertically centred. 96 px is 30 % of the screen width.
pub const NP_ART_SIDE: u16 = 96;
pub const NP_ART_X: u16 = 0;
pub const NP_ART_Y: u16 = WAVE_BAND_Y + (WAVE_BAND_H - NP_ART_SIDE) / 2;
// Owner range: 10-40 % of the width; the square must sit inside the band (never over the header
// or the footer) and end before the playhead column.
const _: () = assert!(NP_ART_SIDE * 10 >= SCREEN_W && NP_ART_SIDE * 100 <= SCREEN_W * 40);
const _: () = assert!(NP_ART_SIDE <= WAVE_BAND_H);
const _: () = assert!(NP_ART_Y >= WAVE_BAND_Y && NP_ART_Y + NP_ART_SIDE <= WAVE_BAND_BOTTOM);
const _: () = assert!(NP_ART_X + NP_ART_SIDE < WAVE_PLAYHEAD_X);

/// Library cover viewer (ART_09, D11): the image is a square of this side, centred in the
/// content box.
pub const COVER_VIEW_SIDE: u16 = 176;
/// The art box on the Library detail screen.
pub const DETAIL_ART_SIDE: u16 = 72;
pub const COVER_VIEW_X: u16 = (SCREEN_W - COVER_VIEW_SIDE) / 2;
pub const COVER_VIEW_Y: u16 = CONTENT_TOP + (CONTENT_BOTTOM - CONTENT_TOP - COVER_VIEW_SIDE) / 2;
const _: () = assert!(COVER_VIEW_SIDE <= CONTENT_BOTTOM - CONTENT_TOP);
const _: () = assert!(COVER_VIEW_Y >= CONTENT_TOP && COVER_VIEW_Y + COVER_VIEW_SIDE <= CONTENT_BOTTOM);
/// Longest header meta (the image title) on the cover screen, in pixels.
const COVER_TITLE_MAX_PX: u16 = 150;

/// Up Next overlay (WAVE_02): a re-skin of the old bottom panel that covers the waveform.
pub const UP_NEXT_TOP: u16 = NP_CONTENT_TOP + SPACE_1;
pub const UP_NEXT_HEADER_Y: u16 = UP_NEXT_TOP + SPACE_1;
/// First pixel row of the first row in the overlay.
pub const UP_NEXT_ROWS_Y: u16 = UP_NEXT_HEADER_Y + ROW_H + SPACE_1;
/// Visible rows: everything above the Now Playing footer, floored to whole rows.
pub const UP_NEXT_ROWS: u16 = (NP_CONTENT_BOTTOM.saturating_sub(UP_NEXT_ROWS_Y)) / ROW_H;
const _: () = assert!(UP_NEXT_ROWS >= 3);

/// Adjust-mode overlay (UI_12 owner decision): the transport / jog / tempo row WAVE_02 removed
/// reappears over the waveform while a ring parameter is being adjusted. It sits at the bottom
/// of the band, clear of the centre line and the fixed playhead's readout.
pub const ADJUST_BAND_H: u16 = ROW_H_DISPLAY;
/// Top pixel row of the overlay band.
pub const ADJUST_BAND_Y: u16 = WAVE_BAND_BOTTOM - ADJUST_BAND_H - SPACE_1;
/// How far the band's backdrop moves each pixel toward `BG`. The owner asked for a 40 %
/// transparent band, i.e. 40 % of the waveform stays visible under a 60 % blend to `BG`.
/// This is a deliberately scoped exception to the design system's "no alpha" rule.
pub const ADJUST_BAND_BLEND_PCT: u8 = 60;
/// Label of the jog ring parameter (pointer scrub).
pub const JOG_LABEL: &str = "JOG";
/// Label of the tempo ring parameter (set speed).
pub const TEMPO_LABEL: &str = "TEMPO";
/// Label of the zoom ring parameter (waveform time window).
pub const ZOOM_LABEL: &str = "ZOOM";

/// Longest single-line text kept for the screen (characters). The display shows far
/// fewer; the cap only bounds hostile input.
pub const SCREEN_TEXT_MAX_CHARS: usize = 120;

/// Most rows kept for one track / chapter browse list.
pub const MAX_BROWSE_ROWS: usize = 2000;

/// Gap between the 72 px art box and the detail info line. The spec places the info line at
/// `y + 82`, i.e. 10 px below the art, and the spacing scale has no 10 px token (UI_10 owns
/// `tokens.rs`), so it is `SPACE_2 + 2`. Named so the layout arithmetic has no anonymous
/// literal and the reason for the extra 2 px lives with the value.
const INFO_GAP: u16 = SPACE_2 + 2;

/// SEC-12: every string that originates from files, tags, metadata or the air (titles,
/// artists, cue text, playlist names, notices, SSIDs) passes through here before it is
/// stored in the model or drawn. Control, bidi-override, zero-width and other invisible
/// characters are removed (shared stripper in `plippa_noubin::sanitise`), whitespace is
/// collapsed and the length is capped.
pub fn screen_text(s: &str) -> String {
    sanitise_line(s, SCREEN_TEXT_MAX_CHARS)
}

/// Detail-screen actions (encoder focus on [`Route::LibraryDetail`]). The visible set
/// depends on the item, see [`ScreenModel::detail_actions`]; **Play** is always first and
/// **Associate** always last; **Cover art** sits just before Associate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LibraryDetailAction {
    Play,
    /// Collections with more than one entry: pick an entry to play from.
    BrowseTracks,
    /// Items with navigable cues: pick a chapter to jump to.
    BrowseChapters,
    /// Items and collections with at least one resolved cover image: scroll through them.
    CoverArt,
    /// A release this session downloaded from a store: take it off the device (BCS_08 /
    /// BCS_10's preview + confirm).
    Remove,
    Associate,
}

impl LibraryDetailAction {
    pub fn label(self) -> &'static str {
        match self {
            Self::Play => "Play",
            Self::BrowseTracks => "Browse tracks",
            Self::BrowseChapters => "Browse chapters",
            Self::CoverArt => "Cover art",
            Self::Remove => "Remove from Plippa",
            Self::Associate => "Associate",
        }
    }
}

/// What a press on a track of a collection offers (TRACK_01), in row order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackAction {
    /// Play the collection from this track.
    Play,
    /// The release's cover images.
    CoverArt,
    /// Tie the release to a tag.
    Associate,
    /// What has been analysed for this track, and re-analysis.
    Analysis,
}

impl TrackAction {
    pub const ALL: [TrackAction; 4] = [Self::Play, Self::CoverArt, Self::Associate, Self::Analysis];

    pub fn label(self) -> &'static str {
        match self {
            Self::Play => "Play",
            Self::CoverArt => "Cover art",
            Self::Associate => "Associate",
            Self::Analysis => "Analysis",
        }
    }
}

/// Plural category label shown on the library root and as list title.
pub fn kind_label(kind: LibraryKind) -> &'static str {
    match kind {
        LibraryKind::Track => "Tracks",
        LibraryKind::Album => "Albums",
        LibraryKind::Playlist => "Playlists",
        LibraryKind::Mix => "Mixes",
        LibraryKind::Audiobook => "Audiobooks",
        LibraryKind::Podcast => "Podcasts",
    }
}

/// Library root rows, top to bottom.
pub const LIBRARY_ROOT_ORDER: [LibraryKind; 6] = [
    LibraryKind::Track,
    LibraryKind::Album,
    LibraryKind::Playlist,
    LibraryKind::Podcast,
    LibraryKind::Audiobook,
    LibraryKind::Mix,
];

/// Views whose rows are individual tracks (Tracks, Mixes).
pub fn kind_is_track_list(kind: Option<LibraryKind>) -> bool {
    matches!(kind, Some(LibraryKind::Track | LibraryKind::Mix))
}

/// A release this session downloaded to this Plippa (BCS_08).
///
/// The server names the `.noudata` it placed; the library row that appears for it carries the
/// same library-relative path, which is how the detail screen recognises the item as a store
/// download and offers Remove from Plippa. Nothing here is persisted: a restart re-reads the
/// library, and the store screens simply do not claim to know.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreItem {
    /// Store release id (`bandcamp`'s id), so the release list can mark it.
    pub release_id: String,
    /// Title as the store named it.
    pub title: String,
    /// Library-relative `.noudata` the server placed, when it named one.
    pub noudata: Option<String>,
    /// Files placed in the library.
    pub files: usize,
}

/// One entry of a collection (`mediaList` item) in the "Browse tracks" list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackRow {
    /// 0-based `mediaList` position.
    pub position: u32,
    pub title: String,
    pub artist: Option<String>,
    pub duration_ms: Option<u64>,
    /// The file exists (the index says it is there). `false` is the only "missing" signal.
    pub available: bool,
    /// This player can decode the file's format. Device-local and never persisted: the index
    /// is portable, and another Plippa may play what this one cannot (MEDIA_01).
    pub supported: bool,
    /// The format as a person reads it (`M4A/AAC`), for the "not supported" message.
    pub format: String,
}

/// One navigable cue in the "Browse chapters" list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChapterRow {
    /// `mediaList` position of the track the cue belongs to.
    pub item_position: u32,
    pub start_ms: u64,
    pub title: String,
}

/// Per-collection data loaded when a detail screen opens (see `LibrarySource::detail_info`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DetailInfo {
    pub playable_id: i64,
    pub kind: Option<LibraryKind>,
    pub tracks: Vec<TrackRow>,
    pub chapters: Vec<ChapterRow>,
    /// At least one entry's media file is missing.
    pub media_missing: bool,
}

/// The resolved cover list of the item on the detail screen (ART_09): a collection row gets the
/// collection's effective covers, a track row of the Tracks / Mixes lists the item's own
/// (item, then release, then collection). Existing files only, primary first.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DetailCovers {
    pub playable_id: i64,
    /// The selected single track (`LibraryItem::item_position`), if any.
    pub track_pos: Option<u32>,
    /// Index into `LibrarySource::roots` of the root that owns the covers.
    pub root_idx: usize,
    pub covers: Vec<CoverRef>,
}

/// What the cover screen paints (ART_09). Owned by `UiApp`, which loads the pictures off-thread;
/// the render path only reads it.
#[derive(Debug, Clone, Default)]
pub struct CoverView {
    /// 0-based index of the image being shown.
    pub index: usize,
    /// Number of images in the set.
    pub count: usize,
    /// Alt text, else the file name stem; already `screen_text`-clean.
    pub title: String,
    /// The picture to paint. While the next image loads this is still the previous one.
    pub image: Option<std::sync::Arc<crate::art::ArtImage>>,
    /// The current image could not be loaded.
    pub failed: bool,
}

/// Index lookups the UI app wants done; serviced by the loop runner (`UiApp::service_browse`)
/// so `handle_input` and rendering never touch SQLite or the stick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BrowseRequest {
    /// Category counts + storage presence (library root screen).
    Counts,
    /// Items of one category.
    Category(LibraryKind),
    /// Tracks / chapters of the selected item.
    Detail {
        item: LibraryItem,
        kind: Option<LibraryKind>,
    },
    /// BCS_07: resolve one `.noudata` to a playable so a just-downloaded release can be
    /// played without making the person walk the library.
    FindForPlay { relpath: String },
    /// BCS_08: open the library detail screen for one `.noudata` (a release this session
    /// downloaded), so "Show in library" lands on the item rather than the library root.
    FindForDetail { relpath: String },
}

/// One row in the local library list / detail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LibraryItem {
    pub id: i64,
    pub title: String,
    pub associated: bool,
    /// Relative `.noudata` path (for associate / IPC).
    pub noudata_relpath: String,
    /// Absolute library root this row was listed from (for multi-root / P6 ids).
    pub library_root: Option<std::path::PathBuf>,
    /// Best-effort artist from `.noudata` (may be empty / unknown).
    pub artist: Option<String>,
    /// Best-effort length from `.noudata` metadata (not decoded from audio).
    pub duration_ms: Option<u64>,
    /// Absolute path to a local cover image if one exists on disk.
    pub art_path: Option<std::path::PathBuf>,
    /// `Some(n)`: the row is one TRACK (0-based `mediaList` position `n`) of the playable
    /// `id` (Tracks and Mixes views); `None`: the row is the whole playable.
    pub item_position: Option<u32>,
}

/// System / debug telemetry (mockable).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SystemStatus {
    pub time_hhmm: String,
    pub temperature_c: Option<i32>,
    pub fan_status: String,
    pub portal_url: Option<String>,
    /// Primary / first IP (compat with older screens).
    pub ip: Option<String>,
    /// All addresses for System → IP (scrollable).
    pub ips: Vec<String>,
}

impl Default for SystemStatus {
    fn default() -> Self {
        Self {
            time_hhmm: "00:00".into(),
            temperature_c: None,
            fan_status: "unknown".into(),
            portal_url: None,
            ip: None,
            ips: Vec::new(),
        }
    }
}

/// State of the `.wfm` sidecar for the current item (WAVE_02).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum WaveState {
    /// Path known, sidecar missing or not `COMPLETE`: flat line + `ANALYSING` in the footer.
    #[default]
    Pending,
    /// Decoded peaks are in [`WaveView::peaks`]; draw bars.
    Ready,
    /// Decode error, over the size cap or unreadable: flat line and **no** footer text.
    Failed,
}

/// Decoded waveform for the playing item (geometry comes from the `.wfm` header).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WaveView {
    pub state: WaveState,
    /// Sidecar path this view was loaded from (`None` = no waveform for this item).
    pub path: Option<String>,
    /// One dB position per bucket, straight from the sidecar.
    pub peaks: Vec<u8>,
    /// Bucket duration from the header (never `session.duration_ms`).
    pub bucket_ms: u16,
    /// Decoded length from the header (authoritative for the picture).
    pub duration_ms: u64,
}

impl WaveView {
    /// Bars are drawn only for a complete, non-empty file.
    pub fn has_bars(&self) -> bool {
        self.state == WaveState::Ready && !self.peaks.is_empty() && self.bucket_ms > 0
    }
}

/// Aggregated UI-visible state for rendering.
#[derive(Debug, Clone)]
pub struct ScreenModel {
    pub system: SystemStatus,
    pub playback: Option<PlaybackStatus>,
    pub volume: Volume,
    pub notices: Vec<UserNotice>,
    pub library: Vec<LibraryItem>,
    /// Playable id shown on [`Route::LibraryDetail`] / associate-wait.
    pub detail_playable_id: Option<i64>,
    /// With `detail_playable_id`: the selected row is this single track (0-based `mediaList`
    /// position) of the playable, as in the Tracks / Mixes lists.
    pub detail_track_pos: Option<u32>,
    pub wifi: Option<WifiForm>,
    /// Last user-visible miss / error line (also in notices).
    pub banner: Option<String>,
    /// Latest `nowPlayingList` from the engine (NB_04). Highlight: the current track is
    /// `entries[current_index]`; the playing cue (if any) is
    /// `current_index + 1 + playback.session.current_cue_index`.
    pub now_playing: Option<NowPlayingListChanged>,
    /// Category being browsed (NB_12). `None` outside the category routes; Play then uses the
    /// single-item `LoadAndPlay` path.
    pub browse_kind: Option<LibraryKind>,
    /// Playable count per kind for the library root (from one grouped query).
    pub kind_counts: Vec<(LibraryKind, u64)>,
    /// False when no library root is mounted ("No media storage").
    pub storage_present: bool,
    /// Tracks / chapters of the item on the detail screen.
    pub detail_info: Option<DetailInfo>,
    /// Cover list of the item on the detail screen (ART_09).
    pub detail_covers: Option<DetailCovers>,
    /// The picture the detail art box and the cover screen paint (ART_09).
    pub cover_view: CoverView,
    /// Now Playing knob focus (NB_15); only used while a list panel is showing.
    pub np_focus: NowPlayingFocus,
    /// Optimistic set speed (permille, signed) shown until the engine status confirms it
    /// (WARP_07). Owned by `UiApp`; `None` = follow `session.target_rate_permille`.
    pub rate_override: Option<i32>,
    /// Resolved Playback settings and Debug choices shown on their screens (WARP_10, UI_16).
    pub play_mode: PlayModeView,
    /// The engine's last audio route plus the persisted audio settings (AUDIO_04).
    pub audio: AudioView,
    /// Radio, known accessories and the Pair new state (ACC_03).
    pub accessories: AccessoriesState,
    /// Jog target next to the actual playhead (WARP_16). Owned by `UiApp`; `None` = not
    /// jogging and no jump running (the screen is then pixel-identical to before).
    pub jog_view: Option<JogView>,
    /// Waveform for the current item (WAVE_02), loaded by `UiApp` from the sidecar.
    pub wave: WaveView,
    /// Dead-reckoned playhead the waveform and the footer clock both use (WAVE_02).
    pub playhead_ms: u64,
    /// Visible waveform window in ms (UI_13 ZOOM ring). Seeded once at startup from the
    /// persisted default; the knob edits this session value only and never persists it.
    pub wave_window_ms: u64,
    // ---- PLAY_04: the engine's session play mode, mirrored for the screens --------------
    // Never persisted and never written by the UI except through a user command; the engine
    // is authoritative and [`Self::apply_playback`] refreshes these on every status.
    /// `OrderMode` as the engine has it (`Normal` / `Shuffle`).
    pub order_mode: plippa_core::OrderMode,
    /// `RepeatMode` as the engine has it (`Off` / `Queue` / `Item`).
    pub repeat_mode: plippa_core::RepeatMode,
    /// Pause between items with the next one current at 0:00.
    pub pause_after_each: bool,
    /// Repeat-queue on a shuffled list starts a fresh shuffle (engine default: on).
    pub reshuffle_on_repeat: bool,
    /// Whether the loaded list has a source order to restore.
    pub source_order: plippa_core::SourceOrder,
    /// ART_08: the playing item's cover, ready to paint (`None` = no art, waveform untouched).
    /// Owned by `UiApp`, which loads it off-thread.
    pub np_art: Option<std::sync::Arc<crate::art::ArtImage>>,
    /// Opacity of [`Self::np_art`] in percent (the scan / jog fade, D10).
    pub np_art_alpha: u8,
    /// BCS_07: the store tap screen's state. `Some` only while [`Route::LibraryStoreTap`] is
    /// on the stack; the app owns it, the painter only reads it.
    pub store_tap: Option<StoreTapView>,
    /// BCS_08: the Stores screen (`System > Stores` and `Library > Stores`).
    pub store_stores: Option<StoresView>,
    /// BCS_08: Connect Bandcamp, showing the portal address as a QR code.
    pub store_connect: Option<ConnectView>,
    /// SECFIX_02: System > Network (the portal's switches and PIN). `Some` only while
    /// [`Route::SystemNetwork`] or [`Route::SystemNetworkPin`] is on the stack.
    pub network: Option<NetworkView>,
    /// SECFIX_02: whether the web portal is switched on, as the server last said (`None` = not
    /// asked yet). Every screen that advertises the portal address shows an off message instead
    /// of an address that goes nowhere when this is `Some(false)`.
    pub portal_enabled: Option<bool>,
    /// BCS_08: the Bandcamp collection list.
    pub store_releases: Option<ReleasesView>,
    /// BCS_08: one release with its tracks, size and download.
    pub store_release: Option<ReleaseView>,
    /// BCS_08: Remove from Plippa (BCS_10's preview / confirm).
    pub store_delete: Option<DeleteView>,
    /// TRACK_01: `mediaList` position of the track the action menu is open for. `Some` only
    /// while [`Route::LibraryTrackActions`] or [`Route::LibraryTrackAnalysis`] is on the stack.
    pub track_menu_pos: Option<u32>,
    /// TRACK_01: the analysis screen's state. `Some` only while
    /// [`Route::LibraryTrackAnalysis`] is on the stack; the app owns it, the painter reads it.
    pub track_analysis: Option<AnalysisView>,
    /// Releases this session downloaded to this Plippa, so the list can mark them and the
    /// detail screen can offer "Remove from Plippa" for the item they became.
    pub store_items: Vec<StoreItem>,
    /// BCS_08: the query keyboard on the release list. `Some` only while
    /// [`ReleasesView::searching`] is true; the app owns it, the painter only reads it.
    pub store_search: Option<KeyboardField>,
}

impl Default for ScreenModel {
    fn default() -> Self {
        Self {
            system: SystemStatus::default(),
            playback: None,
            volume: Volume::default(),
            notices: Vec::new(),
            library: Vec::new(),
            detail_playable_id: None,
            detail_track_pos: None,
            wifi: None,
            banner: None,
            now_playing: None,
            browse_kind: None,
            kind_counts: Vec::new(),
            storage_present: true,
            detail_info: None,
            detail_covers: None,
            cover_view: CoverView::default(),
            np_focus: NowPlayingFocus::default(),
            rate_override: None,
            play_mode: PlayModeView::default(),
            audio: AudioView::default(),
            accessories: AccessoriesState::default(),
            jog_view: None,
            wave: WaveView::default(),
            playhead_ms: 0,
            wave_window_ms: crate::play_mode::ZOOM_DEFAULT_MS,
            order_mode: plippa_core::OrderMode::Normal,
            repeat_mode: plippa_core::RepeatMode::Off,
            pause_after_each: false,
            // Owner decision 5 / engine default: repeat-queue reshuffles unless turned off.
            reshuffle_on_repeat: true,
            source_order: plippa_core::SourceOrder::Fixed,
            np_art: None,
            np_art_alpha: 100,
            store_tap: None,
            store_stores: None,
            store_connect: None,
            network: None,
            portal_enabled: None,
            store_releases: None,
            store_release: None,
            store_delete: None,
            track_menu_pos: None,
            track_analysis: None,
            store_items: Vec::new(),
            store_search: None,
        }
    }
}

/// Readout wording (a constant, one place): pitch-correct speed mode, pitch-shift speed mode,
/// reverse (always pitch-warped).
pub const RATE_WORD_CORRECT: &str = "corr";
pub const RATE_WORD_SHIFT: &str = "shift";
pub const RATE_WORD_REVERSE: &str = "rev";

/// `125% corr` / `125% shift` / `-75% rev` for a signed set speed.
pub fn rate_text(permille: i32, mode: plippa_core::WarpMode) -> String {
    let pct = (permille.unsigned_abs() + 5) / 10;
    if permille < 0 {
        format!("-{pct}% {RATE_WORD_REVERSE}")
    } else {
        let word = match mode {
            plippa_core::WarpMode::PitchCorrect => RATE_WORD_CORRECT,
            plippa_core::WarpMode::PitchShift => RATE_WORD_SHIFT,
        };
        format!("{pct}% {word}")
    }
}

impl ScreenModel {
    pub fn detail_item(&self) -> Option<&LibraryItem> {
        let id = self.detail_playable_id?;
        self.library
            .iter()
            .find(|i| i.id == id && i.item_position == self.detail_track_pos)
    }

    /// Categories shown on the library root, in the order of `libraryUIHeirarchy.md`. Artists
    /// and Files are not implemented yet.
    pub fn library_root_kinds(&self) -> Vec<LibraryKind> {
        LIBRARY_ROOT_ORDER.to_vec()
    }

    /// Loaded detail data for the selected item (ignores stale data of another item).
    pub fn current_detail_info(&self) -> Option<&DetailInfo> {
        let id = self.detail_playable_id?;
        self.detail_info.as_ref().filter(|d| d.playable_id == id)
    }

    /// Covers of the item on the detail screen (ignores stale data of another item).
    pub fn current_detail_covers(&self) -> Option<&DetailCovers> {
        let id = self.detail_playable_id?;
        self.detail_covers
            .as_ref()
            .filter(|d| d.playable_id == id && d.track_pos == self.detail_track_pos)
    }

    /// The track the action menu is open for, from the collection on the detail screen.
    /// `None` when the list no longer holds it (the stick was swapped under the menu).
    pub fn track_menu_row(&self) -> Option<&TrackRow> {
        let pos = self.track_menu_pos?;
        self.current_detail_info()?
            .tracks
            .iter()
            .find(|t| t.position == pos)
    }

    /// Why Play would not start the track (`None` = it would).
    pub fn track_unplayable_reason(track: &TrackRow) -> Option<String> {
        if !track.available {
            Some("Media file missing".to_string())
        } else if !track.supported {
            let what = if track.format.is_empty() {
                "This format"
            } else {
                track.format.as_str()
            };
            Some(format!("{what} is not supported by this Plippa"))
        } else {
            None
        }
    }

    /// Actions offered on the detail screen for the selected item.
    pub fn detail_actions(&self) -> Vec<LibraryDetailAction> {
        let mut out = vec![LibraryDetailAction::Play];
        if let Some(d) = self.current_detail_info() {
            // A row of the Tracks / Mixes lists is already one track: nothing to browse.
            if !kind_is_track_list(self.browse_kind) && d.tracks.len() > 1 {
                out.push(LibraryDetailAction::BrowseTracks);
            }
            if !d.chapters.is_empty() {
                out.push(LibraryDetailAction::BrowseChapters);
            }
        }
        // The resolved list (sibling fallback included, missing files dropped), not a count.
        if self.current_detail_covers().is_some_and(|c| !c.covers.is_empty()) {
            out.push(LibraryDetailAction::CoverArt);
        }
        // A release this session downloaded from a store can be taken off the device again
        // (BCS_08's delete-and-re-add loop). Only those items offer it: the action removes
        // files, and it must not appear for music the person did not just put there.
        if self.store_item_for_detail().is_some() {
            out.push(LibraryDetailAction::Remove);
        }
        out.push(LibraryDetailAction::Associate);
        out
    }

    /// The store download this detail screen's item became, when it is one.
    pub fn store_item_for_detail(&self) -> Option<&StoreItem> {
        let item = self.detail_item()?;
        let current = item.noudata_relpath.trim_start_matches('/');
        self.store_items.iter().find(|record| {
            record.noudata.as_deref().is_some_and(|noudata| {
                let noudata = noudata.trim_start_matches('/');
                !current.is_empty() && (current == noudata || current.ends_with(noudata))
            })
        })
    }

    /// Category that holds the playing item, for the library-root playing marker.
    ///
    /// The engine's `list_kind` is the closest honest source: `Single` is one track, so it
    /// belongs to the Tracks category; every other kind maps one-to-one.
    fn playing_library_kind(&self) -> Option<LibraryKind> {
        match self.now_playing.as_ref()?.list_kind {
            NowPlayingListKind::Single => Some(LibraryKind::Track),
            NowPlayingListKind::Album => Some(LibraryKind::Album),
            NowPlayingListKind::Playlist => Some(LibraryKind::Playlist),
            NowPlayingListKind::Mix => Some(LibraryKind::Mix),
            NowPlayingListKind::Audiobook => Some(LibraryKind::Audiobook),
            NowPlayingListKind::Podcast => Some(LibraryKind::Podcast),
        }
    }

    /// Source `mediaList` position of the track that is playing now, for the Tracks-list
    /// marker, or `None` when the playing entry cannot be attributed to the browsed item.
    ///
    /// PLAY_04: `NowPlayingEntry::item_position` is the source position, so the marker follows
    /// the playing track exactly, including under shuffle. Two guards keep it honest:
    ///
    /// 1. the current track entry's `playable_id` is the browsed item's global id, so the
    ///    collection really is the one playing (albums and single tracks work; playlist /
    ///    mix rows resolve to their release playable, so no marker is shown there), and
    /// 2. the browsed row at that source position still carries the entry's title (a stale
    ///    detail list cannot light an unrelated row).
    ///
    /// `item_position == 0` is both the first source item and the pre-PLAY_02 sentinel, so the
    /// list-level [`source_positions`] flag decides: a real source list marks the exact row
    /// (including row 0); an old peer falls back to the source-order check below and suppresses
    /// the marker otherwise.
    fn playing_track_position(&self, tracks: &[TrackRow]) -> Option<u32> {
        let list = self.now_playing.as_ref()?;
        let entry = list.entries.get(list.current_index as usize)?;
        if entry.kind != NowPlayingEntryKind::Track {
            return None;
        }
        let item = self.detail_item()?;
        let root = item.library_root.as_ref()?;
        if entry.playable_id != Some(PlayableItemId::new(encode_playable_id(root, item.id))) {
            return None;
        }
        if source_positions(list) {
            let track = tracks.iter().find(|t| t.position == entry.item_position)?;
            return (track.title == entry.title).then_some(track.position);
        }
        let entries: Vec<&NowPlayingEntry> = list
            .entries
            .iter()
            .filter(|e| e.kind == NowPlayingEntryKind::Track)
            .collect();
        if entries.len() != tracks.len() {
            return None;
        }
        let in_source_order = entries.iter().zip(tracks).all(|(e, t)| {
            e.title == t.title && (e.duration_ms.is_none() || e.duration_ms == t.duration_ms)
        });
        if !in_source_order {
            return None;
        }
        tracks.get(entry.track_index as usize).map(|t| t.position)
    }

    /// `kind_label` of the item on the detail screen, else the static `Track` title.
    fn detail_kind_label(&self) -> String {
        self.current_detail_info()
            .and_then(|d| d.kind)
            .map_or_else(|| Route::LibraryDetail.title().to_string(), |k| kind_label(k).to_string())
    }

    /// Header title and meta for a route (UI_11 §2). Library routes say where you are; every
    /// other route keeps `Route::title()` and no meta.
    fn header_text(&self, route: &Route, nav: &NavStack) -> (String, Option<String>) {
        match route {
            Route::Library => {
                let meta = if self.kind_counts.is_empty() {
                    None
                } else {
                    let total: u64 = self.kind_counts.iter().map(|(_, n)| *n).sum();
                    Some(items_label(total))
                };
                ("Library".to_string(), meta)
            }
            Route::LibraryCategory => {
                let title = self
                    .browse_kind
                    .map_or_else(|| route.title().to_string(), |k| kind_label(k).to_string());
                (title, Some(items_label(self.library.len() as u64)))
            }
            Route::LibraryTracks => {
                let rows = self.current_detail_info().map_or(0, |d| d.tracks.len());
                (self.detail_kind_label(), position_meta(nav.focus, rows))
            }
            Route::LibraryChapters => {
                let rows = self.current_detail_info().map_or(0, |d| d.chapters.len());
                (self.detail_kind_label(), position_meta(nav.focus, rows))
            }
            Route::LibraryTrackActions => {
                let meta = self.track_menu_row().map(|t| {
                    clip_chars(
                        &screen_text(&t.title),
                        text::fit_chars(TypeRole::Small, COVER_TITLE_MAX_PX),
                    )
                });
                ("Track".to_string(), meta)
            }
            Route::LibraryTrackAnalysis => {
                let meta = self.track_analysis.as_ref().map(|v| {
                    clip_chars(
                        &screen_text(&v.title),
                        text::fit_chars(TypeRole::Small, COVER_TITLE_MAX_PX),
                    )
                });
                ("Analysis".to_string(), meta)
            }
            Route::LibraryDetail => {
                let meta = self.detail_item().and_then(|i| i.duration_ms).map(format_timecode);
                (self.detail_kind_label(), meta)
            }
            Route::LibraryCover => {
                let title = &self.cover_view.title;
                let meta = (!title.is_empty())
                    .then(|| clip_chars(title, text::fit_chars(TypeRole::Small, COVER_TITLE_MAX_PX)));
                ("Cover art".to_string(), meta)
            }
            Route::LibraryAssociate => ("Associate".to_string(), None),
            Route::SystemAudioAccessory => {
                let meta = self
                    .accessories
                    .detail
                    .as_deref()
                    .and_then(|id| self.accessories.accessory(id))
                    .map(|a| screen_text(&a.name));
                ("Accessory".to_string(), meta)
            }
            other => (other.title().to_string(), None),
        }
    }

    /// Number of rows in the held `nowPlayingList` (track rows plus the current track's cues).
    pub fn np_len(&self) -> usize {
        self.now_playing.as_ref().map_or(0, |l| l.entries.len())
    }

    /// The Now Playing list panel is shown for any multi-row list (album, playlist, or a
    /// track with navigable cues); a single track without cues keeps the plain UI_02 layout.
    pub fn list_panel_visible(&self) -> bool {
        self.np_len() > 1
    }

    /// The Up Next overlay is visible only while the list target is reached (WAVE_02):
    /// rotating to it reveals the overlay, Back returns to the waveform.
    pub fn up_next_visible(&self) -> bool {
        self.list_panel_visible() && self.np_focus.target == NpTarget::List
    }

    /// Flat row of the entry that is playing right now: the active cue row when the engine
    /// reports one (`current_cue_index`, derived from media position), else the current
    /// track row. Never computed from a UI clock.
    pub fn now_playing_row(&self) -> Option<usize> {
        let list = self.now_playing.as_ref()?;
        let base = list.current_index as usize;
        list.entries.get(base)?;
        if let Some(ci) = self
            .playback
            .as_ref()
            .and_then(|p| p.session.current_cue_index)
        {
            let row = base + 1 + ci as usize;
            if list
                .entries
                .get(row)
                .is_some_and(|e| e.kind == NowPlayingEntryKind::Cue)
            {
                return Some(row);
            }
        }
        Some(base)
    }

    /// The [`NowPlayingEntry`] that is playing right now (the active cue, else the current
    /// track), or `None` when the engine has not sent a list. This is the per-track source for
    /// the Now Playing header: the session title / artist name the loaded *record*.
    pub fn current_entry(&self) -> Option<&NowPlayingEntry> {
        let list = self.now_playing.as_ref()?;
        let row = self.now_playing_row()?;
        list.entries.get(row)
    }

    /// The `Track` entry that owns the playing row: the flat row of the current track
    /// (`current_index`). A highlighted cue follows it at
    /// `current_index + 1 + current_cue_index`, and cue entries carry no artist of their own.
    pub fn current_track_entry(&self) -> Option<&NowPlayingEntry> {
        let list = self.now_playing.as_ref()?;
        list.entries
            .get(list.current_index as usize)
            .filter(|e| e.kind == NowPlayingEntryKind::Track)
    }

    /// `(ordinal, total)` over **track** rows for the Now Playing header count, or `None`
    /// when the list holds at most one track (a single track is not an album).
    ///
    /// This is **progress through the playing list**: the ordinal is the position of the
    /// playing track in the order the list is being played, so it reads `1/12`, `2/12`, … as
    /// playback advances. It is therefore independent of shuffle — a shuffled album still
    /// counts up one at a time, and the header never jumps to an album track number.
    /// [`np_row_label`] is where the album's own number is shown.
    ///
    /// Cues are not tracks: a 3-track album whose current track has 2 cues reads `2/3`.
    pub fn np_track_ordinal(&self) -> Option<(usize, usize)> {
        let list = self.now_playing.as_ref()?;
        let total = list
            .entries
            .iter()
            .filter(|e| e.kind == NowPlayingEntryKind::Track)
            .count();
        if total <= 1 {
            return None;
        }
        let current = (list.current_index as usize).min(list.entries.len().saturating_sub(1));
        let before = list.entries[..current]
            .iter()
            .filter(|e| e.kind == NowPlayingEntryKind::Track)
            .count();
        Some((before + 1, total))
    }

    /// How the Up Next list numbers its `Track` rows (PLAY_04, revised by owner decision).
    ///
    /// Collections that **are** an ordered work — an album, a mix, a podcast, an audiobook —
    /// keep the collection's own numbering, so a shuffled album reads e.g. `3.`, `1.`, `2.`
    /// down the rows and every row keeps its real track number. Playlists and dynamic queues
    /// have no such numbering of their own, so they count `1..n` down the rows instead; that
    /// stays stable under shuffle because it is the row's position, not a source position.
    pub fn np_row_numbering(&self) -> NpRowNumbering {
        let Some(list) = self.now_playing.as_ref() else {
            return NpRowNumbering::Sequence;
        };
        // A dynamic queue (add-to-queue / play-next) has no source order to number by, whatever
        // kind it presents as.
        if self.source_order == plippa_core::SourceOrder::Dynamic {
            return NpRowNumbering::Sequence;
        }
        match list.list_kind {
            NowPlayingListKind::Album
            | NowPlayingListKind::Mix
            | NowPlayingListKind::Audiobook
            | NowPlayingListKind::Podcast => NpRowNumbering::Source,
            NowPlayingListKind::Playlist | NowPlayingListKind::Single => NpRowNumbering::Sequence,
        }
    }

    /// Store a new engine list (entries already sanitised) and fix up the Now Playing focus:
    /// a *replaced* collection (different track rows) resets the cursor to the playing row;
    /// a track/cue-row update of the same collection keeps the cursor on the same entry;
    /// when the panel no longer applies the focus returns to plain Now Playing.
    pub fn replace_now_playing(&mut self, list: NowPlayingListChanged) {
        let old = self.now_playing.take();
        // PLAY_04: identity by `playable_id` (stable across a reshuffle). A cue has no
        // playable of its own, so its owning track plus `start_ms` identifies it; a track
        // without a playable (an old peer) falls back to its title.
        let anchor = old.as_ref().and_then(|o| {
            o.entries.get(self.np_focus.cursor).map(|e| {
                (
                    e.kind,
                    e.playable_id,
                    e.track_index,
                    e.start_ms,
                    e.title.clone(),
                )
            })
        });
        // PLAY_04: a reorder or track change keeps `list_id`; only a new load bumps it. Titles
        // were the old heuristic and any shuffle looked like a new collection.
        let replaced = old.as_ref().is_none_or(|o| o.list_id != list.list_id);
        self.now_playing = Some(list);
        if !self.list_panel_visible() {
            // Single track: no panel. Keep a ring parameter (Tempo / Jog) the user is in;
            // anything panel-bound resets.
            if self.np_focus.param().is_some() {
                self.np_focus.target = NpTarget::CurrentTime;
                self.np_focus.cursor = 0;
            } else {
                self.np_focus.reset();
            }
            return;
        }
        let playing = self.now_playing_row().unwrap_or(0);
        let len = self.np_len();
        if replaced {
            // WAVE_02: a new collection (or a new NFC load) starts on the waveform; only a
            // track change inside the same collection keeps the list open.
            self.np_focus.reset();
            self.np_focus.cursor = playing;
        } else if let (Some((kind, pid, ti, start, title)), Some(l)) =
            (anchor, self.now_playing.as_ref())
        {
            self.np_focus.cursor = l
                .entries
                .iter()
                .position(|e| {
                    if e.kind != kind || e.start_ms != start {
                        return false;
                    }
                    match e.kind {
                        NowPlayingEntryKind::Track => match pid {
                            Some(p) => e.playable_id == Some(p),
                            None => e.title == title,
                        },
                        NowPlayingEntryKind::Cue => e.track_index == ti,
                    }
                })
                .unwrap_or(playing);
        }
        self.np_focus.cursor = self.np_focus.cursor.min(len - 1);
    }

    pub fn apply_playback(&mut self, mut status: PlaybackStatus) {
        // SEC-12: title / artist / collection come from files and tags.
        status.session.title = screen_text(&status.session.title);
        status.session.artist = screen_text(&status.session.artist);
        status.session.collection_title = screen_text(&status.session.collection_title);
        // PLAY_04: mirror the engine's session play mode. Nothing here is persisted; the
        // "Play modes" screen and the SW2 LED read only these fields.
        let s = &status.session;
        self.order_mode = s.order_mode;
        self.repeat_mode = s.repeat_mode;
        self.pause_after_each = s.pause_after_each;
        self.reshuffle_on_repeat = s.reshuffle_on_repeat;
        self.source_order = s.source_order;
        self.volume = status.session.volume;
        self.playback = Some(status);
    }

    /// PLAY_04: the five session mirrors as one value, for the "Play modes" rows.
    pub fn play_mode_values(&self) -> PlayModeValues {
        PlayModeValues {
            order: self.order_mode,
            repeat: self.repeat_mode,
            pause_after_each: self.pause_after_each,
            reshuffle_on_repeat: self.reshuffle_on_repeat,
            source_order: self.source_order,
        }
    }

    /// PLAY_04: the session rung the SW2 cycle and its LED work from.
    pub fn play_mode_state(&self) -> plippa_core::PlayModeState {
        plippa_core::PlayModeState {
            order: self.order_mode,
            repeat: self.repeat_mode,
        }
    }

    pub fn push_notice(&mut self, mut notice: UserNotice) {
        // SEC-12: notices can quote file names / titles.
        notice.message = screen_text(&notice.message);
        if notice.severity == NoticeSeverity::Error || notice.severity == NoticeSeverity::Warning {
            self.banner = Some(notice.message.clone());
        }
        self.notices.push(notice);
        if self.notices.len() > 32 {
            self.notices.remove(0);
        }
    }

    /// The set speed to display: the optimistic override, else the engine's target.
    pub fn set_speed_permille(&self) -> i32 {
        self.rate_override.unwrap_or_else(|| {
            self.playback
                .as_ref()
                .map_or(plippa_core::RATE_UNITY_PERMILLE, |p| p.session.target_rate_permille)
        })
    }

    pub fn speed_warp_mode(&self) -> plippa_core::WarpMode {
        self.playback
            .as_ref()
            .map_or(plippa_core::SPEED_WARP_DEFAULT, |p| p.session.speed_warp_mode)
    }

    pub fn transport_label(&self) -> &'static str {
        match self.playback.as_ref().map(|p| p.transport) {
            Some(TransportState::Playing) => "Playing",
            Some(TransportState::Paused) => "Paused",
            Some(TransportState::Stopped) | None => "Stopped",
        }
    }

    /// Footer left slot: `POSITION / TOTAL LENGTH` (`12:34 / 45:06`), never a separate
    /// elapsed/remaining pair. Unknown duration prints `--:--`. Both clocks use the Play mode
    /// timecode unit (UI_14 §7), read live from the model.
    pub fn footer_left(&self) -> String {
        let unit = self.play_mode.timecode_unit;
        let duration = self
            .playback
            .as_ref()
            .and_then(|p| p.session.duration_ms)
            .map(|ms| format_clock(ms, unit))
            .unwrap_or_else(|| "--:--".to_string());
        // Nothing loaded: the position is a placeholder `0:00`, never a stale playhead.
        let position = if self.playback.is_some() {
            self.playhead_ms
        } else {
            0
        };
        format!("{} / {}", format_clock(position, unit), duration)
    }

    /// Footer right slot: the set speed, and only when it is not 100 % forward. No `SET`
    /// prefix: this slot *is* the speed.
    pub fn footer_right(&self) -> String {
        if self.playback.is_none() {
            return String::new();
        }
        let rate = self.set_speed_permille();
        if rate == plippa_core::RATE_UNITY_PERMILLE {
            String::new()
        } else {
            rate_text(rate, self.speed_warp_mode())
        }
    }

    /// Footer centre slot: `ANALYSING` while a known path's sidecar is missing / incomplete,
    /// and **empty** for a failed sidecar, when nothing is loaded, or when the right slot
    /// shows a speed (speed wins; the three slots can never overlap).
    pub fn footer_centre(&self) -> String {
        if self.playback.is_none() || !self.footer_right().is_empty() {
            return String::new();
        }
        match self.wave.state {
            WaveState::Pending if self.wave.path.is_some() => "ANALYSING".to_string(),
            WaveState::Pending | WaveState::Ready | WaveState::Failed => String::new(),
        }
    }

    /// Now Playing header strings, top row first (UI_14 §2): `title`, `artist`, `release`,
    /// `count`. Title and artist come from the **playing entry** — `session.title` /
    /// `session.artist` name the loaded record (album or playlist), so they are only the
    /// fallback when the list or the entry is unavailable (UI_14 §1). The release title is the
    /// collection (`collection_title`) and the count is track-only `ordinal/total`.
    ///
    /// Cue entries carry no artist (`nowplaying.rs` builds them empty), so a highlighted cue
    /// takes the artist from its owning `Track` entry; the cue's own title still wins. Only if
    /// that track is unavailable / also has no artist does the record's artist apply.
    pub fn np_header_text(&self) -> (String, String, String, String) {
        let record = self.playback.as_ref().map(|p| &p.session);
        let (title, mut artist) = match self.current_entry() {
            Some(e) => {
                let mut artist = e.artist.clone();
                if e.kind == NowPlayingEntryKind::Cue || artist.is_empty() {
                    artist = self
                        .current_track_entry()
                        .map(|t| t.artist.clone())
                        .unwrap_or_default();
                }
                (e.title.clone(), artist)
            }
            None => record
                .map(|s| (s.title.clone(), s.artist.clone()))
                .unwrap_or_default(),
        };
        if artist.is_empty() {
            artist = record.map(|s| s.artist.clone()).unwrap_or_default();
        }
        let title = if title.is_empty() { "—".into() } else { title };
        let release = record.map(|s| s.collection_title.clone()).unwrap_or_default();
        let count = self
            .np_track_ordinal()
            .map(|(ordinal, total)| format!("{ordinal}/{total}"))
            .unwrap_or_default();
        (title, artist, release, count)
    }

    /// Paint the screen layer only (no clear — compositor owns clear + overlays).
    ///
    /// Legacy [`Route::Menu`] paints Now Playing underneath; the overlay paints
    /// the menu strip.
    pub fn render_screen(&self, nav: &NavStack, fb: &mut FrameBuffer) {
        let route = match nav.current() {
            Route::Menu => Route::Main,
            other => other.clone(),
        };
        if route == Route::Main {
            // No full-screen title-bar menu pattern — Now Playing is home.
            self.render_main(fb);
            return;
        }

        // Shell chrome: header band + accent rule, then the content box. Library routes fill
        // the meta slot (UI_11 §2); every other route keeps the static `Route::title()`.
        let (title, meta) = self.header_text(&route, nav);
        widgets::header(
            fb,
            Header {
                title: &title,
                meta: meta.as_deref(),
            },
        );
        let mut y = CONTENT_TOP;
        if let Some(banner) = &self.banner {
            widgets::banner(fb, y, &screen_text(banner), ERR);
            y = y.saturating_add(ROW_H);
        }
        match route {
            Route::System => self.render_system_menu(nav, fb, y),
            Route::SystemNetwork => self.render_network_menu(nav, fb, y),
            Route::SystemNetworkPin => self.render_network_pin(fb, y),
            Route::SystemCertificate => self.render_system_certificate(fb, y),
            Route::SystemIp => self.render_system_ip(nav, fb, y),
            Route::SystemPlayMode => self.render_play_mode(nav, fb, y),
            Route::SystemAudio => self.render_system_audio(nav, fb, y),
            Route::SystemAudioOutputs => self.render_system_audio_outputs(nav, fb, y),
            Route::SystemAudioDelay => self.render_system_audio_delay(nav, fb, y),
            Route::SystemAudioAccessories => self.render_system_audio_accessories(nav, fb, y),
            Route::SystemAudioAccessory => self.render_system_audio_accessory(nav, fb, y),
            Route::SystemAudioPairNew => self.render_system_audio_pair_new(nav, fb, y),
            Route::SystemDebug => self.render_system_debug(nav, fb, y),
            Route::SystemDebugAudio => self.render_system_debug_audio(nav, fb, y),
            Route::Warnings => self.render_warnings(fb, y),
            Route::Wifi => self.render_wifi(fb, y),
            Route::NetworkInfo => self.render_network(fb, y),
            Route::Library => self.render_library_root(nav, fb, y),
            Route::LibraryCategory => self.render_library_list(nav, fb, y),
            Route::LibraryTracks => self.render_library_tracks(nav, fb, y),
            Route::LibraryChapters => self.render_library_chapters(nav, fb, y),
            Route::LibraryTrackActions => self.render_library_track_actions(nav, fb, y),
            Route::LibraryTrackAnalysis => self.render_library_track_analysis(nav, fb, y),
            Route::LibraryDetail => self.render_library_detail(nav, fb, y),
            Route::LibraryCover => self.render_library_cover(fb),
            Route::LibraryAssociate => self.render_library_associate_wait(fb, y),
            Route::LibraryStoreTap => self.render_store_tap(nav, fb, y),
            Route::SystemStores | Route::LibraryStores => self.render_stores(nav, fb, y),
            Route::StoreConnect => self.render_store_connect(nav, fb, y),
            Route::StoreReleases => self.render_store_releases(nav, fb, y),
            Route::StoreRelease => self.render_store_release(nav, fb, y),
            Route::LibraryRemove => self.render_library_remove(nav, fb, y),
            Route::Debug => self.render_debug(fb, y),
            Route::Main | Route::Menu => {}
        }
    }

    /// Paint the main-menu overlay into the top [`MAIN_MENU_H`] pixels.
    ///
    /// The strip is `Body` text at the screen margin; the focused item is `BodyBold` in
    /// `ACCENT` with a 2 px `ACCENT` underline. Items wrap to a second row when they would
    /// exceed the screen width, so every label stays visible.
    pub fn render_main_menu_overlay(&self, nav: &NavStack, fb: &mut FrameBuffer) {
        let line_h = cell_height(TypeRole::Body);
        let right = fb.width.saturating_sub(MARGIN_X);
        let mut y = SPACE_2;
        let mut x = MARGIN_X;
        for (i, route) in nav.menu_items().iter().enumerate() {
            let label = route.title();
            let focused = i == nav.focus;
            let w = measure(label, TypeRole::Body);
            if x > MARGIN_X && x.saturating_add(w) > right {
                y = y.saturating_add(line_h).saturating_add(SPACE_1);
                x = MARGIN_X;
            }
            if y.saturating_add(line_h) > MAIN_MENU_H {
                break;
            }
            text::draw(
                fb,
                x,
                y,
                label,
                if focused {
                    TypeRole::BodyBold
                } else {
                    TypeRole::Body
                },
                if focused { ACCENT } else { FG_DIM },
            );
            if focused {
                // §4: the focused menu item carries a 2 px underline, not a hairline.
                fill(fb, x, y.saturating_add(line_h).saturating_add(3), w, 2, ACCENT);
            }
            x = x.saturating_add(w).saturating_add(SPACE_3);
        }
    }

    /// Now Playing (UI_14): two-row header (title / artist, release / count), full-width
    /// waveform scrolling under the fixed centre playhead, and a 30 px `Display` footer.
    fn render_main(&self, fb: &mut FrameBuffer) {
        if self.playback.is_none() {
            self.render_idle(fb);
            return;
        }
        // UI_14 §1: the title and artist belong to the *playing entry*, not to the loaded
        // record (`session.title` / `session.artist` name the album or playlist). The list is
        // the per-track source; the record text is only a fallback when it is unavailable.
        let (title, artist, release, count) = self.np_header_text();
        self.render_np_header(fb, &title, &artist, &release, &count);
        self.paint_waveform(fb);
        self.paint_np_art(fb);
        self.paint_adjust_band(fb);

        // The clock is the same dead-reckoned playhead as the picture (1 s granularity).
        let left = self.footer_left();
        let centre = self.footer_centre();
        let right = self.footer_right();
        widgets::np_footer(
            fb,
            widgets::NpFooter {
                left: &left,
                centre: &centre,
                right: &right,
            },
        );
        // The Up Next overlay covers the band (and the footer) while the list target is held.
        if self.up_next_visible() {
            self.render_up_next_overlay(fb);
        }
        // Ephemeral feedback (associate success, tap prompts) uses the status
        // popup — do not sticky-paint model.banner on Now Playing.
    }

    /// No session: the same two-row band with a `—` title and an empty clock.
    fn render_idle(&self, fb: &mut FrameBuffer) {
        widgets::np_header(
            fb,
            widgets::NpHeader {
                title: "—",
                artist: "",
                release: "",
                count: "",
                modes: widgets::NpModeBadges::NONE,
            },
        );
        widgets::np_footer(
            fb,
            widgets::NpFooter {
                left: &self.footer_left(),
                centre: "",
                right: "",
            },
        );
    }

    /// Now Playing header: title / artist, then release / play-mode badges / track count. The
    /// count is `ordinal/total` over track entries only; the badges are the engine's session
    /// play mode, so the header can never claim a mode the engine is not in.
    fn render_np_header(
        &self,
        fb: &mut FrameBuffer,
        title: &str,
        artist: &str,
        release: &str,
        count: &str,
    ) {
        widgets::np_header(
            fb,
            widgets::NpHeader {
                title,
                artist,
                release,
                count,
                modes: self.np_mode_badges(),
            },
        );
    }

    /// The play-mode badges for the Now Playing header, from the status mirrors.
    ///
    /// `OrderMode::Shuffle` shows the shuffle icon; `RepeatMode::Queue` the repeat icon;
    /// `RepeatMode::Item` the repeat icon plus a `1`. Anything else shows nothing, so the
    /// header is unchanged at the default rung.
    pub fn np_mode_badges(&self) -> widgets::NpModeBadges {
        widgets::NpModeBadges {
            shuffle: self.order_mode == plippa_core::OrderMode::Shuffle,
            repeat: self.repeat_mode != plippa_core::RepeatMode::Off,
            repeat_one: self.repeat_mode == plippa_core::RepeatMode::Item,
        }
    }

    /// Paint the waveform band. Every column is derived from the time mapping
    /// (`t0 = playhead - WINDOW/2 + x * WINDOW/width`), never by shifting a previous image, so
    /// the 93.75 ms column phase lands smoothly at 30 FPS.
    ///
    /// Bars are mirrored around [`WAVE_CENTRE_Y`]: `ACCENT_PLAYED` left of the playhead,
    /// `ACCENT` right of it. Nothing is drawn where `t < 0` or `t > duration`. The fixed
    /// playhead (`WAVE_PLAYHEAD_X`) is painted last, over a 1 px `BORDER_SOFT` centre line.
    pub fn paint_waveform(&self, fb: &mut FrameBuffer) {
        fill(fb, 0, WAVE_BAND_Y, fb.width, WAVE_BAND_H, BG);
        if self.wave.has_bars() {
            let half = WAVE_BAND_H / 2;
            let playhead = self.playhead_ms as i64;
            let duration = self.wave.duration_ms as i64;
            let bucket_ms = i64::from(self.wave.bucket_ms);
            for x in 0..fb.width {
                let peak = wave_column_peak(
                    &self.wave.peaks,
                    bucket_ms,
                    playhead,
                    x,
                    fb.width,
                    self.wave_window_ms,
                    duration,
                );
                let Some(peak) = peak else { continue };
                let h = wave_height(peak, half);
                if h == 0 {
                    continue;
                }
                let colour = if x < WAVE_PLAYHEAD_X {
                    ACCENT_PLAYED
                } else {
                    ACCENT
                };
                // 2h + 1 rows keeps the bar symmetric about the centre line.
                fill(
                    fb,
                    x,
                    WAVE_CENTRE_Y.saturating_sub(h),
                    1,
                    h.saturating_mul(2).saturating_add(1),
                    colour,
                );
            }
        }
        hairline(fb, 0, WAVE_CENTRE_Y, fb.width, BORDER_SOFT);
        fill(fb, WAVE_PLAYHEAD_X, WAVE_BAND_Y, 2, WAVE_BAND_H, FG);
        triangle_down(fb, WAVE_PLAYHEAD_X, WAVE_BAND_Y, 2, FG);
    }

    /// ART_08: the playing item's cover over the left of the waveform band. Nothing is drawn
    /// without art, while the Up Next overlay is showing, or at zero opacity, so those frames
    /// are exactly the frames without the feature.
    fn paint_np_art(&self, fb: &mut FrameBuffer) {
        let Some(img) = self.np_art.as_deref() else {
            return;
        };
        if self.up_next_visible() || img.side != NP_ART_SIDE {
            return;
        }
        paint_art_square(fb, NP_ART_X, NP_ART_Y, img, self.np_art_alpha);
    }

    /// Adjust-mode overlay (UI_12 owner decision, see [`ADJUST_BAND_BLEND_PCT`]).
    ///
    /// Only while [`NpMode::Adjust`] is active: the old transport row (transport label,
    /// `JOG` / `TEMPO` / `ZOOM` label and its live readout) reappears over the waveform in a
    /// band whose backdrop is the waveform blended 60 % toward `BG`, so 40 % of it shows
    /// through. Leaving Adjust mode paints the band away again; the `prev_frame` diff presents
    /// the difference. One `Display` font, three zones, no inner badge.
    fn paint_adjust_band(&self, fb: &mut FrameBuffer) {
        let NpMode::Adjust(param) = self.np_focus.mode else {
            return;
        };
        if fb.height <= ADJUST_BAND_Y {
            return;
        }
        let h = ADJUST_BAND_H.min(fb.height.saturating_sub(ADJUST_BAND_Y));
        blend_toward(fb, 0, ADJUST_BAND_Y, fb.width, h, BG, ADJUST_BAND_BLEND_PCT);
        hairline(fb, 0, ADJUST_BAND_Y, fb.width, BORDER_SOFT);
        if h > 1 {
            hairline(fb, 0, ADJUST_BAND_Y + h - 1, fb.width, BORDER_SOFT);
        }

        let ty = ADJUST_BAND_Y + (h.saturating_sub(cell_height(TypeRole::Display))) / 2;
        let transport = self.transport_label();
        text::draw(fb, MARGIN_X, ty, transport, TypeRole::Display, FG);
        let label = match param {
            AdjustParam::Jog => JOG_LABEL,
            AdjustParam::Tempo => TEMPO_LABEL,
            AdjustParam::Zoom => ZOOM_LABEL,
        };
        let right = fb.width.saturating_sub(MARGIN_X);
        let label_w = measure(label, TypeRole::Display);
        text::draw_aligned(
            fb,
            MARGIN_X,
            ty,
            right.saturating_sub(MARGIN_X),
            label,
            TypeRole::Display,
            WARN,
            Align::Right,
        );
        // The live readout (scan rate / set speed / jog target / zoom window) sits left of the
        // label, but never over the transport label.
        let readout = self.adjust_readout(param);
        let transport_w = measure(transport, TypeRole::Display);
        let x_after_transport = MARGIN_X.saturating_add(transport_w).saturating_add(SPACE_2);
        let readout_x = right
            .saturating_sub(label_w)
            .saturating_sub(SPACE_2)
            .saturating_sub(measure(&readout, TypeRole::Display));
        if !readout.is_empty() && readout_x >= x_after_transport {
            text::draw(
                fb,
                readout_x,
                ty,
                &readout,
                TypeRole::Display,
                if param == AdjustParam::Tempo {
                    WARN
                } else {
                    ACCENT
                },
            );
        }
    }

    /// Readout shown next to the `JOG` / `TEMPO` / `ZOOM` label: the scan rate wins, then the
    /// set speed in Tempo mode, the zoom window in Zoom mode, then a live jog target while the
    /// gesture's readout is open.
    fn adjust_readout(&self, param: AdjustParam) -> String {
        let Some(p) = self.playback.as_ref() else {
            return String::new();
        };
        match p.session.scan {
            plippa_core::ScanState::Forward => return scan_text(true, p.session.rate_permille),
            plippa_core::ScanState::Backward => {
                return scan_text(false, p.session.rate_permille)
            }
            _ => {}
        }
        match param {
            AdjustParam::Tempo => rate_text(self.set_speed_permille(), self.speed_warp_mode()),
            AdjustParam::Zoom => crate::play_mode::zoom_text(self.wave_window_ms),
            AdjustParam::Jog => match self.jog_view.filter(|v| v.readout) {
                Some(v) if v.item_delta == 0 => format_timecode(v.target_ms),
                Some(v) => format_timecode(v.item_target_ms),
                None => String::new(),
            },
        }
    }

    /// Up Next overlay header (UI_14 §4): `UP NEXT - <PLAYLIST NAME>` when the playing list is
    /// a playlist with a collection title, otherwise plain `UP NEXT` (album / mix / audiobook
    /// / podcast / single). The name is file-derived and takes the `screen_text` path (SEC-12);
    /// `None`/`""` never leaves a dangling separator.
    pub fn up_next_title(&self) -> String {
        let is_playlist = self
            .now_playing
            .as_ref()
            .is_some_and(|l| l.list_kind == NowPlayingListKind::Playlist);
        if !is_playlist {
            return "UP NEXT".to_string();
        }
        self.playback
            .as_ref()
            .map(|p| screen_text(&p.session.collection_title))
            .filter(|name| !name.is_empty())
            .map(|name| format!("UP NEXT - {name}"))
            .unwrap_or_else(|| "UP NEXT".to_string())
    }

    /// Up Next: an opaque overlay over the waveform, reached by rotating to the list target.
    /// Rows are the engine's `nowPlayingList` verbatim (marker, cursor ring, cue indent,
    /// duration right-aligned); no local queue is invented.
    fn render_up_next_overlay(&self, fb: &mut FrameBuffer) {
        let Some(list) = self.now_playing.as_ref() else {
            return;
        };
        let f = &self.np_focus;
        let len = list.entries.len();
        let playing = self.now_playing_row();
        let list_focused = f.mode == NpMode::List || f.target == NpTarget::List;
        // Opaque ground from the accent edge down: the waveform is never visible through it.
        fill(
            fb,
            0,
            UP_NEXT_TOP,
            fb.width,
            fb.height.saturating_sub(UP_NEXT_TOP),
            SURFACE,
        );
        fill(fb, 0, UP_NEXT_TOP, fb.width, 1, ACCENT);
        // UI_14 §4: name the playlist, so the overlay says which collection is playing.
        // SEC-12: the name is file-derived, so it takes the same `screen_text` path as every
        // other file-derived string. The count is measured first and the title is clipped into
        // what is left, exactly like the Now Playing header rows.
        let title = self.up_next_title();
        let count = format!("{}/{}", playing.unwrap_or(0).saturating_add(1), len);
        let w = fb.width.saturating_sub(MARGIN_X.saturating_mul(2));
        let count_px = measure(&count, TypeRole::Small).saturating_add(SPACE_2);
        text::draw_clipped(
            fb,
            MARGIN_X,
            UP_NEXT_HEADER_Y,
            w.saturating_sub(count_px),
            &title,
            TypeRole::SmallBold,
            FG,
        );
        text::draw_aligned(
            fb,
            MARGIN_X,
            UP_NEXT_HEADER_Y,
            w,
            &count,
            TypeRole::Small,
            FG_DIM,
            Align::Right,
        );
        hairline(
            fb,
            0,
            UP_NEXT_ROWS_Y.saturating_sub(SPACE_2),
            fb.width,
            BORDER_SOFT,
        );

        // PLAY_04 (revised): the numbering depends on the kind of list. An album, mix, podcast
        // or audiobook numbers by its own source positions (scrambled under shuffle, as it
        // should be); a playlist or dynamic queue counts 1..n down the rows.
        let numbering = self.np_row_numbering();
        let has_source_positions = source_positions(list);
        let rows: Vec<(u8, String, Option<String>)> = list
            .entries
            .iter()
            .map(|e| {
                let (indent, label) = np_row_label(e, numbering, has_source_positions);
                (indent, label, e.duration_ms.map(format_timecode))
            })
            .collect();
        let visible = (UP_NEXT_ROWS as usize).min(len);
        // Keep the focused row visible: the cursor row when the list has focus, otherwise the
        // playing row on top (same rule as the old panel).
        let window_start = if list_focused {
            f.cursor.min(len - 1).saturating_sub(1)
        } else {
            playing.unwrap_or(0)
        };
        let start = window_start.min(len.saturating_sub(visible));
        for i in 0..visible {
            let abs = start + i;
            let (indent, label, value) = &rows[abs];
            let state = if list_focused && abs == f.cursor.min(len - 1) {
                widgets::RowState::Focused
            } else if !list.entries[abs].available {
                widgets::RowState::Disabled
            } else {
                widgets::RowState::Normal
            };
            widgets::row(
                fb,
                UP_NEXT_ROWS_Y.saturating_add((i as u16).saturating_mul(ROW_H)),
                widgets::Row {
                    label,
                    value: value.as_deref(),
                    indent: *indent,
                    state,
                    striped: abs % 2 == 1,
                    marker: playing == Some(abs),
                    height: ROW_H,
                },
            );
        }
        scrollbar(
            fb,
            fb.width.saturating_sub(2),
            UP_NEXT_ROWS_Y,
            UP_NEXT_ROWS.saturating_mul(ROW_H),
            start,
            visible,
            len,
        );
    }

    /// Warnings: the newest [`widgets::notice`]s first, each with a severity bar and wrapped
    /// text. Only whole notices that fit are painted; a scrollbar appears when older ones are
    /// pushed off the bottom. Empty is the shared `message_state`.
    fn render_warnings(&self, fb: &mut FrameBuffer, y0: u16) {
        if self.notices.is_empty() {
            widgets::message_state(fb, "No warnings", "Nothing needs attention");
            return;
        }
        // The scrollbar gutter: notices never paint over the thumb column.
        let max_w = content_width(fb).saturating_sub(SPACE_1);
        let text_w = max_w
            .saturating_sub(widgets::NOTICE_BAR_W)
            .saturating_sub(SPACE_1);
        let line_h = cell_height(TypeRole::Small);
        let mut y = y0;
        let mut painted = 0usize;
        for n in self.notices.iter().rev() {
            let h = (text::wrap(&n.message, text_w, TypeRole::Small).len() as u16)
                .saturating_mul(line_h);
            if y.saturating_add(h) > CONTENT_BOTTOM {
                break;
            }
            y = widgets::notice(fb, MARGIN_X, y, max_w, &n.message, notice_severity(n.severity));
            y = y.saturating_add(SPACE_1);
            painted += 1;
        }
        if painted > 0 && painted < self.notices.len() {
            scrollbar(
                fb,
                fb.width.saturating_sub(2),
                y0,
                CONTENT_BOTTOM.saturating_sub(y0),
                0,
                painted,
                self.notices.len(),
            );
        }
    }

    /// System submenu: settings rows (Network / Playback settings / Stores / Audio / Debug)
    /// with the values the model already knows. A value the model does not know is
    /// omitted, never a placeholder `-`.
    fn render_system_menu(&self, nav: &NavStack, fb: &mut FrameBuffer, y0: u16) {
        let ip = self
            .system
            .ip
            .clone()
            .or_else(|| self.system.ips.first().cloned())
            .map(|v| screen_text(&v));
        let rate = rate_text(self.set_speed_permille(), self.speed_warp_mode());
        for (i, route) in nav.system_items().iter().enumerate() {
            let y = y0.saturating_add((i as u16).saturating_mul(ROW_H));
            if y.saturating_add(ROW_H) > CONTENT_BOTTOM {
                break;
            }
            let value = match route {
                Route::SystemNetwork => ip.clone().unwrap_or_default(),
                Route::SystemPlayMode => rate.clone(),
                _ => String::new(),
            };
            widgets::setting_row(
                fb,
                y,
                route.title(),
                &value,
                if i == nav.focus {
                    widgets::RowState::Focused
                } else {
                    widgets::RowState::Normal
                },
                true,
            );
        }
    }

    /// System > Playback settings: a form of settings rows (focus ring, never a `>` prefix), a
    /// rule, then the description of the row under the cursor. The description is dropped a
    /// whole line at a time at [`CONTENT_BOTTOM`] so it can never be clipped mid-glyph.
    ///
    /// PLAY_04: the four engine session modes are the top rows of this same list; the rest are
    /// the settings. UI_16 §1: the row being edited by scrolling shows the shared
    /// [`widgets::RowState::Editing`] cue (same ring, value column in `ACCENT`).
    ///
    /// Eleven rows do not fit above the description, so the list windows on the cursor: the
    /// rows around it stay put and the window moves only when the cursor would leave it.
    fn render_play_mode(&self, nav: &NavStack, fb: &mut FrameBuffer, y0: u16) {
        let len = PLAYBACK_SETTINGS_ROWS.len();
        let focus = nav.focus.min(len - 1);
        let editing = self.play_mode.editing;
        let values = self.play_mode_values();
        let first = window_start(focus, len, PLAY_MODE_VISIBLE_ROWS);
        let last = (first + PLAY_MODE_VISIBLE_ROWS).min(len);
        let rows = PLAYBACK_SETTINGS_ROWS[first..last].iter().enumerate().map(|(i, row)| {
            let index = first + i;
            let state = if row.setting().is_some() && editing == row.setting() {
                widgets::RowState::Editing
            } else if index == focus {
                widgets::RowState::Focused
            } else {
                widgets::RowState::Normal
            };
            (
                row.row_label(),
                row.value_text(&self.play_mode, &values),
                state,
                false,
            )
        });
        self.render_settings_list(
            fb,
            y0,
            rows,
            PLAYBACK_SETTINGS_ROWS[focus].description(),
            CONTENT_BOTTOM,
            PLAY_MODE_DESC_GAP,
            PLAY_MODE_ROW_PITCH,
        );
        list_scrollbar(fb, y0, first, PLAY_MODE_VISIBLE_ROWS, len);
    }

    /// System > Audio (AUDIO_04): Output devices / Delay / Waveform vis delay, in the shared
    /// settings-list language. Every value is read from the engine's last route or from the
    /// persisted settings; nothing here is optimistic.
    fn render_system_audio(&self, nav: &NavStack, fb: &mut FrameBuffer, y0: u16) {
        let focus = nav.focus.min(AUDIO_ROWS.len() - 1);
        let rows = AUDIO_ROWS.iter().enumerate().map(|(i, row)| {
            let state = if i == focus {
                widgets::RowState::Focused
            } else {
                widgets::RowState::Normal
            };
            // ACC_03 §5: the Accessories row opens a screen, so it carries a chevron.
            let chevron = matches!(row, AudioRow::Accessories);
            (row.label(), row.value_text(&self.audio, &self.accessories), state, chevron)
        });
        self.render_settings_list(
            fb,
            y0,
            rows,
            AUDIO_ROWS[focus].description(),
            CONTENT_BOTTOM,
            SPACE_2,
            ROW_H + SPACE_1,
        );
    }

    /// Output devices (AUDIO_04): `Local only` is always usable; the sink rows are disabled
    /// under Local only or when the server says the output is unavailable. The tick is an
    /// icon (the font has no tick character), never a text value.
    fn render_system_audio_outputs(&self, nav: &NavStack, fb: &mut FrameBuffer, y0: u16) {
        let view = &self.audio;
        if view.route.outputs.is_empty() {
            widgets::message_state(fb, "No outputs", "No audio output was found");
            return;
        }
        let outputs = view.sorted_outputs();
        let focus = nav.focus.min(outputs.len());
        let mut y = y0;
        widgets::setting_row(
            fb,
            y,
            "Local only",
            on_off_text(view.route.local_only),
            if focus == 0 {
                widgets::RowState::Focused
            } else {
                widgets::RowState::Normal
            },
            false,
        );
        y = y.saturating_add(ROW_H).saturating_add(SPACE_1);
        for (i, output) in outputs.iter().enumerate() {
            let row = i + 1;
            let disabled = view.route.local_only || !output.available;
            let state = if disabled {
                widgets::RowState::Disabled
            } else if row == focus {
                widgets::RowState::Focused
            } else {
                widgets::RowState::Normal
            };
            let icon = if view.is_enabled(&output.id) {
                crate::ui::icons::Icon::Check
            } else {
                crate::ui::icons::Icon::BoxEmpty
            };
            widgets::setting_row_with_icon(
                fb,
                y,
                &screen_text(&output.description),
                Some(icon),
                state,
            );
            y = y.saturating_add(ROW_H).saturating_add(SPACE_1);
        }
        hairline(fb, MARGIN_X, y, content_width(fb), BORDER_SOFT);
        let description = audio_outputs_description(view, &outputs, focus);
        widgets::paragraph_upto(
            fb,
            MARGIN_X,
            y.saturating_add(SPACE_2),
            content_width(fb),
            CONTENT_BOTTOM,
            &description,
            TypeRole::Small,
            FG_DIM,
        );
    }

    /// Delay (AUDIO_04): `Auto` plus one ms row per effectively enabled output. The rows show
    /// `AudioRoute::manual_delay_ms` (the user's stored values), not the engine-applied
    /// `AudioOutput::delay_ms`. They are disabled under Auto, with fewer than two outputs, or
    /// under Local only, and the description line says which.
    fn render_system_audio_delay(&self, nav: &NavStack, fb: &mut FrameBuffer, y0: u16) {
        let view = &self.audio;
        let enabled = view.enabled_outputs();
        let focus = nav.focus.min(enabled.len());
        let blocked = delay_blocked(&view.route);
        let mut y = y0;
        let local_only = view.route.local_only;
        widgets::setting_row(
            fb,
            y,
            "Auto",
            on_off_text(view.route.delay_auto),
            if local_only {
                widgets::RowState::Disabled
            } else if focus == 0 {
                widgets::RowState::Focused
            } else {
                widgets::RowState::Normal
            },
            false,
        );
        y = y.saturating_add(ROW_H).saturating_add(SPACE_1);
        for (i, output) in enabled.iter().enumerate() {
            let row = i + 1;
            let editing = view.editing == Some(row);
            let state = if editing {
                widgets::RowState::Editing
            } else if blocked.is_some() {
                widgets::RowState::Disabled
            } else if row == focus {
                widgets::RowState::Focused
            } else {
                widgets::RowState::Normal
            };
            widgets::setting_row(
                fb,
                y,
                &screen_text(&output.description),
                &format!("{} ms", view.delay_ms(&output.id)),
                state,
                false,
            );
            y = y.saturating_add(ROW_H).saturating_add(SPACE_1);
        }
        hairline(fb, MARGIN_X, y, content_width(fb), BORDER_SOFT);
        let description = if local_only {
            "Local only is on. Turn it off to route more than one output."
        } else if focus == 0 {
            "Automatic compensation: the engine trims each output to match. Off lets you set a delay per output."
        } else {
            blocked.unwrap_or("Delay for this output in ms. Enter, then turn. 0 to 1000 ms.")
        };
        widgets::paragraph_upto(
            fb,
            MARGIN_X,
            y.saturating_add(SPACE_2),
            content_width(fb),
            CONTENT_BOTTOM,
            description,
            TypeRole::Small,
            FG_DIM,
        );
    }

    /// System > Audio > Accessories (ACC_03): the known accessories (connected first, then
    /// name) with their connected tick, then the transport setting and `Pair new…`. No radio
    /// is a message state, never fake rows.
    fn render_system_audio_accessories(&self, nav: &NavStack, fb: &mut FrameBuffer, y0: u16) {
        let acc = &self.accessories;
        if !acc.has_radio() {
            widgets::message_state(fb, "No radio", "This device has no accessory radio");
            return;
        }
        if matches!(acc.stage, PairStage::Prompt(..)) {
            self.render_accessory_prompt(fb, y0);
            return;
        }
        let sorted = acc.sorted_known();
        let len = sorted.len();
        let focus = nav.focus.min(len);
        let pair_state = if focus == len {
            widgets::RowState::Focused
        } else {
            widgets::RowState::Normal
        };
        if sorted.is_empty() {
            widgets::message_state(fb, "No accessories", "Enter to pair one");
            let pair_y = CONTENT_BOTTOM.saturating_sub(ROW_H);
            widgets::setting_row(fb, pair_y, "Pair new…", "", pair_state, true);
            return;
        }
        let mut y = y0;
        for (i, accessory) in sorted.iter().enumerate() {
            let state = if i == focus {
                widgets::RowState::Focused
            } else {
                widgets::RowState::Normal
            };
            let icon = if accessory.connected {
                crate::ui::icons::Icon::Check
            } else {
                crate::ui::icons::Icon::BoxEmpty
            };
            widgets::setting_row_with_icon(
                fb,
                y,
                &screen_text(&accessory.name),
                Some(icon),
                state,
            );
            y = y.saturating_add(ROW_H).saturating_add(SPACE_1);
        }
        widgets::setting_row(fb, y, "Pair new…", "", pair_state, true);
        y = y.saturating_add(ROW_H).saturating_add(SPACE_1);
        hairline(fb, MARGIN_X, y, content_width(fb), BORDER_SOFT);
        let description = if focus < len {
            let accessory = sorted[focus];
            format!(
                "{}, {}",
                screen_text(&accessory.name),
                acc.status_text(accessory)
            )
        } else if focus == len {
            "Accept play, pause and next/previous from a connected accessory.".to_string()
        } else {
            format!(
                "Scans for {} s. Put the accessory in pairing mode first.",
                acc.settings.scan_seconds
            )
        };
        widgets::paragraph_upto(
            fb,
            MARGIN_X,
            y.saturating_add(SPACE_2),
            content_width(fb),
            CONTENT_BOTTOM,
            &description,
            TypeRole::Small,
            FG_DIM,
        );
    }

    /// One accessory: Connect/Disconnect, Auto connect, Forget (with the confirm popup drawn by
    /// the compositor; Back cancels before the command is sent).
    fn render_system_audio_accessory(&self, nav: &NavStack, fb: &mut FrameBuffer, y0: u16) {
        let acc = &self.accessories;
        if !acc.has_radio() {
            widgets::message_state(fb, "No radio", "This device has no accessory radio");
            return;
        }
        let Some(id) = acc.detail.as_deref() else {
            widgets::message_state(fb, "No accessory", "Go back and pick one");
            return;
        };
        let Some(accessory) = acc.accessory(id) else {
            widgets::message_state(fb, "No accessory", "It is no longer known");
            return;
        };
        let focus = nav.focus.min(3);
        let in_flight = acc.pending_action.as_deref() == Some(id);
        let out_of_range = !accessory.connected && acc.is_unavailable(id);
        let mut y = y0;
        let connect_label = if accessory.connected {
            "Disconnect"
        } else {
            "Connect"
        };
        let connect_state = if in_flight {
            widgets::RowState::Disabled
        } else if focus == 0 {
            widgets::RowState::Focused
        } else {
            widgets::RowState::Normal
        };
        widgets::setting_row(
            fb,
            y,
            connect_label,
            if accessory.connected {
                "Connected"
            } else if out_of_range {
                "Not in range"
            } else {
                "Not connected"
            },
            connect_state,
            false,
        );
        y = y.saturating_add(ROW_H).saturating_add(SPACE_1);
        widgets::setting_row(
            fb,
            y,
            "Auto connect",
            on_off_text(acc.is_auto_connect(id)),
            if focus == 1 {
                widgets::RowState::Focused
            } else {
                widgets::RowState::Normal
            },
            false,
        );
        y = y.saturating_add(ROW_H).saturating_add(SPACE_1);
        widgets::setting_row(
            fb,
            y,
            "Transport commands",
            on_off_text(acc.is_transport_enabled(id)),
            if focus == 2 {
                widgets::RowState::Focused
            } else {
                widgets::RowState::Normal
            },
            false,
        );
        y = y.saturating_add(ROW_H).saturating_add(SPACE_1);
        widgets::setting_row(
            fb,
            y,
            "Forget",
            "",
            if focus == 3 {
                widgets::RowState::Focused
            } else {
                widgets::RowState::Normal
            },
            false,
        );
        y = y.saturating_add(ROW_H).saturating_add(SPACE_1);
        hairline(fb, MARGIN_X, y, content_width(fb), BORDER_SOFT);
        let description = match focus {
            0 if accessory.connected => "Disconnects this accessory now.",
            0 if out_of_range => "Not in range. Move it closer and try again.",
            0 => "Connects this accessory now.",
            1 => "Connects it at start-up when nothing else is connected.",
            2 => "Accept play, pause, stop and next/previous from this accessory.",
            _ => "Removes the pairing. The accessory must be paired again to be used.",
        };
        widgets::paragraph_upto(
            fb,
            MARGIN_X,
            y.saturating_add(SPACE_2),
            content_width(fb),
            CONTENT_BOTTOM,
            description,
            TypeRole::Small,
            FG_DIM,
        );
    }

    /// Pair new (ACC_03): scan result / countdown, the pairing prompt and the waiting state.
    fn render_system_audio_pair_new(&self, nav: &NavStack, fb: &mut FrameBuffer, y0: u16) {
        let acc = &self.accessories;
        if !acc.has_radio() {
            widgets::message_state(fb, "No radio", "This device has no accessory radio");
            return;
        }
        if matches!(acc.stage, PairStage::Prompt(..)) {
            self.render_accessory_prompt(fb, y0);
            return;
        }
        if let PairStage::Pairing(id) = &acc.stage {
            widgets::message_state(fb, "Pairing…", &screen_text(&acc.name_for(id)));
            return;
        }
        if acc.found.is_empty() {
            if acc.is_scanning() {
                widgets::message_state(
                    fb,
                    "Scanning…",
                    &format!("{} s left", acc.scan_seconds_left()),
                );
            } else {
                widgets::message_state(fb, "Nothing found", "Check it is in pairing mode");
            }
            return;
        }
        let visible = visible_rows(y0);
        let len = acc.found.len();
        let focus = nav.focus.min(len - 1);
        let start = window_start(focus, len, visible);
        for i in 0..visible.min(len - start) {
            let abs = start + i;
            let item = &acc.found[abs];
            let state = if abs == focus {
                widgets::RowState::Focused
            } else {
                widgets::RowState::Normal
            };
            widgets::setting_row(
                fb,
                y0.saturating_add((i as u16).saturating_mul(ROW_H)),
                &screen_text(&item.name),
                kind_text(item.kind),
                state,
                false,
            );
        }
        list_scrollbar(fb, y0, start, visible, len);
        let line_y = y0
            .saturating_add((visible.min(len - start) as u16).saturating_mul(ROW_H));
        hairline(fb, MARGIN_X, line_y, content_width(fb), BORDER_SOFT);
        let description = if acc.is_scanning() {
            format!("Scanning, {} s left", acc.scan_seconds_left())
        } else {
            "Enter to pair one, Back to go back.".to_string()
        };
        widgets::paragraph_upto(
            fb,
            MARGIN_X,
            line_y.saturating_add(SPACE_2),
            content_width(fb),
            CONTENT_BOTTOM,
            &description,
            TypeRole::Small,
            FG_DIM,
        );
    }

    fn render_accessory_prompt(&self, fb: &mut FrameBuffer, y0: u16) {
        let PairStage::Prompt(prompt, _) = &self.accessories.stage else {
            return;
        };
        // Every prompt names the peer first (SYS-03): a number or "Authorise ?" on its own was
        // enough for a distracted press to pair a hostile device.
        let y0 = self.render_prompt_peer(fb, y0, prompt);
        match prompt {
            PairPrompt::Confirm { passkey, .. } => {
                let w = content_width(fb);
                let h = cell_height(TypeRole::Display);
                let y = y0.saturating_add(
                    CONTENT_BOTTOM
                        .saturating_sub(y0)
                        .saturating_sub(h)
                        / 2,
                );
                text::draw_aligned(
                    fb,
                    MARGIN_X,
                    y,
                    w,
                    &passkey.to_string(),
                    TypeRole::Display,
                    ACCENT,
                    Align::Centre,
                );
                widgets::hint(fb, "Accept? Push = yes, Back = no");
            }
            PairPrompt::Authorise { .. } => {
                let w = content_width(fb);
                let y = y0.saturating_add(
                    CONTENT_BOTTOM
                        .saturating_sub(y0)
                        .saturating_sub(cell_height(TypeRole::BodyBold))
                        / 2,
                );
                text::draw_aligned(
                    fb,
                    MARGIN_X,
                    y,
                    w,
                    "Authorise this accessory?",
                    TypeRole::BodyBold,
                    FG,
                    Align::Centre,
                );
                widgets::hint(fb, "Push = yes, Back = no");
            }
            PairPrompt::EnterPasskey { .. } | PairPrompt::EnterPin { .. } => {
                if let Some(kb) = &self.accessories.keyboard {
                    self.render_accessory_keyboard(kb, fb, y0);
                } else {
                    widgets::message_state(fb, "Pairing…", "Waiting for the accessory");
                }
            }
        }
    }

    /// Draw the peer's name (sanitised) and canonical address above a prompt; returns the y
    /// below them.
    fn render_prompt_peer(&self, fb: &mut FrameBuffer, y0: u16, prompt: &PairPrompt) -> u16 {
        let w = content_width(fb);
        let mut y = y0;
        for line in prompt_peer_lines(prompt) {
            // The name leads; the address is the identity line under it (smaller and dimmer).
            let (role, colour) = if line == prompt.peer().address {
                (TypeRole::Small, FG_DIM)
            } else {
                (TypeRole::BodyBold, FG)
            };
            text::draw_aligned(fb, MARGIN_X, y, w, &line, role, colour, Align::Centre);
            y = y.saturating_add(cell_height(role)).saturating_add(SPACE_1);
        }
        y.saturating_add(SPACE_1)
    }

    /// The reusable [`KeyboardField`] painted in the same slot language as the Wi-Fi password
    /// screen, without its Wi-Fi-specific title/badge.
    fn render_accessory_keyboard(&self, kb: &KeyboardField, fb: &mut FrameBuffer, y0: u16) {
        let right = fb.width.saturating_sub(MARGIN_X);
        let mut x = MARGIN_X;
        let mut slot_y = y0;
        for i in 0..kb.slot_count() {
            let focused = i == kb.focus();
            let ok_slot = i + 1 == kb.slot_count();
            let blank_slot = i + 2 == kb.slot_count();
            if x > MARGIN_X && x.saturating_add(widgets::SLOT_W) > right {
                x = MARGIN_X;
                slot_y = slot_y.saturating_add(widgets::SLOT_H).saturating_add(SPACE_1);
            }
            if slot_y.saturating_add(widgets::SLOT_H) > CONTENT_BOTTOM {
                break;
            }
            if ok_slot {
                widgets::char_slot(fb, x, slot_y, None, focused);
                text::draw_aligned(
                    fb,
                    x,
                    slot_y.saturating_add(SPACE_1),
                    widgets::SLOT_W,
                    "OK",
                    TypeRole::SmallBold,
                    if focused { ACCENT } else { FG },
                    Align::Centre,
                );
            } else if blank_slot {
                widgets::char_slot(fb, x, slot_y, None, focused);
            } else {
                widgets::char_slot(fb, x, slot_y, Some(kb.slot_display(i)), focused);
            }
            x = x.saturating_add(widgets::SLOT_W).saturating_add(SPACE_1);
        }
        let y = slot_y
            .saturating_add(widgets::SLOT_H)
            .saturating_add(SPACE_1);
        if y.saturating_add(cell_height(TypeRole::Display)) <= CONTENT_BOTTOM {
            text::draw(
                fb,
                MARGIN_X,
                y,
                &kb.current_char().to_string(),
                TypeRole::Display,
                ACCENT,
            );
        }
        widgets::hint(fb, "Enter=next  Back=no");
    }

    /// System > Debug (UI_16 §8, AUDIO_05): the same settings list as Playback settings.
    /// Enter toggles the two-value row; a navigation row gets a chevron and opens its screen.
    fn render_system_debug(&self, nav: &NavStack, fb: &mut FrameBuffer, y0: u16) {
        let focus = nav.focus.min(DEBUG_ROWS.len() - 1);
        let rows = DEBUG_ROWS.iter().enumerate().map(|(i, row)| {
            let state = if i == focus {
                widgets::RowState::Focused
            } else {
                widgets::RowState::Normal
            };
            (
                row.row_label(),
                row.value_text(&self.play_mode).to_string(),
                state,
                row.opens_screen(),
            )
        });
        // UI_17: the build stamp owns the last line, so the description is bounded above it.
        let note_y = CONTENT_BOTTOM.saturating_sub(cell_height(TypeRole::Small));
        self.render_settings_list(
            fb,
            y0,
            rows,
            DEBUG_ROWS[focus].description(),
            note_y.saturating_sub(SPACE_2),
            SPACE_2,
            ROW_H + SPACE_1,
        );
        text::draw_clipped(
            fb,
            MARGIN_X,
            note_y,
            content_width(fb),
            &build_info::line(),
            TypeRole::Small,
            FG_DIM,
        );
    }

    /// System > Debug > Audio info (AUDIO_05): the fixed-order report as a read-only,
    /// windowed data list. The build stamp belongs to the Debug screen and is not repeated here.
    fn render_system_debug_audio(&self, nav: &NavStack, fb: &mut FrameBuffer, y0: u16) {
        let report = diag::audio_report(
            &self.audio.route,
            &self.audio.settings,
            &self.accessories,
            self.accessories.available,
            self.accessories.now_ms,
        );
        if report.is_empty() {
            widgets::message_state(fb, "No data", "The engine is not connected yet");
            return;
        }
        let len = report.len();
        let visible = visible_rows(y0);
        let focus = nav.focus.min(len - 1);
        let start = window_start(focus, len, visible);
        for i in 0..visible.min(len - start) {
            let abs = start + i;
            let line = &report[abs];
            widgets::data_row(
                fb,
                y0.saturating_add((i as u16).saturating_mul(ROW_H)),
                DataRow {
                    label: &line.label,
                    value: Some(&line.value),
                    indent: 0,
                    state: if abs == focus {
                        widgets::RowState::Focused
                    } else {
                        widgets::RowState::Normal
                    },
                    striped: abs % 2 == 1,
                    marker: false,
                },
            );
        }
        list_scrollbar(fb, y0, start, visible, len);
    }

    /// Shared "settings list" body (UI_12 language, UI_16 reuse): `rows` of
    /// `(label, value, state, chevron)`, a rule under them, then the focused row's description,
    /// which is dropped a whole line at a time at `bottom` so it is never clipped mid-glyph.
    fn render_settings_list<'a>(
        &self,
        fb: &mut FrameBuffer,
        y0: u16,
        rows: impl Iterator<Item = (&'a str, String, widgets::RowState, bool)>,
        description: &str,
        bottom: u16,
        desc_gap: u16,
        pitch: u16,
    ) {
        let mut y = y0;
        for (label, value, state, chevron) in rows {
            widgets::setting_row(fb, y, label, &value, state, chevron);
            y = y.saturating_add(pitch);
        }
        hairline(fb, MARGIN_X, y, content_width(fb), BORDER_SOFT);
        widgets::paragraph_upto(
            fb,
            MARGIN_X,
            y.saturating_add(desc_gap),
            content_width(fb),
            bottom,
            description,
            TypeRole::Small,
            FG_DIM,
        );
    }

    /// System > Network (SECFIX_02): the address, the web portal switch, the PIN switch, the PIN
    /// itself, and Wi-Fi. A warning under the rows stays up while the portal is on with no PIN;
    /// a refusal or a state the owner should know about (no place to keep the PIN, a lockout)
    /// reads as a line under it.
    fn render_network_menu(&self, nav: &NavStack, fb: &mut FrameBuffer, y0: u16) {
        let Some(view) = self.network.as_ref() else {
            widgets::message_state(fb, "Network", "Open this from System");
            return;
        };
        let ip = self
            .system
            .ip
            .clone()
            .or_else(|| self.system.ips.first().cloned())
            .map(|v| screen_text(&v));
        let ssid = self
            .wifi
            .as_ref()
            .and_then(|f| f.selected_ssid.clone())
            .map(|v| screen_text(&v));
        let rows = view.rows();
        let focus = nav.focus.min(rows.len().saturating_sub(1));
        let open = view.open_warning().is_some();
        let clear = view.clear_text_warning().is_some();
        let mut y = y0;
        for (i, row) in rows.iter().enumerate() {
            if y.saturating_add(ROW_H) > CONTENT_BOTTOM {
                break;
            }
            let (label, value) = view.row_text(
                *row,
                ip.as_deref(),
                ssid.as_deref(),
                self.play_mode.skip_internet_consent,
            );
            let state = if i == focus {
                widgets::RowState::Focused
            } else if (open && matches!(row, NetworkRow::PinRequired))
                || (clear && matches!(row, NetworkRow::Https))
            {
                widgets::RowState::Warning
            } else {
                widgets::RowState::Normal
            };
            let opens_screen = matches!(
                row,
                NetworkRow::Ip | NetworkRow::Pin | NetworkRow::Wifi | NetworkRow::Certificate
            );
            widgets::setting_row(fb, y, label, &screen_text(&value), state, opens_screen);
            y = y.saturating_add(ROW_H);
        }
        y = y.saturating_add(SPACE_1);
        let width = content_width(fb);
        let room = |y: u16| y.saturating_add(cell_height(TypeRole::Small)) <= CONTENT_BOTTOM;
        for warning in view.warnings() {
            if room(y) {
                y = widgets::notice(fb, MARGIN_X, y, width, warning, widgets::NoticeSeverity::Warning)
                    .saturating_add(SPACE_1);
            }
        }
        let line = view
            .notice
            .clone()
            .map(|n| (n, widgets::NoticeSeverity::Error))
            .or_else(|| {
                view.status_line()
                    .map(|n| (n, widgets::NoticeSeverity::Info))
            });
        if let Some((text, severity)) = line {
            if room(y) {
                y = widgets::notice(fb, MARGIN_X, y, width, &screen_text(&text), severity)
                    .saturating_add(SPACE_1);
            }
        }
        widgets::hint(
            fb,
            if view.busy {
                "Working..."
            } else {
                rows.get(focus).map_or("", |row| row.hint())
            },
        );
    }

    /// System > Network > View certificate (SECFIX_03): the SHA-256 fingerprint the browser's
    /// warning page shows, grouped as it shows it, and the words for comparing it. The Network
    /// menu itself only carries the navigation row, so the fingerprint never crowds the
    /// switches.
    fn render_system_certificate(&self, fb: &mut FrameBuffer, y0: u16) {
        let Some(view) = self.network.as_ref() else {
            widgets::message_state(fb, "Certificate", "Open this from Network");
            return;
        };
        let Some(fingerprint) = view.fingerprint_display() else {
            // HTTPS off, or the server has no certificate yet: say so rather than paint an
            // empty heading (the row only exists while HTTPS is on).
            widgets::message_state(
                fb,
                "No certificate",
                "Turn Web portal HTTPS on to use one",
            );
            return;
        };
        let width = content_width(fb);
        let mut y = y0;
        text::draw(fb, MARGIN_X, y, "SHA-256", TypeRole::SmallBold, FG_DIM);
        y = y
            .saturating_add(cell_height(TypeRole::SmallBold))
            .saturating_add(SPACE_1);
        // The grouped value is 95 glyphs: two `Small` lines at this width.
        y = widgets::paragraph_upto(
            fb,
            MARGIN_X,
            y,
            width,
            CONTENT_BOTTOM,
            &screen_text(&fingerprint),
            TypeRole::Small,
            FG,
        );
        let _ = widgets::paragraph_upto(
            fb,
            MARGIN_X,
            y.saturating_add(SPACE_2),
            width,
            CONTENT_BOTTOM,
            "Compare this with the certificate your browser shows for the portal.",
            TypeRole::Small,
            FG_DIM,
        );
        widgets::hint(fb, "Back = Network");
    }

    /// System > Network > Web portal PIN: the PIN as it is now, large enough to read from across
    /// a desk, and six fixed boxes for typing a new one. The only generated PIN is the first.
    ///
    /// UI_14: the boxes show the digits themselves (the current PIN is printed on this screen
    /// already, so masking would buy nothing) and there is no add/delete slot — six is a
    /// property of the control, not of its contents.
    fn render_network_pin(&self, fb: &mut FrameBuffer, y0: u16) {
        let Some(view) = self.network.as_ref() else {
            widgets::message_state(fb, "PIN", "Open this from Network");
            return;
        };
        let width = content_width(fb);
        let mut y = y0;
        text::draw(fb, MARGIN_X, y, "CURRENT PIN", TypeRole::SmallBold, FG_DIM);
        y = y
            .saturating_add(cell_height(TypeRole::SmallBold))
            .saturating_add(SPACE_1);
        let (current, colour) = match view.pin_display() {
            Some(pin) => (pin, ACCENT),
            None => ("not set yet".to_string(), FG_DIM),
        };
        text::draw_aligned(
            fb,
            MARGIN_X,
            y,
            width,
            &current,
            TypeRole::Display,
            colour,
            Align::Centre,
        );
        y = y
            .saturating_add(cell_height(TypeRole::Display))
            .saturating_add(SPACE_1);
        // The line above the boxes is the refusal when there is one, else their label.
        match view.notice.as_deref() {
            Some(notice) => text::draw_clipped(
                fb,
                MARGIN_X,
                y,
                width,
                &screen_text(notice),
                TypeRole::SmallBold,
                WARN,
            ),
            None => text::draw(fb, MARGIN_X, y, "NEW PIN", TypeRole::SmallBold, FG_DIM),
        }
        y = y
            .saturating_add(cell_height(TypeRole::SmallBold))
            .saturating_add(SPACE_1);
        if let Some(pad) = view.pin_pad.as_ref() {
            // Nothing is painted under the pad; the return value is the next free row.
            let _ = Self::render_pin_pad(pad, fb, y);
        }
        widgets::hint(
            fb,
            if view.pin_pending {
                "Saving..."
            } else {
                "Six digits. Back = cancel"
            },
        );
    }

    /// The six digit boxes and the `OK` box, then the digit under edit at `Display` size and the
    /// `EDIT`/`NAV` badge (UI_14 §5.2). Returns the y below what it drew.
    ///
    /// Seven boxes plus six gaps take 143 px of the 304 px content box, so they are one row; the
    /// wrap below is for a future font, and it never clips a box or separates `OK` from the last
    /// digit line.
    fn render_pin_pad(pad: &PinPad, fb: &mut FrameBuffer, y0: u16) -> u16 {
        use crate::control::InteractionMode;
        let right = fb.width.saturating_sub(MARGIN_X);
        let mut x = MARGIN_X;
        let mut slot_y = y0;
        for i in 0..=PIN_DIGITS {
            if x > MARGIN_X && x.saturating_add(widgets::SLOT_W) > right {
                x = MARGIN_X;
                slot_y = slot_y.saturating_add(widgets::SLOT_H).saturating_add(SPACE_1);
            }
            if slot_y.saturating_add(widgets::SLOT_H) > CONTENT_BOTTOM {
                break;
            }
            let focused = i == pad.focus();
            if i == PIN_DIGITS {
                // The OK box: the keypad's treatment, so it reads as the same affordance.
                widgets::char_slot(fb, x, slot_y, None, focused);
                text::draw_aligned(
                    fb,
                    x,
                    slot_y.saturating_add(SPACE_1),
                    widgets::SLOT_W,
                    "OK",
                    TypeRole::SmallBold,
                    if focused { ACCENT } else { FG },
                    Align::Centre,
                );
            } else {
                // The previewed digit is what the box will hold once committed.
                let shown = pad.digit_at(i).map(|d| char::from(b'0' + d));
                widgets::digit_slot(fb, x, slot_y, shown, focused);
            }
            x = x.saturating_add(widgets::SLOT_W).saturating_add(SPACE_1);
        }
        let mut y = slot_y
            .saturating_add(widgets::SLOT_H)
            .saturating_add(SPACE_1);
        // Blocks below the boxes are dropped whole when they do not fit: a half-drawn block is
        // worse than a missing one (the keypad's rule).
        if y.saturating_add(cell_height(TypeRole::Display)) <= CONTENT_BOTTOM {
            let glyph = match pad.mode() {
                InteractionMode::Edit => char::from(b'0' + pad.pick()).to_string(),
                InteractionMode::Navigate => pad
                    .digit_at(pad.focus())
                    .map(|d| char::from(b'0' + d))
                    .unwrap_or('\u{2013}')
                    .to_string(),
            };
            text::draw(fb, MARGIN_X, y, &glyph, TypeRole::Display, ACCENT);
            y = y
                .saturating_add(cell_height(TypeRole::Display))
                .saturating_add(SPACE_1);
        }
        let badge = if pad.mode() == InteractionMode::Edit {
            "EDIT"
        } else {
            "NAV"
        };
        if y.saturating_add(cell_height(TypeRole::SmallBold)) <= CONTENT_BOTTOM {
            widgets::badge(fb, MARGIN_X, y, badge, ACCENT, SURFACE);
        }
        y
    }

    /// System > IP: an `ADDRESSES` section of ringed address rows (continuation lines indented,
    /// unringed), and a `PORTAL` section whose URL wraps inside the box. The address window
    /// scrolls when the list overflows; the portal block is reserved at the bottom so the
    /// window can never eat it.
    ///
    /// Interface names are not in the model, so the address is the row label and the value
    /// column is omitted (the spec's "else the address" case).
    fn render_system_ip(&self, nav: &NavStack, fb: &mut FrameBuffer, y0: u16) {
        let ips = if self.system.ips.is_empty() {
            self.system.ip.iter().cloned().collect::<Vec<_>>()
        } else {
            self.system.ips.clone()
        };
        if ips.is_empty() {
            widgets::message_state(fb, "No address", "Check the network cable");
            return;
        }
        let ips: Vec<String> = ips.iter().map(|ip| screen_text(ip)).collect();
        let max_w = content_width(fb);
        // Continuation lines are indented by `SPACE_2`; the gutter keeps the scrollbar clear.
        let body_w = max_w.saturating_sub(SPACE_2 + SPACE_1);
        let chunks: Vec<Vec<String>> = ips
            .iter()
            .map(|ip| text::wrap(ip, body_w, TypeRole::Body))
            .collect();
        let total_rows: usize = chunks.iter().map(|c| c.len()).sum();

        // SECFIX_02: a portal that is switched off has no address to show.
        let portal_off = self.portal_enabled == Some(false);
        let portal = if portal_off {
            PORTAL_OFF_TEXT.to_string()
        } else {
            self.system
                .portal_url
                .clone()
                .map(|u| screen_text(&u))
                .unwrap_or_else(|| "(portal offline)".to_string())
        };
        let portal_colour = if self.system.portal_url.is_some() && !portal_off {
            ACCENT
        } else {
            FG_DIM
        };
        let portal_lines = text::wrap(&portal, max_w, TypeRole::Small).len() as u16;
        let portal_h = cell_height(TypeRole::SmallBold)
            .saturating_add(SPACE_1)
            .saturating_add(portal_lines.saturating_mul(cell_height(TypeRole::Small)))
            .saturating_add(SPACE_1);
        let addr_bottom = CONTENT_BOTTOM.saturating_sub(portal_h);

        let mut y = y0;
        text::draw(fb, MARGIN_X, y, "ADDRESSES", TypeRole::SmallBold, FG_DIM);
        y = y.saturating_add(cell_height(TypeRole::SmallBold))
            .saturating_add(SPACE_1);
        let visible = ((addr_bottom.saturating_sub(y)) / ROW_H).max(1) as usize;
        let focus = nav.focus.min(ips.len().saturating_sub(1));
        let focus_row: usize = chunks[..focus].iter().map(|c| c.len()).sum();
        let start = window_start(focus_row, total_rows, visible);
        let mut row = 0usize;
        'rows: for (i, lines) in chunks.iter().enumerate() {
            for (li, line) in lines.iter().enumerate() {
                if row >= start + visible {
                    break 'rows;
                }
                if row >= start {
                    let ry = y.saturating_add(((row - start) as u16).saturating_mul(ROW_H));
                    if li == 0 {
                        widgets::setting_row(
                            fb,
                            ry,
                            line,
                            "",
                            if i == focus {
                                widgets::RowState::Focused
                            } else {
                                widgets::RowState::Normal
                            },
                            false,
                        );
                    } else {
                        text::draw_clipped(
                            fb,
                            MARGIN_X.saturating_add(SPACE_2),
                            ry.saturating_add(
                                (ROW_H.saturating_sub(cell_height(TypeRole::Body))) / 2,
                            ),
                            body_w,
                            line,
                            TypeRole::Body,
                            FG,
                        );
                    }
                }
                row += 1;
            }
        }
        if total_rows > visible {
            scrollbar(
                fb,
                fb.width.saturating_sub(2),
                y,
                addr_bottom.saturating_sub(y),
                start,
                visible,
                total_rows,
            );
        }

        let portal_y = addr_bottom;
        text::draw(fb, MARGIN_X, portal_y, "PORTAL", TypeRole::SmallBold, FG_DIM);
        widgets::paragraph(
            fb,
            MARGIN_X,
            portal_y
                .saturating_add(cell_height(TypeRole::SmallBold))
                .saturating_add(SPACE_1),
            max_w,
            &portal,
            TypeRole::Small,
            portal_colour,
        );
    }

    /// Wi-Fi, one visual shape per phase (UI_12 §6, UI_14 §1/§2). The header carries the route
    /// title; the body carries the phase, so a phase can never fall through to another's layout.
    fn render_wifi(&self, fb: &mut FrameBuffer, y0: u16) {
        let Some(form) = &self.wifi else {
            widgets::message_state(fb, "Wi-Fi unavailable", "No wireless adapter");
            return;
        };
        use crate::wifi::{KnownAction, WifiPhase, WifiRow};
        let max_w = content_width(fb);
        match form.phase {
            WifiPhase::Current => {
                let mut y = y0;
                // Status line: the association and its address, or the honest "not connected".
                // Not a row, so it is never focusable and never ringed.
                let headline = match (form.connected_ssid(), self.system.ip.as_deref()) {
                    (Some(ssid), Some(ip)) => {
                        format!("{} · {}", screen_text(ssid), screen_text(ip))
                    }
                    (Some(ssid), None) => screen_text(ssid),
                    (None, _) => "Not connected".to_string(),
                };
                let colour = if form.connected_ssid().is_some() {
                    FG
                } else {
                    FG_DIM
                };
                text::draw_clipped(fb, MARGIN_X, y, max_w, &headline, TypeRole::Body, colour);
                y = y.saturating_add(cell_height(TypeRole::Body)).saturating_add(SPACE_1);
                if let Some(msg) = &form.message {
                    y = widgets::notice(
                        fb,
                        MARGIN_X,
                        y,
                        max_w,
                        &screen_text(msg),
                        // The only producers of `message` here are a refused disconnect.
                        widgets::NoticeSeverity::Warning,
                    );
                    y = y.saturating_add(SPACE_1);
                }
                let rows = form.rows();
                let labels: Vec<String> = rows.iter().map(|row| form.row_label(*row)).collect();
                let (start, visible) = widgets::list(
                    fb,
                    widgets::List {
                        rows: &labels,
                        focus: form.focus.min(rows.len().saturating_sub(1)),
                        top: y,
                        dense: false,
                        disabled: None,
                        warning: None,
                        marker: None,
                        show_focus: true,
                        indent: None,
                        window_start: None,
                    },
                );
                if visible > 0 {
                    scrollbar(
                        fb,
                        fb.width.saturating_sub(2),
                        y,
                        CONTENT_BOTTOM.saturating_sub(y),
                        start,
                        visible,
                        labels.len(),
                    );
                }
                let hint = match rows.get(form.focus) {
                    Some(WifiRow::Search) => "Enter=scan  Back=leave",
                    Some(WifiRow::Known) => "Enter=saved networks  Back=leave",
                    Some(WifiRow::Disconnect) => "Enter=disconnect  Back=leave",
                    Some(WifiRow::Forget) => "Enter=forget (asks first)  Back=leave",
                    None => "Back=leave",
                };
                widgets::hint(fb, hint);
            }
            WifiPhase::Scan => {
                let mut top = y0;
                if let Some(msg) = &form.message {
                    top = widgets::notice(
                        fb,
                        MARGIN_X,
                        top,
                        max_w,
                        &screen_text(msg),
                        // The only producer of `message` on this phase is a failed connect.
                        widgets::NoticeSeverity::Warning,
                    );
                    top = top.saturating_add(SPACE_1);
                }
                if form.ssids.is_empty() {
                    widgets::message_state(fb, "No networks", "Press Enter to scan again");
                    return;
                }
                let rows: Vec<String> = form.ssids.iter().map(|s| screen_text(s)).collect();
                let (start, visible) = widgets::list(
                    fb,
                    widgets::List {
                        rows: &rows,
                        focus: form.focus,
                        top,
                        dense: false,
                        disabled: None,
                        warning: None,
                        marker: None,
                        show_focus: true,
                        indent: None,
                        window_start: None,
                    },
                );
                if visible > 0 {
                    scrollbar(
                        fb,
                        fb.width.saturating_sub(2),
                        top,
                        CONTENT_BOTTOM.saturating_sub(top),
                        start,
                        visible,
                        rows.len(),
                    );
                }
            }
            WifiPhase::Known => {
                let mut top = y0;
                if let Some(msg) = &form.message {
                    top = widgets::notice(
                        fb,
                        MARGIN_X,
                        top,
                        max_w,
                        &screen_text(msg),
                        widgets::NoticeSeverity::Warning,
                    );
                    top = top.saturating_add(SPACE_1);
                }
                if form.known().is_empty() {
                    // A `message_state`, not a blank body: nothing to list is a state.
                    widgets::message_state(fb, "No saved networks", "Search to join one");
                    widgets::hint(fb, "Back=menu");
                    return;
                }
                let active = form.connected_ssid();
                let visible = visible_rows(top);
                let first = window_start(form.focus, form.known().len(), visible);
                for (i, ssid) in form.known().iter().enumerate().skip(first).take(visible) {
                    let name = screen_text(ssid);
                    let value = if active == Some(ssid.as_str()) {
                        Some("Connected")
                    } else {
                        None
                    };
                    widgets::data_row(
                        fb,
                        top.saturating_add(((i - first) as u16).saturating_mul(ROW_H)),
                        DataRow {
                            label: &name,
                            value,
                            indent: 0,
                            state: if i == form.focus {
                                widgets::RowState::Focused
                            } else {
                                widgets::RowState::Normal
                            },
                            striped: i % 2 == 1,
                            marker: false,
                        },
                    );
                }
                list_scrollbar(fb, top, first, visible, form.known().len());
                widgets::hint(fb, "Enter=choose  Back=menu");
            }
            WifiPhase::KnownActions => {
                let ssid = form
                    .pending_ssid()
                    .map(screen_text)
                    .unwrap_or_else(|| "?".to_string());
                let mut y = y0;
                text::draw_clipped(fb, MARGIN_X, y, max_w, &ssid, TypeRole::BodyBold, FG);
                y = y.saturating_add(cell_height(TypeRole::BodyBold)).saturating_add(SPACE_1);
                if let Some(msg) = &form.message {
                    y = widgets::notice(
                        fb,
                        MARGIN_X,
                        y,
                        max_w,
                        &screen_text(msg),
                        widgets::NoticeSeverity::Warning,
                    );
                    y = y.saturating_add(SPACE_1);
                }
                // Connect keeps its row while disabled (already connected), like a zero-count
                // category: hiding it would move the other rows under the cursor.
                let drawn = form.action_rows();
                let labels: Vec<String> = drawn
                    .iter()
                    .map(|(action, _)| match action {
                        KnownAction::Connect => "Connect".to_string(),
                        KnownAction::Forget => format!("Forget {ssid}"),
                        KnownAction::Back => "Back".to_string(),
                    })
                    .collect();
                let is_disabled = |i: usize| drawn.get(i).is_some_and(|(_, on)| !on);
                let rows = widgets::List {
                    rows: &labels,
                    focus: form.action_drawn_index(),
                    top: y,
                    dense: false,
                    disabled: Some(&is_disabled),
                    warning: None,
                    marker: None,
                    show_focus: true,
                    indent: None,
                    window_start: None,
                };
                widgets::list(fb, rows);
                widgets::hint(fb, "Enter=choose  Back=networks");
            }
            WifiPhase::ConfirmForget => {
                let ssid = form
                    .pending_ssid()
                    .map(screen_text)
                    .unwrap_or_else(|| "?".to_string());
                let mut y = y0;
                text::draw_clipped(
                    fb,
                    MARGIN_X,
                    y,
                    max_w,
                    &format!("Forget {ssid}?"),
                    TypeRole::BodyBold,
                    WARN,
                );
                y = y.saturating_add(cell_height(TypeRole::BodyBold)).saturating_add(SPACE_1);
                if let Some(msg) = &form.message {
                    y = widgets::notice(
                        fb,
                        MARGIN_X,
                        y,
                        max_w,
                        &screen_text(msg),
                        widgets::NoticeSeverity::Warning,
                    );
                    y = y.saturating_add(SPACE_1);
                }
                let labels = [
                    form.confirm_label(0).to_string(),
                    form.confirm_label(1).to_string(),
                ];
                widgets::list(
                    fb,
                    widgets::List {
                        rows: &labels,
                        focus: form.focus.min(1),
                        top: y,
                        dense: false,
                        disabled: None,
                        warning: None,
                        marker: None,
                        show_focus: true,
                        indent: None,
                        window_start: None,
                    },
                );
                widgets::hint(fb, "Enter=choose  Back=cancel");
            }
            WifiPhase::Keyboard => self.render_keyboard(form, fb, y0),
            WifiPhase::Connecting => {
                let mut y = y0;
                let headline = form
                    .message
                    .as_deref()
                    .map(screen_text)
                    .unwrap_or_else(|| "Connecting…".to_string());
                text::draw_clipped(
                    fb,
                    MARGIN_X,
                    y,
                    max_w,
                    &headline,
                    TypeRole::Display,
                    ACCENT,
                );
                y = y.saturating_add(cell_height(TypeRole::Display))
                    .saturating_add(SPACE_2);
                if let Some(ssid) = &form.selected_ssid {
                    text::draw_clipped(
                        fb,
                        MARGIN_X,
                        y,
                        max_w,
                        &screen_text(ssid),
                        TypeRole::Body,
                        FG_DIM,
                    );
                }
                // Back is inert in this phase (WifiForm ignores it), so do not advertise a
                // cancel that cannot happen: the hint only reports the wait.
                widgets::hint(fb, "Connecting… please wait");
            }
        }
    }

    /// Password entry (UI_12 §6): slots that wrap instead of clipping, the character under
    /// edit at `Display` size, an explicit `EDIT`/`NAV` badge, the message as a notice and the
    /// real bindings from `control.rs` in the hint slot.
    fn render_keyboard(&self, form: &WifiForm, fb: &mut FrameBuffer, y0: u16) {
        use crate::control::InteractionMode;
        let max_w = content_width(fb);
        let ssid = form
            .selected_ssid
            .as_deref()
            .map(screen_text)
            .unwrap_or_else(|| "?".to_string());
        let mut y = y0;
        let title = format!("Password for {ssid}");
        text::draw_clipped(fb, MARGIN_X, y, max_w, &title, TypeRole::Small, FG_DIM);
        y = y.saturating_add(ROW_H);

        let kb = &form.keyboard;
        let right = fb.width.saturating_sub(MARGIN_X);
        let mut x = MARGIN_X;
        let mut slot_y = y;
        for i in 0..kb.slot_count() {
            let focused = i == kb.focus();
            // Structurally: the last slot is `OK`, the one before it is the blank append slot,
            // everything before that is a character (so no glyph-sniffing decides the kind).
            let ok_slot = i + 1 == kb.slot_count();
            let blank_slot = i + 2 == kb.slot_count();
            // Wrap to the next slot line rather than clipping the slot.
            if x > MARGIN_X && x.saturating_add(widgets::SLOT_W) > right {
                x = MARGIN_X;
                slot_y = slot_y.saturating_add(widgets::SLOT_H).saturating_add(SPACE_1);
            }
            if slot_y.saturating_add(widgets::SLOT_H) > CONTENT_BOTTOM {
                break;
            }
            if ok_slot {
                // The OK slot is not a character: same control, `OK` inside instead of a mask.
                widgets::char_slot(fb, x, slot_y, None, focused);
                text::draw_aligned(
                    fb,
                    x,
                    slot_y.saturating_add(SPACE_1),
                    widgets::SLOT_W,
                    "OK",
                    TypeRole::SmallBold,
                    if focused { ACCENT } else { FG },
                    Align::Centre,
                );
            } else if blank_slot {
                // The append slot holds no character; the pending glyph is shown at `Display`
                // size below, so this slot stays empty.
                widgets::char_slot(fb, x, slot_y, None, focused);
            } else {
                widgets::char_slot(fb, x, slot_y, Some(kb.slot_display(i)), focused);
            }
            x = x.saturating_add(widgets::SLOT_W).saturating_add(SPACE_1);
        }
        y = slot_y
            .saturating_add(widgets::SLOT_H)
            .saturating_add(SPACE_1);

        // The blocks below the slots are dropped whole when a very long password pushed the
        // slot grid down: a half-drawn block or one under the hint is worse than a missing one.
        let mut y = y;
        if y.saturating_add(cell_height(TypeRole::Display)) <= CONTENT_BOTTOM {
            // The glyph under edit, at `Display` size.
            text::draw(
                fb,
                MARGIN_X,
                y,
                &kb.current_char().to_string(),
                TypeRole::Display,
                ACCENT,
            );
            y = y
                .saturating_add(cell_height(TypeRole::Display))
                .saturating_add(SPACE_1);
        }
        let badge = if kb.mode() == InteractionMode::Edit {
            "EDIT"
        } else {
            "NAV"
        };
        if y.saturating_add(cell_height(TypeRole::SmallBold)) <= CONTENT_BOTTOM {
            widgets::badge(fb, MARGIN_X, y, badge, ACCENT, SURFACE);
            y = y
                .saturating_add(cell_height(TypeRole::SmallBold))
                .saturating_add(SPACE_2);
        }
        if let Some(msg) = &form.message {
            let msg = screen_text(msg);
            let text_w = max_w
                .saturating_sub(widgets::NOTICE_BAR_W)
                .saturating_sub(SPACE_1);
            let h = (text::wrap(&msg, text_w, TypeRole::Small).len() as u16)
                .saturating_mul(cell_height(TypeRole::Small));
            if y.saturating_add(h) <= CONTENT_BOTTOM {
                widgets::notice(fb, MARGIN_X, y, max_w, &msg, widgets::NoticeSeverity::Warning);
            }
        }
        widgets::hint(fb, "Enter=next  Back=cancel");
    }

    /// Network info (UI_12 §7): no QR — the address is a settings row and the portal URL is a
    /// wrapped paragraph, with a hint pointing at the screen that stays live. Portal offline is
    /// a warning notice, not an accent paragraph (an accent URL that is not there would lie).
    fn render_network(&self, fb: &mut FrameBuffer, y0: u16) {
        let max_w = content_width(fb);
        let Some(ip) = &self.system.ip else {
            widgets::message_state(fb, "No address", "Check the network cable");
            widgets::hint(fb, "See System > IP");
            return;
        };
        let mut y = y0;
        widgets::setting_row(
            fb,
            y,
            "IP",
            &screen_text(ip),
            widgets::RowState::Normal,
            false,
        );
        y = y.saturating_add(ROW_H).saturating_add(SPACE_1);
        let portal_url = if self.portal_enabled == Some(false) {
            None
        } else {
            self.system.portal_url.as_ref()
        };
        match portal_url {
            Some(url) => {
                widgets::paragraph(
                    fb,
                    MARGIN_X,
                    y,
                    max_w,
                    &screen_text(url),
                    TypeRole::Body,
                    ACCENT,
                );
            }
            None if self.portal_enabled == Some(false) => {
                widgets::notice(
                    fb,
                    MARGIN_X,
                    y,
                    max_w,
                    PORTAL_OFF_TEXT,
                    widgets::NoticeSeverity::Info,
                );
            }
            None => {
                widgets::notice(
                    fb,
                    MARGIN_X,
                    y,
                    max_w,
                    "Portal offline",
                    widgets::NoticeSeverity::Warning,
                );
            }
        }
        widgets::hint(fb, "See System > IP");
    }

    /// Library root (UI_11 §3): one data row per category, the playable count right-aligned.
    /// A zero-count category is still shown (empty but present) with a muted label; a missing
    /// stick replaces the list with the shared empty state.
    fn render_library_root(&self, nav: &NavStack, fb: &mut FrameBuffer, y0: u16) {
        if !self.storage_present {
            widgets::message_state(fb, "No media storage", "Insert a Noubin stick");
            return;
        }
        self.render_library_root_kinds(nav, fb, y0, &self.library_root_kinds());
        // BCS_08: the trailing row leads to Bandcamp. UI_14 renamed it `Distros` (the library's
        // word for "where downloads come from"); `System > Stores` still says `Stores`.
        //
        // The clamp is `kinds.len()` — inclusive — because this row *is* `kinds.len()`.
        // Clamping one lower painted this row and the last category as focused at once; with no
        // clamp an out-of-range `nav.focus` (storage yanked under the menu) paints no ring at all.
        let kinds = self.library_root_kinds();
        let row = kinds.len();
        let focus = nav.focus.min(row);
        let y = y0.saturating_add((row as u16).saturating_mul(ROW_H));
        if y.saturating_add(ROW_H) <= CONTENT_BOTTOM {
            let connected = self
                .store_stores
                .as_ref()
                .is_some_and(|view| view.connected());
            let value = if connected { "Bandcamp" } else { "Connect" };
            widgets::data_row(
                fb,
                y,
                DataRow {
                    label: "Distros",
                    value: Some(value),
                    indent: 0,
                    state: if focus == row {
                        widgets::RowState::Focused
                    } else {
                        widgets::RowState::Normal
                    },
                    striped: row % 2 == 1,
                    marker: false,
                },
            );
        }
    }

    /// Rows for an explicit kind list. Split from [`Self::render_library_root`] so the empty
    /// case is a `message_state` even though [`Self::library_root_kinds`] is static today:
    /// "nothing here" must never fall through to a blank body.
    fn render_library_root_kinds(
        &self,
        nav: &NavStack,
        fb: &mut FrameBuffer,
        y0: u16,
        kinds: &[LibraryKind],
    ) {
        if kinds.is_empty() {
            widgets::message_state(fb, "No categories", "Nothing to browse");
            return;
        }
        let focus = nav.focus.min(kinds.len());
        let playing = self.playing_library_kind();
        for (i, kind) in kinds.iter().enumerate() {
            let count = self
                .kind_counts
                .iter()
                .find(|(k, _)| k == kind)
                .map_or(0u64, |(_, n)| *n);
            let value = count.to_string();
            let focused = i == focus;
            let state = if count == 0 {
                // "Empty but present": the category is real, the label is muted and the count
                // reads 0. Not an error, so no `WARN`.
                widgets::RowState::Disabled
            } else if focused {
                widgets::RowState::Focused
            } else {
                widgets::RowState::Normal
            };
            let y = y0.saturating_add((i as u16).saturating_mul(ROW_H));
            widgets::data_row(
                fb,
                y,
                DataRow {
                    label: kind_label(*kind),
                    value: Some(&value),
                    indent: 0,
                    state,
                    striped: i % 2 == 1,
                    marker: playing == Some(*kind),
                },
            );
            if focused && count == 0 {
                // Focus is the ring only: an empty category keeps its muted label while
                // still showing the cursor.
                border(
                    fb,
                    SPACE_1,
                    y,
                    fb.width.saturating_sub(SPACE_1.saturating_mul(2)),
                    ROW_H,
                    2,
                    ACCENT,
                );
            }
        }
    }

    /// Items of the browsed category (UI_11 §4): a data-row list windowed around the focus,
    /// with a scrollbar only while the list overflows.
    fn render_library_list(&self, nav: &NavStack, fb: &mut FrameBuffer, y0: u16) {
        if !self.storage_present {
            widgets::message_state(fb, "No media storage", "Insert a Noubin stick");
            return;
        }
        if self.library.is_empty() {
            widgets::message_state(fb, "Library empty", "Add music from the portal");
            return;
        }
        let len = self.library.len();
        let visible = visible_rows(y0);
        let focus = nav.focus.min(len - 1);
        let start = window_start(focus, len, visible);
        for i in 0..visible.min(len - start) {
            let abs = start + i;
            let item = &self.library[abs];
            widgets::data_row(
                fb,
                y0.saturating_add((i as u16).saturating_mul(ROW_H)),
                DataRow {
                    label: &item.title,
                    // An associated item keeps a literal `*` in the value column (SEC/NB_12).
                    // `marker` means "playing" (`ACCENT_2` triangle), so it must not be reused
                    // for association — the `*` is the association signal.
                    value: item.associated.then_some("*"),
                    indent: 0,
                    state: if abs == focus {
                        widgets::RowState::Focused
                    } else {
                        widgets::RowState::Normal
                    },
                    striped: abs % 2 == 1,
                    marker: false,
                },
            );
        }
        list_scrollbar(fb, y0, start, visible, len);
    }

    /// Entries of the selected collection (UI_11 §5): title left, duration right, muted when
    /// the media file is missing. The old `!` availability signal is kept in the value column
    /// next to the duration, so a `Disabled` row still says *why* it is muted.
    fn render_library_tracks(&self, nav: &NavStack, fb: &mut FrameBuffer, y0: u16) {
        let Some(info) = self.current_detail_info().filter(|d| !d.tracks.is_empty()) else {
            widgets::message_state(fb, "No tracks", "This item has no tracks");
            return;
        };
        let len = info.tracks.len();
        let visible = visible_rows(y0);
        let focus = nav.focus.min(len - 1);
        let start = window_start(focus, len, visible);
        let playing = self.playing_track_position(&info.tracks);
        for i in 0..visible.min(len - start) {
            let abs = start + i;
            let track = &info.tracks[abs];
            let duration = track
                .duration_ms
                .map(format_timecode)
                .unwrap_or_else(|| "--:--".to_string());
            // `!` means the file is missing. An unplayable row is muted without it, so the two
            // states differ without a new glyph in the 320 px value column.
            let value = if track.available {
                duration
            } else {
                format!("{duration} !")
            };
            let label = format!("{}. {}", track.position.saturating_add(1), track.title);
            widgets::data_row(
                fb,
                y0.saturating_add((i as u16).saturating_mul(ROW_H)),
                DataRow {
                    label: &label,
                    value: Some(&value),
                    indent: 0,
                    state: if abs == focus {
                        widgets::RowState::Focused
                    } else if !track.available || !track.supported {
                        widgets::RowState::Disabled
                    } else {
                        widgets::RowState::Normal
                    },
                    striped: abs % 2 == 1,
                    marker: playing == Some(track.position),
                },
            );
        }
        list_scrollbar(fb, y0, start, visible, len);
    }

    /// The per-track action menu (TRACK_01): the same action-row look as the detail screen.
    /// The track's title is in the header. A track that is no longer in the list gets the
    /// usual "not found" state, and Press or Back both leave it.
    fn render_library_track_actions(&self, nav: &NavStack, fb: &mut FrameBuffer, y0: u16) {
        let Some(track) = self.track_menu_row() else {
            widgets::message_state(fb, "Track not found", "Press or go back to the list");
            return;
        };
        let has_art = self
            .current_detail_covers()
            .is_some_and(|c| !c.covers.is_empty());
        let unplayable = Self::track_unplayable_reason(track).is_some();
        let focus = nav.focus.min(TrackAction::ALL.len() - 1);
        for (i, action) in TrackAction::ALL.iter().enumerate() {
            // A muted row still opens: Play says why it cannot start, Cover art says there is
            // none. Muting is only the hint that nothing will happen.
            let muted = match action {
                TrackAction::Play => unplayable,
                TrackAction::CoverArt => !has_art,
                _ => false,
            };
            let value = match action {
                TrackAction::Play if !track.available => Some("Missing"),
                TrackAction::Play if unplayable => Some("Unsupported"),
                _ => None,
            };
            widgets::data_row(
                fb,
                y0.saturating_add((i as u16).saturating_mul(ROW_H)),
                DataRow {
                    label: action.label(),
                    value,
                    indent: 0,
                    state: if i == focus {
                        widgets::RowState::Focused
                    } else if muted {
                        widgets::RowState::Disabled
                    } else {
                        widgets::RowState::Normal
                    },
                    striped: i % 2 == 1,
                    marker: false,
                },
            );
        }
    }

    /// The analysis screen (TRACK_01): one row per analysis kind, then Re-analyse. Kinds the
    /// server did not answer for read "Not available yet", muted and unreachable.
    fn render_library_track_analysis(&self, nav: &NavStack, fb: &mut FrameBuffer, y0: u16) {
        let Some(view) = self.track_analysis.as_ref() else {
            widgets::message_state(fb, "No track", "Go back and pick one");
            return;
        };
        let rows = view.rows();
        let focus = nav.focus.min(view.actions().len().saturating_sub(1));
        let mut selectable = 0usize;
        for (i, row) in rows.iter().enumerate() {
            let focused = row.selectable && selectable == focus;
            if row.selectable {
                selectable += 1;
            }
            widgets::data_row(
                fb,
                y0.saturating_add((i as u16).saturating_mul(ROW_H)),
                DataRow {
                    label: row.label,
                    value: (!row.value.is_empty()).then_some(row.value.as_str()),
                    indent: 0,
                    state: if focused {
                        widgets::RowState::Focused
                    } else if !row.selectable {
                        widgets::RowState::Disabled
                    } else {
                        widgets::RowState::Normal
                    },
                    striped: i % 2 == 1,
                    marker: false,
                },
            );
        }
        if let Some(text) = view.explanation() {
            let y = y0
                .saturating_add((rows.len() as u16).saturating_mul(ROW_H))
                .saturating_add(SPACE_1);
            let colour = if view.waveform_state() == Some(crate::analysis::AnalysisState::Unavailable)
                || matches!(view.phase, crate::analysis::AnalysisPhase::Failed(_))
            {
                WARN
            } else {
                FG_DIM
            };
            let _ = store_para(fb, y, CONTENT_BOTTOM, &screen_text(&text), TypeRole::Small, colour, 3);
        }
    }

    /// Navigable cues of the selected item (UI_11 §5): chapter title left, start time right.
    fn render_library_chapters(&self, nav: &NavStack, fb: &mut FrameBuffer, y0: u16) {
        let Some(info) = self.current_detail_info().filter(|d| !d.chapters.is_empty()) else {
            widgets::message_state(fb, "No chapters", "This item has no cues");
            return;
        };
        let len = info.chapters.len();
        let visible = visible_rows(y0);
        let focus = nav.focus.min(len - 1);
        let start = window_start(focus, len, visible);
        for i in 0..visible.min(len - start) {
            let abs = start + i;
            let chapter = &info.chapters[abs];
            let value = format_timecode(chapter.start_ms);
            widgets::data_row(
                fb,
                y0.saturating_add((i as u16).saturating_mul(ROW_H)),
                DataRow {
                    label: &chapter.title,
                    value: Some(&value),
                    indent: 0,
                    state: if abs == focus {
                        widgets::RowState::Focused
                    } else {
                        widgets::RowState::Normal
                    },
                    striped: abs % 2 == 1,
                    marker: false,
                },
            );
        }
        list_scrollbar(fb, y0, start, visible, len);
    }

    /// Detail screen (UI_11 §6): art box + title/artist/length column, an optional info line
    /// and the action list. The action list is never clipped: if the info line would push the
    /// last action past `CONTENT_BOTTOM`, the info line is dropped instead.
    fn render_library_detail(&self, nav: &NavStack, fb: &mut FrameBuffer, y0: u16) {
        let Some(item) = self.detail_item() else {
            widgets::message_state(fb, "No item", "Go back and pick something");
            return;
        };
        let art_size = DETAIL_ART_SIDE;
        let art_y = y0;
        let has_art = self
            .current_detail_covers()
            .map_or_else(
                || item.art_path.as_ref().is_some_and(|p| p.is_file()),
                |c| !c.covers.is_empty(),
            );
        // The primary image once decoded; until then (or if it cannot be decoded) the box says
        // so, and the `—` placeholder stays for items without any art.
        match self.cover_view.image.as_ref().filter(|i| has_art && i.side == art_size) {
            Some(img) => paint_art_square(fb, MARGIN_X, art_y, img, 100),
            None => widgets::art_box(
                fb,
                MARGIN_X,
                art_y,
                art_size,
                has_art,
                match (has_art, self.cover_view.failed) {
                    (false, _) => "—",
                    (true, true) => "No preview",
                    (true, false) => "…",
                },
            ),
        }

        // Beside the art: title, artist, length.
        let text_x = MARGIN_X.saturating_add(art_size).saturating_add(SPACE_2);
        let text_w = fb.width.saturating_sub(text_x).saturating_sub(MARGIN_X);
        text::draw_clipped(
            fb,
            text_x,
            art_y,
            text_w,
            &item.title,
            TypeRole::Body,
            ACCENT,
        );
        text::draw_clipped(
            fb,
            text_x,
            art_y.saturating_add(ROW_H),
            text_w,
            item.artist.as_deref().unwrap_or(""),
            TypeRole::Small,
            FG_DIM,
        );
        let length = item
            .duration_ms
            .map(format_timecode)
            .unwrap_or_else(|| "--:--".to_string());
        text::draw_clipped(
            fb,
            text_x,
            art_y.saturating_add(ROW_H.saturating_mul(2)),
            text_w,
            &length,
            TypeRole::Small,
            FG_DIM,
        );

        // Info line under the art: "12 tracks  8 chapters" plus the missing-media warning.
        let info_y = art_y.saturating_add(art_size).saturating_add(INFO_GAP);
        let actions = self.detail_actions();
        let actions_with_info = info_y.saturating_add(ROW_H);
        let with_info_end = actions_with_info
            .saturating_add((actions.len() as u16).saturating_mul(ROW_H));
        let show_info = self.current_detail_info().is_some() && with_info_end <= CONTENT_BOTTOM;
        let actions_y = if show_info { actions_with_info } else { info_y };

        if show_info {
            if let Some(info) = self.current_detail_info() {
                self.paint_detail_info(fb, info_y, info);
            }
        }

        let focus = nav.focus % actions.len().max(1);
        // Five actions plus a banner can exceed the content box: scroll so the focus is shown.
        let visible = (CONTENT_BOTTOM.saturating_sub(actions_y) / ROW_H).max(1) as usize;
        let first = window_start(focus, actions.len(), visible);
        for (i, action) in actions.iter().enumerate().skip(first).take(visible) {
            widgets::data_row(
                fb,
                actions_y.saturating_add(((i - first) as u16).saturating_mul(ROW_H)),
                DataRow {
                    label: action.label(),
                    value: None,
                    indent: 0,
                    state: if i == focus {
                        widgets::RowState::Focused
                    } else {
                        widgets::RowState::Normal
                    },
                    // The `ACCENT_2` marker means "playing"; an action is never playing, so
                    // focus is the ring only.
                    striped: i % 2 == 1,
                    marker: false,
                },
            );
        }
    }

    /// The Library cover screen (ART_09, D11): the picture centred in the content box, the
    /// `n/m` indicator in the margin at its bottom-right and chevrons at its sides when there is
    /// more than one image.
    fn render_library_cover(&self, fb: &mut FrameBuffer) {
        let v = &self.cover_view;
        let (x, y, side) = (COVER_VIEW_X, COVER_VIEW_Y, COVER_VIEW_SIDE);
        match v.image.as_ref().filter(|i| i.side == side && !v.failed) {
            Some(img) => paint_art_square(fb, x, y, img, 100),
            None => widgets::art_box(
                fb,
                x,
                y,
                side,
                !v.failed,
                if v.failed { "No preview" } else { "…" },
            ),
        }
        if v.count == 0 {
            return;
        }
        let gutter = x.saturating_sub(MARGIN_X);
        let right = x.saturating_add(side);
        let indicator = format!("{}/{}", v.index.saturating_add(1), v.count);
        text::draw_aligned(
            fb,
            right.saturating_add(SPACE_1),
            y.saturating_add(side).saturating_sub(cell_height(TypeRole::Small)),
            gutter.saturating_sub(SPACE_1),
            &indicator,
            TypeRole::Small,
            FG_DIM,
            Align::Right,
        );
        if v.count > 1 {
            let cy = y.saturating_add(side / 2);
            let arm = SPACE_2 / 2;
            let more_before = v.index > 0;
            let more_after = v.index.saturating_add(1) < v.count;
            let colour = |on: bool| if on { FG_DIM } else { FG_MUTED };
            chevron_left(fb, x.saturating_sub(SPACE_2 + arm), cy, arm, colour(more_before));
            chevron_right(fb, right.saturating_add(SPACE_2), cy, arm, colour(more_after));
        }
    }

    /// Counts + missing-media line for the detail screen. Counts are `FG_DIM`; the warning is
    /// appended in `WARN`. The counts are clipped to leave room for the warning, so the two
    /// can never overlap.
    fn paint_detail_info(&self, fb: &mut FrameBuffer, y: u16, info: &DetailInfo) {
        let mut parts: Vec<String> = Vec::new();
        if info.tracks.len() > 1 && !matches!(self.browse_kind, Some(LibraryKind::Track)) {
            parts.push(format!("{} tracks", info.tracks.len()));
        }
        if !info.chapters.is_empty() {
            parts.push(format!("{} chapters", info.chapters.len()));
        }
        let counts = parts.join("  ");
        let max_w = content_width(fb);
        if info.media_missing {
            let missing = "Media file missing";
            let missing_w = measure(missing, TypeRole::Small);
            let counts_w = max_w.saturating_sub(missing_w.saturating_add(SPACE_2));
            text::draw_clipped(
                fb,
                MARGIN_X,
                y,
                counts_w,
                &counts,
                TypeRole::Small,
                FG_DIM,
            );
            let used = measure(&counts, TypeRole::Small).min(counts_w);
            let x = if counts.is_empty() {
                MARGIN_X
            } else {
                MARGIN_X.saturating_add(used).saturating_add(SPACE_2)
            };
            text::draw_clipped(
                fb,
                x,
                y,
                fb.width.saturating_sub(MARGIN_X).saturating_sub(x),
                missing,
                TypeRole::Small,
                WARN,
            );
        } else {
            text::draw_clipped(fb, MARGIN_X, y, max_w, &counts, TypeRole::Small, FG_DIM);
        }
    }

    /// Associate wait (UI_11 §7): a centred prompt, the item being associated and the cancel
    /// hint. The title is sanitised (SEC-12) and clipped to the content width.
    fn render_library_associate_wait(&self, fb: &mut FrameBuffer, _y0: u16) {
        let title = self.detail_item().map_or("track", |i| i.title.as_str());
        let max_w = content_width(fb);
        // ~y 60 on the 320x240 panel; `SCREEN_H / 4` keeps it a token-derived expression.
        let prompt_y = SCREEN_H / 4;
        text::draw_aligned(
            fb,
            MARGIN_X,
            prompt_y,
            max_w,
            "Tap a Noubin",
            TypeRole::Display,
            ACCENT,
            Align::Centre,
        );
        text::draw_aligned(
            fb,
            MARGIN_X,
            prompt_y
                .saturating_add(cell_height(TypeRole::Display))
                .saturating_add(SPACE_2),
            max_w,
            &format!("for {}", screen_text(title)),
            TypeRole::Small,
            FG_DIM,
            Align::Centre,
        );
        text::draw_aligned(
            fb,
            MARGIN_X,
            CONTENT_BOTTOM
                .saturating_sub(ROW_H)
                .saturating_add((ROW_H.saturating_sub(cell_height(TypeRole::Small))) / 2),
            max_w,
            "Back cancels",
            TypeRole::Small,
            FG_MUTED,
            Align::Centre,
        );
    }

    /// Diagnostics: the same three values as before, on the shared row vocabulary. This is a
    /// measurement screen, so it gains no styling or data beyond the token/role swap.
    fn render_debug(&self, fb: &mut FrameBuffer, y0: u16) {
        let temp = self
            .system
            .temperature_c
            .map(|t| format!("{t}C"))
            .unwrap_or_else(|| "n/a".into());
        let rows = [
            ("Temperature", temp),
            ("Fan", self.system.fan_status.clone()),
            ("Volume", format!("{}%", self.volume.percent())),
        ];
        for (i, (label, value)) in rows.iter().enumerate() {
            let y = y0.saturating_add((i as u16).saturating_mul(ROW_H));
            if y.saturating_add(ROW_H) > CONTENT_BOTTOM {
                break;
            }
            widgets::setting_row(fb, y, label, value, widgets::RowState::Normal, false);
        }
    }
}

/// `"1 item"` / `"n items"` for the library header meta (singular-aware).
fn items_label(n: u64) -> String {
    if n == 1 {
        "1 item".to_string()
    } else {
        format!("{n} items")
    }
}

/// `s` cut to at most `max` characters, ending in `..` when it had to be shortened.
fn clip_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let keep = max.saturating_sub(2);
    let mut out: String = s.chars().take(keep).collect();
    out.push_str("..");
    out
}

/// `i/n` header meta for a browsable list: 1-based position of `focus`, `None` when empty.
fn position_meta(focus: usize, rows: usize) -> Option<String> {
    if rows == 0 {
        None
    } else {
        Some(format!("{}/{}", focus.min(rows - 1).saturating_add(1), rows))
    }
}

/// Rows that fit between `top` and [`CONTENT_BOTTOM`] at the [`ROW_H`] rhythm.
fn visible_rows(top: u16) -> usize {
    (CONTENT_BOTTOM.saturating_sub(top) / ROW_H).max(1) as usize
}

/// First painted row of a window that keeps `focus` visible (same rule as `widgets::list`).
fn window_start(focus: usize, len: usize, visible: usize) -> usize {
    if len <= visible || visible == 0 {
        0
    } else {
        focus.saturating_add(1).saturating_sub(visible).min(len - visible)
    }
}

/// Right-margin scrollbar for a windowed data list. `primitives::scrollbar` paints nothing
/// when every row fits, so short lists show no dead track.
fn list_scrollbar(fb: &mut FrameBuffer, top: u16, first: usize, visible: usize, total: usize) {
    let bottom = CONTENT_BOTTOM.min(fb.height);
    if bottom <= top {
        return;
    }
    scrollbar(
        fb,
        fb.width.saturating_sub(2),
        top,
        bottom.saturating_sub(top),
        first,
        visible,
        total,
    );
}

/// Which number an Up Next track row shows.
///
/// Chosen per list by [`ScreenModel::np_row_numbering`]; the two modes exist because a
/// collection can either *be* an ordered work or just be a bag of tracks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NpRowNumbering {
    /// The collection's own numbering (`item_position + 1`): album track numbers, chapter
    /// numbers, mix parts. A shuffled list therefore reads scrambled, which is the point —
    /// every row still says which part of the work it is.
    Source,
    /// A plain `1..n` count down the rows as displayed, for lists with no numbering of their
    /// own (playlists, dynamic queues).
    Sequence,
}

/// Row text and indent: tracks show their number, cues their start time (`12:34`).
///
/// `numbering` decides *which* number a track row shows (see
/// [`ScreenModel::np_row_numbering`]): the collection's own source position for albums, mixes,
/// podcasts and audiobooks, or the plain `1..n` row count for playlists and dynamic queues.
pub(crate) fn np_row_label(
    e: &NowPlayingEntry,
    numbering: NpRowNumbering,
    has_source_positions: bool,
) -> (u8, String) {
    match e.kind {
        // A source-numbered row prints the `mediaList` position, so a shuffled album still
        // reads 1..n. `item_position == 0` is both the first source item and the pre-PLAY_02
        // sentinel, so the list-level `has_source_positions` flag decides; the row count is
        // the honest fallback for an old peer.
        NowPlayingEntryKind::Track => {
            let number = if numbering == NpRowNumbering::Source && has_source_positions {
                e.item_position + 1
            } else {
                e.track_index + 1
            };
            (0, format!("{number}. {}", e.title))
        }
        NowPlayingEntryKind::Cue => (8, format!("{} {}", format_timecode(e.start_ms), e.title)),
    }
}

/// Row text without the indent (tests / logging).
#[cfg(test)]
pub(crate) fn np_row_text(
    e: &NowPlayingEntry,
    numbering: NpRowNumbering,
    has_source_positions: bool,
) -> String {
    np_row_label(e, numbering, has_source_positions).1
}

/// PLAY_04: whether the engine sends real source positions. True once any track entry carries a
/// non-zero `item_position` (a PLAY_02+ engine); false for an old peer, where every entry is `0`
/// and the playback index is the only number available.
fn source_positions(list: &NowPlayingListChanged) -> bool {
    list.entries
        .iter()
        .any(|e| e.kind == NowPlayingEntryKind::Track && e.item_position != 0)
}

fn format_timecode(ms: u64) -> String {
    let total_secs = ms / 1000;
    let mins = total_secs / 60;
    let secs = total_secs % 60;
    format!("{mins}:{secs:02}")
}

/// Now Playing footer clock with the Play mode sub-second unit (UI_14 §7, UI_16 §4):
/// `MM:SS:FF` for a frame rate, `MM:SS.t` for decimal tenths, `MM:SS.tt` for decimal
/// hundredths, and plain `MM:SS` when the unit is off. Sub-second digits are truncated, never
/// rounded (the existing frame behaviour, kept for both decimal modes). Only the footer's
/// position / total length slots use this; every other caller keeps [`format_timecode`].
pub(crate) fn format_clock(ms: u64, unit: TimecodeUnit) -> String {
    if unit == TimecodeUnit::Off {
        return format_timecode(ms);
    }
    let total_secs = ms / 1000;
    let mins = total_secs / 60;
    let secs = total_secs % 60;
    match unit {
        TimecodeUnit::Off => format_timecode(ms),
        TimecodeUnit::DecTenths => {
            let tenths = (ms % 1000) / 100;
            format!("{mins:02}:{secs:02}.{tenths}")
        }
        TimecodeUnit::DecHundredths => {
            let hundredths = (ms % 1000) / 10;
            format!("{mins:02}:{secs:02}.{hundredths:02}")
        }
        _ => {
            let fps = unit.frames_per_second().unwrap_or(75);
            let frames = (ms % 1000) * u64::from(fps) / 1000;
            format!("{mins:02}:{secs:02}:{frames:02}")
        }
    }
}

/// Screen column showing `t_ms` for a `width`-wide band: the inverse of the column mapping
/// (`t0 = playhead - WINDOW/2 + x * WINDOW/width`). May be negative / past the width.
#[cfg(test)]
pub(crate) fn wave_x_for_time(t_ms: i64, playhead_ms: i64, width: u16, window_ms: u64) -> i64 {
    let window = window_ms.max(1) as i64;
    let rel = t_ms - playhead_ms + window / 2;
    rel.saturating_mul(i64::from(width)) / window
}

/// Time span `[t0, t1)` covered by band column `x`, in ms relative to the track start.
///
/// `t0 = playhead - window/2 + x * window/width` and `t1` is the **next** column's `t0`, so
/// the columns tile the window exactly: no gap, no overlap, and `x = 160` starts exactly at
/// the playhead. `window_ms` is the live ZOOM window (analysis buckets are unchanged).
///
/// The multiplication happens before the division on purpose. At 320 px and a 30 s window a
/// column is 93.75 ms, so the integer spans alternate 93 / 94 ms; truncating the column width
/// to 93 ms first would put the centre column 120 ms early (160 × 93 = 14 880 instead of
/// 15 000) and compress the whole window to 29 760 ms.
pub(crate) fn wave_column_span(
    x: u16,
    playhead_ms: i64,
    width: u16,
    window_ms: u64,
) -> (i64, i64) {
    let w = i64::from(width.max(1));
    let window = window_ms.max(1) as i64;
    let left = playhead_ms - window / 2;
    (
        left + i64::from(x) * window / w,
        left + (i64::from(x) + 1) * window / w,
    )
}

/// Peak for one band column: max of the buckets its time span covers, clipped to the file.
///
/// `[t0, t1) = wave_column_span(x)`; `b0 = floor(t0 / bucket_ms)`,
/// `b1 = floor((t1 - 1) / bucket_ms)`. A bucket on a boundary belongs to exactly one column
/// (`t1 - 1` is inclusive). `None` when the column is outside `[0, duration)` or no bucket of
/// it exists (silent / empty file).
pub(crate) fn wave_column_peak(
    peaks: &[u8],
    bucket_ms: i64,
    playhead_ms: i64,
    x: u16,
    width: u16,
    window_ms: u64,
    duration_ms: i64,
) -> Option<u8> {
    if peaks.is_empty() || bucket_ms <= 0 || width == 0 {
        return None;
    }
    let (t0, t1) = wave_column_span(x, playhead_ms, width, window_ms);
    // Nothing before 0:00 or past the decoded end (no invented bars).
    if t0 >= duration_ms || t1 <= 0 {
        return None;
    }
    let b0 = t0.div_euclid(bucket_ms).max(0);
    let b1 = (t1 - 1)
        .div_euclid(bucket_ms)
        .min(peaks.len() as i64 - 1);
    if b1 < b0 {
        return None;
    }
    peaks[b0 as usize..=b1 as usize].iter().copied().max()
}

/// Bar half-height for a stored peak: `round(half * (peak/255)^WAVE_GAMMA)`; a silent bucket
/// is 0 px and any non-silent bucket is at least 1 px.
pub(crate) fn wave_height(peak: u8, half: u16) -> u16 {
    if peak == 0 || half == 0 {
        return 0;
    }
    let frac = (f32::from(peak) / 255.0).powf(WAVE_GAMMA);
    let h = (f32::from(half) * frac).round() as u16;
    h.clamp(1, half)
}

/// `On` / `Off` for the Audio rows (no bool formatter elsewhere in the crate).
fn on_off_text(value: bool) -> &'static str {
    if value {
        "On"
    } else {
        "Off"
    }
}

/// Description under the Output devices rows: the Local only sentence, an unavailable
/// output's reconnect hint, or the general multi-output hint.
fn audio_outputs_description(
    view: &AudioView,
    outputs: &[plippa_core::audio_route::AudioOutput],
    focus: usize,
) -> String {
    if view.route.local_only {
        return "Local only: built-in output, lowest latency, no delay. Extra outputs are paused, not forgotten.".into();
    }
    if focus == 0 {
        return "Use the built-in output only. Extra outputs keep their tick, paused until this is off.".into();
    }
    match outputs.get(focus - 1) {
        Some(output) if !output.available => format!(
            "{} is not connected. It comes back when it reappears.",
            screen_text(&output.description)
        ),
        _ => "Tick the outputs that play together. The built-in output always keeps the lowest latency.".into(),
    }
}

/// Width of the body column: the screen minus one [`MARGIN_X`] on each side.
/// What a screen that would show the portal address says while the portal is switched off.
pub const PORTAL_OFF_TEXT: &str = "Web portal is off. Turn it on in System > Network";

fn content_width(fb: &FrameBuffer) -> u16 {
    fb.width.saturating_sub(MARGIN_X.saturating_mul(2))
}

/// Map the IPC notice severity onto the widget severity (no "OK" level crosses the IPC).
fn notice_severity(s: NoticeSeverity) -> widgets::NoticeSeverity {
    match s {
        NoticeSeverity::Error => widgets::NoticeSeverity::Error,
        NoticeSeverity::Warning => widgets::NoticeSeverity::Warning,
        NoticeSeverity::Info => widgets::NoticeSeverity::Info,
    }
}

/// Scan arrows from the instantaneous rate: `>> 4.0x` / `<< 4.0x` (WARP_18 drives the scan).
pub fn scan_text(forward: bool, rate_permille: i32) -> String {
    let x = rate_permille.unsigned_abs() as f64 / 1000.0;
    format!("{} {x:.1}x", if forward { ">>" } else { "<<" })
}


// ---------------------------------------------------------------------------
// BCS_07: the store tap screen
// ---------------------------------------------------------------------------

/// BCS_11: the side of the release cover on the Owned / Confirm / Buy screens, top right
/// beside the title block. Small on purpose: the screen's job is the decision under it.
pub const STORE_COVER_SIDE: u16 = 48;
/// The square the purchase QR may use, in pixels. It sits in the left column of the
/// not-owned screen; the short link and the primary action use the rest.
pub const STORE_QR_BOX: u16 = 120;
/// The smallest module the QR may use: below this the symbol stops being reliably scannable
/// on a 320x240 panel held at reading distance. The download of the screen never shrinks it;
/// a URL that cannot reach it is shown as text instead.
///
/// Measured on the POC2 panel (ILI9341 at rotation 90 + mirror, the same paint path as this
/// screen): a version-4 symbol at 2 px per module scanned reliably on a phone camera, 3 px and
/// 4 px always did. 2 px only decides what a long URL is allowed to fall back to — a short URL
/// still gets the largest module the box can carry — so the measured floor is the right value:
/// it costs nothing for normal links and keeps purchase links from losing their QR.
pub const STORE_QR_MIN_MODULE_PX: u16 = 2;
/// Quiet zone around the symbol, in modules (the QR spec asks for 4; 2 fits the box and the
/// light background around the symbol still separates it from the dark screen).
pub const STORE_QR_QUIET_MODULES: u32 = 2;
/// Longest short-link text kept for the screen (characters) — one line of `Small` is 38.
pub const STORE_URL_MAX_CHARS: usize = 76;

/// Pixel size of one QR module in a `box_side` box, quiet zone included, or `None` when the
/// symbol would need modules smaller than [`STORE_QR_MIN_MODULE_PX`].
pub fn store_qr_module_px(modules: u32, box_side: u16) -> Option<u16> {
    if modules == 0 {
        return None;
    }
    let total = modules.saturating_add(STORE_QR_QUIET_MODULES * 2);
    if total > u32::from(u16::MAX) {
        return None;
    }
    let fit = box_side / total as u16;
    (fit >= STORE_QR_MIN_MODULE_PX).then_some(fit)
}

/// The purchase text the person can retype: scheme and query dropped, nothing added.
pub fn store_short_url(url: &str) -> String {
    let without_scheme = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .unwrap_or(url);
    let without_query = without_scheme
        .split(['?', '#'])
        .next()
        .unwrap_or(without_scheme);
    let trimmed = without_query.trim_end_matches('/');
    let text = if trimmed.is_empty() {
        without_query
    } else {
        trimmed
    };
    clip_chars(text, STORE_URL_MAX_CHARS)
}

/// Encode the purchase page for the screen, preferring the largest module that fits: a bigger
/// module is what the camera needs, so a coarser symbol at Low error correction beats a denser
/// one at Medium. Ties keep Medium (stronger correction at the same density).
fn store_qr(url: &str) -> Option<(plippa_core::qr::QrMatrix, u16)> {
    use plippa_core::qr::{QrErrorLevel, QrMatrix};
    let mut best: Option<(QrMatrix, u16)> = None;
    for level in [QrErrorLevel::Medium, QrErrorLevel::Low] {
        if let Ok(matrix) = QrMatrix::encode(url, level) {
            if let Some(module) = store_qr_module_px(matrix.width, STORE_QR_BOX) {
                // Strictly greater: the Medium attempt comes first, so equal sizes keep it.
                if best.as_ref().is_none_or(|(_, best_module)| module > *best_module) {
                    best = Some((matrix, module));
                }
            }
        }
    }
    best
}

/// Paint `matrix` at (`x`, `y`) with a light background and a quiet zone. Returns the side.
fn paint_store_qr(
    fb: &mut FrameBuffer,
    x: u16,
    y: u16,
    matrix: &plippa_core::qr::QrMatrix,
    module: u16,
) -> u16 {
    let quiet = STORE_QR_QUIET_MODULES as u16;
    let side = (matrix.width as u16).saturating_add(quiet * 2).saturating_mul(module);
    fill(fb, x, y, side, side, FG);
    for my in 0..matrix.width {
        for mx in 0..matrix.width {
            if matrix.get(mx, my) {
                // `matrix.width` is at most 177, so the u16 arithmetic cannot overflow here.
                let px = x.saturating_add((mx as u16).saturating_add(quiet).saturating_mul(module));
                let py = y.saturating_add((my as u16).saturating_add(quiet).saturating_mul(module));
                fill(fb, px, py, module, module, BG);
            }
        }
    }
    side
}

/// `12.3 MB` / `480 KB` — decimal for MB so it reads like a file size, not a disk cap.
fn store_bytes_text(bytes: u64) -> String {
    const MB: u64 = 1024 * 1024;
    if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else {
        format!("{} KB", bytes / 1024)
    }
}

/// One action row's label and value column.
fn store_action_text(action: crate::store_tap::StoreTapAction, view: &StoreTapView) -> String {
    use crate::store_tap::StoreTapAction as A;
    match action {
        A::Download => "Download".to_string(),
        A::Buy => "Buy on Bandcamp".to_string(),
        A::Pick(index) => match &view.phase {
            StoreTapPhase::Ambiguous { candidates, .. } => candidates
                .get(index)
                .map(|c| clip_chars(&screen_text(&c.title), 34))
                .unwrap_or_else(|| "Release".to_string()),
            _ => "Release".to_string(),
        },
        A::ConfirmCandidate => "This is the one".to_string(),
        A::RejectCandidate => "Not mine".to_string(),
        A::Play => "Play".to_string(),
        A::ShowInLibrary => "Show in library".to_string(),
        A::Retry => "Try again".to_string(),
        A::Sync => "Resync now".to_string(),
        A::AllowLookup => "Look up release".to_string(),
        A::Back => "Cancel".to_string(),
    }
}

/// The value column of an action row: the artist / year hint, never a second label.
fn store_action_value(action: crate::store_tap::StoreTapAction, view: &StoreTapView) -> Option<String> {
    use crate::store_tap::StoreTapAction as A;
    match action {
        A::Pick(index) => match &view.phase {
            StoreTapPhase::Ambiguous { candidates, .. } => candidates
                .get(index)
                .map(|c| clip_chars(&screen_text(&c.artist), 12))
                .filter(|s| !s.is_empty()),
            _ => None,
        },
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// BCS_08: shared painting for the store screens
// ---------------------------------------------------------------------------

/// The last row the body may use when `rows` action rows are pinned to the bottom.
fn store_body_bottom(rows: usize) -> u16 {
    CONTENT_BOTTOM
        .saturating_sub((rows as u16).saturating_mul(ROW_H))
        .saturating_sub(SPACE_1)
}

/// One clipped line of body text at the screen margin, returning the next line's top.
fn store_line(
    fb: &mut FrameBuffer,
    y: u16,
    text: &str,
    role: TypeRole,
    colour: Rgb565,
) -> u16 {
    let width = content_width(fb);
    text::draw_clipped(fb, MARGIN_X, y, width, text, role, colour);
    y.saturating_add(cell_height(role))
}

/// Wrapped body text kept above `bottom`. Never paints a partial line into the action rows.
fn store_para(
    fb: &mut FrameBuffer,
    y: u16,
    bottom: u16,
    text: &str,
    role: TypeRole,
    colour: Rgb565,
    max_lines: usize,
) -> u16 {
    let width = content_width(fb);
    let mut y = y;
    for (i, line) in text::wrap(text, width, role).into_iter().enumerate() {
        if i >= max_lines || y.saturating_add(cell_height(role)) > bottom {
            break;
        }
        text::draw_clipped(fb, MARGIN_X, y, width, &line, role, colour);
        y = y.saturating_add(cell_height(role));
    }
    y
}

/// Paint the action block pinned to [`CONTENT_BOTTOM`], the focused row highlighted.
fn store_action_rows(fb: &mut FrameBuffer, rows: &[(String, Option<String>, bool)], focus: usize) {
    if rows.is_empty() {
        return;
    }
    let top = CONTENT_BOTTOM.saturating_sub((rows.len() as u16).saturating_mul(ROW_H));
    let focus = focus.min(rows.len() - 1);
    for (i, (label, value, marker)) in rows.iter().enumerate() {
        widgets::data_row(
            fb,
            top.saturating_add((i as u16).saturating_mul(ROW_H)),
            DataRow {
                label,
                value: value.as_deref(),
                indent: 0,
                state: if i == focus {
                    widgets::RowState::Focused
                } else {
                    widgets::RowState::Normal
                },
                striped: i % 2 == 1,
                marker: *marker,
            },
        );
    }
}

/// The download progress bar plus the track / byte counts, above the action rows.
fn store_progress(
    fb: &mut FrameBuffer,
    y0: u16,
    bottom: u16,
    progress: &crate::store_api::DownloadProgress,
) -> u16 {
    let width = content_width(fb);
    let ratio = if progress.total_bytes > 0 {
        (progress.done_bytes as f32 / progress.total_bytes as f32).clamp(0.0, 1.0)
    } else if progress.tracks_total > 0 {
        (progress.tracks_done as f32 / progress.tracks_total as f32).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let mut y = y0;
    fill(fb, MARGIN_X, y, width, 6, SURFACE);
    let filled = (f32::from(width) * ratio) as u16;
    if filled > 0 {
        fill(fb, MARGIN_X, y, filled, 6, ACCENT);
    }
    y = y.saturating_add(10);
    if progress.tracks_total > 0 {
        y = store_line(
            fb,
            y,
            &format!(
                "{} of {} tracks",
                progress.tracks_done, progress.tracks_total
            ),
            TypeRole::Small,
            FG_DIM,
        );
    }
    if progress.total_bytes > 0 && y.saturating_add(cell_height(TypeRole::Small)) <= bottom {
        y = store_line(
            fb,
            y,
            &format!(
                "{} of {}",
                store_bytes_text(progress.done_bytes),
                store_bytes_text(progress.total_bytes)
            ),
            TypeRole::Small,
            FG_DIM,
        );
    }
    y
}

/// The Stores screen's action rows.
fn stores_row(action: StoresAction, view: &StoresView) -> (String, Option<String>, bool) {
    let label = match action {
        StoresAction::Bandcamp => "Bandcamp",
        StoresAction::Sync => "Resync library",
        StoresAction::Connect => "Connect Bandcamp",
        StoresAction::Refresh => "Refresh",
        StoresAction::Retry => "Try again",
        StoresAction::Back => "Back",
    };
    let value = match action {
        StoresAction::Bandcamp => Some(view.status_text()),
        StoresAction::Sync => view.sync_value(),
        _ => None,
    };
    (label.to_string(), value, false)
}

/// The Connect screen's action rows.
fn connect_row(action: ConnectAction, _view: &ConnectView) -> (String, Option<String>, bool) {
    let label = match action {
        ConnectAction::Retry => "Refresh address",
        ConnectAction::Back => "Back",
    };
    (label.to_string(), None, false)
}

/// The release detail screen's action rows.
fn release_row(action: ReleaseAction, view: &ReleaseView) -> (String, Option<String>, bool) {
    let label = match action {
        ReleaseAction::Download => "Download",
        ReleaseAction::CancelDownload => "Cancel download",
        ReleaseAction::OpenInLibrary => "Show in library",
        ReleaseAction::Remove => "Remove from Plippa",
        ReleaseAction::Retry => "Try again",
        ReleaseAction::Back => "Back",
    };
    let value = match action {
        ReleaseAction::Download => match &view.download {
            DownloadPhase::Confirm { consent } => consent
                .estimated_bytes
                .map(store_bytes_text)
                .or_else(|| Some(items_label(consent.tracks as u64))),
            _ => None,
        },
        ReleaseAction::CancelDownload => match &view.download {
            DownloadPhase::Running { progress, .. } if progress.tracks_total > 0 => Some(format!(
                "{} of {}",
                progress.tracks_done, progress.tracks_total
            )),
            _ => None,
        },
        _ => None,
    };
    (label.to_string(), value, false)
}

/// The Remove screen's action rows.
fn delete_row(action: DeleteAction, view: &DeleteView) -> (String, Option<String>, bool) {
    let label = match action {
        DeleteAction::Remove => "Remove from Plippa",
        DeleteAction::Retry => "Try again",
        DeleteAction::Back => "Back",
    };
    let value = match action {
        DeleteAction::Remove => view.preview().map(|preview| {
            format!(
                "{} files - {}",
                preview.files,
                store_bytes_text(preview.total_bytes)
            )
        }),
        _ => None,
    };
    (label.to_string(), value, false)
}

impl ScreenModel {
    /// Store tap (BCS_07): title, at most a few body lines, and the actions on fixed rows at
    /// the bottom so the primary action is always in the same place and reachable.
    fn render_store_tap(&self, _nav: &NavStack, fb: &mut FrameBuffer, y0: u16) {
        let Some(view) = self.store_tap.as_ref() else {
            widgets::message_state(fb, "No tap", "Tap a Noubin to check its store");
            return;
        };
        let left = MARGIN_X;
        let width = content_width(fb);
        let actions = view.actions();
        let action_top = CONTENT_BOTTOM.saturating_sub((actions.len() as u16).saturating_mul(ROW_H));
        // Everything above the action block, minus one hairline gap.
        let body_bottom = action_top.saturating_sub(SPACE_1);
        let line = |fb: &mut FrameBuffer, y: u16, text: &str, role: TypeRole, colour: Rgb565| {
            text::draw_clipped(fb, left, y, width, text, role, colour);
            y.saturating_add(cell_height(role))
        };
        // Wrapped body text, kept above `body_bottom`; returns the next free row.
        let paragraph = |fb: &mut FrameBuffer,
                         y: u16,
                         text: &str,
                         role: TypeRole,
                         colour: Rgb565,
                         max_lines: usize|
         -> u16 {
            let mut y = y;
            for (i, wrapped) in text::wrap(text, width, role).into_iter().enumerate() {
                if i >= max_lines {
                    break;
                }
                if y.saturating_add(cell_height(role)) > body_bottom {
                    break;
                }
                text::draw_clipped(fb, left, y, width, &wrapped, role, colour);
                y = y.saturating_add(cell_height(role));
            }
            y
        };

        let mut y = y0;
        // BCS_11: Owned, Confirm and Buy show the matched release's cover (or the "no art"
        // placeholder) top right; the title block beside it is narrower, and anything under it
        // starts below the picture.
        let shows_cover = matches!(
            view.phase,
            StoreTapPhase::Owned { .. }
                | StoreTapPhase::Confirm { .. }
                | StoreTapPhase::NotOwned { .. }
        );
        let cover_x = left.saturating_add(width).saturating_sub(STORE_COVER_SIDE);
        let cover_bottom = y0.saturating_add(STORE_COVER_SIDE);
        let head_w = if shows_cover {
            width
                .saturating_sub(STORE_COVER_SIDE)
                .saturating_sub(SPACE_2)
        } else {
            width
        };
        if shows_cover {
            let wanted = view.cover_release_id();
            match &view.cover {
                CoverState::Ready(img)
                    if view.cover_for.as_deref() == wanted && img.side == STORE_COVER_SIDE =>
                {
                    paint_art_square(fb, cover_x, y0, img, 100)
                }
                state => widgets::art_box(
                    fb,
                    cover_x,
                    y0,
                    STORE_COVER_SIDE,
                    false,
                    if matches!(state, CoverState::Loading) { "…" } else { "—" },
                ),
            }
        }
        let head_line = |fb: &mut FrameBuffer, y: u16, text: &str, role: TypeRole, colour: Rgb565| {
            text::draw_clipped(fb, left, y, head_w, text, role, colour);
            y.saturating_add(cell_height(role))
        };
        match &view.phase {
            StoreTapPhase::Lookup => {
                let title = if view.tag_title.is_empty() {
                    "Looking this up"
                } else {
                    &view.tag_title
                };
                y = line(fb, y, &screen_text(title), TypeRole::BodyBold, FG);
                y = y.saturating_add(SPACE_1);
                paragraph(
                    fb,
                    y,
                    "Checking your Bandcamp collection on this Plippa. Nothing is downloaded yet.",
                    TypeRole::Small,
                    FG_DIM,
                    3,
                );
            }
            StoreTapPhase::Preparing => {
                y = line(fb, y, "Checking the download", TypeRole::BodyBold, FG);
                y = y.saturating_add(SPACE_1);
                paragraph(
                    fb,
                    y,
                    "Working out how many tracks and how much space. Nothing is fetched yet.",
                    TypeRole::Small,
                    FG_DIM,
                    3,
                );
            }
            StoreTapPhase::Owned { release, message } => {
                y = head_line(fb, y, &screen_text(&release.title), TypeRole::BodyBold, FG);
                if !release.artist.is_empty() {
                    y = head_line(fb, y, &screen_text(&release.artist), TypeRole::Small, FG_DIM);
                }
                if let Some(tracks) = release.track_count {
                    y = head_line(fb, y, &items_label(u64::from(tracks)), TypeRole::Small, FG_DIM);
                }
                y = y.max(cover_bottom).saturating_add(SPACE_1);
                y = paragraph(fb, y, &screen_text(message), TypeRole::Small, FG, 2);
                y = y.saturating_add(SPACE_1);
                paragraph(fb, y, STORE_LOSSY_NOTE, TypeRole::Small, WARN, 3);
            }
            StoreTapPhase::LookupConsent { host, title } => {
                y = line(fb, y, &screen_text(title), TypeRole::BodyBold, FG);
                y = y.saturating_add(SPACE_1);
                let text = if host.is_empty() {
                    "This tag names a Noubin page. Reading it looks up the release's store link."
                        .to_string()
                } else {
                    format!("This tag names a Noubin page. Reading {host} looks up the release's store link.")
                };
                y = paragraph(fb, y, &screen_text(&text), TypeRole::Small, FG, 4);
                y = y.saturating_add(SPACE_1);
                paragraph(
                    fb,
                    y,
                    "Nothing is downloaded. The store is only checked after this.",
                    TypeRole::Small,
                    FG_DIM,
                    2,
                );
            }
            StoreTapPhase::NotOwned {
                title,
                artist,
                url,
                interest_saved,
                from_pick,
                ..
            } => {
                y = head_line(fb, y, &screen_text(title), TypeRole::BodyBold, FG);
                if !artist.is_empty() {
                    y = head_line(fb, y, &screen_text(artist), TypeRole::Small, FG_DIM);
                }
                if *from_pick {
                    y = head_line(fb, y, "The release you picked", TypeRole::Small, FG_MUTED);
                }
                let qr_y = y.saturating_add(SPACE_1);
                let mut right_x = left;
                let mut right_w = width;
                if let Some((matrix, module)) = store_qr(url) {
                    let side = (matrix.width as u16)
                        .saturating_add(STORE_QR_QUIET_MODULES as u16 * 2)
                        .saturating_mul(module);
                    if qr_y.saturating_add(side) <= body_bottom {
                        paint_store_qr(fb, left, qr_y, &matrix, module);
                        right_x = left.saturating_add(STORE_QR_BOX).saturating_add(SPACE_2);
                        right_w = (left.saturating_add(width)).saturating_sub(right_x);
                    }
                }
                // The QR column starts at `qr_y`; the text column starts below the cover.
                let mut ry = qr_y.max(cover_bottom.saturating_add(SPACE_1));
                text::draw_clipped(fb, right_x, ry, right_w, "Buy on Bandcamp", TypeRole::BodyBold, ACCENT);
                ry = ry.saturating_add(cell_height(TypeRole::BodyBold));
                for wrapped in text::wrap(&store_short_url(url), right_w, TypeRole::Small) {
                    if ry.saturating_add(cell_height(TypeRole::Small)) > body_bottom {
                        break;
                    }
                    text::draw_clipped(fb, right_x, ry, right_w, &wrapped, TypeRole::Small, FG);
                    ry = ry.saturating_add(cell_height(TypeRole::Small));
                }
                ry = ry.saturating_add(SPACE_1);
                let hint = if qr_y.saturating_add(STORE_QR_BOX) <= body_bottom {
                    "Scan with your phone"
                } else {
                    "Visit this page to buy it"
                };
                if ry.saturating_add(cell_height(TypeRole::Small)) <= body_bottom {
                    text::draw_clipped(fb, right_x, ry, right_w, hint, TypeRole::Small, FG_MUTED);
                }
                let note = if *interest_saved {
                    "Saved to your wants"
                } else {
                    "Plippa never takes payment"
                };
                let note_y = body_bottom.saturating_sub(cell_height(TypeRole::Small));
                if note_y > y.max(cover_bottom) {
                    text::draw_clipped(fb, left, note_y, width, note, TypeRole::Small, FG_MUTED);
                }
            }
            StoreTapPhase::Ambiguous { .. } => {
                y = line(fb, y, "Which release is this?", TypeRole::BodyBold, FG);
                y = y.saturating_add(SPACE_1);
                for wrapped in text::wrap(
                    "Pick the one the tag names. Downloading needs you to own it.",
                    width,
                    TypeRole::Small,
                ) {
                    if y.saturating_add(cell_height(TypeRole::Small)) > body_bottom {
                        break;
                    }
                    text::draw_clipped(fb, left, y, width, &wrapped, TypeRole::Small, FG_DIM);
                    y = y.saturating_add(cell_height(TypeRole::Small));
                }
            }
            StoreTapPhase::LowConfidence { candidate, .. } => {
                y = line(fb, y, "Is this the one?", TypeRole::BodyBold, FG);
                y = y.saturating_add(SPACE_1);
                y = line(fb, y, &screen_text(&candidate.title), TypeRole::Body, FG);
                if !candidate.artist.is_empty() {
                    y = line(fb, y, &screen_text(&candidate.artist), TypeRole::Small, FG_DIM);
                }
                let percent = (candidate.confidence.clamp(0.0, 1.0) * 100.0).round() as u32;
                y = y.saturating_add(SPACE_1);
                paragraph(
                    fb,
                    y,
                    &format!("A {percent}% match. Say so and Plippa will download it."),
                    TypeRole::Small,
                    FG_DIM,
                    2,
                );
            }
            StoreTapPhase::Blocked { blocked, message } => {
                let title = if matches!(blocked, crate::store_api::TapBlocked::NoStoreCache) {
                    "Library not synced yet"
                } else {
                    "Can't check the store"
                };
                y = line(fb, y, title, TypeRole::BodyBold, WARN);
                y = y.saturating_add(SPACE_1);
                paragraph(fb, y, &screen_text(message), TypeRole::Small, FG, 4);
            }
            StoreTapPhase::Syncing { status } => {
                y = line(fb, y, "Syncing your library", TypeRole::BodyBold, FG);
                y = y.saturating_add(SPACE_1);
                let progress = match status {
                    Some(status) => format!("{} pages, {} releases so far", status.pages, status.releases),
                    None => "Starting the sync".to_string(),
                };
                y = line(fb, y, &progress, TypeRole::Small, FG_DIM);
                y = y.saturating_add(SPACE_1);
                paragraph(
                    fb,
                    y,
                    "Then this tag is checked again. Back stops waiting; the sync carries on.",
                    TypeRole::Small,
                    FG_MUTED,
                    3,
                );
            }
            StoreTapPhase::SyncFailed { message } => {
                y = line(fb, y, "The sync didn't finish", TypeRole::BodyBold, ERR);
                y = y.saturating_add(SPACE_1);
                paragraph(fb, y, &screen_text(message), TypeRole::Small, FG, 4);
            }
            StoreTapPhase::Confirm { release, consent } => {
                y = head_line(fb, y, &screen_text(&release.title), TypeRole::BodyBold, FG);
                if !release.artist.is_empty() {
                    y = head_line(fb, y, &screen_text(&release.artist), TypeRole::Small, FG_DIM);
                }
                let tracks = if consent.tracks > 0 {
                    consent.tracks
                } else {
                    release.track_count.unwrap_or(0) as usize
                };
                y = head_line(fb, y, &items_label(tracks as u64), TypeRole::Small, FG_DIM);
                if let Some(bytes) = consent.estimated_bytes {
                    y = head_line(
                        fb,
                        y,
                        &format!("About {}", store_bytes_text(bytes)),
                        TypeRole::Small,
                        FG_DIM,
                    );
                }
                y = y.max(cover_bottom).saturating_add(SPACE_1);
                let note = if consent.note.is_empty() {
                    STORE_LOSSY_NOTE
                } else {
                    consent.note.as_str()
                };
                paragraph(fb, y, &screen_text(note), TypeRole::Small, WARN, 3);
            }
            StoreTapPhase::Downloading { progress, .. } => {
                y = line(fb, y, "Downloading", TypeRole::BodyBold, FG);
                y = y.saturating_add(SPACE_1);
                // Bar: bytes when the store declared them, else tracks, else a moving band.
                let bar_w = width;
                let ratio = if progress.total_bytes > 0 {
                    (progress.done_bytes as f32 / progress.total_bytes as f32).clamp(0.0, 1.0)
                } else if progress.tracks_total > 0 {
                    (progress.tracks_done as f32 / progress.tracks_total as f32).clamp(0.0, 1.0)
                } else {
                    0.0
                };
                fill(fb, left, y, bar_w, 6, SURFACE);
                let filled = (f32::from(bar_w) * ratio) as u16;
                if filled > 0 {
                    fill(fb, left, y, filled, 6, ACCENT);
                }
                y = y.saturating_add(10);
                if progress.tracks_total > 0 {
                    y = line(
                        fb,
                        y,
                        &format!("{} of {} tracks", progress.tracks_done, progress.tracks_total),
                        TypeRole::Small,
                        FG_DIM,
                    );
                }
                if progress.total_bytes > 0 {
                    y = line(
                        fb,
                        y,
                        &format!(
                            "{} of {}",
                            store_bytes_text(progress.done_bytes),
                            store_bytes_text(progress.total_bytes)
                        ),
                        TypeRole::Small,
                        FG_DIM,
                    );
                }
                y = y.saturating_add(SPACE_1);
                paragraph(
                    fb,
                    y,
                    "Bandcamp sends a compressed stream. Keep the screen on until it finishes.",
                    TypeRole::Small,
                    FG_MUTED,
                    2,
                );
            }
            StoreTapPhase::Complete {
                files,
                noudata,
                partial,
            } => {
                if *partial {
                    y = line(fb, y, "Partly added to your library", TypeRole::BodyBold, WARN);
                    y = y.saturating_add(SPACE_1);
                    y = line(
                        fb,
                        y,
                        &format!("{files} files arrived; some tracks failed"),
                        TypeRole::Small,
                        FG_DIM,
                    );
                } else if *files == 0 {
                    y = line(fb, y, "Already in your library", TypeRole::BodyBold, OK);
                    y = y.saturating_add(SPACE_1);
                    y = line(
                        fb,
                        y,
                        "Nothing was downloaded again.",
                        TypeRole::Small,
                        FG_DIM,
                    );
                } else {
                    y = line(fb, y, "Added to your library", TypeRole::BodyBold, OK);
                    y = y.saturating_add(SPACE_1);
                    let files_text = if *files == 1 {
                        "1 file added".to_string()
                    } else {
                        format!("{files} files added")
                    };
                    y = line(fb, y, &files_text, TypeRole::Small, FG_DIM);
                }
                if let Some(path) = noudata {
                    paragraph(fb, y, &screen_text(path), TypeRole::Small, FG_MUTED, 2);
                }
            }
            StoreTapPhase::Failed { message, .. } => {
                y = line(fb, y, "That didn't work", TypeRole::BodyBold, ERR);
                y = y.saturating_add(SPACE_1);
                paragraph(fb, y, &screen_text(message), TypeRole::Small, FG, 4);
            }
        }

        // Actions: fixed rows at the bottom, focused one highlighted like every other list.
        let focused = view
            .focused_action()
            .map(|_| view.focus.min(actions.len().saturating_sub(1)));
        for (i, action) in actions.iter().enumerate() {
            let label = store_action_text(*action, view);
            let value = store_action_value(*action, view);
            let row_y = action_top.saturating_add((i as u16).saturating_mul(ROW_H));
            widgets::data_row(
                fb,
                row_y,
                DataRow {
                    label: &label,
                    value: value.as_deref(),
                    indent: 0,
                    // No highlight on a row Enter will ignore: a disarmed screen (an Enter
                    // just changed the phase) and the Back-only transitional phases have no
                    // focused action until the person turns.
                    state: if focused == Some(i) {
                        widgets::RowState::Focused
                    } else {
                        widgets::RowState::Normal
                    },
                    striped: i % 2 == 1,
                    marker: false,
                },
            );
        }
    }

    // ---- BCS_08: stores, connect by QR, browse, release, remove ----

    /// Stores (BCS_08): the Bandcamp row, the connect row, and a status line that says
    /// exactly why the store cannot be used when it cannot.
    fn render_stores(&self, nav: &NavStack, fb: &mut FrameBuffer, y0: u16) {
        let Some(view) = self.store_stores.as_ref() else {
            widgets::message_state(fb, "No stores", "Open this from System or Library");
            return;
        };
        let actions = view.actions();
        let rows: Vec<(String, Option<String>, bool)> =
            actions.iter().map(|a| stores_row(*a, view)).collect();
        let bottom = store_body_bottom(rows.len());
        let mut y = y0;
        if let Some((blocked, message)) = view.blocked() {
            y = store_line(fb, y, blocked.title(), TypeRole::BodyBold, WARN);
            y = store_para(fb, y, bottom, message, TypeRole::Small, FG, 4);
            let _ = store_para(fb, y, bottom, blocked.hint(), TypeRole::Small, FG_MUTED, 2);
        } else {
            y = store_line(fb, y, "Bandcamp", TypeRole::BodyBold, FG);
            let colour = if view.connected() {
                OK
            } else if view.rejected() {
                WARN
            } else {
                FG_DIM
            };
            y = store_line(fb, y, &view.status_text(), TypeRole::Body, colour);
            y = y.saturating_add(SPACE_1);
            let hint = if view.connected() {
                "Browse your collection and download releases to this Plippa."
            } else {
                "Sign in on your phone: nothing is typed on this screen."
            };
            y = store_para(fb, y, bottom, hint, TypeRole::Small, FG_DIM, 3);
            if view.connected() {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |d| d.as_secs());
                y = store_line(
                    fb,
                    y,
                    &crate::store_browse::synced_text(view.synced_at, now),
                    TypeRole::Small,
                    FG_MUTED,
                );
            }
            if let Some(failure) = view.sync_failure() {
                y = store_para(fb, y, bottom, &screen_text(&failure), TypeRole::Small, WARN, 3);
            }
            if let Some(reason) = view.unavailable_reason() {
                let _ = store_para(
                    fb,
                    y,
                    bottom,
                    &format!("No secrets folder: {reason}"),
                    TypeRole::Small,
                    WARN,
                    2,
                );
            }
        }
        store_action_rows(fb, &rows, nav.focus);
    }

    /// Connect Bandcamp (BCS_08): the QR code of the portal address, the address as text,
    /// and one line telling the person what to do with their phone.
    fn render_store_connect(&self, nav: &NavStack, fb: &mut FrameBuffer, y0: u16) {
        let Some(view) = self.store_connect.as_ref() else {
            widgets::message_state(fb, "No address", "Open this from Stores");
            return;
        };
        let actions = view.actions();
        let rows: Vec<(String, Option<String>, bool)> =
            actions.iter().map(|a| connect_row(*a, view)).collect();
        let bottom = store_body_bottom(rows.len());
        let left = MARGIN_X;
        let width = content_width(fb);
        let mut y = y0;
        match &view.phase {
            ConnectPhase::Looking => {
                y = store_line(fb, y, "Finding this Plippa's address", TypeRole::BodyBold, FG);
                y = y.saturating_add(SPACE_1);
                let _ = store_para(
                    fb,
                    y,
                    bottom,
                    "Asking the service for an address your phone can reach.",
                    TypeRole::Small,
                    FG_DIM,
                    2,
                );
            }
            ConnectPhase::PortalOff => {
                y = store_line(fb, y, "Web portal is off", TypeRole::BodyBold, WARN);
                y = y.saturating_add(SPACE_1);
                let _ = store_para(
                    fb,
                    y,
                    bottom,
                    "Turn it on in System > Network, then refresh. Connecting Bandcamp needs the portal page on your phone.",
                    TypeRole::Small,
                    FG,
                    4,
                );
            }
            ConnectPhase::Unreachable { blocked, message } => {
                y = store_line(fb, y, blocked.title(), TypeRole::BodyBold, WARN);
                y = store_para(fb, y, bottom, message, TypeRole::Small, FG, 4);
                let _ = store_para(fb, y, bottom, blocked.hint(), TypeRole::Small, FG_MUTED, 2);
            }
            ConnectPhase::Ready(_) => match view.qr_url().map(store_qr) {
                Some(Some((matrix, module))) => {
                    y = store_line(fb, y, "Connect Bandcamp", TypeRole::BodyBold, FG);
                    y = y.saturating_add(SPACE_1);
                    let side = paint_store_qr(fb, left, y, &matrix, module);
                    let right_x = left.saturating_add(side).saturating_add(SPACE_2);
                    let right_w = (left.saturating_add(width)).saturating_sub(right_x);
                    let mut ry = y;
                    if right_w > 0 {
                        text::draw_clipped(
                            fb,
                            right_x,
                            ry,
                            right_w,
                            CONNECT_INSTRUCTION,
                            TypeRole::Small,
                            FG,
                        );
                        // The instruction can wrap to three lines in the narrow column.
                        for line in text::wrap(CONNECT_INSTRUCTION, right_w, TypeRole::Small)
                            .into_iter()
                            .skip(1)
                            .take(2)
                        {
                            ry = ry.saturating_add(cell_height(TypeRole::Small));
                            text::draw_clipped(
                                fb,
                                right_x,
                                ry,
                                right_w,
                                &line,
                                TypeRole::Small,
                                FG,
                            );
                        }
                        ry = ry.saturating_add(SPACE_1);
                        if let Some(address) = view.address_text() {
                            for line in
                                text::wrap(&address, right_w, TypeRole::SmallBold).into_iter().take(3)
                            {
                                ry = ry.saturating_add(cell_height(TypeRole::SmallBold));
                                if ry.saturating_add(cell_height(TypeRole::SmallBold)) > bottom {
                                    break;
                                }
                                text::draw_clipped(
                                    fb,
                                    right_x,
                                    ry,
                                    right_w,
                                    &line,
                                    TypeRole::SmallBold,
                                    ACCENT,
                                );
                            }
                        }
                        // SECFIX_03: the address is `http://`, so say what that means for the
                        // password about to be typed on the phone.
                        if let Some(warning) = view.clear_text_warning() {
                            ry = ry.saturating_add(SPACE_1);
                            for line in text::wrap(warning, right_w, TypeRole::Small)
                                .into_iter()
                                .take(3)
                            {
                                if ry.saturating_add(cell_height(TypeRole::Small)) > bottom {
                                    break;
                                }
                                text::draw_clipped(
                                    fb,
                                    right_x,
                                    ry,
                                    right_w,
                                    &line,
                                    TypeRole::Small,
                                    WARN,
                                );
                                ry = ry.saturating_add(cell_height(TypeRole::Small));
                            }
                        }
                    }
                }
                _ => {
                    y = store_line(fb, y, "No address yet", TypeRole::BodyBold, WARN);
                    y = y.saturating_add(SPACE_1);
                    let note = view.note().unwrap_or(
                        "No address another device can reach. Connect this Plippa to Wi-Fi.",
                    );
                    let _ = store_para(fb, y, bottom, note, TypeRole::Small, FG, 4);
                }
            },
        }
        store_action_rows(fb, &rows, nav.focus);
    }

    /// The Bandcamp collection (BCS_08): a windowed list, the paging row, and Back.
    fn render_store_releases(&self, nav: &NavStack, fb: &mut FrameBuffer, y0: u16) {
        let Some(view) = self.store_releases.as_ref() else {
            widgets::message_state(fb, "No collection", "Open this from Stores");
            return;
        };
        // The keyboard owns the body while a query is being typed.
        if view.searching {
            let prompt = if view.query.is_empty() {
                "Search your collection".to_string()
            } else {
                format!("Search: {}", clip_chars(&screen_text(&view.query), 26))
            };
            let mut y = store_line(fb, y0, &prompt, TypeRole::BodyBold, FG);
            y = y.saturating_add(SPACE_1);
            if let Some(kb) = self.store_search.as_ref() {
                self.render_accessory_keyboard(kb, fb, y);
            }
            return;
        }
        let rows: Vec<(String, Option<String>, bool)> = view
            .rows()
            .into_iter()
            .map(|row| self.releases_row(row, view))
            .collect();
        // A one-line heading keeps the count visible without stealing a row.
        let mut y = y0;
        if !view.releases.is_empty() {
            let loaded = view.releases.len();
            let scope = if view.query.is_empty() {
                String::new()
            } else {
                format!(" for \"{}\"", clip_chars(&screen_text(&view.query), 18))
            };
            let heading = if view.stale {
                format!("{loaded} of {} releases{scope} (list may be old)", view.total)
            } else {
                format!("{loaded} of {} releases{scope}", view.total)
            };
            y = store_line(fb, y, &heading, TypeRole::Small, FG_DIM);
        }
        if view.is_empty() {
            let (title, hint) = if view.query.is_empty() {
                (
                    "Nothing in your collection",
                    "Buy something on Bandcamp, then sync it on the portal.",
                )
            } else {
                ("Nothing matches that search", "Try fewer words, or search again.")
            };
            y = store_line(fb, y, title, TypeRole::BodyBold, FG);
            y = y.saturating_add(SPACE_1);
            let _ = store_para(
                fb,
                y,
                CONTENT_BOTTOM.saturating_sub(ROW_H),
                hint,
                TypeRole::Small,
                FG_DIM,
                3,
            );
        }
        if let ReleasesPhase::PageFailed { message, .. } = &view.phase {
            y = store_para(
                fb,
                y,
                CONTENT_BOTTOM.saturating_sub(ROW_H),
                message,
                TypeRole::Small,
                WARN,
                2,
            );
        } else if let ReleasesPhase::Blocked { blocked, message } = &view.phase {
            // Nothing loaded: the failure owns the body, the rows are Retry / Back.
            y = store_line(fb, y, blocked.title(), TypeRole::BodyBold, WARN);
            let _ = store_para(
                fb,
                y,
                store_body_bottom(rows.len()),
                message,
                TypeRole::Small,
                FG,
                4,
            );
        }
        if rows.is_empty() {
            return;
        }
        let len = rows.len();
        let visible = visible_rows(y);
        let focus = nav.focus.min(len - 1);
        let start = window_start(focus, len, visible);
        for i in 0..visible.min(len.saturating_sub(start)) {
            let abs = start + i;
            let (label, value, marker) = &rows[abs];
            widgets::data_row(
                fb,
                y.saturating_add((i as u16).saturating_mul(ROW_H)),
                DataRow {
                    label,
                    value: value.as_deref(),
                    indent: 0,
                    state: if abs == focus {
                        widgets::RowState::Focused
                    } else {
                        widgets::RowState::Normal
                    },
                    striped: abs % 2 == 1,
                    marker: *marker,
                },
            );
        }
    }

    /// One release (BCS_08): tracks, size, the lossy note, and the download's progress.
    fn render_store_release(&self, nav: &NavStack, fb: &mut FrameBuffer, y0: u16) {
        let Some(view) = self.store_release.as_ref() else {
            widgets::message_state(fb, "No release", "Open this from the collection");
            return;
        };
        let actions = view.actions();
        let rows: Vec<(String, Option<String>, bool)> = actions
            .iter()
            .map(|a| release_row(*a, view))
            .collect();
        let bottom = store_body_bottom(rows.len());
        let mut y = y0;
        match (&view.phase, &view.download) {
            (ReleasePhase::Loading, _) => {
                let _ = store_para(
                    fb,
                    y,
                    bottom,
                    "Looking up this release.",
                    TypeRole::Small,
                    FG_DIM,
                    2,
                );
            }
            (ReleasePhase::Blocked { blocked, message }, _) => {
                y = store_line(fb, y, blocked.title(), TypeRole::BodyBold, WARN);
                y = store_para(fb, y, bottom, message, TypeRole::Small, FG, 4);
                let _ = store_para(fb, y, bottom, blocked.hint(), TypeRole::Small, FG_MUTED, 2);
            }
            (ReleasePhase::Ready, download) => {
                if let Some(info) = view.info.as_ref() {
                    y = store_line(fb, y, &info.release.title, TypeRole::BodyBold, FG);
                    if !info.release.artist.is_empty() {
                        y = store_line(fb, y, &info.release.artist, TypeRole::Small, FG_DIM);
                    }
                    let mut facts = items_label(info.tracks.len() as u64);
                    if let Some(bytes) = info.declared_bytes {
                        facts.push_str(" - about ");
                        facts.push_str(&store_bytes_text(bytes));
                    }
                    y = store_line(fb, y, &facts, TypeRole::Small, FG_DIM);
                    // The first tracks: enough to recognise the release, never the whole list.
                    for (i, track) in info.tracks.iter().take(3).enumerate() {
                        let line = format!("{}. {}", i + 1, track.title);
                        y = store_line(fb, y, &line, TypeRole::Small, FG);
                    }
                    if info.tracks.len() > 3 {
                        y = store_line(
                            fb,
                            y,
                            &format!("and {} more tracks", info.tracks.len() - 3),
                            TypeRole::Small,
                            FG_MUTED,
                        );
                    }
                    if info.lossy {
                        let note = if info.quality_note.is_empty() {
                            STORE_LOSSY_NOTE
                        } else {
                            info.quality_note.as_str()
                        };
                        y = y.saturating_add(SPACE_1);
                        y = store_para(fb, y, bottom, note, TypeRole::Small, WARN, 3);
                    }
                    match download {
                        DownloadPhase::Confirm { consent } => {
                            let _ = consent;
                            let _ = store_para(
                                fb,
                                y,
                                bottom,
                                "Press Download to fetch it to this Plippa.",
                                TypeRole::Small,
                                FG,
                                2,
                            );
                        }
                        DownloadPhase::Running { progress, .. } => {
                            let _ = store_progress(fb, y.saturating_add(SPACE_1), bottom, progress);
                        }
                        DownloadPhase::Done { files, partial } => {
                            let text = if *partial {
                                format!("Partly added: {files} files, some tracks failed")
                            } else if *files == 0 {
                                "Already in your library. Nothing was downloaded again."
                                    .to_string()
                            } else if *files == 1 {
                                "Added to your library: 1 file".to_string()
                            } else {
                                format!("Added to your library: {files} files")
                            };
                            let _ = store_para(
                                fb,
                                y,
                                bottom,
                                &text,
                                TypeRole::Small,
                                if *partial { WARN } else { OK },
                                2,
                            );
                        }
                        DownloadPhase::Failed { message, .. } => {
                            y = store_line(fb, y, "That didn't work", TypeRole::SmallBold, ERR);
                            let _ = store_para(fb, y, bottom, message, TypeRole::Small, FG, 3);
                        }
                        DownloadPhase::Preparing | DownloadPhase::Idle => {}
                    }
                }
            }
        }
        store_action_rows(fb, &rows, nav.focus);
    }

    /// Remove from Plippa (BCS_08/BCS_10): what would go, why it cannot, and the confirmation.
    fn render_library_remove(&self, nav: &NavStack, fb: &mut FrameBuffer, y0: u16) {
        let Some(view) = self.store_delete.as_ref() else {
            widgets::message_state(fb, "Nothing to remove", "Open this from an item");
            return;
        };
        let actions = view.actions();
        let rows: Vec<(String, Option<String>, bool)> =
            actions.iter().map(|a| delete_row(*a, view)).collect();
        let bottom = store_body_bottom(rows.len());
        let mut y = y0;
        match &view.phase {
            DeletePhase::Loading => {
                let _ = store_para(
                    fb,
                    y,
                    bottom,
                    "Checking what would go.",
                    TypeRole::Small,
                    FG_DIM,
                    2,
                );
            }
            DeletePhase::Ready(preview) | DeletePhase::Refused { preview: Some(preview), .. } => {
                y = store_line(fb, y, &preview.title, TypeRole::BodyBold, FG);
                let facts = format!(
                    "{} files - {}",
                    preview.files,
                    store_bytes_text(preview.total_bytes)
                );
                y = store_line(fb, y, &facts, TypeRole::Small, FG_DIM);
                y = y.saturating_add(SPACE_1);
                if preview.blocked() {
                    y = store_line(fb, y, "Can't remove this yet", TypeRole::SmallBold, WARN);
                    for blocker in preview.blockers.iter().take(2) {
                        y = store_para(fb, y, bottom, blocker, TypeRole::Small, FG, 2);
                    }
                } else {
                    y = store_para(
                        fb,
                        y,
                        bottom,
                        "The files and its .noudata go. You can download it again from Bandcamp.",
                        TypeRole::Small,
                        FG,
                        3,
                    );
                    for warning in preview.warnings.iter().take(1) {
                        y = store_para(fb, y, bottom, warning, TypeRole::Small, FG_MUTED, 2);
                    }
                }
            }
            DeletePhase::Refused {
                blocked,
                message,
                preview: None,
            }
            | DeletePhase::Failed { blocked, message } => {
                y = store_line(fb, y, blocked.title(), TypeRole::BodyBold, WARN);
                y = store_para(fb, y, bottom, message, TypeRole::Small, FG, 4);
                let _ = store_para(fb, y, bottom, blocked.hint(), TypeRole::Small, FG_MUTED, 2);
            }
            DeletePhase::Deleting => {
                let _ = store_para(
                    fb,
                    y,
                    bottom,
                    "Removing the files from this Plippa.",
                    TypeRole::Small,
                    FG_DIM,
                    2,
                );
            }
            DeletePhase::Done(done) => {
                y = store_line(fb, y, "Removed from Plippa", TypeRole::BodyBold, OK);
                let text = if done.partial {
                    format!("{} files removed; some could not be", done.deleted)
                } else if done.deleted == 1 {
                    "1 file removed".to_string()
                } else {
                    format!("{} files removed", done.deleted)
                };
                y = store_line(fb, y, &text, TypeRole::Small, FG_DIM);
                let _ = store_line(
                    fb,
                    y,
                    &format!("Freed {}", store_bytes_text(done.freed_bytes)),
                    TypeRole::Small,
                    FG_MUTED,
                );
            }
        }
        store_action_rows(fb, &rows, nav.focus);
    }

    /// One row of the collection list: title, and either the year / track count or the
    /// "on device" marker for a release this session downloaded.
    fn releases_row(&self, row: ReleasesRow, view: &ReleasesView) -> (String, Option<String>, bool) {
        match row {
            ReleasesRow::Release(index) => {
                let Some(release) = view.releases.get(index) else {
                    return ("Release".to_string(), None, false);
                };
                let title = screen_text(&release.title);
                let downloaded = self
                    .store_items
                    .iter()
                    .any(|item| item.release_id == release.id);
                if downloaded {
                    return (title, Some("on device".to_string()), true);
                }
                let mut facts = String::new();
                if let Some(year) = release.year {
                    facts.push_str(&year.to_string());
                }
                if let Some(tracks) = release.track_count {
                    if !facts.is_empty() {
                        facts.push_str(" - ");
                    }
                    facts.push_str(&items_label(u64::from(tracks)));
                }
                let value = (!facts.is_empty()).then_some(facts);
                (title, value, false)
            }
            ReleasesRow::More => {
                let loaded = view.releases.len() as u32;
                (
                    "More releases".to_string(),
                    Some(format!("{loaded} of {}", view.total)),
                    false,
                )
            }
            ReleasesRow::Retry => ("Try again".to_string(), None, false),
            ReleasesRow::Search => (
                "Search".to_string(),
                (!view.query.is_empty()).then(|| clip_chars(&screen_text(&view.query), 16)),
                false,
            ),
            ReleasesRow::Back => ("Back".to_string(), None, false),
        }
    }
}

/// The lossy note the download screens always carry: the purchase is lossless, the stream is
/// not, and the person must be told before the bytes move.
pub const STORE_LOSSY_NOTE: &str =
    "Bandcamp sends a compressed stream, not the purchased FLAC.";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compose::{SCREEN_H, SCREEN_W};
    use crate::ui::tokens::{ACCENT, BG, NP_HEADER_H, ROW_H_DISPLAY};

    /// A pixel that carries foreground ink rather than a painted ground.
    ///
    /// UI_11 rows paint `BG` / `SURFACE` / `SURFACE_ALT` across their whole width and the
    /// scrollbar track is `BORDER_SOFT`, so a "non-zero pixel" assertion would pass on a blank
    /// screen (the ground is non-zero too). Count only real ink: text, accents, focus ring,
    /// warning and the scrollbar thumb (`BORDER`).
    fn ink(p: u16) -> bool {
        !matches!(p, 0 | BG | BG_ALT | SURFACE | SURFACE_ALT | BORDER_SOFT)
    }

    #[test]
    fn wrap_splits_long_portal_url() {
        // 320×240 body with 8 px margins → 304 px = 50 cells at the `Small` advance.
        let max_w = 304u16;
        let url = "http://192.168.1.42:8787/very/long/path?token=abcdef";
        let lines = text::wrap(url, max_w, TypeRole::Small);
        assert!(lines.len() > 1, "expected wrap, got {lines:?}");
        assert_eq!(lines.concat(), url);
        for line in &lines {
            assert!(
                text::measure(line, TypeRole::Small) <= max_w,
                "line {line:?} wider than {max_w}"
            );
        }
    }

    #[test]
    fn wrap_keeps_short_ipv4() {
        let lines = text::wrap("192.168.1.42", 304, TypeRole::Small);
        assert_eq!(lines, vec!["192.168.1.42".to_string()]);
    }

    #[test]
    fn main_menu_overlay_paints_three_labels() {
        let model = ScreenModel::default();
        let nav = NavStack::new();
        assert_eq!(nav.menu_items().len(), 3);
        let mut fb = FrameBuffer::new(SCREEN_W, SCREEN_H);
        fb.clear(0);
        model.render_main_menu_overlay(&nav, &mut fb);
        // One `Body` row at y = SPACE_2; the focus underline sits 3 px under the cell.
        let line_h = cell_height(TypeRole::Body) as usize;
        let row: usize = fb.pixels[SPACE_2 as usize * SCREEN_W as usize
            ..(SPACE_2 as usize + line_h) * SCREEN_W as usize]
            .iter()
            .filter(|&&p| p != 0)
            .count();
        assert!(row > 0, "expected the three menu labels on one row");
        // §4: the focused item carries a **2 px** underline, not a hairline.
        let label = nav.menu_items()[0].title();
        let w = measure(label, TypeRole::Body) as usize;
        let underline = (SPACE_2 + line_h as u16 + 3) as usize;
        for y in [underline, underline + 1] {
            for x in MARGIN_X as usize..MARGIN_X as usize + w {
                assert_eq!(
                    fb.pixels[y * SCREEN_W as usize + x],
                    ACCENT,
                    "underline row {y} at x = {x}"
                );
            }
        }
        // Nothing painted below the underline of that single row.
        let below = ((SPACE_2 + line_h as u16 + 5) as usize * SCREEN_W as usize).min(fb.pixels.len());
        assert!(fb.pixels[below..].iter().all(|&p| p == 0));
    }

    #[test]
    fn shell_header_title_is_fg() {
        let m = ScreenModel::default();
        let mut nav = NavStack::new();
        nav.force_push(Route::Warnings);
        let fb = render(&m, &nav);
        let header_ink: Vec<u16> = (0..HEADER_H)
            .flat_map(|y| (0..SCREEN_W as u16).map(move |x| (x, y)))
            .map(|(x, y)| fb.pixels[y as usize * SCREEN_W as usize + x as usize])
            .filter(|p| *p != SURFACE)
            .collect();
        assert!(header_ink.len() > 10, "title painted in the header band");
        assert!(
            header_ink.iter().all(|p| *p == FG),
            "ordinary screens use an FG title, never ACCENT"
        );
    }

    fn peer_prompt(name: Option<&str>) -> PairPrompt {
        PairPrompt::Confirm {
            peer: plippa_hw::Peer {
                name: name.map(str::to_string),
                address: "58:8A:D2:D8:22:7F".into(),
            },
            passkey: 123_456,
        }
    }

    /// Ink painted in the rows `y0..y1` of a frame, as (count, colours seen).
    fn band_ink(fb: &FrameBuffer, y0: u16, y1: u16) -> (usize, Vec<u16>) {
        let mut count = 0;
        let mut colours = Vec::new();
        for y in y0..y1 {
            for x in 0..SCREEN_W as u16 {
                let p = fb.pixels[y as usize * SCREEN_W as usize + x as usize];
                if ink(p) {
                    count += 1;
                    if !colours.contains(&p) {
                        colours.push(p);
                    }
                }
            }
        }
        (count, colours)
    }

    #[test]
    fn a_pairing_prompt_paints_the_peer_name_above_its_address() {
        let m = ScreenModel::default();
        let mut fb = FrameBuffer::new(SCREEN_W, SCREEN_H);
        fb.clear(BG);
        let y = m.render_prompt_peer(&mut fb, SPACE_2, &peer_prompt(Some("Headphones")));
        let name_h = cell_height(TypeRole::BodyBold);
        let addr_y = SPACE_2 + name_h + SPACE_1;
        let addr_h = cell_height(TypeRole::Small);

        let (name_ink, name_colours) = band_ink(&fb, SPACE_2, SPACE_2 + name_h);
        assert!(name_ink > 10, "the name is painted");
        assert_eq!(name_colours, vec![FG], "the name is the bright line");

        let (addr_ink, addr_colours) = band_ink(&fb, addr_y, addr_y + addr_h);
        assert!(addr_ink > 10, "the address is painted under the name");
        assert_eq!(addr_colours, vec![FG_DIM], "the address is the dimmer identity line");

        assert_eq!(y, addr_y + addr_h + SPACE_1 + SPACE_1, "the block reports its height");
    }

    #[test]
    fn a_prompt_with_no_name_shows_the_address_not_just_a_number() {
        let m = ScreenModel::default();
        let mut fb = FrameBuffer::new(SCREEN_W, SCREEN_H);
        fb.clear(BG);
        m.render_prompt_peer(&mut fb, SPACE_2, &peer_prompt(None));
        let addr_h = cell_height(TypeRole::Small);
        let (addr_ink, addr_colours) = band_ink(&fb, SPACE_2, SPACE_2 + addr_h);
        assert!(addr_ink > 10, "the address alone names the peer");
        assert_eq!(addr_colours, vec![FG_DIM]);
    }

    #[test]
    fn a_hostile_peer_name_paints_only_sanitised_glyphs() {
        let hostile = "Ev\u{202E}il\u{200B}\r\n\u{001B}[2Jx";
        let m = ScreenModel::default();
        let mut fb = FrameBuffer::new(SCREEN_W, SCREEN_H);
        fb.clear(BG);
        m.render_prompt_peer(&mut fb, SPACE_2, &peer_prompt(Some(hostile)));
        let name_h = cell_height(TypeRole::BodyBold);
        let (name_ink, name_colours) = band_ink(&fb, SPACE_2, SPACE_2 + name_h);
        assert!(name_ink > 10);
        assert_eq!(name_colours, vec![FG]);
    }

    // ---- WAVE_02: waveform geometry, painting and states ----

    fn wave_model(peaks: Vec<u8>, bucket_ms: u16, duration_ms: u64, playhead_ms: u64) -> ScreenModel {
        use plippa_core::SessionState;
        let mut session = SessionState::default();
        session.transport = TransportState::Playing;
        session.title = "Tone".into();
        session.artist = "Artist".into();
        session.position_ms = playhead_ms;
        session.duration_ms = Some(duration_ms);
        session.waveform_path = Some("/tmp/x.wfm".into());
        let mut model = ScreenModel::default();
        model.apply_playback(PlaybackStatus::from_session(session));
        model.wave = WaveView {
            state: WaveState::Ready,
            path: Some("/tmp/x.wfm".into()),
            peaks,
            bucket_ms,
            duration_ms,
        };
        model.playhead_ms = playhead_ms;
        model
    }

    fn render_wave(m: &ScreenModel) -> FrameBuffer {
        let mut fb = FrameBuffer::new(SCREEN_W, SCREEN_H);
        fb.clear(BG);
        m.render_main(&mut fb);
        fb
    }

    #[test]
    fn wave_geometry_maps_time_to_columns() {
        // The band is a 30 s window: playhead - 15 s at the left edge, the playhead at 160.
        let w = crate::play_mode::ZOOM_DEFAULT_MS;
        assert_eq!(wave_x_for_time(0, 15_000, SCREEN_W, w), 0);
        assert_eq!(wave_x_for_time(15_000, 15_000, SCREEN_W, w), 160);
        assert_eq!(wave_x_for_time(29_999, 15_000, SCREEN_W, w), 319);

        // Fractional columns: 320 px over 30 s is 93.75 ms, so spans alternate 93/94 ms,
        // tile the window with no gap, and the column under the playhead starts exactly on
        // it (93 ms columns would start the centre 120 ms early and lose 240 ms of span).
        let p = 123_456;
        assert_eq!(wave_column_span(WAVE_PLAYHEAD_X, p, SCREEN_W, w), (p, p + 93));
        assert_eq!(wave_column_span(0, p, SCREEN_W, w).0, p - 15_000);
        assert_eq!(wave_column_span(SCREEN_W - 1, p, SCREEN_W, w).1, p + 15_000);
        for x in 0..SCREEN_W - 1 {
            assert_eq!(
                wave_column_span(x, p, SCREEN_W, w).1,
                wave_column_span(x + 1, p, SCREEN_W, w).0,
                "column {x} must end where {} starts",
                x + 1
            );
        }

        // The column under the playhead covers the playhead's own bucket: a 20 ms bucket at
        // the playhead is found there and in neither neighbour.
        let bucket = 20i64;
        let p = 123_440; // on a bucket boundary
        let mut column_peaks = vec![0u8; 7_000];
        column_peaks[(p / bucket) as usize] = 255;
        assert_eq!(
            wave_column_peak(&column_peaks, bucket, p, WAVE_PLAYHEAD_X, SCREEN_W, w, 180_000),
            Some(255)
        );
        for x in [WAVE_PLAYHEAD_X - 1, WAVE_PLAYHEAD_X + 1] {
            assert_eq!(
                wave_column_peak(&column_peaks, bucket, p, x, SCREEN_W, w, 180_000),
                Some(0),
                "column {x} must not see the playhead's bucket"
            );
        }

        // AUDIO_05: the app subtracts the visual offset before assigning `model.playhead_ms`,
        // so a 300 ms offset moves the column that was at 10 000 ms to 9 700 ms (the fixed
        // centre column then spans the earlier media time).
        let offset_ms = 300u64;
        let playhead = 10_000i64;
        let shifted = playhead - offset_ms as i64;
        assert!(
            wave_x_for_time(playhead, shifted, SCREEN_W, w)
                > wave_x_for_time(playhead, playhead, SCREEN_W, w),
            "the same media time (10 000 ms) sits right of centre when the picture is late"
        );
        assert_eq!(
            wave_column_span(WAVE_PLAYHEAD_X, shifted, SCREEN_W, w).0,
            playhead - offset_ms as i64,
            "the centre column maps to 9 700 ms, not 10 000 ms"
        );

        let peaks = vec![255u8; 1_000];
        // Nothing before 0:00.
        assert_eq!(wave_column_peak(&peaks, 20, 5_000, 0, SCREEN_W, w, 20_000), None);
        assert_eq!(wave_column_peak(&peaks, 20, 5_000, 160, SCREEN_W, w, 20_000), Some(255));
        // Nothing past the decoded end (no invented bars).
        assert_eq!(wave_column_peak(&peaks, 20, 10_000, 319, SCREEN_W, w, 20_000), None);
        // The bucket range uses `t1 - 1`: a boundary bucket belongs to exactly one column.
        // playhead 100 → column 160 covers t = 100..193 → buckets 5..9, never bucket 10.
        let peaks = [0, 0, 0, 0, 0, 10, 20, 30, 40, 50, 255];
        assert_eq!(wave_column_peak(&peaks, 20, 100, 160, SCREEN_W, w, 60_000), Some(50));
    }

    #[test]
    fn wave_column_span_honours_window() {
        // The band is full width (320 px): one column spans `window/320` ms, and the centre
        // column always starts exactly at the playhead whatever the zoom.
        for (window, span, approx) in [(8_000u64, 25i64, "26 ms"), (600_000, 1_875, "1 974 ms")] {
            let p = 123_456;
            let (t0, t1) = wave_column_span(WAVE_PLAYHEAD_X, p, SCREEN_W, window);
            assert_eq!(t0, p, "centre column starts at the playhead");
            assert_eq!(t1 - t0, span, "one column at {approx}");
            assert_eq!(
                wave_column_span(WAVE_PLAYHEAD_X + 1, p, SCREEN_W, window).0,
                t1,
                "columns tile the window"
            );
        }
    }

    #[test]
    fn wave_column_max_spans_buckets_and_skips_silence() {
        let peaks = [0, 200, 50, 0];
        let w = crate::play_mode::ZOOM_DEFAULT_MS;
        // t = 0..93 covers buckets 0..=3: the max wins.
        assert_eq!(wave_column_peak(&peaks, 20, 15_000, 0, SCREEN_W, w, 60_000), Some(200));
        // A silent bucket draws no bar.
        assert_eq!(wave_column_peak(&[0, 0, 0, 0], 20, 15_000, 0, SCREEN_W, w, 60_000), Some(0));
        assert_eq!(wave_height(0, 90), 0);
        // Degenerate inputs are safe (untrusted file values never index out of range).
        assert_eq!(wave_column_peak(&[], 20, 0, 0, SCREEN_W, w, 1_000), None);
        assert_eq!(wave_column_peak(&[1], 0, 0, 0, SCREEN_W, w, 1_000), None);
        // An empty bucket in range → no bar for that column (peaks[0..=4] are all silent).
        let sparse = [0u8, 0, 0, 0, 0, 255];
        assert_eq!(wave_column_peak(&sparse, 20, 15_000, 0, SCREEN_W, w, 60_000), Some(0));
    }

    #[test]
    fn wave_display_curve_is_gamma_with_a_one_pixel_floor() {
        let half = WAVE_BAND_H / 2;
        assert_eq!(wave_height(0, half), 0, "silence is flat");
        assert_eq!(wave_height(255, half), half, "full scale reaches the band edge");
        assert_eq!(wave_height(1, half), 1, "any non-silent bucket is visible");
        let mut last = 0;
        for p in 0..=255u16 {
            let h = wave_height(p as u8, half);
            assert!(h >= last, "curve dropped at {p}");
            last = h;
        }
        // Gamma 1.5: half scale is visibly below half height (a linear map would be half).
        assert!(wave_height(128, half) < half / 2);
    }

    #[test]
    fn waveform_paints_bars_under_a_fixed_playhead() {
        // One loud bucket at 5 s with the playhead at 15 s: column 53.
        let mut peaks = vec![0u8; 1_500];
        peaks[250] = 255;
        let fb = render_wave(&wave_model(peaks, 20, 30_000, 15_000));
        let px = |x: u16, y: u16| fb.pixels[y as usize * SCREEN_W as usize + x as usize];
        let half = WAVE_BAND_H / 2;
        let top = WAVE_CENTRE_Y - half;
        // Mirrored around the centre line, and already played → the half-dim tint.
        assert_eq!(px(53, top), ACCENT_PLAYED);
        assert_eq!(px(53, WAVE_CENTRE_Y + half - 2), ACCENT_PLAYED);
        assert_eq!(px(52, top), BG);
        assert_eq!(px(54, top), BG);
        // Nothing above the band.
        assert_eq!(px(53, WAVE_BAND_Y - 1), BG);

        // The same bar while it is still upcoming is the accent colour: at playhead 2 s the
        // bar is 3 s to the right, i.e. column (5000 - 2000 + 15 000) / 93.75 = 192.
        let mut peaks = vec![0u8; 1_500];
        peaks[250] = 255;
        let fb = render_wave(&wave_model(peaks, 20, 30_000, 2_000));
        assert_eq!(
            fb.pixels[top as usize * SCREEN_W as usize + 192],
            ACCENT,
            "upcoming bars use ACCENT"
        );

        // Empty band: only the centre line and the fixed playhead.
        let fb = render_wave(&wave_model(vec![0u8; 1_500], 20, 30_000, 15_000));
        assert_eq!(fb.pixels[WAVE_CENTRE_Y as usize * SCREEN_W as usize + 100], BORDER_SOFT);
        for x in [WAVE_PLAYHEAD_X, WAVE_PLAYHEAD_X + 1] {
            assert_eq!(
                fb.pixels[(WAVE_BAND_Y + 5) as usize * SCREEN_W as usize + x as usize],
                FG,
                "2 px playhead at x = {x}"
            );
        }
        assert_eq!(
            fb.pixels[(WAVE_BAND_Y + 5) as usize * SCREEN_W as usize + (WAVE_PLAYHEAD_X + 2) as usize],
            BG
        );
    }

    #[test]
    fn waveform_played_bars_use_accent_played() {
        // One loud bucket at 5 s, playhead 15 s: column 53 is already played. The bar is
        // half as dim as the old ACCENT_DIM on both sides of the centre mirror.
        let mut peaks = vec![0u8; 1_500];
        peaks[250] = 255;
        let fb = render_wave(&wave_model(peaks, 20, 30_000, 15_000));
        let px = |x: u16, y: u16| fb.pixels[y as usize * SCREEN_W as usize + x as usize];
        let half = WAVE_BAND_H / 2;
        let top = WAVE_CENTRE_Y - half;
        let bottom = WAVE_CENTRE_Y + half - 1;
        assert_eq!(px(53, top), ACCENT_PLAYED);
        assert_eq!(px(53, bottom), ACCENT_PLAYED);
        assert_ne!(px(53, top), ACCENT_DIM, "the old, twice-too-dark tint");
        // The upcoming side stays full ACCENT (playhead 2 s puts the bar at column 192).
        let mut peaks = vec![0u8; 1_500];
        peaks[250] = 255;
        let fb = render_wave(&wave_model(peaks, 20, 30_000, 2_000));
        assert_eq!(fb.pixels[top as usize * SCREEN_W as usize + 192], ACCENT);
    }

    #[test]
    fn now_playing_geometry_recovers_thirty_pixels() {
        // UI_14 §3: every value derived from the two-row header; no literal screen rows.
        assert_eq!(NP_HEADER_H, 2 * ROW_H_DISPLAY);
        assert_eq!(NP_CONTENT_TOP, NP_HEADER_H + RULE_H + SPACE_1);
        assert_eq!(NP_CONTENT_BOTTOM, SCREEN_H - NP_FOOTER_H);
        assert_eq!((NP_HEADER_H, NP_CONTENT_TOP, NP_CONTENT_BOTTOM), (60, 65, 210));
        // The waveform band grew 115 -> 145 px and its centre line moved with it.
        assert_eq!(WAVE_BAND_Y, NP_CONTENT_TOP);
        assert_eq!(WAVE_BAND_H, NP_CONTENT_BOTTOM - NP_CONTENT_TOP);
        assert_eq!(WAVE_CENTRE_Y, WAVE_BAND_Y + WAVE_BAND_H / 2);
        assert_eq!((WAVE_BAND_H, WAVE_CENTRE_Y), (145, 137));
        // The Adjust band and the footer did not move.
        assert_eq!(ADJUST_BAND_H, ROW_H_DISPLAY);
        assert_eq!(ADJUST_BAND_Y, NP_CONTENT_BOTTOM - ADJUST_BAND_H - SPACE_1);
        assert_eq!((ADJUST_BAND_H, ADJUST_BAND_Y), (30, 176));
        // The Up Next overlay starts after the second header row and now shows six rows.
        assert_eq!(UP_NEXT_TOP, NP_CONTENT_TOP + SPACE_1);
        assert_eq!(
            UP_NEXT_ROWS_Y,
            UP_NEXT_TOP + SPACE_1 + ROW_H + SPACE_1
        );
        assert_eq!((UP_NEXT_TOP, UP_NEXT_ROWS_Y), (69, 95));
        assert_eq!(UP_NEXT_ROWS, (NP_CONTENT_BOTTOM - UP_NEXT_ROWS_Y) / ROW_H);
        assert_eq!(UP_NEXT_ROWS, 6);
        // Header text baselines: a 20 px `Display` cell centred in each 30 px row.
        let inset = (ROW_H_DISPLAY - cell_height(TypeRole::Display)) / 2;
        assert_eq!(inset, 5);
        assert_eq!(inset + ROW_H_DISPLAY, 35, "row 1 starts at y = 35");
    }

    #[test]
    fn footer_centre_reflects_the_wave_state() {
        let mut m = wave_model(vec![1], 20, 30_000, 0);
        assert_eq!(m.footer_centre(), "", "pure playback has an empty centre");
        assert_eq!(m.footer_right(), "", "100 % forward shows no speed");
        // UI_14 §7: the default unit is 75 fps, so both footer clocks carry frames.
        assert_eq!(m.footer_left(), "00:00:00 / 00:30:00");
        m.playhead_ms = 15_000;
        assert_eq!(m.footer_left(), "00:15:00 / 00:30:00");
        m.wave.state = WaveState::Pending;
        assert_eq!(m.footer_centre(), "ANALYSING");
        m.wave.state = WaveState::Failed;
        assert_eq!(m.footer_centre(), "", "a failed sidecar is silent");
        // No path at all: silent even in Pending.
        m.wave.state = WaveState::Pending;
        m.wave.path = None;
        assert_eq!(m.footer_centre(), "");
        // The speed wins the right slot and suppresses ANALYSING; there is no `SET` prefix.
        m.wave.state = WaveState::Pending;
        m.wave.path = Some("/tmp/x.wfm".into());
        m.rate_override = Some(1_500);
        assert_eq!(m.footer_right(), "150% corr");
        assert_eq!(m.footer_centre(), "", "speed wins the centre slot");
        m.rate_override = Some(-750);
        assert_eq!(m.footer_right(), "-75% rev");
        assert_eq!(m.footer_centre(), "");
        // No session: every slot is empty except the position placeholder.
        m.rate_override = None;
        m.playback = None;
        assert_eq!(m.footer_centre(), "");
        assert_eq!(m.footer_right(), "");
        assert_eq!(m.footer_left(), "00:00:00 / --:--");
    }

    #[test]
    fn footer_clocks_follow_the_timecode_unit_live() {
        // UI_14 §7 / UI_16 §4: a frame rate gives `MM:SS:FF`, decimal tenths `MM:SS.t`,
        // decimal hundredths `MM:SS.tt`, and the model reads the resolved unit on every call
        // (no restart).
        let mut m = wave_model(vec![1], 20, 30_000, 0);
        m.playhead_ms = 65_432;
        assert_eq!(m.footer_left(), "01:05:32 / 00:30:00", "75 fps default");
        m.play_mode.timecode_unit = crate::play_mode::TimecodeUnit::Frames24;
        assert_eq!(m.footer_left(), "01:05:10 / 00:30:00", "24 fps");
        m.play_mode.timecode_unit = crate::play_mode::TimecodeUnit::DecTenths;
        assert_eq!(m.footer_left(), "01:05.4 / 00:30.0", "tenths");
        m.play_mode.timecode_unit = crate::play_mode::TimecodeUnit::DecHundredths;
        assert_eq!(m.footer_left(), "01:05.43 / 00:30.00", "hundredths");
        // Off: both clocks go back to the plain `MM:SS` form.
        m.play_mode.timecode_unit = crate::play_mode::TimecodeUnit::Off;
        assert_eq!(m.footer_left(), "1:05 / 0:30", "off is plain MM:SS");
        assert_eq!(m.footer_centre(), "", "no session sidecar: centre empty");
        assert_eq!(m.footer_right(), "");
        // Frame maths: 75 fps maps 999 ms to frame 74 (never 75), 24 fps to 23.
        assert_eq!(format_clock(999, crate::play_mode::TimecodeUnit::Frames75), "00:00:74");
        assert_eq!(format_clock(999, crate::play_mode::TimecodeUnit::Frames24), "00:00:23");
        assert_eq!(format_clock(999, crate::play_mode::TimecodeUnit::Off), "0:00");
        assert_eq!(format_clock(1_999, crate::play_mode::TimecodeUnit::Frames60), "00:01:59");
        assert_eq!(format_clock(999, crate::play_mode::TimecodeUnit::DecTenths), "00:00.9");
        // Hundredths truncate (999 ms -> .99, never 1.00) and keep two digits.
        assert_eq!(format_clock(999, crate::play_mode::TimecodeUnit::DecHundredths), "00:00.99");
        assert_eq!(format_clock(1_050, crate::play_mode::TimecodeUnit::DecHundredths), "00:01.05");
    }

    #[test]
    fn now_playing_header_shows_title_artist_and_count() {
        let mut m = wave_model(vec![0], 20, 30_000, 0);
        {
            let s = &mut m.playback.as_mut().unwrap().session;
            s.title = "Title".into();
            s.artist = "Artist".into();
            s.collection_title = "Album".into();
        }
        m.now_playing = Some(NowPlayingListChanged {
            entries: (0..3)
                .map(|i| NowPlayingEntry {
                    kind: NowPlayingEntryKind::Track,
                    title: if i == 2 { "Title" } else { "Other" }.into(),
                    artist: if i == 2 { "Artist" } else { "Someone" }.into(),
                    start_ms: 0,
                    end_ms: None,
                    duration_ms: Some(1_000),
                    track_index: i,
                    playable_id: None,
                    available: true,
                    item_position: i,
                })
                .collect(),
            current_index: 2,
            list_kind: plippa_core::NowPlayingListKind::Album,
            list_id: 1,
        });
        assert_eq!(
            m.np_header_text(),
            (
                "Title".to_string(),
                "Artist".to_string(),
                "Album".to_string(),
                "3/3".to_string()
            )
        );
        let fb = render_wave(&m);
        let px = |x: u16, y: u16| fb.pixels[y as usize * SCREEN_W as usize + x as usize];
        let ink_rows = |y0: u16, y1: u16| {
            (y0..y1)
                .flat_map(|y| (0..SCREEN_W).map(move |x| (x, y)))
                .filter(|&(x, y)| px(x, y) == FG || px(x, y) == FG_DIM || px(x, y) == ACCENT)
                .count()
        };
        assert!(ink_rows(5, 25) > 20, "row 0: title + artist");
        assert!(ink_rows(35, 55) > 20, "row 1: release + count");
        // Row 0 is the only `ACCENT` line; the artist and row 1 are `FG_DIM`.
        let has = |y0: u16, y1: u16, c: u16| {
            (y0..y1)
                .flat_map(|y| (0..SCREEN_W).map(move |x| (x, y)))
                .any(|(x, y)| px(x, y) == c)
        };
        assert!(has(5, 25, ACCENT), "title is accent");
        assert!(has(5, 25, FG_DIM), "artist on the same row, not accent");
        assert!(!has(35, 55, ACCENT), "release/count are not accent");
        assert_eq!(px(100, NP_HEADER_H), ACCENT_DIM);
        assert_eq!(px(100, NP_HEADER_H - 1), SURFACE, "rule is a hairline");

        // The single-track case has no count and no collection: row 1 stays blank.
        m.now_playing = None;
        m.playback.as_mut().unwrap().session.collection_title = String::new();
        let fb = render_wave(&m);
        let dim = |y0: u16, y1: u16| {
            (y0..y1)
                .flat_map(|y| (0..SCREEN_W).map(move |x| (x, y)))
                .filter(|&(x, y)| fb.pixels[y as usize * SCREEN_W as usize + x as usize] == FG_DIM)
                .count()
        };
        assert!(dim(5, 25) > 20, "artist on row 0");
        assert_eq!(dim(35, 55), 0, "a single track is not an album");
    }

    #[test]
    fn np_header_reads_the_playing_entry_not_the_record() {
        // UI_14 §1 regression: a playlist whose record title is the playlist name ("Tool")
        // and whose current track is "Parabol" used to show `Tool` as the title and no
        // artist. The playing entry must win; the record is only the fallback.
        let mut m = wave_model(vec![0], 20, 30_000, 0);
        {
            let s = &mut m.playback.as_mut().unwrap().session;
            s.title = "Tool".into();
            s.artist = String::new(); // a playlist record has no artist
            s.collection_title = "Tool".into();
        }
        let entry = |title: &str, artist: &str, kind| NowPlayingEntry {
            kind,
            title: title.into(),
            artist: artist.into(),
            start_ms: 0,
            end_ms: None,
            duration_ms: Some(60_000),
            track_index: 0,
            playable_id: None,
            available: true,
            item_position: 0,
        };
        m.now_playing = Some(NowPlayingListChanged {
            entries: vec![
                entry("Parabol", "Tool", NowPlayingEntryKind::Track),
                entry("Parabola", "Tool", NowPlayingEntryKind::Track),
            ],
            current_index: 0,
            list_kind: NowPlayingListKind::Playlist,
            list_id: 1,
        });
        assert_eq!(
            m.np_header_text(),
            (
                "Parabol".to_string(),
                "Tool".to_string(),
                "Tool".to_string(),
                "1/2".to_string()
            ),
            "title and artist come from the playing entry; release stays the collection"
        );

        // Album-style: the record title is the album (`Lateralus`), the track is `Parabol`.
        {
            let s = &mut m.playback.as_mut().unwrap().session;
            s.title = "Lateralus".into();
            s.artist = String::new();
            s.collection_title = "Lateralus".into();
        }
        m.now_playing.as_mut().unwrap().list_kind = NowPlayingListKind::Album;
        assert_eq!(
            m.np_header_text(),
            (
                "Parabol".to_string(),
                "Tool".to_string(),
                "Lateralus".to_string(),
                "1/2".to_string()
            )
        );

        // No list / no entry: the record text is the fallback (nothing regresses).
        m.now_playing = None;
        assert_eq!(
            m.np_header_text(),
            (
                "Lateralus".to_string(),
                String::new(),
                "Lateralus".to_string(),
                String::new()
            )
        );
    }

    #[test]
    fn cue_keeps_the_track_artist() {
        // UI_14 §1 follow-up: cue entries are built with an empty artist, so a highlighted
        // cue used to blank the header's artist slot as the playhead crossed a cue start.
        // The cue keeps its own title but borrows the owning track's artist.
        let mut m = wave_model(vec![0], 20, 180_000, 30_000);
        {
            let s = &mut m.playback.as_mut().unwrap().session;
            s.title = "Lateralus".into();
            s.artist = String::new(); // an album record carries no artist here
            s.collection_title = "Lateralus".into();
            s.current_cue_index = Some(0);
        }
        let entry = |title: &str, artist: &str, kind| NowPlayingEntry {
            kind,
            title: title.into(),
            artist: artist.into(),
            start_ms: 0,
            end_ms: None,
            duration_ms: Some(60_000),
            track_index: 0,
            playable_id: None,
            available: true,
            item_position: 0,
        };
        m.now_playing = Some(NowPlayingListChanged {
            entries: vec![
                entry("Parabola", "Tool", NowPlayingEntryKind::Track),
                entry("Intro", "", NowPlayingEntryKind::Cue),
                entry("Schism", "Tool", NowPlayingEntryKind::Track),
            ],
            current_index: 0,
            list_kind: NowPlayingListKind::Album,
            list_id: 1,
        });
        assert_eq!(
            m.current_entry().map(|e| e.kind),
            Some(NowPlayingEntryKind::Cue),
            "the playhead is inside the cue"
        );
        assert_eq!(
            m.np_header_text(),
            (
                "Intro".to_string(),
                "Tool".to_string(),
                "Lateralus".to_string(),
                "1/2".to_string()
            ),
            "the cue title stays; the artist comes from the owning track"
        );

        // The header actually paints that artist: with an empty one row 0 would have no
        // `FG_DIM` ink at all (the title is `ACCENT`, the release row is below).
        let fb = render_wave(&m);
        let artist_ink = (5..25)
            .flat_map(|y| (0..SCREEN_W).map(move |x| (x, y)))
            .filter(|&(x, y)| fb.pixels[y as usize * SCREEN_W as usize + x as usize] == FG_DIM)
            .count();
        assert!(artist_ink > 20, "artist slot painted, got {artist_ink} px");

        // Track entry unavailable / artist-less: fall back to the record's artist, then "".
        m.now_playing.as_mut().unwrap().entries[0].artist = String::new();
        m.playback.as_mut().unwrap().session.artist = "Session Artist".into();
        assert_eq!(m.np_header_text().1, "Session Artist");
        m.playback.as_mut().unwrap().session.artist = String::new();
        assert_eq!(m.np_header_text().1, "");
    }

    #[test]
    fn up_next_header_names_only_playlists() {
        // UI_14 §4: `UP NEXT - <PLAYLIST NAME>` for playlists; plain `UP NEXT` otherwise.
        let mut m = wave_model(vec![0], 20, 30_000, 0);
        let track = NowPlayingEntry {
            kind: NowPlayingEntryKind::Track,
            title: "One".into(),
            artist: "A".into(),
            start_ms: 0,
            end_ms: None,
            duration_ms: Some(60_000),
            track_index: 0,
            playable_id: None,
            available: true,
            item_position: 0,
        };
        m.now_playing = Some(NowPlayingListChanged {
            entries: vec![track],
            current_index: 0,
            list_kind: NowPlayingListKind::Playlist,
            list_id: 1,
        });
        assert_eq!(m.up_next_title(), "UP NEXT", "empty collection title");
        m.playback.as_mut().unwrap().session.collection_title = "Road trip".into();
        assert_eq!(m.up_next_title(), "UP NEXT - Road trip");
        for kind in [
            NowPlayingListKind::Album,
            NowPlayingListKind::Mix,
            NowPlayingListKind::Audiobook,
        ] {
            m.now_playing.as_mut().unwrap().list_kind = kind;
            assert_eq!(m.up_next_title(), "UP NEXT", "{kind:?} keeps the plain text");
        }
        // SEC-12: the playlist name is file-derived, so hostile characters are stripped.
        m.now_playing.as_mut().unwrap().list_kind = NowPlayingListKind::Playlist;
        m.playback.as_mut().unwrap().session.collection_title = "Road\u{0007} trip".into();
        assert_eq!(m.up_next_title(), "UP NEXT - Road trip");
        // A single kept name cannot outgrow the screen: `screen_text` bounds it.
        m.playback.as_mut().unwrap().session.collection_title = "x".repeat(10_000);
        assert!(m.up_next_title().chars().count() <= SCREEN_TEXT_MAX_CHARS + 10);
        m.playback = None;
        assert_eq!(m.up_next_title(), "UP NEXT");
    }

    #[test]
    fn up_next_overlay_clips_a_long_playlist_name_against_the_count() {
        // UI_14 §4: the count is right-aligned and measured first; a long playlist name is
        // clipped into the pixels left over and can never overpaint the count. `up_next_title`
        // itself is tested above — this is the layout of the overlay header.
        let mut m = wave_model(vec![0], 20, 180_000, 0);
        m.playback.as_mut().unwrap().session.collection_title =
            "A very long playlist name that cannot possibly fit beside the count".into();
        m.now_playing = Some(NowPlayingListChanged {
            entries: (0..12)
                .map(|i| NowPlayingEntry {
                    kind: NowPlayingEntryKind::Track,
                    title: format!("T{i}"),
                    artist: "A".into(),
                    start_ms: 0,
                    end_ms: None,
                    duration_ms: Some(60_000),
                    track_index: i,
                    playable_id: None,
                    available: true,
                    item_position: i,
                })
                .collect(),
            current_index: 0,
            list_kind: NowPlayingListKind::Playlist,
            list_id: 1,
        });
        m.np_focus.target = NpTarget::List;
        assert!(m.up_next_visible());

        let fb = render_wave(&m);
        let band = UP_NEXT_HEADER_Y..UP_NEXT_HEADER_Y + cell_height(TypeRole::Small);
        let cols = |colour: u16| -> Vec<u16> {
            (0..SCREEN_W)
                .filter(|&x| {
                    band.clone()
                        .any(|y| fb.pixels[y as usize * SCREEN_W as usize + x as usize] == colour)
                })
                .collect()
        };
        let title = cols(FG); // `SmallBold` playlist name
        let count = cols(FG_DIM); // `Small` `1/12`
        let count_x = SCREEN_W - MARGIN_X - measure("1/12", TypeRole::Small);
        assert!(!title.is_empty(), "the playlist name is painted");
        assert!(!count.is_empty(), "the count is painted");
        assert!(
            *title.last().unwrap() < count_x,
            "title ink at {} must stay left of the count at {count_x}",
            title.last().unwrap()
        );
        assert!(
            count.iter().all(|x| *x >= count_x),
            "the count must stay in its right-aligned box"
        );
        // The name is truncated visibly (`..`) rather than merely short: it fills the space
        // it is allowed, one glyph short of the count's box.
        assert!(
            *title.last().unwrap()
                >= count_x - SPACE_2 - text::advance(TypeRole::SmallBold),
            "the long name stops early: {:?}",
            title.last()
        );

        // The count box is identical to a render with a short (non-colliding) name: the
        // count is painted last and nothing else reaches it.
        let mut short = m.clone();
        short.playback.as_mut().unwrap().session.collection_title = "Short".into();
        let fb_short = render_wave(&short);
        for y in band {
            for x in count_x..SCREEN_W - MARGIN_X {
                assert_eq!(
                    fb.pixels[y as usize * SCREEN_W as usize + x as usize],
                    fb_short.pixels[y as usize * SCREEN_W as usize + x as usize],
                    "count box differs at ({x}, {y})"
                );
            }
        }
    }

    #[test]
    fn composed_collection_line_with_a_playing_mix_reaches_the_screen_intact() {
        // POLISH_02: the engine publishes "Collection (Mix)" (<= 120 chars); the release line
        // and the Up Next header show it whole, and a long one still clips against the count.
        use plippa_core::{compose_collection_title, LoadedKind};
        let entry = |i: u32| NowPlayingEntry {
            kind: NowPlayingEntryKind::Track,
            title: format!("T{i}"),
            artist: "A".into(),
            start_ms: 0,
            end_ms: None,
            duration_ms: Some(60_000),
            track_index: i,
            playable_id: None,
            available: true,
            item_position: i,
        };
        let build = |collection: &str| {
            let mut m = wave_model(vec![0], 20, 180_000, 0);
            m.playback.as_mut().unwrap().session.collection_title = compose_collection_title(
                collection,
                LoadedKind::Collection,
                Some("Live In New York"),
            );
            m.now_playing = Some(NowPlayingListChanged {
                entries: (0..12).map(entry).collect(),
                current_index: 0,
                list_kind: NowPlayingListKind::Playlist,
                list_id: 1,
            });
            m.np_focus.target = NpTarget::List;
            m
        };

        let m = build("Band Name Bangers");
        assert_eq!(m.np_header_text().2, "Band Name Bangers (Live In New York)");
        assert_eq!(m.up_next_title(), "UP NEXT - Band Name Bangers (Live In New York)");

        let long = build(&"Very long playlist ".repeat(12));
        let line = long.playback.as_ref().unwrap().session.collection_title.clone();
        assert_eq!(long.up_next_title(), format!("UP NEXT - {line}"), "the suffix survives screen_text");
        assert!(line.ends_with("… (Live In New York)"), "{line}");
        assert!(long.up_next_visible());
        let fb = render_wave(&long);
        let band = UP_NEXT_HEADER_Y..UP_NEXT_HEADER_Y + cell_height(TypeRole::Small);
        let count_x = SCREEN_W - MARGIN_X - measure("1/12", TypeRole::Small);
        let ink_right = (0..SCREEN_W)
            .filter(|&x| {
                band.clone()
                    .any(|y| fb.pixels[y as usize * SCREEN_W as usize + x as usize] == FG)
            })
            .max()
            .expect("the header name is painted");
        assert!(ink_right < count_x, "title ink at {ink_right} must stay left of the count at {count_x}");
    }

    #[test]
    fn np_header_count_counts_tracks_not_cues() {
        let mut entries = Vec::new();
        for i in 0..3 {
            entries.push(NowPlayingEntry {
                kind: NowPlayingEntryKind::Track,
                title: format!("T{i}"),
                artist: String::new(),
                start_ms: 0,
                end_ms: None,
                duration_ms: Some(60_000),
                track_index: i,
                playable_id: None,
                available: true,
                item_position: i,
            });
            if i == 1 {
                for c in 0..2 {
                    entries.push(NowPlayingEntry {
                        kind: NowPlayingEntryKind::Cue,
                        title: format!("C{c}"),
                        artist: String::new(),
                        start_ms: 10_000 * (c + 1),
                        end_ms: Some(10_000 * (c + 2)),
                        duration_ms: None,
                        track_index: i,
                        playable_id: None,
                        available: true,
                        item_position: i,
                    });
                }
            }
        }
        let mut m = wave_model(vec![0], 20, 30_000, 0);
        m.now_playing = Some(NowPlayingListChanged {
            entries,
            current_index: 1,
            list_kind: plippa_core::NowPlayingListKind::Album,
            list_id: 1,
        });
        assert_eq!(m.np_track_ordinal(), Some((2, 3)), "cues are not tracks");
        // A single track and an absent list both have no count.
        m.now_playing
            .as_mut()
            .unwrap()
            .entries
            .retain(|e| e.kind == NowPlayingEntryKind::Cue || e.track_index == 1);
        m.now_playing.as_mut().unwrap().current_index = 0;
        assert_eq!(m.np_track_ordinal(), None);
        m.now_playing = None;
        assert_eq!(m.np_track_ordinal(), None);
    }


    #[test]
    fn library_detail_paints_title_artist_length_beside_art() {
        let mut model = ScreenModel::default();
        model.library = vec![LibraryItem {
            id: 1,
            title: "Tone".into(),
            associated: false,
            noudata_relpath: "a.noudata".into(),
            library_root: None,
            artist: Some("Artist".into()),
            duration_ms: Some(185_000), // 3:05
            art_path: None,
            item_position: None,
        }];
        model.detail_playable_id = Some(1);
        let mut nav = NavStack::new();
        nav.force_push(Route::LibraryDetail);

        let mut fb = FrameBuffer::new(SCREEN_W, SCREEN_H);
        fb.clear(0);
        model.render_screen(&nav, &mut fb);

        // Art at x = MARGIN_X..MARGIN_X+72; text starts SPACE_2 right of it.
        let text_x = (MARGIN_X + 72 + SPACE_2) as usize;
        let band = |y0: u16, h: u16| {
            let y0 = y0 as usize;
            let y1 = (y0 + h as usize).min(SCREEN_H as usize);
            let mut n = 0usize;
            for y in y0..y1 {
                for x in text_x..SCREEN_W as usize {
                    if ink(fb.pixels[y * SCREEN_W as usize + x]) {
                        n += 1;
                    }
                }
            }
            n
        };
        assert!(
            band(CONTENT_TOP, cell_height(TypeRole::Display)) > 10,
            "expected title beside art"
        );
        assert!(
            band(CONTENT_TOP + ROW_H, cell_height(TypeRole::Small)) > 10,
            "expected artist beside art"
        );
        assert!(
            band(CONTENT_TOP + ROW_H * 2, cell_height(TypeRole::Small)) > 10,
            "expected length beside art"
        );
    }

    // ---- NB_12 ----

    fn item(id: i64, title: &str) -> LibraryItem {
        LibraryItem {
            id,
            title: title.into(),
            associated: false,
            noudata_relpath: format!("{id}.noudata"),
            library_root: None,
            artist: None,
            duration_ms: None,
            art_path: None,
            item_position: None,
        }
    }

    /// One-track engine list, enough for the playing-kind / playing-track markers.
    fn now_playing_list(kind: NowPlayingListKind, track_index: u32) -> NowPlayingListChanged {
        NowPlayingListChanged {
            entries: vec![NowPlayingEntry {
                kind: NowPlayingEntryKind::Track,
                title: "T".into(),
                artist: String::new(),
                start_ms: 0,
                end_ms: None,
                duration_ms: Some(1_000),
                track_index,
                playable_id: None,
                available: true,
                item_position: track_index,
            }],
            current_index: 0,
            list_kind: kind,
            list_id: 1,
        }
    }

    fn render(model: &ScreenModel, nav: &NavStack) -> FrameBuffer {
        let mut fb = FrameBuffer::new(SCREEN_W, SCREEN_H);
        fb.clear(0);
        model.render_screen(nav, &mut fb);
        fb
    }

    /// ACCENT pixels painted on Now Playing row 1 (the release / badges / count row).
    fn np_row1_accent(fb: &FrameBuffer) -> Vec<u16> {
        (NP_HEADER_H - ROW_H_DISPLAY..NP_HEADER_H)
            .flat_map(|y| (0..SCREEN_W).map(move |x| (x, y)))
            .filter(|&(x, y)| fb.pixels[y as usize * SCREEN_W as usize + x as usize] == ACCENT)
            .map(|(x, _)| x)
            .collect()
    }

    #[test]
    fn now_playing_header_shows_the_engine_play_mode_badges() {
        // PLAY_05: the badges are the engine's session state, never a local flag, and they live
        // left of the count. Nothing is drawn at the default rung.
        let nav = NavStack::new();
        let mut m = wave_model(vec![0], 20, 30_000, 0);
        m.now_playing = Some(NowPlayingListChanged {
            entries: (0..3)
                .map(|i| NowPlayingEntry {
                    kind: NowPlayingEntryKind::Track,
                    title: "Track".into(),
                    artist: "Artist".into(),
                    start_ms: 0,
                    end_ms: None,
                    duration_ms: Some(1_000),
                    track_index: i,
                    playable_id: None,
                    available: true,
                    item_position: i,
                })
                .collect(),
            current_index: 2,
            list_kind: plippa_core::NowPlayingListKind::Album,
            list_id: 1,
        });
        let badges = |m: &ScreenModel| {
            let b = m.np_mode_badges();
            (b.shuffle, b.repeat, b.repeat_one)
        };
        assert_eq!(badges(&m), (false, false, false));

        let plain = render(&m, &nav);
        assert!(np_row1_accent(&plain).is_empty(), "no badge at Normal/Off");

        // Count `3/3` is 30 px from the right margin (ends at x = 312) and `SPACE_2` separates
        // it from the badge group. Shuffle alone is one 16 px icon, so it occupies 258..274.
        m.order_mode = plippa_core::OrderMode::Shuffle;
        let shuffled = render(&m, &nav);
        let shuffle_ink = np_row1_accent(&shuffled);
        assert!(!shuffle_ink.is_empty(), "Shuffle draws the shuffle icon");
        assert!(
            shuffle_ink.iter().all(|x| (258..274).contains(x)),
            "the shuffle badge sits left of the count: {shuffle_ink:?}"
        );

        m.repeat_mode = plippa_core::RepeatMode::Queue;
        let queue = render(&m, &nav);
        let queue_ink = np_row1_accent(&queue);
        assert!(
            queue_ink.len() > shuffle_ink.len(),
            "Repeat queue adds the repeat icon"
        );
        assert_eq!(badges(&m), (true, true, false));

        // Repeat one: same two icons plus the `1`, so more ink again and the group is wider.
        m.repeat_mode = plippa_core::RepeatMode::Item;
        let item = render(&m, &nav);
        let item_ink = np_row1_accent(&item);
        assert!(item_ink.len() > queue_ink.len(), "Repeat one adds its 1");
        assert_eq!(badges(&m), (true, true, true));

        // Nothing set: back to a bare header.
        m.order_mode = plippa_core::OrderMode::Normal;
        m.repeat_mode = plippa_core::RepeatMode::Off;
        assert!(np_row1_accent(&render(&m, &nav)).is_empty());
    }

    /// Lit pixels of row `r` of a drawn list starting at [`CONTENT_TOP`] (step [`ROW_H`]).
    fn row_lit(fb: &FrameBuffer, r: usize) -> usize {
        let y0 = (CONTENT_TOP as usize) + r * ROW_H as usize;
        let y1 = (y0 + ROW_H as usize).min(SCREEN_H as usize);
        (y0..y1)
            .flat_map(|y| (0..SCREEN_W as usize).map(move |x| (x, y)))
            .filter(|&(x, y)| ink(fb.pixels[y * SCREEN_W as usize + x]))
            .count()
    }

    /// Same as [`row_lit`], ignoring the 2 px scrollbar gutter at the right margin so a
    /// scrollbar is never mistaken for row content.
    fn row_lit_no_gutter(fb: &FrameBuffer, r: usize) -> usize {
        let y0 = (CONTENT_TOP as usize) + r * ROW_H as usize;
        let y1 = (y0 + ROW_H as usize).min(SCREEN_H as usize);
        (y0..y1)
            .flat_map(|y| (0..SCREEN_W as usize - 2).map(move |x| (x, y)))
            .filter(|&(x, y)| ink(fb.pixels[y * SCREEN_W as usize + x]))
            .count()
    }

    fn lit_in(fb: &FrameBuffer, y0: u16, h: u16) -> usize {
        let y0 = y0 as usize;
        let y1 = (y0 + h as usize).min(SCREEN_H as usize);
        (y0..y1)
            .flat_map(|y| (0..SCREEN_W as usize).map(move |x| (x, y)))
            .filter(|&(x, y)| ink(fb.pixels[y * SCREEN_W as usize + x]))
            .count()
    }

    fn px(fb: &FrameBuffer, x: u16, y: u16) -> u16 {
        fb.pixels[y as usize * SCREEN_W as usize + x as usize]
    }

    /// `message_state` is the only empty state: the body must be exactly what the widget paints.
    fn assert_message_state(fb: &FrameBuffer, title: &str, hint: &str) {
        let mut control = FrameBuffer::new(SCREEN_W, SCREEN_H);
        control.clear(0);
        widgets::message_state(&mut control, title, hint);
        let from = CONTENT_TOP as usize * SCREEN_W as usize;
        let to = CONTENT_BOTTOM as usize * SCREEN_W as usize;
        assert_eq!(
            fb.pixels[from..to],
            control.pixels[from..to],
            "body is not the {title:?}/{hint:?} message state"
        );
    }

    /// The value column is right-aligned and identical to a control render of the value alone,
    /// so it neither wraps nor overlaps the label. `striped` picks the row's ground.
    fn assert_value_matches(fb: &FrameBuffer, y: u16, striped: bool, value: &str) {
        let bg = if striped { SURFACE_ALT } else { BG };
        let mut control = FrameBuffer::new(SCREEN_W, SCREEN_H);
        control.clear(bg);
        text::draw_aligned(
            &mut control,
            MARGIN_X,
            y.saturating_add((ROW_H.saturating_sub(cell_height(TypeRole::Small))) / 2),
            SCREEN_W.saturating_sub(MARGIN_X.saturating_mul(2)),
            value,
            TypeRole::Small,
            FG_DIM,
            Align::Right,
        );
        let x0 = SCREEN_W - MARGIN_X - measure(value, TypeRole::Small);
        let mut inked = 0;
        for yy in y..y + ROW_H {
            for x in x0..SCREEN_W - MARGIN_X {
                assert_eq!(
                    px(fb, x, yy),
                    px(&control, x, yy),
                    "value column pixel ({x},{yy}) for {value:?}"
                );
                if px(&control, x, yy) != bg {
                    inked += 1;
                }
            }
        }
        assert!(inked > 0, "value {value:?} painted nothing");
    }

    #[test]
    fn root_lists_all_six_categories_in_hierarchy_order_even_when_empty() {
        let mut m = ScreenModel::default();
        let all = vec![
            LibraryKind::Track,
            LibraryKind::Album,
            LibraryKind::Playlist,
            LibraryKind::Podcast,
            LibraryKind::Audiobook,
            LibraryKind::Mix,
        ];
        assert_eq!(m.library_root_kinds(), all, "empty library");
        m.kind_counts = vec![(LibraryKind::Album, 3), (LibraryKind::Mix, 2)];
        assert_eq!(m.library_root_kinds(), all, "with content");
        assert_eq!(kind_label(LibraryKind::Mix), "Mixes");
    }

    #[test]
    fn root_without_storage_shows_message_state_and_no_rows() {
        let mut m = ScreenModel::default();
        m.storage_present = false;
        let mut nav = NavStack::new();
        nav.force_push(Route::Library);
        let fb = render(&m, &nav);
        // The body is the shared empty state, not a row painted at CONTENT_TOP.
        assert_message_state(&fb, "No media storage", "Insert a Noubin stick");
        assert_eq!(row_lit(&fb, 0), 0, "a category row was painted without storage");
    }

    #[test]
    fn category_list_windows_around_focus() {
        let mut m = ScreenModel::default();
        m.browse_kind = Some(LibraryKind::Album);
        m.library = (0..200).map(|i| item(i, &format!("Album {i:03}"))).collect();
        let mut nav = NavStack::new();
        nav.force_push(Route::LibraryCategory);
        nav.focus = 150;
        let fb = render(&m, &nav);
        // (CONTENT_BOTTOM - CONTENT_TOP) / ROW_H = 10 rows, focus row is the last one painted.
        for r in 0..10 {
            assert!(row_lit_no_gutter(&fb, r) > 20, "row {r}");
        }
        // Row 10 starts past CONTENT_BOTTOM: only the scrollbar gutter may still be lit there.
        assert_eq!(row_lit_no_gutter(&fb, 10), 0, "an eleventh row was painted");
    }

    #[test]
    fn detail_actions_follow_the_loaded_item() {
        let mut m = ScreenModel::default();
        m.library = vec![item(1, "A")];
        m.detail_playable_id = Some(1);
        assert_eq!(
            m.detail_actions(),
            vec![LibraryDetailAction::Play, LibraryDetailAction::Associate]
        );
        let track = |p| TrackRow {
            position: p,
            title: format!("T{p}"),
            artist: None,
            duration_ms: None,
            available: true,
            supported: true,
            format: String::new(),
        };
        m.detail_info = Some(DetailInfo {
            playable_id: 1,
            kind: Some(LibraryKind::Album),
            tracks: vec![track(0), track(1)],
            chapters: vec![],
            media_missing: false,
        });
        m.browse_kind = Some(LibraryKind::Album);
        assert_eq!(
            m.detail_actions(),
            vec![
                LibraryDetailAction::Play,
                LibraryDetailAction::BrowseTracks,
                LibraryDetailAction::Associate
            ]
        );
        m.detail_info.as_mut().unwrap().chapters = vec![ChapterRow {
            item_position: 0,
            start_ms: 0,
            title: "c".into(),
        }];
        assert_eq!(m.detail_actions().len(), 4);
        // Stale info of another item is ignored.
        m.detail_playable_id = Some(2);
        assert_eq!(m.detail_actions().len(), 2);
    }

    #[test]
    fn detail_paints_counts_row_and_all_actions() {
        let mut m = ScreenModel::default();
        let mut it = item(1, "Long book");
        it.duration_ms = Some(3_600_000);
        m.library = vec![it];
        m.detail_playable_id = Some(1);
        m.browse_kind = Some(LibraryKind::Audiobook);
        m.detail_info = Some(DetailInfo {
            playable_id: 1,
            kind: Some(LibraryKind::Audiobook),
            tracks: vec![],
            chapters: vec![
                ChapterRow { item_position: 0, start_ms: 0, title: "One".into() },
                ChapterRow { item_position: 0, start_ms: 9_000, title: "Two".into() },
            ],
            media_missing: true,
        });
        let mut nav = NavStack::new();
        nav.force_push(Route::LibraryDetail);
        let fb = render(&m, &nav);
        let lit = |y0: u16, h: u16| {
            let y0 = y0 as usize;
            let y1 = (y0 + h as usize).min(SCREEN_H as usize);
            (y0..y1)
                .flat_map(|y| (0..SCREEN_W as usize).map(move |x| (x, y)))
                .filter(|&(x, y)| ink(fb.pixels[y * SCREEN_W as usize + x]))
                .count()
        };
        // Art 72 px from CONTENT_TOP; INFO_GAP below it; info row then actions.
        let info_y = CONTENT_TOP + 72 + INFO_GAP;
        let action_y = info_y + ROW_H;
        assert_eq!(info_y, CONTENT_TOP + 82);
        assert_eq!(action_y, CONTENT_TOP + 100);
        assert!(lit(info_y, ROW_H) > 50, "\"2 chapters  Media file missing\" row");
        for i in 0..3u16 {
            assert!(
                lit(action_y + i * ROW_H, ROW_H) > 20,
                "action row at {}",
                action_y + i * ROW_H
            );
        }
        assert_eq!(lit(action_y + 3 * ROW_H, ROW_H), 0, "only three actions");
        // Focus is the ring: an action is never "playing", so no `ACCENT_2` marker.
        let marker = (action_y as usize..(action_y + ROW_H) as usize)
            .flat_map(|y| (0..SCREEN_W as usize).map(move |x| (x, y)))
            .any(|(x, y)| fb.pixels[y * SCREEN_W as usize + x] == ACCENT_2);
        assert!(!marker, "focused action must not use the playing marker");
        // The missing-media line is painted in the warning colour.
        let warn = (info_y as usize..(info_y + ROW_H) as usize)
            .flat_map(|y| (0..SCREEN_W as usize).map(move |x| (x, y)))
            .any(|(x, y)| fb.pixels[y * SCREEN_W as usize + x] == WARN);
        assert!(warn);
    }

    #[test]
    fn tracks_and_chapters_screens_paint_rows_and_empty_states() {
        let mut m = ScreenModel::default();
        m.library = vec![item(1, "A")];
        m.detail_playable_id = Some(1);
        let mut nav = NavStack::new();
        nav.force_push(Route::LibraryTracks);
        // Empty states are the shared centred message, not a row at CONTENT_TOP.
        assert_message_state(&render(&m, &nav), "No tracks", "This item has no tracks");
        nav.force_push(Route::LibraryChapters);
        assert_message_state(&render(&m, &nav), "No chapters", "This item has no cues");

        m.detail_info = Some(DetailInfo {
            playable_id: 1,
            kind: None,
            tracks: vec![
                TrackRow { position: 0, title: "One".into(), artist: None, duration_ms: None, available: true, supported: true, format: String::new() },
                TrackRow { position: 1, title: "Gone".into(), artist: None, duration_ms: None, available: false, supported: true, format: String::new() },
            ],
            chapters: vec![ChapterRow { item_position: 0, start_ms: 65_000, title: "Ch".into() }],
            media_missing: true,
        });
        nav.force_push(Route::LibraryChapters);
        let fb = render(&m, &nav);
        assert!(row_lit(&fb, 0) > 20, "chapter row");
        assert_eq!(row_lit(&fb, 1), 0, "one chapter only");
        nav.force_push(Route::LibraryTracks);
        let fb = render(&m, &nav);
        assert!(row_lit(&fb, 0) > 20 && row_lit(&fb, 1) > 20);
        // The unavailable row is muted, not a warning; its `!` signal lives in the value
        // column next to the `--:--` duration.
        let muted = (CONTENT_TOP + ROW_H..CONTENT_TOP + ROW_H * 2)
            .flat_map(|y| (0..SCREEN_W as usize).map(move |x| (x, y)))
            .any(|(x, y)| px(&fb, x as u16, y as u16) == FG_MUTED);
        assert!(muted, "unavailable row uses FG_MUTED");
        assert_value_matches(&fb, CONTENT_TOP + ROW_H, true, "--:-- !");
    }

    #[test]
    fn library_root_rows_and_counts() {
        let mut m = ScreenModel::default();
        m.kind_counts = vec![(LibraryKind::Album, 3), (LibraryKind::Mix, 2)];
        m.now_playing = Some(now_playing_list(NowPlayingListKind::Album, 0));
        let mut nav = NavStack::new();
        nav.force_push(Route::Library);
        nav.focus = 0; // Tracks focused; the other rows are compared.
        let fb = render(&m, &nav);
        // One row per kind, count right-aligned (root order: Track, Album, Playlist,
        // Podcast, Audiobook, Mix).
        let expected = ["0", "3", "0", "0", "0", "2"];
        for i in 1..expected.len() {
            let y = CONTENT_TOP + i as u16 * ROW_H;
            assert_value_matches(&fb, y, i % 2 == 1, expected[i]);
        }
        // Zero-count kinds are muted; populated ones keep `FG`.
        let label_has = |row: u16, colour: u16| {
            (row..row + ROW_H)
                .flat_map(|y| (MARGIN_X..SCREEN_W / 2).map(move |x| (x, y)))
                .any(|(x, y)| px(&fb, x, y) == colour)
        };
        assert!(label_has(CONTENT_TOP, FG_MUTED), "zero-count label muted");
        assert!(label_has(CONTENT_TOP + ROW_H, FG), "populated label is FG");
        // The playing category (Album) carries the marker; Tracks does not.
        let marker = |row: u16| {
            (row..row + ROW_H)
                .flat_map(|y| (MARGIN_X..MARGIN_X + 8).map(move |x| (x, y)))
                .any(|(x, y)| px(&fb, x, y) == ACCENT_2)
        };
        assert!(marker(CONTENT_TOP + ROW_H), "playing category marker");
        assert!(!marker(CONTENT_TOP), "no marker on a non-playing category");
    }

    #[test]
    fn focused_zero_count_root_row_keeps_muted_label_and_ring() {
        let mut m = ScreenModel::default();
        m.kind_counts = vec![(LibraryKind::Album, 3)];
        let mut nav = NavStack::new();
        nav.force_push(Route::Library);
        nav.focus = 3; // Podcast: present but empty.
        let fb = render(&m, &nav);
        let y = CONTENT_TOP + 3 * ROW_H;
        // 2 px ACCENT ring around the focused row (left and right verticals, top and bottom).
        for x in [SPACE_1, SCREEN_W - SPACE_1 - 1] {
            for dy in [0, 1, ROW_H - 2, ROW_H - 1] {
                assert_eq!(px(&fb, x, y + dy), ACCENT, "ring at ({x},{})", y + dy);
            }
        }
        // The empty category keeps its muted label: focus is the ring, not a colour change.
        let label_has = |colour: u16| {
            (y..y + ROW_H)
                .flat_map(|yy| (MARGIN_X..SCREEN_W / 2).map(move |x| (x, yy)))
                .any(|(x, yy)| px(&fb, x, yy) == colour)
        };
        assert!(label_has(FG_MUTED), "zero-count label stays muted when focused");
        assert!(!label_has(FG), "focus must not brighten an empty category");
    }

    #[test]
    fn empty_root_kind_list_is_a_message_state() {
        let m = ScreenModel::default();
        let nav = NavStack::new();
        let mut fb = FrameBuffer::new(SCREEN_W, SCREEN_H);
        fb.clear(0);
        m.render_library_root_kinds(&nav, &mut fb, CONTENT_TOP, &[]);
        // "Nothing here" is always the shared message state, never a blank body.
        assert_message_state(&fb, "No categories", "Nothing to browse");
        assert_eq!(row_lit(&fb, 0), 0, "no category row painted");
    }

    #[test]
    fn library_list_scrollbar_only_when_overflowing() {
        let mut m = ScreenModel::default();
        m.browse_kind = Some(LibraryKind::Album);
        let mut nav = NavStack::new();
        nav.force_push(Route::LibraryCategory);
        let gutter_has_track = |fb: &FrameBuffer| {
            (CONTENT_TOP..CONTENT_BOTTOM).any(|y| {
                (SCREEN_W - 2..SCREEN_W).any(|x| matches!(px(fb, x, y), BORDER | BORDER_SOFT))
            })
        };

        // Three rows fit: no scrollbar anywhere in the right margin.
        m.library = (0..3).map(|i| item(i, &format!("Album {i}"))).collect();
        let fb = render(&m, &nav);
        assert!(!gutter_has_track(&fb), "short list must not paint a scrollbar");

        // 200 rows overflow: a scrollbar appears and its thumb follows the focus.
        m.library = (0..200).map(|i| item(i, &format!("Album {i:03}"))).collect();
        let thumb_top = |fb: &FrameBuffer| {
            (CONTENT_TOP..CONTENT_BOTTOM).find(|&y| px(fb, SCREEN_W - 2, y) == BORDER)
        };
        nav.focus = 0;
        let top_focus = render(&m, &nav);
        assert!(gutter_has_track(&top_focus));
        assert_eq!(thumb_top(&top_focus), Some(CONTENT_TOP), "thumb at the top");
        // Focus 0 shows the first row with a ring.
        assert_eq!(px(&top_focus, SPACE_1, CONTENT_TOP), ACCENT);

        nav.focus = 150;
        let deep_focus = render(&m, &nav);
        let deep_top = thumb_top(&deep_focus).expect("scrollbar thumb");
        assert!(
            deep_top > CONTENT_TOP,
            "thumb must move with the focus: {deep_top}"
        );
        // The focused row is the last painted one and is ringed.
        let focus_row = CONTENT_TOP + 9 * ROW_H;
        assert_eq!(px(&deep_focus, SPACE_1, focus_row), ACCENT);
    }

    #[test]
    fn library_tracks_value_is_duration() {
        use plippa_noubin::encode_playable_id;
        let root = std::path::Path::new("/media/noubin");
        let mut m = ScreenModel::default();
        let mut it = item(1, "A");
        it.library_root = Some(root.to_path_buf());
        m.library = vec![it];
        m.detail_playable_id = Some(1);
        m.detail_info = Some(DetailInfo {
            playable_id: 1,
            kind: Some(LibraryKind::Album),
            tracks: vec![
                TrackRow { position: 0, title: "One".into(), artist: None, duration_ms: Some(65_000), available: true, supported: true, format: String::new() },
                TrackRow { position: 1, title: "Gone".into(), artist: None, duration_ms: None, available: false, supported: true, format: String::new() },
                TrackRow { position: 2, title: "Two".into(), artist: None, duration_ms: Some(5_000), available: true, supported: true, format: String::new() },
            ],
            chapters: vec![],
            media_missing: false,
        });
        m.now_playing = Some(now_playing_tracks(
            NowPlayingListKind::Album,
            Some(plippa_core::PlayableItemId::new(encode_playable_id(root, 1))),
            &["One", "Gone", "Two"],
            &[Some(65_000), None, Some(5_000)],
            0,
        ));
        let mut nav = NavStack::new();
        nav.force_push(Route::LibraryTracks);
        nav.focus = 2;
        let fb = render(&m, &nav);
        assert_value_matches(&fb, CONTENT_TOP, false, "1:05");
        assert_value_matches(&fb, CONTENT_TOP + ROW_H, true, "--:-- !");
        // Available row label is `FG`; unavailable row is muted.
        let label_has = |row: u16, colour: u16| {
            (row..row + ROW_H)
                .flat_map(|y| (MARGIN_X..SCREEN_W / 2).map(move |x| (x, y)))
                .any(|(x, y)| px(&fb, x, y) == colour)
        };
        assert!(label_has(CONTENT_TOP, FG));
        assert!(label_has(CONTENT_TOP + ROW_H, FG_MUTED));
        // Currently playing track (position 0) is marked.
        assert_eq!(marked_rows(&fb, 3), vec![0], "playing track marker");
    }

    #[test]
    fn an_unplayable_track_is_muted_without_the_missing_marker() {
        let mut m = ScreenModel::default();
        m.library = vec![item(1, "A")];
        m.detail_playable_id = Some(1);
        let row = |position: u32, available: bool, supported: bool| TrackRow {
            position,
            title: format!("T{position}"),
            artist: None,
            duration_ms: Some(65_000),
            available,
            supported,
            format: "M4A/AAC".into(),
        };
        m.detail_info = Some(DetailInfo {
            playable_id: 1,
            kind: None,
            // playable, unplayable here, missing
            tracks: vec![row(0, true, true), row(1, true, false), row(2, false, true)],
            chapters: vec![],
            media_missing: false,
        });
        let mut nav = NavStack::new();
        nav.force_push(Route::LibraryTracks);
        nav.focus = 0;
        let fb = render(&m, &nav);
        let label_has = |y0: u16, colour: u16| {
            (y0..y0 + ROW_H)
                .flat_map(|y| (MARGIN_X..SCREEN_W / 2).map(move |x| (x, y)))
                .any(|(x, y)| px(&fb, x, y) == colour)
        };
        assert!(label_has(CONTENT_TOP + ROW_H, FG_MUTED), "unplayable row is muted");
        assert!(!label_has(CONTENT_TOP + ROW_H, FG), "and not drawn as playable");
        assert_value_matches(&fb, CONTENT_TOP + ROW_H, true, "1:05");
        assert_value_matches(&fb, CONTENT_TOP + ROW_H * 2, false, "1:05 !");
    }

    /// A multi-track engine list. `playable` is the *collection* id the entries resolve to
    /// (the engine sends the release id for playlist rows), which is what the marker's
    /// collection-identity check compares against.
    fn now_playing_tracks(
        list_kind: NowPlayingListKind,
        playable: Option<plippa_core::PlayableItemId>,
        titles: &[&str],
        durations: &[Option<u64>],
        current: usize,
    ) -> NowPlayingListChanged {
        NowPlayingListChanged {
            entries: titles
                .iter()
                .enumerate()
                .map(|(i, title)| NowPlayingEntry {
                    kind: NowPlayingEntryKind::Track,
                    title: (*title).into(),
                    artist: String::new(),
                    start_ms: 0,
                    end_ms: None,
                    duration_ms: durations[i],
                    track_index: i as u32,
                    playable_id: playable,
                    available: true,
                    item_position: i as u32,
                })
                .collect(),
            current_index: current as u32,
            list_kind,
            list_id: 1,
        }
    }

    /// Browsed album `id` with `titles` in `mediaList` order.
    fn album_with_tracks(id: i64, titles: &[&str]) -> ScreenModel {
        let root = std::path::Path::new("/media/noubin");
        let mut m = ScreenModel::default();
        let mut it = item(id, "Album");
        it.library_root = Some(root.to_path_buf());
        m.library = vec![it];
        m.detail_playable_id = Some(id);
        m.detail_info = Some(DetailInfo {
            playable_id: id,
            kind: Some(LibraryKind::Album),
            tracks: titles
                .iter()
                .enumerate()
                .map(|(p, t)| TrackRow {
                    position: p as u32,
                    title: (*t).into(),
                    artist: None,
                    duration_ms: Some(1_000 + p as u64),
                    available: true,
                    supported: true,
                    format: String::new(),
                })
                .collect(),
            chapters: vec![],
            media_missing: false,
        });
        m
    }

    /// Indices of the painted rows that carry the `ACCENT_2` playing marker.
    fn marked_rows(fb: &FrameBuffer, rows: usize) -> Vec<usize> {
        (0..rows)
            .filter(|r| {
                let y = CONTENT_TOP + *r as u16 * ROW_H;
                (y..y + ROW_H)
                    .any(|yy| (MARGIN_X..MARGIN_X + 8).any(|x| px(fb, x, yy) == ACCENT_2))
            })
            .collect()
    }

    fn render_tracks(m: &ScreenModel) -> FrameBuffer {
        let mut nav = NavStack::new();
        nav.force_push(Route::LibraryTracks);
        render(m, &nav)
    }

    #[test]
    fn library_tracks_marker_shows_for_the_browsed_collection() {
        use plippa_noubin::encode_playable_id;
        let root = std::path::Path::new("/media/noubin");
        let mut m = album_with_tracks(1, &["T0", "T1", "T2"]);
        m.now_playing = Some(now_playing_tracks(
            NowPlayingListKind::Album,
            Some(plippa_core::PlayableItemId::new(encode_playable_id(root, 1))),
            &["T0", "T1", "T2"],
            &[Some(1_000), Some(1_001), Some(1_002)],
            1,
        ));
        assert_eq!(
            marked_rows(&render_tracks(&m), 3),
            vec![1],
            "the playing row of the browsed album is marked"
        );
    }

    #[test]
    fn library_tracks_marker_ignores_a_different_collection() {
        // Album A is playing at playback index 1; album B has a row at source position 1.
        // The old code compared the playback index against `TrackRow::position`, so this
        // painted album B's row 1 as if it were playing.
        use plippa_noubin::encode_playable_id;
        let root = std::path::Path::new("/media/noubin");
        let mut m = album_with_tracks(2, &["B0", "B1", "B2"]);
        m.now_playing = Some(now_playing_tracks(
            NowPlayingListKind::Album,
            Some(plippa_core::PlayableItemId::new(encode_playable_id(root, 1))),
            &["A0", "A1", "A2"],
            &[Some(1_000), Some(1_001), Some(1_002)],
            1,
        ));
        assert!(
            marked_rows(&render_tracks(&m), 3).is_empty(),
            "another album's row index is not the playing track"
        );
    }

    #[test]
    fn the_library_marker_follows_the_playing_item_under_shuffle() {
        // Same collection, `track_index` is playback order and T2 plays first; the marker
        // follows `item_position` (2), i.e. the album's third row.
        use plippa_noubin::encode_playable_id;
        let root = std::path::Path::new("/media/noubin");
        let mut m = album_with_tracks(1, &["T0", "T1", "T2"]);
        let mut list = now_playing_tracks(
            NowPlayingListKind::Album,
            Some(plippa_core::PlayableItemId::new(encode_playable_id(root, 1))),
            &["T2", "T0", "T1"],
            &[Some(1_002), Some(1_000), Some(1_001)],
            0,
        );
        for (i, position) in [2, 0, 1].into_iter().enumerate() {
            list.entries[i].item_position = position;
        }
        m.now_playing = Some(list);
        assert_eq!(
            marked_rows(&render_tracks(&m), 3),
            vec![2],
            "the playing entry's source row is marked, not its playback index"
        );
    }

    #[test]
    fn library_tracks_marker_is_suppressed_for_playlists() {
        // Playlist rows resolve to their release playable, so the entries never share the
        // browsed playlist's id: no row can be attributed and none is marked.
        use plippa_noubin::encode_playable_id;
        let root = std::path::Path::new("/media/noubin");
        let mut m = album_with_tracks(3, &["P0", "P1", "P2"]);
        m.now_playing = Some(now_playing_tracks(
            NowPlayingListKind::Playlist,
            Some(plippa_core::PlayableItemId::new(encode_playable_id(root, 99))),
            &["P0", "P1", "P2"],
            &[Some(1_000), Some(1_001), Some(1_002)],
            0,
        ));
        assert!(
            marked_rows(&render_tracks(&m), 3).is_empty(),
            "a playlist cannot be attributed from the engine list"
        );
    }

    #[test]
    fn track_rows_show_their_source_position_under_shuffle() {
        // An album shuffled into T2, T0, T1 still numbers 3, 1, 2 by source position.
        let entry = |title: &str, position: u32, playback: u32| NowPlayingEntry {
            kind: NowPlayingEntryKind::Track,
            title: title.into(),
            artist: String::new(),
            start_ms: 0,
            end_ms: None,
            duration_ms: Some(1_000),
            track_index: playback,
            playable_id: None,
            available: true,
            item_position: position,
        };
        let shuffled = [entry("T2", 2, 0), entry("T0", 0, 1), entry("T1", 1, 2)];
        let labels: Vec<String> = shuffled
            .iter()
            .map(|e| np_row_text(e, NpRowNumbering::Source, true))
            .collect();
        assert_eq!(labels, ["3. T2", "1. T0", "2. T1"]);
        // The same rows in a playlist count down the display, not the source positions.
        let labels: Vec<String> = shuffled
            .iter()
            .map(|e| np_row_text(e, NpRowNumbering::Sequence, true))
            .collect();
        assert_eq!(labels, ["1. T2", "2. T0", "3. T1"]);
        // An old peer (every `item_position` zero) falls back to the playback index.
        let old = [entry("T2", 0, 0), entry("T0", 0, 1)];
        let labels: Vec<String> = old
            .iter()
            .map(|e| np_row_text(e, NpRowNumbering::Source, false))
            .collect();
        assert_eq!(labels, ["1. T2", "2. T0"]);
        // The list-level flag recognises a real source list even when its first entry is 0.
        let list = NowPlayingListChanged {
            entries: shuffled.to_vec(),
            current_index: 0,
            list_kind: NowPlayingListKind::Album,
            list_id: 1,
        };
        assert!(source_positions(&list));
    }

    #[test]
    fn row_numbering_follows_the_kind_of_list() {
        // Owner decision: albums / mixes / podcasts / audiobooks keep their own numbers;
        // playlists and dynamic queues count 1..n.
        let entry = |title: &str, position: u32, playback: u32| NowPlayingEntry {
            kind: NowPlayingEntryKind::Track,
            title: title.into(),
            artist: String::new(),
            start_ms: 0,
            end_ms: None,
            duration_ms: Some(1_000),
            track_index: playback,
            playable_id: None,
            available: true,
            item_position: position,
        };
        let shuffled = vec![entry("T2", 2, 0), entry("T0", 0, 1), entry("T1", 1, 2)];
        let mut m = ScreenModel::default();
        assert_eq!(
            m.np_row_numbering(),
            NpRowNumbering::Sequence,
            "no list loaded: nothing to number by source"
        );
        for (kind, want) in [
            (NowPlayingListKind::Album, NpRowNumbering::Source),
            (NowPlayingListKind::Mix, NpRowNumbering::Source),
            (NowPlayingListKind::Podcast, NpRowNumbering::Source),
            (NowPlayingListKind::Audiobook, NpRowNumbering::Source),
            (NowPlayingListKind::Playlist, NpRowNumbering::Sequence),
            (NowPlayingListKind::Single, NpRowNumbering::Sequence),
        ] {
            m.now_playing = Some(NowPlayingListChanged {
                entries: shuffled.clone(),
                current_index: 0,
                list_kind: kind,
                list_id: 1,
            });
            assert_eq!(m.np_row_numbering(), want, "{kind:?}");
        }

        // A dynamic queue is a sequence whatever kind it presents as: there is no source
        // order to fall back to, so an album-shaped dynamic queue still counts 1..n.
        m.now_playing = Some(NowPlayingListChanged {
            entries: shuffled.clone(),
            current_index: 0,
            list_kind: NowPlayingListKind::Album,
            list_id: 1,
        });
        m.source_order = plippa_core::SourceOrder::Dynamic;
        assert_eq!(m.np_row_numbering(), NpRowNumbering::Sequence);
        m.source_order = plippa_core::SourceOrder::Fixed;
        assert_eq!(m.np_row_numbering(), NpRowNumbering::Source);
    }

    #[test]
    fn the_header_count_is_progress_through_the_list_under_shuffle() {
        // A shuffled album into T2, T0, T1: the playing row is the *first* one played, so the
        // header reads 1/3 even though the row itself is album track 3.
        let mut m = ScreenModel::default();
        m.now_playing = Some(NowPlayingListChanged {
            entries: ["T2", "T0", "T1"]
                .iter()
                .enumerate()
                .map(|(playback, title)| NowPlayingEntry {
                    kind: NowPlayingEntryKind::Track,
                    title: (*title).into(),
                    artist: String::new(),
                    start_ms: 0,
                    end_ms: None,
                    duration_ms: Some(1_000),
                    track_index: playback as u32,
                    playable_id: None,
                    available: true,
                    item_position: [2, 0, 1][playback],
                })
                .collect(),
            current_index: 0,
            list_kind: NowPlayingListKind::Album,
            list_id: 1,
        });
        assert_eq!(
            m.np_track_ordinal(),
            Some((1, 3)),
            "progress, not the album track number"
        );
        assert_eq!(
            np_row_text(m.current_track_entry().unwrap(), m.np_row_numbering(), true),
            "3. T2",
            "the row still names album track 3"
        );
        // Advancing one item advances the count by exactly one, whatever the album numbers do.
        m.now_playing.as_mut().unwrap().current_index = 1;
        assert_eq!(m.np_track_ordinal(), Some((2, 3)));
        m.now_playing.as_mut().unwrap().current_index = 2;
        assert_eq!(m.np_track_ordinal(), Some((3, 3)));
    }

    #[test]
    fn library_chapters_value_is_timecode() {
        let mut m = ScreenModel::default();
        m.library = vec![item(1, "A")];
        m.detail_playable_id = Some(1);
        m.detail_info = Some(DetailInfo {
            playable_id: 1,
            kind: Some(LibraryKind::Mix),
            tracks: vec![],
            chapters: vec![
                ChapterRow { item_position: 0, start_ms: 65_000, title: "One".into() },
                ChapterRow { item_position: 0, start_ms: 130_000, title: "Two".into() },
            ],
            media_missing: false,
        });
        let mut nav = NavStack::new();
        nav.force_push(Route::LibraryChapters);
        nav.focus = 1; // second row focused, first compared
        let fb = render(&m, &nav);
        assert_value_matches(&fb, CONTENT_TOP, false, "1:05");
        let label_has_fg = (CONTENT_TOP..CONTENT_TOP + ROW_H)
            .flat_map(|y| (MARGIN_X..SCREEN_W / 2).map(move |x| (x, y)))
            .any(|(x, y)| px(&fb, x, y) == FG);
        assert!(label_has_fg, "chapter title in the label column");
    }

    #[test]
    fn library_detail_actions_all_visible() {
        let mut m = ScreenModel::default();
        let mut it = item(1, "Album");
        it.duration_ms = Some(180_000);
        m.library = vec![it];
        m.browse_kind = Some(LibraryKind::Album);
        m.detail_playable_id = Some(1);
        m.detail_info = Some(DetailInfo {
            playable_id: 1,
            kind: Some(LibraryKind::Album),
            tracks: vec![
                TrackRow { position: 0, title: "One".into(), artist: None, duration_ms: Some(1_000), available: true, supported: true, format: String::new() },
                TrackRow { position: 1, title: "Two".into(), artist: None, duration_ms: Some(1_000), available: true, supported: true, format: String::new() },
            ],
            chapters: vec![ChapterRow { item_position: 0, start_ms: 0, title: "Cue".into() }],
            media_missing: false,
        });
        let actions = m.detail_actions();
        assert_eq!(actions.len(), 4, "worst case action set");
        let mut nav = NavStack::new();
        nav.force_push(Route::LibraryDetail);
        let fb = render(&m, &nav);
        let info_y = CONTENT_TOP + 82;
        let action_y = CONTENT_TOP + 100;
        assert!(lit_in(&fb, info_y, ROW_H) > 20, "info line");
        for i in 0..4u16 {
            assert!(
                lit_in(&fb, action_y + i * ROW_H, ROW_H) > 20,
                "action row {i} missing at {}",
                action_y + i * ROW_H
            );
        }
        // The bottom-most label pixel is still inside the content box (never clipped).
        let last_ink = (action_y..CONTENT_BOTTOM)
            .rev()
            .find(|&y| (0..SCREEN_W).any(|x| ink(px(&fb, x, y))))
            .expect("action labels painted");
        assert!(
            last_ink < CONTENT_BOTTOM,
            "action ink at {last_ink} is below CONTENT_BOTTOM"
        );
        // Focus is the ring on the first action, and never the playing marker.
        assert_eq!(px(&fb, SPACE_1, action_y), ACCENT);
        assert!(
            !(action_y..action_y + ROW_H)
                .flat_map(|y| (0..SCREEN_W).map(move |x| (x, y)))
                .any(|(x, y)| px(&fb, x, y) == ACCENT_2),
            "focused action must not use the playing marker"
        );
    }

    #[test]
    fn detail_with_banner_drops_the_info_line_so_all_actions_fit() {
        let dir = tempfile::tempdir().unwrap();
        let cover = dir.path().join("cover.bmp");
        std::fs::write(&cover, b"not decoded by the UI").unwrap();
        let mut m = ScreenModel::default();
        let mut it = item(1, "Album");
        it.duration_ms = Some(180_000);
        it.art_path = Some(cover);
        m.library = vec![it];
        m.browse_kind = Some(LibraryKind::Album);
        m.detail_playable_id = Some(1);
        m.detail_info = Some(DetailInfo {
            playable_id: 1,
            kind: Some(LibraryKind::Album),
            tracks: vec![
                TrackRow { position: 0, title: "One".into(), artist: None, duration_ms: Some(1_000), available: true, supported: true, format: String::new() },
                TrackRow { position: 1, title: "Two".into(), artist: None, duration_ms: Some(1_000), available: true, supported: true, format: String::new() },
            ],
            chapters: vec![ChapterRow { item_position: 0, start_ms: 0, title: "Cue".into() }],
            // Would paint "Media file missing" in WARN on the info line.
            media_missing: true,
        });
        m.banner = Some("Tap a noubin".into());
        assert_eq!(m.detail_actions().len(), 4, "worst case action set");
        let mut nav = NavStack::new();
        nav.force_push(Route::LibraryDetail);
        let fb = render(&m, &nav);
        // A one-line banner pushes the whole block down by ROW_H; with the info line the
        // fourth action would end below the content box, so the info line must be dropped.
        let art_y = CONTENT_TOP + ROW_H;
        let info_y = art_y + 72 + INFO_GAP;
        assert_eq!(info_y, CONTENT_TOP + ROW_H + 82);
        for i in 0..4u16 {
            let row = info_y + i * ROW_H; // actions start where the info line would be
            assert!(
                row + ROW_H <= CONTENT_BOTTOM,
                "action row {i} at {row} is not fully inside the content box"
            );
            assert!(lit_in(&fb, row, ROW_H) > 20, "action row {i} missing at {row}");
        }
        // The info line is absent: its WARN text is not painted and the focused Play row
        // occupies the info band instead.
        let warn = (info_y..info_y + ROW_H)
            .any(|y| (0..SCREEN_W).any(|x| px(&fb, x, y) == WARN));
        assert!(!warn, "info line painted although it would push the actions out");
        assert_eq!(px(&fb, SPACE_1, info_y), ACCENT, "Play row is focused");
        // Nothing is pushed into the footer either.
        assert_eq!(
            lit_in(&fb, CONTENT_BOTTOM, SCREEN_H - CONTENT_BOTTOM),
            0,
            "detail painted below CONTENT_BOTTOM"
        );
    }

    #[test]
    fn library_detail_art_box_placeholder() {
        let render_with = |art: Option<std::path::PathBuf>| {
            let mut m = ScreenModel::default();
            let mut it = item(1, "Tone");
            it.art_path = art;
            m.library = vec![it];
            m.detail_playable_id = Some(1);
            let mut nav = NavStack::new();
            nav.force_push(Route::LibraryDetail);
            render(&m, &nav)
        };
        let art_size = 72u16;
        let inside = |fb: &FrameBuffer, colour: u16| {
            (CONTENT_TOP + 1..CONTENT_TOP + art_size - 1)
                .flat_map(|y| (MARGIN_X + 1..MARGIN_X + art_size - 1).map(move |x| (x, y)))
                .filter(|&(x, y)| px(fb, x, y) == colour)
                .count()
        };

        let missing = render_with(None);
        assert_eq!(px(&missing, MARGIN_X, CONTENT_TOP), WARN, "placeholder border");
        assert!(inside(&missing, FG_MUTED) > 0, "the `-` placeholder is painted");
        assert_eq!(inside(&missing, FG), 0, "missing art never draws `ART`");

        // Art that exists but has not been decoded (yet): the accent box says `...`.
        let dir = tempfile::tempdir().unwrap();
        let cover = dir.path().join("cover.bmp");
        std::fs::write(&cover, b"not decoded here").unwrap();
        let present = render_with(Some(cover));
        assert_eq!(px(&present, MARGIN_X, CONTENT_TOP), ACCENT, "art border");
        assert!(inside(&present, FG) > 0, "loading label painted");
        assert_ne!(missing.pixels, present.pixels);
    }

    #[test]
    fn associate_wait_shows_sanitised_title() {
        let render_title = |title: &str| {
            let mut m = ScreenModel::default();
            let mut it = item(1, title);
            it.title = title.to_string();
            m.library = vec![it];
            m.detail_playable_id = Some(1);
            let mut nav = NavStack::new();
            nav.force_push(Route::LibraryAssociate);
            render(&m, &nav)
        };
        // SEC-12: a hostile title paints exactly like its sanitised form, and something is
        // painted for the item line (not an empty prompt).
        let hostile = render_title(HOSTILE);
        let clean = render_title("Evil[2Jx");
        assert_eq!(hostile.pixels, clean.pixels, "associate title not sanitised");
        let hint_y = SCREEN_H / 4 + cell_height(TypeRole::Display) + SPACE_2;
        let dim = (hint_y..hint_y + cell_height(TypeRole::Small))
            .flat_map(|y| (0..SCREEN_W).map(move |x| (x, y)))
            .filter(|&(x, y)| px(&clean, x, y) == FG_DIM)
            .count();
        assert!(dim > 0, "`for <title>` line painted");
    }

    #[test]
    fn no_route_paints_below_footer() {
        let mut m = ScreenModel::default();
        m.library = vec![item(1, "Album")];
        m.browse_kind = Some(LibraryKind::Album);
        m.detail_playable_id = Some(1);
        m.detail_info = Some(DetailInfo {
            playable_id: 1,
            kind: Some(LibraryKind::Album),
            tracks: vec![
                TrackRow { position: 0, title: "One".into(), artist: None, duration_ms: Some(1_000), available: true, supported: true, format: String::new() },
                TrackRow { position: 1, title: "Two".into(), artist: None, duration_ms: Some(1_000), available: true, supported: true, format: String::new() },
            ],
            chapters: vec![ChapterRow { item_position: 0, start_ms: 0, title: "Cue".into() }],
            media_missing: true,
        });
        for route in [
            Route::Library,
            Route::LibraryCategory,
            Route::LibraryTracks,
            Route::LibraryChapters,
            Route::LibraryDetail,
            Route::LibraryAssociate,
        ] {
            let mut nav = NavStack::new();
            nav.force_push(route.clone());
            let mut fb = FrameBuffer::new(SCREEN_W, SCREEN_H);
            fb.clear(BG);
            m.render_screen(&nav, &mut fb);
            for y in SCREEN_H - FOOTER_H..SCREEN_H {
                for x in 0..SCREEN_W {
                    assert_eq!(
                        px(&fb, x, y),
                        BG,
                        "{route:?} painted below the footer boundary at ({x},{y})"
                    );
                }
            }
        }
    }

    #[test]
    fn library_header_meta_says_where_you_are() {
        let mut m = ScreenModel::default();
        let nav = NavStack::new();
        assert_eq!(
            m.header_text(&Route::Library, &nav),
            ("Library".to_string(), None)
        );
        m.kind_counts = vec![(LibraryKind::Album, 3), (LibraryKind::Mix, 2)];
        assert_eq!(
            m.header_text(&Route::Library, &nav),
            ("Library".to_string(), Some("5 items".to_string()))
        );

        m.browse_kind = Some(LibraryKind::Album);
        m.library = (0..3).map(|i| item(i, "A")).collect();
        assert_eq!(
            m.header_text(&Route::LibraryCategory, &nav),
            ("Albums".to_string(), Some("3 items".to_string()))
        );

        m.detail_playable_id = Some(1);
        let mut it = item(1, "A");
        it.duration_ms = Some(3_600_000);
        m.library = vec![it];
        m.detail_info = Some(DetailInfo {
            playable_id: 1,
            kind: Some(LibraryKind::Album),
            tracks: (0..3).map(|p| TrackRow { position: p, title: "T".into(), artist: None, duration_ms: None, available: true, supported: true, format: String::new() }).collect(),
            chapters: vec![ChapterRow { item_position: 0, start_ms: 0, title: "C".into() }],
            media_missing: false,
        });
        let mut nav2 = NavStack::new();
        nav2.focus = 1;
        assert_eq!(
            m.header_text(&Route::LibraryTracks, &nav2),
            ("Albums".to_string(), Some("2/3".to_string()))
        );
        nav2.focus = 0;
        assert_eq!(
            m.header_text(&Route::LibraryChapters, &nav2),
            ("Albums".to_string(), Some("1/1".to_string()))
        );
        assert_eq!(
            m.header_text(&Route::LibraryDetail, &nav),
            ("Albums".to_string(), Some("60:00".to_string()))
        );
        assert_eq!(
            m.header_text(&Route::LibraryAssociate, &nav),
            ("Associate".to_string(), None)
        );

        // No browse kind: keep the static `Route::title()` rather than inventing a name.
        m.browse_kind = None;
        assert_eq!(
            m.header_text(&Route::LibraryCategory, &nav),
            ("Category".to_string(), Some("1 item".to_string()))
        );
        // Singular-aware counts on the root total as well.
        m.kind_counts = vec![(LibraryKind::Album, 1)];
        assert_eq!(
            m.header_text(&Route::Library, &nav),
            ("Library".to_string(), Some("1 item".to_string()))
        );
    }

    /// Text that must never reach the display: bidi override, zero-width, controls, CSI.
    const HOSTILE: &str = "Ev\u{202E}il\u{200B}\u{FEFF}\r\n\u{0007}\u{001B}[2J\u{2066}x\u{E0041}";

    #[test]
    fn screen_text_strips_hostile_characters() {
        let clean = screen_text(HOSTILE);
        assert_eq!(clean, "Evil[2Jx");
        assert!(!clean.chars().any(|c| {
            c.is_control()
                || matches!(
                    c,
                    '\u{200B}'..='\u{200F}'
                        | '\u{202A}'..='\u{202E}'
                        | '\u{2066}'..='\u{2069}'
                        | '\u{E0000}'..='\u{E007F}'
                        | '\u{FEFF}'
                )
        }));
        // Bounded even for a huge input.
        let huge = "a".repeat(5_000_000);
        assert!(screen_text(&huge).chars().count() <= SCREEN_TEXT_MAX_CHARS);
    }

    /// NB_VERIFY NB_12: text that reaches the screen by paths other than the library (Wi-Fi
    /// failure messages quote the SSID / nmcli stderr, the title-bar banner, the associate
    /// wait screen) is stripped at draw time: a hostile string paints exactly like its clean form.
    #[test]
    fn wifi_message_banner_and_associate_title_are_stripped_when_drawn() {
        use crate::wifi::WifiForm;
        let painted = |hostile: bool| {
            let t = |s: &str| if hostile { s.replace("@@", HOSTILE) } else { s.replace("@@", "Evil[2Jx") };
            // Wi-Fi status screen with a failure message.
            let mut m = ScreenModel::default();
            let mut form = WifiForm::new(vec![]);
            form.message = Some(t("connect failed: @@ not found"));
            m.wifi = Some(form);
            let mut nav = NavStack::new();
            nav.force_push(Route::Wifi);
            let wifi_fb = render(&m, &nav);
            // Banner under the title bar of a leaf screen.
            let mut m2 = ScreenModel::default();
            m2.banner = Some(t("tap a noubin for @@"));
            let mut nav2 = NavStack::new();
            nav2.force_push(Route::Library);
            let banner_fb = render(&m2, &nav2);
            // Associate wait screen title.
            let mut m3 = ScreenModel::default();
            m3.library = vec![item(1, &t("@@"))];
            m3.library[0].title = t("@@");
            m3.detail_playable_id = Some(1);
            let mut nav3 = NavStack::new();
            nav3.force_push(Route::LibraryAssociate);
            let assoc_fb = render(&m3, &nav3);
            (wifi_fb.pixels, banner_fb.pixels, assoc_fb.pixels)
        };
        let (w1, b1, a1) = painted(true);
        let (w2, b2, a2) = painted(false);
        assert!(w1 == w2, "wifi message drawn unsanitised");
        assert!(b1 == b2, "banner drawn unsanitised");
        assert!(a1 == a2, "associate title drawn unsanitised");
    }

    #[test]
    fn hostile_playback_and_notice_text_is_sanitised_before_it_is_drawn() {
        use plippa_core::SessionState;
        let mut m = ScreenModel::default();
        let mut session = SessionState::default();
        session.title = HOSTILE.into();
        session.artist = HOSTILE.into();
        m.apply_playback(PlaybackStatus::from_session(session));
        let p = m.playback.as_ref().unwrap();
        assert_eq!(p.session.title, "Evil[2Jx");
        assert_eq!(p.session.artist, "Evil[2Jx");
        m.push_notice(UserNotice {
            severity: NoticeSeverity::Warning,
            code: "x".into(),
            message: format!("Skipped \"{HOSTILE}\""),
        });
        assert_eq!(m.banner.as_deref(), Some("Skipped \"Evil[2Jx\""));
        assert_eq!(m.notices[0].message, "Skipped \"Evil[2Jx\"");
    }

    // ---- UI_12: system / Wi-Fi / diagnostics routes and the whole-app audit ----

    /// Every [`Route`], spelled out because `nav.rs` has no `all()` and is out of scope here.
    const ALL_ROUTES: &[Route] = &[
        Route::Main,
        Route::Menu,
        Route::System,
        Route::SystemIp,
        Route::SystemCertificate,
        Route::SystemPlayMode,
        Route::SystemAudio,
        Route::SystemAudioOutputs,
        Route::SystemAudioDelay,
        Route::SystemDebug,
        Route::SystemDebugAudio,
        Route::Warnings,
        Route::Wifi,
        Route::NetworkInfo,
        Route::Library,
        Route::LibraryCategory,
        Route::LibraryDetail,
        Route::LibraryTracks,
        Route::LibraryChapters,
        Route::LibraryCover,
        Route::LibraryAssociate,
        Route::Debug,
    ];

    /// Routes whose documented chrome is the footer / hint band: their paint legitimately
    /// reaches into the bottom `FOOTER_H` rows.
    const FOOTER_ROUTES: &[Route] = &[Route::Main, Route::Menu, Route::Wifi, Route::NetworkInfo];

    fn clear_frame() -> FrameBuffer {
        let mut fb = FrameBuffer::new(SCREEN_W, SCREEN_H);
        fb.clear(BG);
        fb
    }

    /// A model with something on every screen (list data, addresses, a notice, Wi-Fi).
    fn populated_model() -> ScreenModel {
        let mut m = ScreenModel::default();
        m.library = vec![item(1, "Album")];
        m.browse_kind = Some(LibraryKind::Album);
        m.detail_playable_id = Some(1);
        m.detail_info = Some(DetailInfo {
            playable_id: 1,
            kind: Some(LibraryKind::Album),
            tracks: vec![
                TrackRow { position: 0, title: "One".into(), artist: None, duration_ms: Some(1_000), available: true, supported: true, format: String::new() },
                TrackRow { position: 1, title: "Two".into(), artist: None, duration_ms: Some(1_000), available: true, supported: true, format: String::new() },
            ],
            chapters: vec![ChapterRow { item_position: 0, start_ms: 0, title: "Cue".into() }],
            media_missing: true,
        });
        m.system.ip = Some("192.168.1.42".into());
        m.system.ips = vec!["192.168.1.42".into(), "fd00::42".into()];
        m.system.portal_url = Some("http://192.168.1.42:8787/".into());
        m.system.temperature_c = Some(45);
        m.system.fan_status = "on".into();
        m.wifi = Some(WifiForm::new(vec!["home".into(), "studio".into()]));
        m.push_notice(UserNotice {
            severity: NoticeSeverity::Info,
            code: "ui12".into(),
            message: "Media stick inserted".into(),
        });
        m
    }

    fn count_ink(fb: &FrameBuffer) -> usize {
        fb.pixels.iter().filter(|p| ink(**p)).count()
    }

    #[test]
    fn every_route_paints_and_stays_in_its_content_box() {
        let m = populated_model();
        for route in ALL_ROUTES {
            let mut nav = NavStack::new();
            nav.force_push(route.clone());
            let mut fb = clear_frame();
            m.render_screen(&nav, &mut fb);
            assert!(count_ink(&fb) > 0, "{route:?} painted nothing");
            // No body may paint above the content top: the header band and its rule occupy
            // `0..CONTENT_TOP`, the three rows between are breathing space. Now Playing has its
            // own 90 px header, so its box starts at `NP_CONTENT_TOP`.
            let (top, bottom) = if matches!(route, Route::Main | Route::Menu) {
                (NP_HEADER_H + RULE_H, NP_CONTENT_TOP)
            } else {
                (HEADER_H + RULE_H, CONTENT_TOP)
            };
            for y in top..bottom {
                assert!(
                    (0..SCREEN_W).all(|x| px(&fb, x, y) == BG),
                    "{route:?} painted above the content top at row {y}"
                );
            }
            if !FOOTER_ROUTES.contains(route) {
                for y in SCREEN_H - FOOTER_H..SCREEN_H {
                    assert!(
                        (0..SCREEN_W).all(|x| px(&fb, x, y) == BG),
                        "{route:?} painted into the footer rows at row {y}"
                    );
                }
            }
        }
    }

    #[test]
    fn play_mode_rows_do_not_overlap() {
        for focus in 0..PLAYBACK_SETTINGS_ROWS.len() {
            let m = ScreenModel::default();
            let mut nav = NavStack::new();
            nav.force_push(Route::SystemPlayMode);
            nav.focus = focus;
            let fb = render(&m, &nav);
            let first = window_start(focus, PLAYBACK_SETTINGS_ROWS.len(), PLAY_MODE_VISIBLE_ROWS);
            let visible = (PLAYBACK_SETTINGS_ROWS.len() - first).min(PLAY_MODE_VISIBLE_ROWS);
            // Only the window is painted; the rows outside it must not leave ink behind.
            for row in 0..PLAY_MODE_VISIBLE_ROWS {
                let y0 = CONTENT_TOP + (row as u16) * PLAY_MODE_ROW_PITCH;
                if row >= visible {
                    assert!(
                        (y0..y0 + ROW_H)
                            .flat_map(|y| (0..SCREEN_W).map(move |x| (x, y)))
                            .all(|(x, y)| px(&fb, x, y) == BG),
                        "row {row} (focus {focus}) is outside the window but painted"
                    );
                    continue;
                }
                let (mut label_right, mut value_left) = (0u16, SCREEN_W);
                for x in MARGIN_X..SCREEN_W - MARGIN_X {
                    for y in y0..y0 + ROW_H {
                        match px(&fb, x, y) {
                            p if p == FG => label_right = label_right.max(x),
                            p if p == FG_DIM => value_left = value_left.min(x),
                            _ => {}
                        }
                    }
                }
                assert!(
                    label_right.saturating_add(SPACE_2) <= value_left,
                    "row {row} (focus {focus}): label ends {label_right}, value starts {value_left}"
                );
                // The value column must actually exist and keep the settings language: the
                // owner decision is `setting_row` (`Small`/`FG_DIM`), not §4's `Body`/`ACCENT`
                // selected-row styling. Without this the loop above would pass vacuously if a
                // future change painted the value in `ACCENT` (it would find no `FG_DIM`).
                assert!(
                    value_left < SCREEN_W,
                    "row {row} (focus {focus}): no FG_DIM value text painted"
                );
                // Interior only: the focused row's 2 px ring is ACCENT by design.
                let accent_text = (y0 + 2..y0 + ROW_H - 2)
                    .flat_map(|y| {
                        (MARGIN_X..SCREEN_W - MARGIN_X - SPACE_1).map(move |x| (x, y))
                    })
                    .filter(|(x, y)| px(&fb, *x, *y) == ACCENT)
                    .count();
                assert_eq!(
                    accent_text, 0,
                    "row {row} (focus {focus}) paints ACCENT text: §4's Body/ACCENT styling is back"
                );
            }
        }
    }

    /// The window follows the cursor instead of dropping rows off the screen: a row outside the
    /// window paints nothing, the focused row is always painted, and the scrollbar appears
    /// because the list is longer than the window.
    #[test]
    fn the_settings_window_follows_the_cursor() {
        let mut nav = NavStack::new();
        nav.force_push(Route::SystemPlayMode);
        let m = ScreenModel::default();
        // `Art LEDs` is the last row, outside the window while the cursor is on the first.
        let mut changed = m.clone();
        changed.play_mode.art_leds = !m.play_mode.art_leds;
        let paint = |model: &ScreenModel, focus: usize, nav: &mut NavStack| {
            nav.focus = focus;
            render(model, nav).pixels
        };
        assert_eq!(
            paint(&m, 0, &mut nav),
            paint(&changed, 0, &mut nav),
            "an out-of-window row is not painted"
        );
        // Cursor on the last row: the window slides down and the row is painted.
        let last = PLAYBACK_SETTINGS_ROWS.len() - 1;
        nav.focus = last;
        let bottom = render(&m, &nav);
        assert_ne!(
            bottom.pixels,
            paint(&changed, last, &mut nav),
            "the focused last row is painted"
        );
        // A scrollbar track is painted at the right edge (the list is longer than the window).
        let right = SCREEN_W - 2;
        let bar = (CONTENT_TOP..CONTENT_BOTTOM)
            .filter(|y| px(&bottom, right, *y) != BG)
            .count();
        assert!(bar > 0, "no scrollbar for a windowed list");
    }

    #[test]
    fn play_mode_description_is_whole_lines() {
        for focus in 0..PLAYBACK_SETTINGS_ROWS.len() {
            let m = ScreenModel::default();
            let mut nav = NavStack::new();
            nav.force_push(Route::SystemPlayMode);
            nav.focus = focus;
            let fb = render(&m, &nav);
            let first = window_start(focus, PLAYBACK_SETTINGS_ROWS.len(), PLAY_MODE_VISIBLE_ROWS);
            let visible = (PLAYBACK_SETTINGS_ROWS.len() - first).min(PLAY_MODE_VISIBLE_ROWS);
            let rule_y = CONTENT_TOP + (visible as u16) * PLAY_MODE_ROW_PITCH;
            let desc_top = rule_y + PLAY_MODE_DESC_GAP;
            let desc_ink: Vec<u16> = (desc_top..CONTENT_BOTTOM)
                .flat_map(|y| (0..SCREEN_W).map(move |x| (x, y)))
                .filter(|(x, y)| px(&fb, *x, *y) == FG_DIM)
                .map(|(_, y)| y)
                .collect();
            assert!(!desc_ink.is_empty(), "row {focus}: no description painted");
            assert!(
                *desc_ink.iter().max().unwrap() < CONTENT_BOTTOM,
                "row {focus}: description runs past CONTENT_BOTTOM"
            );
            // Every painted description row is a whole cell row (no half-line at the edge):
            // derive the cell top from the ink row (glyph ink sits inside its 13 px cell) and
            // check that the whole cell ends inside the box.
            let line_h = cell_height(TypeRole::Small);
            let mut cells: Vec<u16> = desc_ink
                .iter()
                .map(|y| desc_top + (y - desc_top) / line_h * line_h)
                .collect();
            cells.sort_unstable();
            cells.dedup();
            for cell_top in &cells {
                assert!(
                    cell_top + line_h <= CONTENT_BOTTOM,
                    "row {focus}: line at {cell_top} is clipped"
                );
            }
            // Every wrapped line is actually painted: the whole description fits instead of
            // being silently dropped one whole line at a time at the bottom.
            let expected = text::wrap(
                PLAYBACK_SETTINGS_ROWS[focus].description(),
                content_width(&fb),
                TypeRole::Small,
            );
            assert!(
                expected.len() <= 2,
                "row {focus}: the description is {} lines; eight visible rows leave room for two",
                expected.len()
            );
            assert_eq!(
                cells.len(),
                expected.len(),
                "row {focus}: painted {} of {} description lines",
                cells.len(),
                expected.len()
            );
            assert!(
                (expected.len() as u16) * line_h <= CONTENT_BOTTOM - desc_top,
                "row {focus}: {} lines do not fit above CONTENT_BOTTOM",
                expected.len()
            );
        }
    }

    /// UI_16 §8 / AUDIO_05: the Debug screen is a settings list with the setting row and the
    /// Audio info navigation row, in the same language as Playback settings (focus ring,
    /// `Small` dim value, chevron on the navigation row).
    #[test]
    fn debug_screen_renders_the_setting_and_navigation_rows() {
        let mut m = ScreenModel::default();
        let mut nav = NavStack::new();
        nav.force_push(Route::SystemDebug);
        let fb = render(&m, &nav);
        // The focused row starts at CONTENT_TOP and carries the shared 2 px ACCENT ring.
        assert_eq!(px(&fb, SPACE_1, CONTENT_TOP), ACCENT);
        assert_eq!(px(&fb, SPACE_1, CONTENT_TOP + ROW_H - 1), ACCENT);
        // Its value is drawn in the settings language and a description sits below the rule.
        let row_value = (CONTENT_TOP..CONTENT_TOP + ROW_H)
            .flat_map(|y| (0..SCREEN_W).map(move |x| (x, y)))
            .filter(|(x, y)| px(&fb, *x, *y) == FG_DIM)
            .count();
        assert!(row_value > 0, "no dim value painted on the row");
        let desc_top = CONTENT_TOP + 2 * (ROW_H + SPACE_1) + SPACE_2;
        let desc = (desc_top..CONTENT_BOTTOM)
            .flat_map(|y| (0..SCREEN_W).map(move |x| (x, y)))
            .filter(|(x, y)| px(&fb, *x, *y) == FG_DIM)
            .count();
        assert!(desc > 100, "no description painted");
        // On shows `On` (the value changes, the row count does not).
        m.play_mode.rec_enables_nfc_poll = true;
        let on = render(&m, &nav);
        assert_ne!(on.pixels, fb.pixels, "the value column follows the setting");
        assert_eq!(
            crate::play_mode::DebugRow::RecEnablesNfcPoll.value_text(&m.play_mode),
            "On"
        );
        // AUDIO_05: the second row is navigation, so it has a chevron and no value column.
        assert_eq!(DEBUG_ROWS[1], crate::play_mode::DebugRow::AudioInfo);
        assert!(DEBUG_ROWS[1].opens_screen());
        assert!(!DEBUG_ROWS[0].opens_screen());
        nav.focus = 1;
        let navigation = render(&m, &nav);
        assert_eq!(px(&navigation, SPACE_1, CONTENT_TOP + ROW_H + SPACE_1), ACCENT);
        assert_ne!(navigation.pixels, on.pixels);
    }

    /// AUDIO_04 §8.9: a route with no sinks is the shared empty state, exactly.
    #[test]
    fn audio_outputs_with_no_sink_paint_the_shared_empty_state() {
        let m = ScreenModel::default();
        let mut nav = NavStack::new();
        nav.force_push(Route::SystemAudioOutputs);
        let fb = render(&m, &nav);
        let mut want = FrameBuffer::new(SCREEN_W, SCREEN_H);
        want.clear(0);
        widgets::header(
            &mut want,
            widgets::Header {
                title: "Output devices",
                meta: None,
            },
        );
        widgets::message_state(&mut want, "No outputs", "No audio output was found");
        assert_eq!(fb.pixels, want.pixels);
    }

    /// AUDIO_04: the three Audio rows and the two child screens paint in their content box
    /// with a focused row, a tick icon on an enabled output and no optimistic value.
    #[test]
    fn audio_screens_paint_rows_and_ticks() {
        let mut m = ScreenModel::default();
        m.audio.route = plippa_core::audio_route::AudioRoute {
            outputs: vec![
                plippa_core::audio_route::AudioOutput {
                    id: "local".into(),
                    description: "Built-in".into(),
                    available: true,
                    is_local: true,
                    ..Default::default()
                },
                plippa_core::audio_route::AudioOutput {
                    id: "usb".into(),
                    description: "USB DAC".into(),
                    available: true,
                    is_local: false,
                    ..Default::default()
                },
            ],
            enabled: vec!["local".into(), "usb".into()],
            delay_auto: false,
            ..Default::default()
        };
        let mut nav = NavStack::new();
        nav.force_push(Route::SystemAudio);
        let audio = render(&m, &nav);
        assert!(count_ink(&audio) > 0);
        // Focus ring on the first row and the value column needs a value (`2 on`).
        assert_eq!(px(&audio, SPACE_1, CONTENT_TOP), ACCENT);
        assert!(m.audio.output_summary() == "2 on");
        assert_eq!(m.audio.delay_mode_text(), "Manual");

        // Output devices: the tick is ACCENT on the focused row (row 0 is Local only).
        nav.force_push(Route::SystemAudioOutputs);
        nav.focus = 1;
        let outputs = render(&m, &nav);
        let icon_x = SCREEN_W - MARGIN_X - crate::ui::icons::ICON_W;
        let icon_y = CONTENT_TOP + ROW_H + SPACE_1 + (ROW_H - crate::ui::icons::ICON_H) / 2;
        let tick = (icon_x..SCREEN_W - MARGIN_X)
            .flat_map(|x| {
                (icon_y..icon_y + crate::ui::icons::ICON_H).map(move |y| (x, y))
            })
            .filter(|&(x, y)| px(&outputs, x, y) == ACCENT)
            .count();
        assert!(tick > 20, "enabled output row has no focused tick ({tick})");

        // Delay: Auto plus one ms row per enabled output, manual values from the route.
        nav.force_push(Route::SystemAudioDelay);
        m.audio.route.manual_delay_ms.insert("usb".into(), 40);
        let delay = render(&m, &nav);
        assert!(count_ink(&delay) > 0);
        assert_eq!(m.audio.delay_ms("usb"), 40);
    }

    /// UI_17: the Debug screen ends with the build stamp — the commit, build time and profile
    /// of the running UI binary — so the device can be identified without a shell.
    #[test]
    fn debug_screen_prints_the_build_stamp_on_the_last_line() {
        let mut m = ScreenModel::default();
        let mut nav = NavStack::new();
        nav.force_push(Route::SystemDebug);
        let fb = render(&m, &nav);
        let note_y = CONTENT_BOTTOM - cell_height(TypeRole::Small);
        // The stamp is a dim `Small` line sitting directly above the footer band.
        let painted = (note_y..CONTENT_BOTTOM)
            .flat_map(|y| (0..SCREEN_W).map(move |x| (x, y)))
            .filter(|(x, y)| px(&fb, *x, *y) == FG_DIM)
            .count();
        assert!(painted > 40, "no build stamp painted ({painted} px)");
        // It is the only thing on that line, so every painted column is inside the text.
        let line = build_info::line();
        let width = text::measure(&line, TypeRole::Small);
        assert!(
            width <= content_width(&fb),
            "stamp {line:?} ({width} px) overflows {} px",
            content_width(&fb)
        );
        // And it fits the screen: the row above it is left for the description.
        assert!(note_y > CONTENT_TOP + ROW_H + SPACE_1 + SPACE_2);
    }

    /// UI_16 §1: the row under edit keeps the focus ring and paints its value column in
    /// `ACCENT`; every other row keeps the normal `FG_DIM` value.
    #[test]
    fn play_mode_editing_row_paints_its_value_in_accent() {
        let mut m = ScreenModel::default();
        let mut nav = NavStack::new();
        nav.force_push(Route::SystemPlayMode);
        let fine = PLAYBACK_SETTINGS_ROWS
            .iter()
            .position(|r| r.setting() == Some(crate::play_mode::PlayModeRow::FineTime))
            .expect("Fine time is a Playback settings row");
        nav.focus = fine;
        let idle = render(&m, &nav);
        m.play_mode.editing = Some(crate::play_mode::PlayModeRow::FineTime);
        let editing = render(&m, &nav);
        assert_ne!(
            idle.pixels, editing.pixels,
            "entering the edit state changes the row"
        );
        // Inside the focused row: accent only from the 2 px ring; FG_DIM was the value.
        // `Fine time` is below the window when the list starts at the top, so its band is its
        // offset inside the window, not its index in the list.
        let first = window_start(fine, PLAYBACK_SETTINGS_ROWS.len(), PLAY_MODE_VISIBLE_ROWS);
        let band = CONTENT_TOP + (fine - first) as u16 * PLAY_MODE_ROW_PITCH;
        let interior =
            |x: u16, y: u16| x > SPACE_1 + 1 && x < SCREEN_W - SPACE_1 - 2 && y > band + 1 && y < band + ROW_H - 2;
        let accent_inside = (band..band + ROW_H)
            .flat_map(|y| (0..SCREEN_W).map(move |x| (x, y)))
            .filter(|(x, y)| interior(*x, *y) && px(&editing, *x, *y) == ACCENT)
            .count();
        let dim_inside = (band..band + ROW_H)
            .flat_map(|y| (0..SCREEN_W).map(move |x| (x, y)))
            .filter(|(x, y)| interior(*x, *y) && px(&editing, *x, *y) == FG_DIM)
            .count();
        assert!(accent_inside > 0, "the value under edit is not accented");
        assert_eq!(dim_inside, 0, "the value is still dim while editing");
        // The other rows are untouched (their values stay dim).
        let other = CONTENT_TOP..CONTENT_TOP + ROW_H;
        assert!(other
            .flat_map(|y| (0..SCREEN_W).map(move |x| (x, y)))
            .any(|(x, y)| px(&editing, x, y) == FG_DIM));
    }

    #[test]
    fn warnings_wrap_and_scroll() {
        let mut m = ScreenModel::default();
        // 20 notices: only the newest fit, so a scrollbar must appear.
        for i in 0..20 {
            m.push_notice(UserNotice {
                severity: if i == 19 {
                    NoticeSeverity::Error
                } else {
                    NoticeSeverity::Warning
                },
                code: "x".into(),
                message: format!(
                    "warning {i}: {}",
                    "the media stick was removed while playing ".repeat(2)
                ),
            });
        }
        let mut nav = NavStack::new();
        nav.force_push(Route::Warnings);
        // The sticky banner (above the list) is covered elsewhere; this test is the list.
        m.banner = None;
        let mut fb = clear_frame();
        m.render_screen(&nav, &mut fb);
        // The newest notice is the top one and its bar is `ERR`.
        let bar_rows = (CONTENT_TOP..CONTENT_BOTTOM)
            .take_while(|y| px(&fb, MARGIN_X, *y) == ERR)
            .count();
        assert!(
            bar_rows > cell_height(TypeRole::Small) as usize,
            "the newest (long) notice did not wrap: {bar_rows} rows"
        );
        assert_eq!(px(&fb, MARGIN_X, CONTENT_TOP), ERR, "newest first");
        // A scrollbar track + thumb appears in the gutter.
        let gutter: Vec<u16> = (CONTENT_TOP..CONTENT_BOTTOM)
            .map(|y| px(&fb, SCREEN_W - 2, y))
            .collect();
        assert!(gutter.contains(&BORDER_SOFT), "no scrollbar track");
        assert!(gutter.contains(&BORDER), "no scrollbar thumb");
        // Notice text stays `FG` whatever the severity: only the bar carries colour.
        let notice_text = (CONTENT_TOP..CONTENT_TOP + cell_height(TypeRole::Small))
            .flat_map(|y| {
                (MARGIN_X + widgets::NOTICE_BAR_W + SPACE_1..SCREEN_W - SPACE_1)
                    .map(move |x| (x, y))
            })
            .map(|(x, y)| px(&fb, x, y))
            .filter(|p| *p != BG && *p != FG)
            .count();
        assert_eq!(notice_text, 0, "notice text was recoloured by severity");

        // Empty is the shared message state, not blank text.
        let empty = ScreenModel::default();
        let mut fbe = clear_frame();
        empty.render_screen(&nav, &mut fbe);
        assert!(count_ink(&fbe) > 0);
        assert_eq!(px(&fbe, MARGIN_X, CONTENT_TOP), BG, "no severity bar when empty");
    }

    fn keyboard_form(password: &str, focus_slot: i8) -> WifiForm {
        use crate::keyboard::{KeyboardEvent, KeyboardField};
        let mut form = WifiForm::new(vec![]);
        form.phase = crate::wifi::WifiPhase::Keyboard;
        form.selected_ssid = Some("home".into());
        let mut kb = KeyboardField::from_str(password);
        kb.handle(KeyboardEvent::Rotate(focus_slot));
        form.keyboard = kb;
        form
    }

    /// Rows on which a slot's top edge sits (one per slot line).
    fn slot_rows(fb: &FrameBuffer) -> Vec<u16> {
        (CONTENT_TOP..CONTENT_BOTTOM)
            .filter(|y| {
                let top = px(fb, MARGIN_X + 1, *y);
                (top == BORDER_SOFT || top == ACCENT) && px(fb, MARGIN_X + 1, y + 1) == BG_ALT
                    || (top == ACCENT && px(fb, MARGIN_X + 1, y + 1) == ACCENT)
                        && px(fb, MARGIN_X + 1, y + 2) == BG_ALT
            })
            .collect()
    }

    #[test]
    fn keyboard_slots_wrap_instead_of_clipping() {
        // 20 characters + blank + OK = 22 slots; `SLOT_W + SPACE_1` per slot overflows one line.
        let mut m = ScreenModel::default();
        m.wifi = Some(keyboard_form("abcdefghijklmnopqrst", 0));
        let mut nav = NavStack::new();
        nav.force_push(Route::Wifi);
        let mut fb = clear_frame();
        m.render_screen(&nav, &mut fb);
        let mut rows = slot_rows(&fb);
        rows.dedup();
        assert_eq!(rows.len(), 2, "slots should wrap to two lines: {rows:?}");
        assert!(
            *rows.iter().max().unwrap() + widgets::SLOT_H <= CONTENT_BOTTOM,
            "a slot line runs past the content box"
        );
        // Nothing painted off the right margin.
        for x in SCREEN_W - 4..SCREEN_W {
            for y in CONTENT_TOP..CONTENT_BOTTOM {
                assert_ne!(px(&fb, x, y), ACCENT, "slot painted at x = {x}");
            }
        }
    }

    #[test]
    fn keyboard_focused_slot_ringed_and_masked() {
        // `from_str` puts the cursor on the blank slot; two detents left lands on a character.
        let mut m = ScreenModel::default();
        m.wifi = Some(keyboard_form("secret12", -2));
        let mut nav = NavStack::new();
        nav.force_push(Route::Wifi);
        let fb = render(&m, &nav);
        // Slot geometry: the first slot row starts at CONTENT_TOP + ROW_H, left at MARGIN_X.
        let y = CONTENT_TOP + ROW_H;
        let step = widgets::SLOT_W + SPACE_1;
        let filled = MARGIN_X + step; // slot 1: a character
        let focused = MARGIN_X + step * 6; // slot 6: cursor moved two left from the blank slot 8
        // Filled slot: a mask glyph inside, never the character itself.
        let mask_ink = (y..y + widgets::SLOT_H)
            .flat_map(|yy| (filled..filled + widgets::SLOT_W).map(move |x| (x, yy)))
            .filter(|(x, yy)| px(&fb, *x, *yy) == FG)
            .count();
        assert!(mask_ink > 0, "filled slot shows no mask glyph");
        // Focused slot: the 2 px `ACCENT` ring.
        for x in focused..focused + widgets::SLOT_W {
            assert_eq!(px(&fb, x, y), ACCENT, "focused ring at x = {x}");
            assert_eq!(px(&fb, x, y + 1), ACCENT);
        }
        // Empty (blank) slot: no glyph at all, not a ring.
        let blank = MARGIN_X + step * 8;
        let blank_ink = (y..y + widgets::SLOT_H)
            .flat_map(|yy| (blank..blank + widgets::SLOT_W).map(move |x| (x, yy)))
            .filter(|(x, yy)| {
                let p = px(&fb, *x, *yy);
                p != BG_ALT && p != BORDER_SOFT
            })
            .count();
        assert_eq!(blank_ink, 0, "empty slot painted a glyph");
        // The password itself never appears as text: the slot row shows only masks, and the
        // `Display` line below shows exactly the one glyph under edit.
        assert_eq!(m.wifi.as_ref().unwrap().keyboard.text(), "secret12");
    }

    #[test]
    fn keyboard_long_password_stays_in_the_content_box() {
        // 40 characters: 42 slots need three lines, so the blocks below are dropped whole
        // rather than overrunning the footer.
        let mut m = ScreenModel::default();
        m.wifi = Some(keyboard_form("abcdefghijklmnopqrstuvwxyz0123456789ABCD", -1));
        let mut nav = NavStack::new();
        nav.force_push(Route::Wifi);
        let mut fb = clear_frame();
        m.render_screen(&nav, &mut fb);
        for y in CONTENT_BOTTOM..SCREEN_H {
            assert!(
                (0..SCREEN_W).all(|x| {
                    let p = px(&fb, x, y);
                    p == BG || p == BG_ALT || p == BORDER_SOFT || p == FG_MUTED
                }),
                "a slot or block painted into the footer at row {y}"
            );
        }
        let mut rows = slot_rows(&fb);
        rows.dedup();
        assert!(rows.len() >= 3, "40 characters should need three slot lines");
    }

    #[test]
    fn pin_screen_paints_six_digit_boxes_and_no_mask() {
        use crate::pinpad::PinEvent;
        let paint = |digits: [i8; PIN_DIGITS]| {
            let mut view = NetworkView::looking(0);
            view.settings = Some(crate::store_api::PortalSettingsInfo {
                available: true,
                unavailable_reason: None,
                enabled: true,
                https_enabled: true,
                pin_required: true,
                pin: Some("483920".into()),
                locked: false,
                listening: true,
                fingerprint: None,
                unsaved: None,
                listen_error: None,
            });
            let mut pad = PinPad::new();
            for d in digits {
                pad.handle(PinEvent::Enter); // edit the box
                if d != 0 {
                    pad.handle(PinEvent::Rotate(d));
                }
                pad.handle(PinEvent::Enter); // commit, auto-advance
            }
            view.pin_pad = Some(pad);
            let mut m = ScreenModel::default();
            m.network = Some(view);
            let mut nav = NavStack::new();
            nav.force_push(Route::SystemNetworkPin);
            render(&m, &nav)
        };

        let ascending = paint([1, 2, 3, 4, 5, 6]);
        // The box line: the first row with a slot's top border and ground at the left margin.
        // (The focused slot is `ACCENT`; here the cursor is on `OK`, so the digits are soft.)
        let y = (CONTENT_TOP..CONTENT_BOTTOM)
            .find(|&y| {
                matches!(px(&ascending, MARGIN_X, y), BORDER_SOFT | ACCENT)
                    && px(&ascending, MARGIN_X + 1, y + 1) == BG_ALT
            })
            .expect("no PIN box line");
        let step = widgets::SLOT_W + SPACE_1;
        assert!(
            MARGIN_X + (PIN_DIGITS as u16) * step + widgets::SLOT_W <= SCREEN_W - MARGIN_X,
            "seven boxes (six digits + OK) must fit one line"
        );
        let box_bitmap = |i: u16| {
            let x = MARGIN_X + i * step;
            let mut out = Vec::new();
            for yy in y..y + widgets::SLOT_H {
                for xx in x..x + widgets::SLOT_W {
                    out.push(px(&ascending, xx, yy));
                }
            }
            out
        };
        // Six digit boxes, all painted, then the OK box: all seven slots exist on that line.
        for i in 0..PIN_DIGITS as u16 {
            assert!(
                box_bitmap(i).iter().any(|&p| p == FG),
                "digit box {i} painted no glyph"
            );
        }
        assert!(
            box_bitmap(PIN_DIGITS as u16)
                .iter()
                .any(|&p| p == FG || p == ACCENT),
            "the OK box is drawn (its label is ACCENT while focused)"
        );
        // Digits, not masks: six different digits paint six different boxes. A masked box would
        // paint the same bullet in every one of them.
        for i in 0..PIN_DIGITS as u16 {
            for j in i + 1..PIN_DIGITS as u16 {
                assert_ne!(
                    box_bitmap(i),
                    box_bitmap(j),
                    "boxes {i} and {j} paint the same glyph (a mask?)"
                );
            }
        }
    }

    #[test]
    fn library_root_highlights_exactly_one_row_at_the_trailing_row() {
        let mut m = ScreenModel::default();
        m.kind_counts = vec![(LibraryKind::Album, 3), (LibraryKind::Mix, 2)];
        let kinds = m.library_root_kinds().len();
        let ringed = |focus: usize| {
            let mut nav = NavStack::new();
            nav.force_push(Route::Library);
            nav.focus = focus;
            let fb = render(&m, &nav);
            (0..=kinds as u16)
                .filter(|&row| px(&fb, SPACE_1, CONTENT_TOP + row * ROW_H) == ACCENT)
                .collect::<Vec<u16>>()
        };
        // UI_14 §4: from the last category onto the trailing `Distros` row, one ring only. The
        // old clamp rewrote the focus to the last category, so Mixes and Distros both ringed.
        assert_eq!(ringed(kinds), vec![kinds as u16], "trailing row focused");
        assert_eq!(ringed(kinds - 1), vec![(kinds - 1) as u16], "last category focused");
        // No wrap artefact at the other end either.
        assert_eq!(ringed(0), vec![0], "first row focused");
        // An out-of-range focus (storage yanked under the menu) still paints exactly one ring.
        assert_eq!(ringed(kinds + 3), vec![kinds as u16], "clamped");
    }

    #[test]
    fn library_root_names_the_trailing_row_distros() {
        // The row itself, and the title of the screen it opens; `System > Stores` keeps its name.
        assert_eq!(Route::LibraryStores.title(), "Distros");
        assert_eq!(Route::SystemStores.title(), "Stores", "the System row was not renamed");

        let mut m = ScreenModel::default();
        m.kind_counts = vec![(LibraryKind::Album, 1)];
        let mut nav = NavStack::new();
        nav.force_push(Route::Library);
        let row = m.library_root_kinds().len();
        nav.focus = row; // focused, so the row's ground is `SURFACE`
        let fb = render(&m, &nav);
        let y = CONTENT_TOP + row as u16 * ROW_H;
        let ty = y + (ROW_H - cell_height(TypeRole::Body)) / 2;
        let mut control = FrameBuffer::new(SCREEN_W, SCREEN_H);
        control.clear(SURFACE);
        text::draw(&mut control, MARGIN_X, ty, "Distros", TypeRole::Body, FG);
        let w = measure("Distros", TypeRole::Body);
        // Every glyph pixel of `Distros` is where it belongs in `FG`. The focus ring shares the
        // row's top and bottom pixel rows, so only the control's glyph ink is compared (the ring
        // is `ACCENT`, which no glyph here uses).
        let mut glyphs = 0;
        for xx in MARGIN_X..MARGIN_X + w {
            for yy in y..y + ROW_H {
                if px(&control, xx, yy) == FG {
                    glyphs += 1;
                    assert_eq!(
                        px(&fb, xx, yy),
                        FG,
                        "the label reads `Distros` at ({xx},{yy})"
                    );
                }
            }
        }
        assert!(glyphs > 0, "the control drew nothing");
        // And nothing else in `FG` sits in the label column: the row is not some longer word.
        let extra = (MARGIN_X..MARGIN_X + w)
            .flat_map(|xx| (y..y + ROW_H).map(move |yy| (xx, yy)))
            .filter(|&(xx, yy)| px(&fb, xx, yy) == FG && px(&control, xx, yy) != FG)
            .count();
        assert_eq!(extra, 0, "glyph ink outside `Distros` in the label column");
    }

    #[test]
    fn wifi_phases_distinct() {
        use crate::wifi::{MockWifi, WifiFormEvent, WifiPhase};
        let base = || MockWifi {
            networks: vec!["home".into(), "studio".into()],
            known_ssids: vec!["HomeLAN".into()],
            current_ssid: Some("HomeLAN".into()),
            ip: Some("192.168.1.42".into()),
            ..MockWifi::default()
        };
        // Every phase built the way the owner reaches it (or, for the transient ones, by
        // setting the field): the new phases have private state, so poking `phase` alone would
        // paint a half-empty screen.
        let reach = |steps: &dyn Fn(&mut MockWifi, &mut WifiForm)| {
            let mut wifi = base();
            let mut f = WifiForm::new(wifi.networks.clone());
            f.observe(&wifi);
            steps(&mut wifi, &mut f);
            f
        };
        let with_phase = |phase: WifiPhase| reach(&move |_, f| f.phase = phase);

        let mut forms = vec![
            with_phase(WifiPhase::Current),
            with_phase(WifiPhase::Scan),
            with_phase(WifiPhase::Connecting),
        ];
        // Keyboard: the password field is the state.
        let mut kb = with_phase(WifiPhase::Keyboard);
        kb.selected_ssid = Some("studio".into());
        kb.keyboard = crate::keyboard::KeyboardField::from_str("secret");
        forms.push(kb);
        // Known networks, its per-network actions, and the confirm.
        let known = reach(&|wifi, f| {
            f.focus = 1; // Known networks
            f.handle(WifiFormEvent::Enter, wifi);
        });
        assert_eq!(known.phase, WifiPhase::Known);
        forms.push(known);
        let actions = reach(&|wifi, f| {
            f.focus = 1;
            f.handle(WifiFormEvent::Enter, wifi);
            f.handle(WifiFormEvent::Enter, wifi);
        });
        assert_eq!(actions.phase, WifiPhase::KnownActions);
        forms.push(actions);
        let confirm = reach(&|wifi, f| {
            f.focus = 1;
            f.handle(WifiFormEvent::Enter, wifi);
            f.handle(WifiFormEvent::Enter, wifi);
            f.focus = 0; // Forget (Connect is disabled for the connected SSID)
            f.handle(WifiFormEvent::Enter, wifi);
        });
        assert_eq!(confirm.phase, WifiPhase::ConfirmForget);
        forms.push(confirm);

        let mut signatures = Vec::new();
        for form in forms {
            let mut m = populated_model();
            m.wifi = Some(form);
            let mut nav = NavStack::new();
            nav.force_push(Route::Wifi);
            let fb = render(&m, &nav);
            assert!(count_ink(&fb) > 0, "a Wi-Fi phase painted nothing");
            signatures.push(fb.pixels.clone());
        }
        for i in 0..signatures.len() {
            for j in i + 1..signatures.len() {
                assert_ne!(signatures[i], signatures[j], "two Wi-Fi phases paint the same frame");
            }
        }
    }

    #[test]
    fn network_has_no_qr() {
        let mut m = populated_model();
        m.system.portal_url = Some("http://192.168.1.42:8787/very/long/path?token=abcdef".into());
        let mut nav = NavStack::new();
        nav.force_push(Route::NetworkInfo);
        let fb = render(&m, &nav);
        // Structural guard, independent of how the text wraps. The deleted fake code was a
        // 21×21 module grid at 4 px/module: an 84×84 block at ~56 % ink, built from solid 4×4
        // modules with three solid 12×12 finder cores. Text never produces either shape.
        for side in [42u16, 63, 84] {
            let density = max_fg_density(&fb, side);
            assert!(
                density < 0.35,
                "a {side}×{side} block is {:.0}% FG: a QR-sized dense grid is back",
                density * 100.0
            );
        }
        assert!(
            !has_solid_fg_square(&fb, 12),
            "a solid 12×12 FG block: a QR finder core is back"
        );
        // The URL is wrapped, not cut: ink exists on the line after the IP row.
        let url_top = CONTENT_TOP + ROW_H + SPACE_1;
        let body_max = (CONTENT_TOP..CONTENT_BOTTOM)
            .rev()
            .find(|y| (0..SCREEN_W).any(|x| ink(px(&fb, x, *y))))
            .unwrap_or(0);
        assert!(body_max > url_top, "portal URL did not wrap");
    }

    /// Largest share of `FG` pixels in any `side`×`side` window of `fb` (0.0 when the frame
    /// is smaller than the window). Integral image keeps it O(W·H).
    fn max_fg_density(fb: &FrameBuffer, side: u16) -> f32 {
        let (w, h, s) = (fb.width as usize, fb.height as usize, side as usize);
        if s == 0 || w < s || h < s {
            return 0.0;
        }
        let stride = w + 1;
        let mut sum = vec![0u32; stride * (h + 1)];
        for y in 0..h {
            let mut row = 0u32;
            for x in 0..w {
                row += u32::from(fb.pixels[y * w + x] == FG);
                sum[(y + 1) * stride + x + 1] = sum[y * stride + x + 1] + row;
            }
        }
        let mut best = 0u32;
        for y in 0..=h - s {
            for x in 0..=w - s {
                let n = sum[(y + s) * stride + x + s] + sum[y * stride + x]
                    - sum[y * stride + x + s]
                    - sum[(y + s) * stride + x];
                best = best.max(n);
            }
        }
        best as f32 / (s * s) as f32
    }

    /// Whether any `side`×`side` block is entirely `FG`. The fake QR's 3×3-module finder
    /// cores were solid 12×12 px at scale 4; glyph strokes are far thinner.
    fn has_solid_fg_square(fb: &FrameBuffer, side: u16) -> bool {
        let (w, h, s) = (fb.width as usize, fb.height as usize, side as usize);
        if s == 0 || w < s || h < s {
            return false;
        }
        (0..=h - s).any(|y| {
            (0..=w - s).any(|x| {
                (y..y + s).all(|yy| (x..x + s).all(|xx| fb.pixels[yy * w + xx] == FG))
            })
        })
    }

    // ---- UI_12: Adjust-mode overlay (owner decision) ----

    fn adjust_model(param: AdjustParam) -> ScreenModel {
        let mut m = wave_model(vec![255u8; 1_500], 20, 30_000, 15_000);
        m.np_focus.mode = NpMode::Adjust(param);
        m
    }

    fn select_model() -> ScreenModel {
        wave_model(vec![255u8; 1_500], 20, 30_000, 15_000)
    }

    #[test]
    fn adjust_band_is_display_and_thirty() {
        let mut m = adjust_model(AdjustParam::Zoom);
        m.wave_window_ms = 8_000;
        let select = render_wave(&select_model());
        let adjust = render_wave(&m);
        assert_eq!(ADJUST_BAND_H, ROW_H_DISPLAY, "30 px band");
        // Rows above the band are untouched: the overlay is additive.
        for y in WAVE_BAND_Y..ADJUST_BAND_Y {
            for x in 0..SCREEN_W {
                assert_eq!(
                    px(&select, x, y),
                    px(&adjust, x, y),
                    "changed above the band at ({x},{y})"
                );
            }
        }
        // Inside the band the waveform is still there, blended toward `BG` (60 %).
        let mut blended = 0;
        for x in 0..SCREEN_W {
            let before = px(&select, x, ADJUST_BAND_Y + 1);
            let after = px(&adjust, x, ADJUST_BAND_Y + 1);
            if before == ACCENT {
                assert_ne!(after, ACCENT, "bar not blended at x = {x}");
                assert_ne!(after, BG, "bar erased at x = {x}, not 40 % transparent");
                blended += 1;
            }
        }
        assert!(blended > 100, "band did not cover the waveform: {blended}");
        // `ZOOM` (WARN) is right-aligned; there is no left-hand badge any more.
        let warn_left = (ADJUST_BAND_Y..ADJUST_BAND_Y + ADJUST_BAND_H)
            .flat_map(|y| (0..SCREEN_W / 2).map(move |x| (x, y)))
            .filter(|(x, y)| px(&adjust, *x, *y) == WARN)
            .count();
        assert_eq!(warn_left, 0, "no badge in the left half of the band");
        let warn_right = (ADJUST_BAND_Y..ADJUST_BAND_Y + ADJUST_BAND_H)
            .flat_map(|y| (SCREEN_W / 2..SCREEN_W).map(move |x| (x, y)))
            .filter(|(x, y)| px(&adjust, *x, *y) == WARN)
            .count();
        assert!(warn_right > 20, "ZOOM label is missing: {warn_right}");
        // `8s` (ACCENT) sits left of the label.
        let accent_right = (ADJUST_BAND_Y..ADJUST_BAND_Y + ADJUST_BAND_H)
            .flat_map(|y| (SCREEN_W / 2..SCREEN_W).map(move |x| (x, y)))
            .filter(|(x, y)| px(&adjust, *x, *y) == ACCENT)
            .count();
        assert!(accent_right > 5, "zoom readout is missing: {accent_right}");
        // Text ink is Display-sized (>= 9 rows); a Body/Small fallback is at most ~7.
        let (mut top, mut bottom) = (u16::MAX, 0);
        for y in ADJUST_BAND_Y..ADJUST_BAND_Y + ADJUST_BAND_H {
            for x in 0..SCREEN_W {
                if px(&adjust, x, y) == WARN {
                    top = top.min(y);
                    bottom = bottom.max(y);
                }
            }
        }
        assert!(
            bottom.saturating_sub(top) + 1 >= 9,
            "ZOOM is not Display-sized: rows {top}..={bottom}"
        );
    }

    #[test]
    fn adjust_overlay_disappears_and_repaints_the_band() {
        let select = render_wave(&select_model());
        let mut fb = render_wave(&adjust_model(AdjustParam::Jog));
        assert_ne!(fb.pixels, select.pixels);
        // Leaving Adjust re-renders over the same buffer: no residue from the band.
        let mut m = adjust_model(AdjustParam::Jog);
        m.np_focus.mode = NpMode::Select;
        m.render_main(&mut fb);
        assert_eq!(fb.pixels, select.pixels, "band residue after leaving Adjust");
    }
    // ---- ART_08: Now Playing cover ---------------------------------------------------------

    const MAGENTA: u16 = crate::display::rgb565(255, 0, 255);

    fn solid_art(colour: u16) -> std::sync::Arc<crate::art::ArtImage> {
        let side = NP_ART_SIDE;
        std::sync::Arc::new(crate::art::ArtImage {
            side,
            rgb565: vec![colour; usize::from(side) * usize::from(side)],
            rgb_side: 64,
            rgb: vec![0; 64 * 64 * 3],
        })
    }

    fn art_model() -> ScreenModel {
        let peaks: Vec<u8> = (0..400).map(|i| (i * 7 % 200) as u8).collect();
        wave_model(peaks, 20, 180_000, 60_000)
    }

    fn in_art_box(x: usize, y: usize) -> bool {
        (usize::from(NP_ART_X)..usize::from(NP_ART_X + NP_ART_SIDE)).contains(&x)
            && (usize::from(NP_ART_Y)..usize::from(NP_ART_Y + NP_ART_SIDE)).contains(&y)
    }

    #[test]
    fn np_art_fills_its_box_and_nothing_else() {
        let mut m = art_model();
        m.np_art = Some(solid_art(MAGENTA));
        let fb = render_wave(&m);
        let w = SCREEN_W as usize;
        let mut inside = 0;
        for (i, px) in fb.pixels.iter().enumerate() {
            let (x, y) = (i % w, i / w);
            if in_art_box(x, y) {
                inside += usize::from(*px == MAGENTA);
            } else {
                assert_ne!(*px, MAGENTA, "art colour outside its box at ({x}, {y})");
            }
        }
        // The adjust band is not showing, so every box pixel is art.
        assert_eq!(inside, usize::from(NP_ART_SIDE) * usize::from(NP_ART_SIDE));
        // Header, footer and the playhead column are never touched.
        assert!(NP_ART_Y >= NP_CONTENT_TOP && NP_ART_Y + NP_ART_SIDE <= NP_CONTENT_BOTTOM);
        assert!(NP_ART_X + NP_ART_SIDE < WAVE_PLAYHEAD_X);
    }

    #[test]
    fn no_art_or_zero_opacity_leaves_the_frame_untouched() {
        let m = art_model();
        let plain = render_wave(&m);
        let mut hidden = art_model();
        hidden.np_art = Some(solid_art(MAGENTA));
        hidden.np_art_alpha = 0;
        assert_eq!(render_wave(&hidden).pixels, plain.pixels, "alpha 0 paints nothing");
        let mut shown = art_model();
        shown.np_art = Some(solid_art(MAGENTA));
        assert_ne!(render_wave(&shown).pixels, plain.pixels);
    }

    #[test]
    fn np_art_fades_over_the_waveform_with_the_shared_blend() {
        let plain = render_wave(&art_model());
        let mut m = art_model();
        m.np_art = Some(solid_art(MAGENTA));
        m.np_art_alpha = 40;
        let fb = render_wave(&m);
        let w = SCREEN_W as usize;
        for (i, px) in fb.pixels.iter().enumerate() {
            let (x, y) = (i % w, i / w);
            let want = if in_art_box(x, y) {
                crate::ui::primitives::blend_pixel(plain.pixels[i], MAGENTA, 40)
            } else {
                plain.pixels[i]
            };
            assert_eq!(*px, want, "({x}, {y})");
        }
    }

    #[test]
    fn up_next_overlay_hides_the_art() {
        let mut m = art_model();
        m.now_playing = Some(NowPlayingListChanged {
            entries: (0..3)
                .map(|i| NowPlayingEntry {
                    kind: NowPlayingEntryKind::Track,
                    title: format!("T{i}"),
                    artist: "A".into(),
                    start_ms: 0,
                    end_ms: None,
                    duration_ms: Some(60_000),
                    track_index: i,
                    playable_id: None,
                    available: true,
                    item_position: i,
                })
                .collect(),
            current_index: 0,
            list_kind: NowPlayingListKind::Playlist,
            list_id: 1,
        });
        m.np_art = Some(solid_art(MAGENTA));
        assert!(render_wave(&m).pixels.contains(&MAGENTA), "art shows over the waveform");
        m.np_focus.target = NpTarget::List;
        assert!(m.up_next_visible());
        assert!(!render_wave(&m).pixels.contains(&MAGENTA), "the overlay owns the band");
    }

    #[test]
    fn the_adjust_band_still_covers_the_art() {
        let mut m = art_model();
        m.np_art = Some(solid_art(MAGENTA));
        m.np_focus.mode = NpMode::Adjust(AdjustParam::Jog);
        assert!(ADJUST_BAND_Y < NP_ART_Y + NP_ART_SIDE, "the band really overlaps the art box");
        let fb = render_wave(&m);
        let w = SCREEN_W as usize;
        for y in ADJUST_BAND_Y..ADJUST_BAND_Y + ADJUST_BAND_H {
            for x in 0..SCREEN_W as usize {
                assert_ne!(fb.pixels[usize::from(y) * w + x], MAGENTA, "({x}, {y})");
            }
        }
    }

    // ---- ART_09: cover art action and screen -------------------------------------------------

    fn cover_ref(name: &str, alt: Option<&str>) -> CoverRef {
        CoverRef {
            relpath: format!("Album/{name}"),
            source: plippa_noubin::covers::CoverSource::Local,
            alt_text: alt.map(str::to_string),
            is_image_of_noubin: false,
        }
    }

    fn detail_model(covers: Vec<CoverRef>) -> ScreenModel {
        let mut m = ScreenModel::default();
        m.library = vec![item(1, "Album")];
        m.browse_kind = Some(LibraryKind::Album);
        m.detail_playable_id = Some(1);
        m.detail_info = Some(DetailInfo {
            playable_id: 1,
            kind: Some(LibraryKind::Album),
            tracks: vec![
                TrackRow { position: 0, title: "One".into(), artist: None, duration_ms: None, available: true, supported: true, format: String::new() },
                TrackRow { position: 1, title: "Two".into(), artist: None, duration_ms: None, available: true, supported: true, format: String::new() },
            ],
            chapters: vec![ChapterRow { item_position: 0, start_ms: 0, title: "Cue".into() }],
            media_missing: false,
        });
        m.detail_covers = Some(DetailCovers { playable_id: 1, track_pos: None, root_idx: 0, covers });
        m
    }

    #[test]
    fn cover_art_action_needs_a_resolved_image_and_sits_between_chapters_and_associate() {
        use LibraryDetailAction::*;
        assert_eq!(
            detail_model(vec![]).detail_actions(),
            vec![Play, BrowseTracks, BrowseChapters, Associate],
            "no images: unchanged"
        );
        assert_eq!(
            detail_model(vec![cover_ref("a.jpg", None)]).detail_actions(),
            vec![Play, BrowseTracks, BrowseChapters, CoverArt, Associate]
        );
        let mut stale = detail_model(vec![cover_ref("a.jpg", None)]);
        stale.detail_covers.as_mut().unwrap().playable_id = 2;
        assert!(!stale.detail_actions().contains(&CoverArt), "covers of another item are ignored");
        let mut other_track = detail_model(vec![cover_ref("a.jpg", None)]);
        other_track.detail_track_pos = Some(3);
        assert!(!other_track.detail_actions().contains(&CoverArt));
        assert_eq!(CoverArt.label(), "Cover art");
        assert_eq!(Route::LibraryCover.title(), "Cover art");
    }

    fn solid_art_sq(side: u16, colour: u16) -> std::sync::Arc<crate::art::ArtImage> {
        std::sync::Arc::new(crate::art::ArtImage {
            side,
            rgb565: vec![colour; usize::from(side) * usize::from(side)],
            rgb_side: 2,
            rgb: vec![0; 12],
        })
    }

    fn cover_screen(view: CoverView) -> FrameBuffer {
        let mut m = ScreenModel::default();
        m.cover_view = view;
        let mut nav = NavStack::new();
        nav.force_push(Route::LibraryCover);
        render(&m, &nav)
    }

    const CYAN: u16 = 0x07FF;

    #[test]
    fn cover_screen_paints_the_square_centred_with_indicator_and_chevrons() {
        let fb = cover_screen(CoverView {
            index: 1,
            count: 3,
            title: "Back".into(),
            image: Some(solid_art_sq(COVER_VIEW_SIDE, CYAN)),
            failed: false,
        });
        let (x0, y0) = (COVER_VIEW_X, COVER_VIEW_Y);
        let count = (0..SCREEN_H)
            .flat_map(|y| (0..SCREEN_W).map(move |x| (x, y)))
            .filter(|&(x, y)| px(&fb, x, y) == CYAN)
            .count();
        assert_eq!(count, usize::from(COVER_VIEW_SIDE) * usize::from(COVER_VIEW_SIDE), "exactly the square");
        for (x, y) in [(x0, y0), (x0 + COVER_VIEW_SIDE - 1, y0 + COVER_VIEW_SIDE - 1)] {
            assert_eq!(px(&fb, x, y), CYAN);
        }
        assert_eq!(x0, SCREEN_W - x0 - COVER_VIEW_SIDE, "centred horizontally");
        assert!(y0 >= CONTENT_TOP && y0 + COVER_VIEW_SIDE <= CONTENT_BOTTOM, "inside the content box");
        assert!((y0 - CONTENT_TOP).abs_diff(CONTENT_BOTTOM - y0 - COVER_VIEW_SIDE) <= 1);

        // `2/3` lives in the right margin at the bottom of the square; chevrons at both sides.
        let ink_in = |xa: u16, xb: u16, ya: u16, yb: u16| {
            (ya..yb).flat_map(|y| (xa..xb).map(move |x| (x, y))).filter(|&(x, y)| ink(px(&fb, x, y))).count()
        };
        let right = x0 + COVER_VIEW_SIDE;
        assert!(ink_in(right, SCREEN_W, y0 + COVER_VIEW_SIDE - 12, y0 + COVER_VIEW_SIDE) > 10, "indicator");
        let mid = y0 + COVER_VIEW_SIDE / 2;
        assert!(ink_in(0, x0, mid - 8, mid + 8) > 0, "left chevron");
        assert!(ink_in(right, SCREEN_W, mid - 8, mid + 8) > 0, "right chevron");
    }

    #[test]
    fn a_single_image_has_an_indicator_but_no_chevrons() {
        let fb = cover_screen(CoverView {
            index: 0,
            count: 1,
            title: String::new(),
            image: Some(solid_art_sq(COVER_VIEW_SIDE, CYAN)),
            failed: false,
        });
        let mid = COVER_VIEW_Y + COVER_VIEW_SIDE / 2;
        let ink_rows = |xa: u16, xb: u16| {
            (mid - 8..mid + 8).flat_map(|y| (xa..xb).map(move |x| (x, y))).filter(|&(x, y)| ink(px(&fb, x, y))).count()
        };
        assert_eq!(ink_rows(0, COVER_VIEW_X), 0);
        assert_eq!(ink_rows(COVER_VIEW_X + COVER_VIEW_SIDE, SCREEN_W), 0);
    }

    #[test]
    fn cover_screen_placeholders_for_loading_and_failure_are_never_blank() {
        let loading = cover_screen(CoverView { index: 0, count: 2, ..CoverView::default() });
        let failed = cover_screen(CoverView { index: 0, count: 2, failed: true, ..CoverView::default() });
        let box_ink = |fb: &FrameBuffer| {
            (COVER_VIEW_Y + 1..COVER_VIEW_Y + COVER_VIEW_SIDE - 1)
                .flat_map(|y| (COVER_VIEW_X + 1..COVER_VIEW_X + COVER_VIEW_SIDE - 1).map(move |x| (x, y)))
                .filter(|&(x, y)| px(fb, x, y) == FG || px(fb, x, y) == FG_MUTED)
                .count()
        };
        assert!(box_ink(&loading) > 0, "the loading box carries its label");
        assert!(box_ink(&failed) > 0, "the failure box carries `No preview`");
        assert_ne!(loading.pixels, failed.pixels);
        assert_eq!(px(&failed, COVER_VIEW_X, COVER_VIEW_Y), WARN, "failure border");
        // A picture of the wrong size (left over from the detail box) is not painted.
        let stale = cover_screen(CoverView { count: 1, image: Some(solid_art_sq(DETAIL_ART_SIDE, CYAN)), ..CoverView::default() });
        assert!((0..SCREEN_H).all(|y| (0..SCREEN_W).all(|x| px(&stale, x, y) != CYAN)));
    }

    #[test]
    fn cover_header_meta_is_the_clipped_sanitised_title() {
        let mut m = ScreenModel::default();
        m.cover_view.title = "x".repeat(80);
        let nav = NavStack::new();
        let (title, meta) = m.header_text(&Route::LibraryCover, &nav);
        assert_eq!(title, "Cover art");
        let meta = meta.unwrap();
        assert!(measure(&meta, TypeRole::Small) <= COVER_TITLE_MAX_PX);
        assert!(meta.ends_with(".."));
        m.cover_view.title.clear();
        assert_eq!(m.header_text(&Route::LibraryCover, &nav).1, None);
    }

    #[test]
    fn five_actions_with_a_banner_scroll_instead_of_clipping() {
        let mut m = detail_model(vec![cover_ref("a.jpg", None)]);
        m.banner = Some("Tap a noubin".into());
        assert_eq!(m.detail_actions().len(), 5);
        let mut nav = NavStack::new();
        nav.force_push(Route::LibraryDetail);
        nav.focus = 4; // Associate, the last row
        let fb = render(&m, &nav);
        assert_eq!(lit_in(&fb, CONTENT_BOTTOM, SCREEN_H - CONTENT_BOTTOM), 0, "nothing below the content box");
        let focused_rows = (CONTENT_TOP..CONTENT_BOTTOM)
            .filter(|&y| px(&fb, SPACE_1, y) == ACCENT)
            .count();
        assert!(focused_rows > 0, "the focused last action is visible");
    }

    /// A held-at-reading-distance panel needs big modules more than strong correction: when Low
    /// buys a coarser symbol than Medium, the coarser one is the one that scans.
    #[test]
    fn the_buy_qr_prefers_a_bigger_module_over_stronger_correction() {
        // 70 bytes: Medium needs version 5 (37 modules → 2 px), Low fits version 4 (33 → 3 px).
        let url = format!("https://example.bandcamp.com/album/{}", "a".repeat(35));
        assert_eq!(url.len(), 70);
        let (matrix, module) = store_qr(&url).expect("a 70-byte URL fits the box");
        assert_eq!(matrix.width, 33, "the Low symbol is the coarser one");
        assert_eq!(module, 3);
        assert_eq!(store_qr_module_px(matrix.width, STORE_QR_BOX), Some(3));
    }

    // ---- TRACK_01: the track action menu and the analysis screen --------------------------

    fn menu_model(position: u32) -> ScreenModel {
        let mut m = ScreenModel::default();
        m.library = vec![item(1, "A")];
        m.detail_playable_id = Some(1);
        let track = |p: u32, available: bool, supported: bool| TrackRow {
            position: p,
            title: format!("Track {p} with a rather long title that must be clipped"),
            artist: None,
            duration_ms: Some(65_000),
            available,
            supported,
            format: "M4A/AAC".into(),
        };
        m.detail_info = Some(DetailInfo {
            playable_id: 1,
            kind: Some(LibraryKind::Album),
            tracks: vec![track(0, true, true), track(1, true, false), track(2, false, true)],
            chapters: vec![],
            media_missing: false,
        });
        m.track_menu_pos = Some(position);
        m
    }

    fn nav_on(route: Route, focus: usize) -> NavStack {
        let mut nav = NavStack::new();
        nav.force_push(route);
        nav.focus = focus;
        nav
    }

    /// Ink anywhere between `y0` and the bottom of the screen.
    fn ink_from(fb: &FrameBuffer, y0: u16) -> usize {
        lit_in(fb, y0, SCREEN_H.saturating_sub(y0))
    }

    #[test]
    fn the_track_menu_paints_four_rows_inside_the_content_box() {
        let m = menu_model(0);
        for focus in 0..TrackAction::ALL.len() {
            let fb = render(&m, &nav_on(Route::LibraryTrackActions, focus));
            for row in 0..TrackAction::ALL.len() {
                assert!(row_lit_no_gutter(&fb, row) > 0, "row {row} is painted");
            }
            assert_eq!(
                row_lit_no_gutter(&fb, TrackAction::ALL.len()),
                0,
                "no fifth row"
            );
            assert_eq!(ink_from(&fb, CONTENT_BOTTOM), 0, "nothing below the content box");
            // The focused row carries the ring and no other row does.
            let ring = |row: usize| {
                let y = CONTENT_TOP + row as u16 * ROW_H;
                (MARGIN_X..SCREEN_W - 2)
                    .any(|x| px(&fb, x, y) == ACCENT || px(&fb, x, y + 1) == ACCENT)
            };
            for row in 0..TrackAction::ALL.len() {
                assert_eq!(ring(row), row == focus, "focus {focus}, row {row}");
            }
        }
    }

    #[test]
    fn the_track_menu_header_names_the_track_without_overflowing() {
        let m = menu_model(0);
        let nav = nav_on(Route::LibraryTrackActions, 0);
        let (title, meta) = m.header_text(&Route::LibraryTrackActions, &nav);
        assert_eq!(title, "Track");
        let meta = meta.expect("the track title");
        assert!(meta.starts_with("Track 0"));
        assert!(
            measure(&meta, TypeRole::Small) <= COVER_TITLE_MAX_PX,
            "the clipped title fits its slot"
        );
    }

    #[test]
    fn an_unplayable_track_still_opens_its_menu_and_says_why_on_play() {
        // Unsupported (row 1) and missing (row 2) both reach the menu; Play carries the reason.
        for position in [1, 2] {
            let m = menu_model(position);
            let fb = render(&m, &nav_on(Route::LibraryTrackActions, 0));
            assert!(row_lit_no_gutter(&fb, 0) > 0);
            assert_eq!(ink_from(&fb, CONTENT_BOTTOM), 0);
        }
        let m = menu_model(1);
        let reason = ScreenModel::track_unplayable_reason(m.track_menu_row().unwrap());
        assert_eq!(reason.as_deref(), Some("M4A/AAC is not supported by this Plippa"));
        let m = menu_model(2);
        assert_eq!(
            ScreenModel::track_unplayable_reason(m.track_menu_row().unwrap()).as_deref(),
            Some("Media file missing")
        );
        assert!(ScreenModel::track_unplayable_reason(menu_model(0).track_menu_row().unwrap())
            .is_none());
    }

    #[test]
    fn a_track_that_left_the_list_shows_the_not_found_state_not_a_blank_screen() {
        let m = menu_model(9);
        assert!(m.track_menu_row().is_none());
        let fb = render(&m, &nav_on(Route::LibraryTrackActions, 0));
        assert!(lit_in(&fb, CONTENT_TOP, CONTENT_BOTTOM - CONTENT_TOP) > 0, "a message is shown");
        assert_eq!(row_lit_no_gutter(&fb, 0), 0, "and no action rows");
        assert_eq!(ink_from(&fb, CONTENT_BOTTOM), 0);
    }

    fn analysis_model(
        kinds: Vec<crate::analysis::AnalysisKindStatus>,
        phase: crate::analysis::AnalysisPhase,
    ) -> ScreenModel {
        let mut m = ScreenModel::default();
        let mut view = crate::analysis::AnalysisView::new(7, 0, "A very long track title".into(), 0);
        view.phase = phase;
        view.kinds = kinds;
        view.waiting = false;
        m.track_analysis = Some(view);
        m
    }

    fn analysed(skipped: u64) -> crate::analysis::AnalysisKindStatus {
        crate::analysis::AnalysisKindStatus {
            kind: crate::analysis::AnalysisKind::Waveform,
            state: crate::analysis::AnalysisState::Analysed,
            skipped_frames: skipped,
        }
    }

    #[test]
    fn the_analysis_screen_paints_every_kind_then_re_analyse_inside_the_content_box() {
        use crate::analysis::AnalysisPhase;
        let m = analysis_model(vec![analysed(0)], AnalysisPhase::Ready);
        // Two reachable rows (Waveform, Re-analyse): focus walks those, never BPM / Downbeat.
        for focus in 0..2 {
            let fb = render(&m, &nav_on(Route::LibraryTrackAnalysis, focus));
            for row in 0..4 {
                assert!(row_lit_no_gutter(&fb, row) > 0, "row {row} is painted");
            }
            assert_eq!(row_lit_no_gutter(&fb, 4), 0, "no fifth row");
            assert_eq!(ink_from(&fb, CONTENT_BOTTOM), 0);
            let ring = |row: usize| {
                let y = CONTENT_TOP + row as u16 * ROW_H;
                (MARGIN_X..SCREEN_W - 2)
                    .any(|x| px(&fb, x, y) == ACCENT || px(&fb, x, y + 1) == ACCENT)
            };
            let focused_row = if focus == 0 { 0 } else { 3 };
            for row in 0..4 {
                assert_eq!(ring(row), row == focused_row, "focus {focus}, row {row}");
            }
        }
    }

    #[test]
    fn the_longest_analysis_value_fits_its_row_beside_the_label() {
        // The widest state text the screen can build, at the widest label, on a 320 px row.
        let status = analysed(99);
        let value = status.text();
        let room = SCREEN_W - 2 * MARGIN_X;
        let used = measure("Waveform", TypeRole::Body) + measure(&value, TypeRole::Small);
        assert!(
            used + SPACE_2 <= room,
            "{used} px of label and value do not fit {room} px: {value:?}"
        );
    }

    #[test]
    fn a_missing_file_and_an_unreadable_status_explain_themselves_inside_the_box() {
        use crate::analysis::{AnalysisKindStatus, AnalysisKind, AnalysisPhase, AnalysisState};
        let missing = analysis_model(
            vec![AnalysisKindStatus {
                kind: AnalysisKind::Waveform,
                state: AnalysisState::Unavailable,
                skipped_frames: 0,
            }],
            AnalysisPhase::Ready,
        );
        let fb = render(&missing, &nav_on(Route::LibraryTrackAnalysis, 0));
        assert!(lit_in(&fb, CONTENT_TOP + 4 * ROW_H, 20) > 0, "the explanation line is painted");
        assert_eq!(ink_from(&fb, CONTENT_BOTTOM), 0);

        let failed = analysis_model(vec![], AnalysisPhase::Failed("down".into()));
        let fb = render(&failed, &nav_on(Route::LibraryTrackAnalysis, 0));
        assert!(row_lit_no_gutter(&fb, 3) > 0, "Try again is painted");
        assert_eq!(ink_from(&fb, CONTENT_BOTTOM), 0);

        let loading = analysis_model(vec![], AnalysisPhase::Loading);
        let fb = render(&loading, &nav_on(Route::LibraryTrackAnalysis, 0));
        assert_eq!(ink_from(&fb, CONTENT_BOTTOM), 0);
    }
}
