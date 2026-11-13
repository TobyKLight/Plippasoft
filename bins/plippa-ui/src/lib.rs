//! `plippa-ui` — local screen, inputs, NFC tap ownership (MVP_08 / MVP_11 / MVP_15).
//!
//! # NFC ownership
//!
//! UI polls NFC via `plippa-hw`, completes a full NDEF read before feedback
//! (Noubin §3.2), then forwards raw bytes to the engine as [`IpcMessage::TapNdef`].
//! Miss / error notices from the engine are shown on screen.
//!
//! On poc2, AW9523 + PN532 share i2c-1 under a dedicated I²C service thread;
//! ILI9341 SPI and WS2811 run on separate display/LED workers. The coordinator
//! only drains channels and enqueues frames/cues (see `docs/design/ui-threading.md`).
//!
//! # Library browse
//!
//! Sourced from a **read-only WAL** SQLite reader (`LibrarySource`) — see
//! [`library`] module docs. Non-blocking for the UI frame loop. NB_12: category root →
//! A-Z list → detail (Play / Browse tracks / Browse chapters / Associate); the UI only
//! sends `LoadCollection` / `JumpToEntry`, the engine owns the `nowPlayingList`.
//!
//! # Wi‑Fi
//!
//! [`WifiBackend`] trait; [`MockWifi`] for tests, [`NmcliWifi`] (NetworkManager)
//! for the device.
//!
//! # Integration harness
//!
//! Tests use an **in-process** harness (UI app + engine + mock NFC/display), not
//! multi-process UDS. Documented in `tests/integration_harness.rs`.

mod accessories;
mod analysis;
mod accessory_worker;
mod app;
pub mod art;
mod assign;
mod audio;
pub mod build_info;
mod compose;
mod control;
mod control_roles;
mod diag;
mod display;
mod display_worker;
mod hold;
pub mod hw;
mod i2c_service;
mod input;
mod keyboard;
mod led;
mod led_client;
mod led_compose;
mod led_worker;
mod library;
mod loop_runner;
mod nav;
mod network;
mod pinpad;
mod play_mode;
mod power;
mod record_leds;
mod screens;
mod store_api;
mod store_browse;
mod store_tap;
mod store_worker;
mod switches;
mod sys;
/// Design system: tokens, type roles, primitives and shared widgets. Every screen paints
/// through this module (see its rustdoc for the rules). Public so the pixel-level integration
/// tests can assert against [`ui::tokens`]; no other crate uses it.
pub mod ui;
mod warp_ui;
mod wifi;
mod wifi_nmcli;

pub use app::{consent_skipped_log_line, default_mock_wifi, NavAvailability, TransportLedState, UiApp, UiConfig, UiOutgoing};
pub use analysis::{
    parse_analysis, AnalysisAction, AnalysisKind, AnalysisKindStatus, AnalysisPhase, AnalysisRow,
    AnalysisState, AnalysisView, ANALYSIS_POLL_MS,
};
pub use assign::{state_file_path, AssignableId, AssignStore, ASSIGN_FILE_NAME, ENV_STATE_DIR};
pub use accessories::{
    accessory_error_text, kind_text, pair_failure_text, prompt_keyboard, AccessoriesState,
    AccessorySettings, AccessoryStore, FoundItem, PairStage, ACCESSORY_FILE_NAME,
    MIN_SCAN_SECONDS,
};
pub use accessory_worker::{AccessoryCmd, AccessoryHandle, AccessoryMsg, AccessoryWorker};
pub use audio::{
    delay_blocked, delay_step, volume_step, wanted_from_route, AudioRow, AudioSettings,
    AudioState, AudioStore, AudioView, AUDIO_FILE_NAME, AUDIO_ROWS,
};
pub use play_mode::{
    speed_percent_text, speed_row_ladder, speed_row_step, warp_mode_text, DebugRow, PlayModesRow,
    PlayModeRow, PlayModeSettings, PlayModeStore, PlayModeValues, PlayModeView, SettingsRow,
    TimecodeUnit, DEBUG_ROWS, PLAYBACK_SETTINGS_ROWS, PLAY_MODE_FILE_NAME, PLAY_MODE_ROWS,
    PLAY_MODES_ROWS,
};
pub use compose::{
    Compositor, StatusPopup, MAIN_MENU_H, MAIN_MENU_IDLE_MS, SCREEN_H, SCREEN_W,
    STATUS_POPUP_IDLE_MS, STATUS_POPUP_MAX_H, STATUS_POPUP_MAX_LINES,
};
pub use control::{
    AdjustParam, ControlGraph, FocusableItem, InteractionMode, NowPlayingFocus, NpMode, NpPush, NpTarget, PARAM_RING,
};
pub use diag::{audio_report, DiagLine};
pub use display::{DisplayBackend, FrameBuffer, MockDisplay, Rect, Rgb565};
pub use display_worker::{spawn_display_worker, DisplayProxy, DisplayWorkerHandle};
pub use hold::{HoldConfig, HoldDetector, HoldEvent, HoldTarget};
pub use i2c_service::{
    ChannelInput, I2cServiceHandle, NfcChannelClient, NfcServiceEvent, SwitchRoles,
};
pub use input::{InputEvent, InputSource, MockInput};
pub use led::{
    LedCue, LogLedCue, MockLedCue, NullLedCue, PlayButtonLed, Rgb, TransportPulse,
    TRANSPORT_LED_IDLE, TRANSPORT_LED_PRESSED,
};
pub use led_client::{LedsClient, RECONNECT_MAX, RECONNECT_MIN};
pub use led_compose::{LedComposer, LedLayout};
pub use led_worker::{spawn_led_worker, LedCmd, LedProxy, LedWorkerHandle};
pub use library::{sort_library_items, LibrarySource, ENV_LIBRARY_ROOT};
pub use loop_runner::{
    flash_nav_leds, run_ui_loop, sync_nav_leds, sync_playmode_led, sync_power_led, sync_record_led, LoopExit,
    LoopLimits, LoopState, NfcBackend, PendingAssociate,
};
pub use nav::{NavAction, NavStack, Route};
pub use network::{NetworkPhase, NetworkRow, NetworkView};
pub use pinpad::{PinAction, PinEvent, PinPad};
pub use store_api::{
    CollectionPageInfo, DeleteDone, DeletePreviewInfo, DownloadConsent, DownloadProgress,
    DownloadSession, DownloadState, HttpStoreApi, NoStoreApi, PortalAddress, PortalSettingsInfo, ReleaseInfo,
    ReleaseRow, ReleaseTrackInfo, StoreApi, StoreApiError, StoreInfo, StoreRelease, StoresInfo,
    CoverBytes, LookupConsentInfo, SyncPhase, SyncStatus, TapAnswer, TapBlocked, TapCandidate,
    TapState, MAX_COVER_BYTES,
};
pub use store_browse::{
    blocked_for, blocked_message, ConnectAction, ConnectPhase, ConnectView, DeleteAction,
    DeletePhase, DeleteView, DownloadPhase, DownloadedRelease, ReleaseAction, ReleasePhase,
    ReleaseView, ReleasesPhase, ReleasesRow, ReleasesView, StoreBlocked, StoresAction, StoresPhase,
    StoresView, SyncNote, CONNECT_INSTRUCTION, RELEASE_PAGE_SIZE,
};
pub use store_tap::{
    CoverState, StoreTapAction, StoreTapDecision, StoreTapPhase, StoreTapView, TapRelease,
    DOWNLOAD_POLL_MS, MAX_TAP_RETRIES, STORE_TIMEOUT_MS, SYNC_POLL_MS,
};
pub use store_worker::{StoreEvent, StoreRequest, StoreWorker};
pub use record_leds::{RecordFeed, RecordFrame, RecordInput, RecordUpdate};
pub use power::{PowerHold, PowerHoldConfig, PowerHoldEvent};
pub use screens::{
    kind_label, screen_text, BrowseRequest, ChapterRow, DetailInfo, LibraryDetailAction, TrackAction,
    LibraryItem, SCREEN_TEXT_MAX_CHARS, ScreenModel, SystemStatus, TrackRow, WaveState, WaveView,
    UP_NEXT_HEADER_Y,
    NP_ART_SIDE, NP_ART_X, NP_ART_Y, COVER_VIEW_SIDE, COVER_VIEW_X, COVER_VIEW_Y, DETAIL_ART_SIDE,
    CoverView, DetailCovers, StoreItem, STORE_LOSSY_NOTE, STORE_QR_BOX, STORE_QR_MIN_MODULE_PX,
    STORE_QR_QUIET_MODULES, STORE_COVER_SIDE, store_qr_module_px, store_short_url,
    UP_NEXT_ROWS, UP_NEXT_ROWS_Y, UP_NEXT_TOP, WAVE_BAND_BOTTOM, WAVE_BAND_H, WAVE_BAND_Y,
    WAVE_CENTRE_Y, WAVE_FRAME_MS, WAVE_PLAYHEAD_X,
    PLAY_MODE_DESC_GAP, PLAY_MODE_ROW_PITCH, PLAY_MODE_VISIBLE_ROWS,
};
pub use switches::{SwitchDebouncer, SwitchEvent, SwitchId};
pub use sys::{
    ClockProvider, FanProvider, MockClock, MockFan, MockTemp, SysProviders, SysfsFan, SysfsTemp,
    SystemClock, TempProvider,
};
pub use warp_ui::{
    ffrw_led_level, ffrw_pulse_ff, ffrw_pulse_rw, jog_multiplier, ladder_step, ladder_step_enabled,
    ladder_step_with, push_coalesced, scan_speed_text, tap_end_text, tap_rate_text,
    tempo_apply_click,
    tempo_step_permille, warp_settings_from_json, warp_settings_to_json, Commit, DesiredRate,
    HoldTap, HoldTapEvent, JogAccel, JogClick, JogDisplay, JogGesture, JogView, NeighbourCtx,
    StatusSample,
    TempoSticky, Velocity,
    FFRW_LED_IDLE, FFRW_LED_PRESSED, FFRW_LED_SLOW_BLUE, HOLD_START_MS, JOG_COMMIT_IDLE_MS,
    LADDER_RUNGS, LONG_REVERSE_AVAILABLE, PLAYMODE_HOLD_MS,
};
pub use wifi::{
    KnownAction, MockWifi, WifiBackend, WifiConnectResult, WifiForm, WifiFormEvent, WifiPhase,
    WifiRow,
};
pub use wifi_nmcli::NmcliWifi;
pub use keyboard::{CharsetRegion, KeyboardAction, KeyboardEvent, KeyboardField, CHARSET};

/// True for `NAME=1` / `NAME=true` / `NAME=yes` (unset, empty and `0`/`false` are false).
///
/// Used by the screen-fault bisect switches (`PLIPPA_UI_NO_*`); reading the environment
/// once per decision keeps the production path unchanged when no switch is set.
pub fn env_flag(name: &str) -> bool {
    std::env::var_os(name).is_some_and(|v| {
        let v = v.to_string_lossy();
        !v.is_empty() && v != "0" && !v.eq_ignore_ascii_case("false")
    })
}

#[cfg(test)]
mod env_flag_tests {
    use super::env_flag;

    #[test]
    fn env_flag_reads_truthy_values_only() {
        let name = "PLIPPA_TEST_ENV_FLAG_PARSE";
        std::env::remove_var(name);
        assert!(!env_flag(name));

        for (value, expected) in [
            ("1", true),
            ("true", true),
            ("yes", true),
            ("0", false),
            ("", false),
            ("false", false),
            ("FALSE", false),
        ] {
            std::env::set_var(name, value);
            assert_eq!(env_flag(name), expected, "value {value:?}");
        }
        std::env::remove_var(name);
    }
}
