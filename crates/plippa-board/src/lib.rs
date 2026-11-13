//! Board profile loader: TOML → [`Capabilities`] + pin/role map.
//!
//! Profiles live under `boards/prototypes/<id>/config/board.toml` (POC) or
//! `boards/<id>/config/board.toml` (production).

use plippa_core::{AudioFormatSupport, Capabilities};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use thiserror::Error;

/// Errors loading or validating a board profile.
#[derive(Debug, Error)]
pub enum BoardError {
    #[error("board profile not found for id `{0}`")]
    NotFound(String),
    #[error("read {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("parse board.toml: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("{0}")]
    Invalid(String),
}

/// Full loaded board profile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoardProfile {
    pub capabilities: Capabilities,
    pub model: ProductModel,
    pub display: DisplayConfig,
    pub nfc: NfcConfig,
    pub roles: RoleMap,
    pub encoders: BTreeMap<String, EncoderPins>,
    pub buttons: BTreeMap<String, ButtonRef>,
    /// WS2811 data streams (`[led_streams.*]`), by stream name.
    pub led_streams: BTreeMap<String, LedStream>,
    /// LED role → span on a stream (`[leds.*]`). Missing role = board lacks that LED.
    pub leds: BTreeMap<LedRole, LedSpan>,
    pub expander: ExpanderConfig,
    pub audio: AudioHwConfig,
    pub fan: FanConfig,
    /// Global input policy (debounce, etc.) — expander and direct GPIO alike.
    pub inputs: InputsConfig,
    /// Absolute path of the directory that contained `board.toml`.
    pub config_dir: PathBuf,
}

/// Product family (concept.md Model D / Model P).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum ProductModel {
    D,
    P,
}

impl Default for ProductModel {
    fn default() -> Self {
        Self::D
    }
}

/// Local display wiring (POC2: ILI9341 SPI).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DisplayConfig {
    pub driver: String,
    /// Native panel size before rotation (ILI9341 GRAM is 240×320).
    pub width: u16,
    pub height: u16,
    /// Clockwise rotation in degrees: 0, 90, 180, or 270.
    /// Model D front panel is landscape → typically `90` or `270`.
    #[serde(default)]
    pub rotation: u16,
    /// Mirror across the display’s horizontal axis (fixes backwards text).
    #[serde(default)]
    pub mirrored: bool,
    #[serde(default = "default_spi_dev")]
    pub spi_dev: String,
    /// Panel colour-component order: `"rgb"` (default) or `"bgr"`.
    ///
    /// ILI9341 modules come both ways round, and the wrong value swaps red and blue for every
    /// pixel on the panel (a photo looks like its blue/red are exchanged). It cannot be seen by
    /// the software (the framebuffer is always correct RGB565), so it is declared here.
    #[serde(default = "default_color_order")]
    pub color_order: String,
    pub dc_gpio: u32,
    pub rst_gpio: u32,
    pub bl_gpio: u32,
}

fn default_spi_dev() -> String {
    "/dev/spidev0.0".into()
}

fn default_color_order() -> String {
    "rgb".into()
}

impl Default for DisplayConfig {
    fn default() -> Self {
        Self {
            driver: "ili9341".into(),
            width: 240,
            height: 320,
            rotation: 0,
            mirrored: false,
            spi_dev: default_spi_dev(),
            color_order: default_color_order(),
            dc_gpio: 24,
            rst_gpio: 25,
            bl_gpio: 16,
        }
    }
}

impl DisplayConfig {
    /// Logical framebuffer size after [`Self::rotation`] (axes swap at 90°/270°).
    pub fn logical_size(&self) -> (u16, u16) {
        match self.rotation {
            90 | 270 => (self.height, self.width),
            _ => (self.width, self.height),
        }
    }

    /// `true` when the panel is wired BGR (`MADCTL` BGR bit set), which swaps red and blue.
    pub fn bgr_panel(&self) -> bool {
        self.color_order.trim().eq_ignore_ascii_case("bgr")
    }
}

/// NFC backend selection for the board.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NfcConfig {
    pub backend: String,
    #[serde(default = "default_i2c_bus")]
    pub i2c_bus: u32,
    #[serde(default = "default_pn532_addr")]
    pub address: u8,
    /// InListPassiveTarget timeout when no tag answers (ms). Shorter = snappier
    /// shared-bus UI; tag-present usually returns earlier than this ceiling.
    #[serde(default = "default_inlist_timeout_ms")]
    pub inlist_timeout_ms: u64,
    /// Idle field-check rate (Hz). Interval = 1000 / poll_hz.
    #[serde(default = "default_nfc_poll_hz")]
    pub poll_hz: u32,
}

fn default_i2c_bus() -> u32 {
    1
}
fn default_pn532_addr() -> u8 {
    0x24
}
fn default_inlist_timeout_ms() -> u64 {
    40
}
fn default_nfc_poll_hz() -> u32 {
    5
}

impl NfcConfig {
    /// Idle poll interval derived from [`Self::poll_hz`] (min 1 Hz).
    pub fn poll_interval_ms(&self) -> u64 {
        let hz = self.poll_hz.max(1);
        (1000 / u64::from(hz)).max(1)
    }
}

impl Default for NfcConfig {
    fn default() -> Self {
        Self {
            backend: "pn532_i2c".into(),
            i2c_bus: default_i2c_bus(),
            address: default_pn532_addr(),
            inlist_timeout_ms: default_inlist_timeout_ms(),
            poll_hz: default_nfc_poll_hz(),
        }
    }
}

/// Logical UI roles → hardware control names.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct RoleMap {
    /// Combined volume + hold-to-power encoder (Model D).
    pub volume_encoder: String,
    /// Menu navigate / push-to-enter.
    pub menu_encoder: String,
    /// Pop navigation stack.
    pub back_button: String,
    /// Explicit power control name (often the volume encoder push).
    pub power_button: String,
    /// Front play/stop (optional; Model D POC2 = `BT_SW3`).
    #[serde(default)]
    pub play_stop_button: Option<String>,
    /// Assignable Encoder A (optional; Model D POC2 = `SWE3`). Encoder B is separate.
    #[serde(default)]
    pub assignable_encoder_a: Option<String>,
    /// Previous-track button (optional; POC2 = `BT_SW1`).
    #[serde(default)]
    pub prev_track_button: Option<String>,
    /// Next-track button (optional; POC2 = `BT_SW5`).
    #[serde(default)]
    pub next_track_button: Option<String>,
    /// Rewind button (optional; POC2 = `BT_SW2`).
    #[serde(default)]
    pub rewind_button: Option<String>,
    /// Fast-forward button (optional; POC2 = `BT_SW4`).
    #[serde(default)]
    pub fast_forward_button: Option<String>,
    /// Record button (optional; POC2 = `BT_SW6`).
    #[serde(default)]
    pub record_button: Option<String>,
    /// Play-mode button (optional; POC2 = front `SW2`).
    #[serde(default)]
    pub play_mode_button: Option<String>,
}

/// Logical push-button / switch roles. App code asks for these; the board
/// profile says whether the board has the control and where it is patched.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ControlRole {
    /// Menu encoder push (enter / select).
    MenuEnter,
    /// Pop the navigation stack.
    MenuBack,
    /// Power / hold-to-shutdown push.
    Power,
    /// Play / pause (front transport button).
    Play,
    PrevTrack,
    NextTrack,
    Rewind,
    FastForward,
    Record,
    PlayMode,
    /// Push contact of Assignable Encoder A.
    AssignableAPush,
}

impl ControlRole {
    pub const ALL: [ControlRole; 11] = [
        ControlRole::MenuEnter,
        ControlRole::MenuBack,
        ControlRole::Power,
        ControlRole::Play,
        ControlRole::PrevTrack,
        ControlRole::NextTrack,
        ControlRole::Rewind,
        ControlRole::FastForward,
        ControlRole::Record,
        ControlRole::PlayMode,
        ControlRole::AssignableAPush,
    ];

    /// Roles a board must provide for the UI to be usable.
    pub fn is_required(self) -> bool {
        matches!(
            self,
            ControlRole::MenuEnter | ControlRole::MenuBack | ControlRole::Power
        )
    }

    /// Stable snake_case name (logs, docs, `[roles]` keys where they exist).
    pub fn name(self) -> &'static str {
        match self {
            ControlRole::MenuEnter => "menu_enter",
            ControlRole::MenuBack => "menu_back",
            ControlRole::Power => "power",
            ControlRole::Play => "play",
            ControlRole::PrevTrack => "prev_track",
            ControlRole::NextTrack => "next_track",
            ControlRole::Rewind => "rewind",
            ControlRole::FastForward => "fast_forward",
            ControlRole::Record => "record",
            ControlRole::PlayMode => "play_mode",
            ControlRole::AssignableAPush => "assignable_a_push",
        }
    }
}

/// Logical rotary-encoder roles.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum EncoderRole {
    Volume,
    Menu,
    AssignableA,
}

impl EncoderRole {
    pub const ALL: [EncoderRole; 3] = [
        EncoderRole::Volume,
        EncoderRole::Menu,
        EncoderRole::AssignableA,
    ];

    pub fn name(self) -> &'static str {
        match self {
            EncoderRole::Volume => "volume",
            EncoderRole::Menu => "menu",
            EncoderRole::AssignableA => "assignable_a",
        }
    }
}

/// Logical LED roles (`[leds.<role>]` keys in `board.toml`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LedRole {
    /// Play button backlight (transport colour).
    PlayButton,
    PrevTrackButton,
    NextTrackButton,
    RewindButton,
    FastForwardButton,
    RecordButton,
    PlayModeButton,
    MenuBackButton,
    /// Ring around the NFC reader (one span, `count > 1`).
    NfcRing,
    PowerLed,
    /// Left / right volume meters (spans).
    VuLeft,
    VuRight,
    /// Level meters around the assignable encoders A / B (spans).
    AssignableAMeter,
    AssignableBMeter,
}

impl LedRole {
    pub const ALL: [LedRole; 14] = [
        LedRole::PlayButton,
        LedRole::PrevTrackButton,
        LedRole::NextTrackButton,
        LedRole::RewindButton,
        LedRole::FastForwardButton,
        LedRole::RecordButton,
        LedRole::PlayModeButton,
        LedRole::MenuBackButton,
        LedRole::NfcRing,
        LedRole::PowerLed,
        LedRole::VuLeft,
        LedRole::VuRight,
        LedRole::AssignableAMeter,
        LedRole::AssignableBMeter,
    ];

    /// The `[leds.*]` key for this role.
    pub fn name(self) -> &'static str {
        match self {
            LedRole::PlayButton => "play_button",
            LedRole::PrevTrackButton => "prev_track_button",
            LedRole::NextTrackButton => "next_track_button",
            LedRole::RewindButton => "rewind_button",
            LedRole::FastForwardButton => "fast_forward_button",
            LedRole::RecordButton => "record_button",
            LedRole::PlayModeButton => "play_mode_button",
            LedRole::MenuBackButton => "menu_back_button",
            LedRole::NfcRing => "nfc_ring",
            LedRole::PowerLed => "power_led",
            LedRole::VuLeft => "vu_left",
            LedRole::VuRight => "vu_right",
            LedRole::AssignableAMeter => "assignable_a_meter",
            LedRole::AssignableBMeter => "assignable_b_meter",
        }
    }
}

/// Any role, for [`BoardProfile::has`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoardRole {
    Control(ControlRole),
    Encoder(EncoderRole),
    Led(LedRole),
}

impl From<ControlRole> for BoardRole {
    fn from(r: ControlRole) -> Self {
        Self::Control(r)
    }
}
impl From<EncoderRole> for BoardRole {
    fn from(r: EncoderRole) -> Self {
        Self::Encoder(r)
    }
}
impl From<LedRole> for BoardRole {
    fn from(r: LedRole) -> Self {
        Self::Led(r)
    }
}

/// One WS2811 data line (`[led_streams.<name>]`), e.g. `F_LED_DATA`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LedStream {
    /// Pi GPIO carrying the data line.
    pub gpio: u32,
    /// rs_ws281x channel (0 / 1) for that GPIO.
    pub channel: u8,
    /// Number of LEDs on the line.
    pub count: u16,
}

/// Where a LED role lives: `count` consecutive LEDs from `start` on `stream`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LedSpan {
    /// Key into `[led_streams]`.
    pub stream: String,
    pub start: u16,
    /// Span length (1 = single LED; rings/meters > 1).
    #[serde(default = "default_led_count")]
    pub count: u16,
    /// Optional wire colour order of this span (`grb`, `rgb`, …). The driver runs every
    /// strip as GRB; `plippa-ui`'s `LedComposer::driver_frames` compensates per span.
    /// Omitted = the strip's own order (GRB on plippa-D-poc2).
    #[serde(default)]
    pub color_order: Option<String>,
    /// Optional per-role brightness ceiling, 1–100 % of the global level.
    #[serde(default)]
    pub max_brightness_pct: Option<u8>,
}

fn default_led_count() -> u16 {
    1
}

/// Quadrature encoder A/B GPIO + optional push contact name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EncoderPins {
    pub a_gpio: u32,
    pub b_gpio: u32,
    /// Logical button name on the expander (e.g. `SWE1_SW`).
    #[serde(default)]
    pub push: Option<String>,
}

/// Button / switch reference (GPIO or expander pin id).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ButtonRef {
    Gpio { gpio: u32 },
    Expander { expander: String },
}

/// AW9523 (or similar) expander used for switch matrix on POC2.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExpanderConfig {
    #[serde(default = "default_expander_driver")]
    pub driver: String,
    #[serde(default = "default_i2c_bus")]
    pub i2c_bus: u32,
    #[serde(default = "default_aw9523_addr")]
    pub address: u8,
    pub rst_gpio: u32,
    #[serde(default)]
    pub int_gpio: Option<u32>,
}

fn default_expander_driver() -> String {
    "aw9523".into()
}
fn default_aw9523_addr() -> u8 {
    0x58
}

impl Default for ExpanderConfig {
    fn default() -> Self {
        Self {
            driver: default_expander_driver(),
            i2c_bus: default_i2c_bus(),
            address: default_aw9523_addr(),
            rst_gpio: 22,
            int_gpio: Some(23),
        }
    }
}

/// Board-wide input policy — applies to expander *and* direct-GPIO switches.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputsConfig {
    /// Edge cooldown (ms) for all switches. Fires on first sample of a new
    /// level; further edges on the same switch are ignored until this elapses.
    #[serde(default = "default_debounce_ms")]
    pub debounce_ms: u64,
}

fn default_debounce_ms() -> u64 {
    20
}

impl Default for InputsConfig {
    fn default() -> Self {
        Self {
            debounce_ms: default_debounce_ms(),
        }
    }
}

/// DAC / I2S notes for the board (PipeWire sink is OS-side).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct AudioHwConfig {
    #[serde(default)]
    pub dac: String,
    #[serde(default)]
    pub bck_gpio: Option<u32>,
    #[serde(default)]
    pub lrck_gpio: Option<u32>,
    #[serde(default)]
    pub data_gpio: Option<u32>,
    /// Exact PipeWire sink name of the board's built-in DAC (AUDIO_01). Filled in by
    /// AUDIO_OPS_01 for production boards; the engine still works when it is absent.
    #[serde(default)]
    pub local_sink: Option<String>,
    /// Substring matched against a sink id or one of its properties when the exact
    /// `local_sink` name moved (card profile change, kernel rename).
    #[serde(default)]
    pub local_sink_match: Option<String>,
}

/// Fan / thermal debug sources (CM4 IO board EMC2301).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct FanConfig {
    #[serde(default)]
    pub controller: String,
    #[serde(default)]
    pub notes: String,
}

#[derive(Debug, Deserialize)]
struct BoardFile {
    board: BoardMeta,
    capabilities: CapsFile,
    #[serde(default)]
    display: DisplayConfig,
    #[serde(default)]
    nfc: NfcConfig,
    roles: RoleMap,
    #[serde(default)]
    encoders: BTreeMap<String, EncoderPins>,
    #[serde(default)]
    buttons: BTreeMap<String, ButtonRef>,
    #[serde(default)]
    led_streams: BTreeMap<String, LedStream>,
    #[serde(default)]
    leds: BTreeMap<LedRole, LedSpan>,
    #[serde(default)]
    expander: ExpanderConfig,
    #[serde(default)]
    inputs: InputsConfig,
    #[serde(default)]
    audio: AudioHwConfig,
    #[serde(default)]
    fan: FanConfig,
}

#[derive(Debug, Deserialize)]
struct BoardMeta {
    id: String,
    #[serde(default)]
    model: ProductModel,
}

#[derive(Debug, Deserialize)]
struct CapsFile {
    nfc_write: bool,
    media_library_management: bool,
    local_display: bool,
    #[serde(default)]
    hdmi_output: bool,
    #[serde(default)]
    microphone: bool,
    #[serde(default)]
    sync_reset_io: bool,
    #[serde(default = "default_sr")]
    max_sample_rate_hz: u32,
    #[serde(default = "default_bits")]
    max_bit_depth: u8,
    #[serde(default)]
    audio_formats: AudioFormatsFile,
}

fn default_sr() -> u32 {
    48_000
}
fn default_bits() -> u8 {
    16
}

#[derive(Debug, Deserialize)]
struct AudioFormatsFile {
    #[serde(default = "default_true")]
    mp3: bool,
    #[serde(default = "default_true")]
    wav: bool,
    #[serde(default = "default_true")]
    flac: bool,
    #[serde(default)]
    aac: bool,
    #[serde(default)]
    opus: bool,
}

fn default_true() -> bool {
    true
}

impl Default for AudioFormatsFile {
    fn default() -> Self {
        Self {
            mp3: true,
            wav: true,
            flac: true,
            aac: false,
            opus: false,
        }
    }
}

impl BoardProfile {
    /// Load a board by id, searching known roots under `repo_root`.
    pub fn load(repo_root: impl AsRef<Path>, board_id: &str) -> Result<Self, BoardError> {
        let path = find_board_toml(repo_root.as_ref(), board_id)?;
        Self::load_file(&path)
    }

    /// Load from an explicit `board.toml` path.
    pub fn load_file(path: impl AsRef<Path>) -> Result<Self, BoardError> {
        let path = path.as_ref();
        let text = fs::read_to_string(path).map_err(|source| BoardError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        let config_dir = path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        Self::from_toml_str(&text, config_dir)
    }

    /// Parse + validate `board.toml` text (`config_dir` recorded as-is).
    pub fn from_toml_str(text: &str, config_dir: PathBuf) -> Result<Self, BoardError> {
        let parsed: BoardFile = toml::from_str(text)?;

        if parsed.roles.volume_encoder.is_empty()
            || parsed.roles.menu_encoder.is_empty()
            || parsed.roles.back_button.is_empty()
            || parsed.roles.power_button.is_empty()
        {
            return Err(BoardError::Invalid(
                "roles.volume_encoder, menu_encoder, back_button, power_button are required".into(),
            ));
        }
        if !parsed.encoders.contains_key(&parsed.roles.volume_encoder) {
            return Err(BoardError::Invalid(format!(
                "roles.volume_encoder `{}` missing from [encoders]",
                parsed.roles.volume_encoder
            )));
        }
        if !parsed.encoders.contains_key(&parsed.roles.menu_encoder) {
            return Err(BoardError::Invalid(format!(
                "roles.menu_encoder `{}` missing from [encoders]",
                parsed.roles.menu_encoder
            )));
        }
        validate_roles(&parsed.roles, &parsed.encoders, &parsed.buttons)?;
        validate_leds(&parsed.led_streams, &parsed.leds)?;
        let order = parsed.display.color_order.trim().to_ascii_lowercase();
        if order != "rgb" && order != "bgr" {
            return Err(BoardError::Invalid(format!(
                "display.color_order must be \"rgb\" or \"bgr\", got {:?}",
                parsed.display.color_order
            )));
        }

        let caps = Capabilities {
            board_id: parsed.board.id,
            nfc_write: parsed.capabilities.nfc_write,
            media_library_management: parsed.capabilities.media_library_management,
            local_display: parsed.capabilities.local_display,
            hdmi_output: parsed.capabilities.hdmi_output,
            microphone: parsed.capabilities.microphone,
            sync_reset_io: parsed.capabilities.sync_reset_io,
            max_sample_rate_hz: parsed.capabilities.max_sample_rate_hz,
            max_bit_depth: parsed.capabilities.max_bit_depth,
            audio_formats: AudioFormatSupport {
                mp3: parsed.capabilities.audio_formats.mp3,
                wav: parsed.capabilities.audio_formats.wav,
                flac: parsed.capabilities.audio_formats.flac,
                aac: parsed.capabilities.audio_formats.aac,
                opus: parsed.capabilities.audio_formats.opus,
            },
        };

        Ok(Self {
            capabilities: caps,
            model: parsed.board.model,
            display: parsed.display,
            nfc: parsed.nfc,
            roles: parsed.roles,
            encoders: parsed.encoders,
            buttons: parsed.buttons,
            led_streams: parsed.led_streams,
            leds: parsed.leds,
            expander: parsed.expander,
            inputs: parsed.inputs,
            audio: parsed.audio,
            fan: parsed.fan,
            config_dir,
        })
    }

    /// Button name patched to `role` (`[roles]`, or the push of a role's encoder).
    pub fn button_name_for(&self, role: ControlRole) -> Option<&str> {
        let r = &self.roles;
        match role {
            ControlRole::MenuEnter => self.encoder_push(&r.menu_encoder),
            ControlRole::AssignableAPush => r
                .assignable_encoder_a
                .as_deref()
                .and_then(|e| self.encoder_push(e)),
            ControlRole::MenuBack => Some(r.back_button.as_str()),
            ControlRole::Power => Some(r.power_button.as_str()),
            ControlRole::Play => r.play_stop_button.as_deref(),
            ControlRole::PrevTrack => r.prev_track_button.as_deref(),
            ControlRole::NextTrack => r.next_track_button.as_deref(),
            ControlRole::Rewind => r.rewind_button.as_deref(),
            ControlRole::FastForward => r.fast_forward_button.as_deref(),
            ControlRole::Record => r.record_button.as_deref(),
            ControlRole::PlayMode => r.play_mode_button.as_deref(),
        }
    }

    fn encoder_push(&self, encoder: &str) -> Option<&str> {
        self.encoders.get(encoder).and_then(|e| e.push.as_deref())
    }

    /// Where `role`'s button is wired; `None` = this board has no such control.
    pub fn button_for(&self, role: ControlRole) -> Option<&ButtonRef> {
        self.button_name_for(role).and_then(|n| self.buttons.get(n))
    }

    /// Encoder name (key in `[encoders]`) patched to `role`; `None` = board lacks it.
    pub fn encoder_name_for(&self, role: EncoderRole) -> Option<&str> {
        match role {
            EncoderRole::Volume => Some(self.roles.volume_encoder.as_str()),
            EncoderRole::Menu => Some(self.roles.menu_encoder.as_str()),
            EncoderRole::AssignableA => self.roles.assignable_encoder_a.as_deref(),
        }
    }

    /// Encoder pins for `role`; `None` = board lacks it.
    pub fn encoder_for(&self, role: EncoderRole) -> Option<&EncoderPins> {
        self.encoder_name_for(role)
            .and_then(|n| self.encoders.get(n))
    }

    /// LED span for `role`; `None` = board has no such LED(s).
    pub fn led_for(&self, role: LedRole) -> Option<&LedSpan> {
        self.leds.get(&role)
    }

    /// Does this board have `role` (control, encoder or LED)?
    pub fn has(&self, role: impl Into<BoardRole>) -> bool {
        match role.into() {
            BoardRole::Control(r) => self.button_for(r).is_some(),
            BoardRole::Encoder(r) => self.encoder_for(r).is_some(),
            BoardRole::Led(r) => self.led_for(r).is_some(),
        }
    }

    /// One-line summary of the *optional* roles this board has / lacks (startup log).
    pub fn optional_roles_summary(&self) -> String {
        let mut present = Vec::new();
        let mut absent = Vec::new();
        let mut note = |name: String, has: bool| {
            if has {
                present.push(name)
            } else {
                absent.push(name)
            }
        };
        for r in ControlRole::ALL.into_iter().filter(|r| !r.is_required()) {
            note(format!("control:{}", r.name()), self.has(r));
        }
        note(
            "encoder:assignable_a".into(),
            self.has(EncoderRole::AssignableA),
        );
        for r in LedRole::ALL {
            note(format!("led:{}", r.name()), self.has(r));
        }
        format!(
            "optional roles present [{}]; absent [{}]",
            present.join(", "),
            absent.join(", ")
        )
    }

    /// Model D–appropriate: local display + NFC write + library management.
    pub fn is_model_d_mvp_capable(&self) -> bool {
        self.model == ProductModel::D
            && self.capabilities.local_display
            && self.capabilities.nfc_write
            && self.capabilities.media_library_management
    }
}

fn invalid<T>(msg: String) -> Result<T, BoardError> {
    Err(BoardError::Invalid(msg))
}

/// Every role must point at a defined `[buttons.*]` / `[encoders.*]` entry.
fn validate_roles(
    roles: &RoleMap,
    encoders: &BTreeMap<String, EncoderPins>,
    buttons: &BTreeMap<String, ButtonRef>,
) -> Result<(), BoardError> {
    let button_roles: [(&str, Option<&String>); 9] = [
        ("back_button", Some(&roles.back_button)),
        ("power_button", Some(&roles.power_button)),
        ("play_stop_button", roles.play_stop_button.as_ref()),
        ("prev_track_button", roles.prev_track_button.as_ref()),
        ("next_track_button", roles.next_track_button.as_ref()),
        ("rewind_button", roles.rewind_button.as_ref()),
        ("fast_forward_button", roles.fast_forward_button.as_ref()),
        ("record_button", roles.record_button.as_ref()),
        ("play_mode_button", roles.play_mode_button.as_ref()),
    ];
    for (key, name) in button_roles {
        if let Some(name) = name {
            if !buttons.contains_key(name.as_str()) {
                return invalid(format!("roles.{key} `{name}` missing from [buttons]"));
            }
        }
    }
    let enc_roles = [
        ("volume_encoder", Some(&roles.volume_encoder)),
        ("menu_encoder", Some(&roles.menu_encoder)),
        ("assignable_encoder_a", roles.assignable_encoder_a.as_ref()),
    ];
    for (key, name) in enc_roles {
        if let Some(name) = name {
            if !encoders.contains_key(name.as_str()) {
                return invalid(format!("roles.{key} `{name}` missing from [encoders]"));
            }
        }
    }
    // Encoder pushes used as roles (menu enter, assignable A push) must be defined buttons.
    let push_roles = [
        ("menu_encoder", Some(&roles.menu_encoder), true),
        (
            "assignable_encoder_a",
            roles.assignable_encoder_a.as_ref(),
            false,
        ),
    ];
    for (key, enc, required) in push_roles {
        let Some(enc) = enc else { continue };
        match encoders.get(enc.as_str()).and_then(|e| e.push.as_ref()) {
            Some(push) if !buttons.contains_key(push.as_str()) => {
                return invalid(format!(
                    "encoders.{enc}.push `{push}` (roles.{key}) missing from [buttons]"
                ));
            }
            None if required => {
                return invalid(format!(
                    "encoders.{enc} (roles.{key}) needs a `push` button for the menu-enter role"
                ));
            }
            _ => {}
        }
    }
    Ok(())
}

const COLOR_ORDERS: [&str; 6] = ["rgb", "rbg", "grb", "gbr", "brg", "bgr"];

/// Every LED span must sit on a defined stream, inside it, without overlaps.
fn validate_leds(
    streams: &BTreeMap<String, LedStream>,
    leds: &BTreeMap<LedRole, LedSpan>,
) -> Result<(), BoardError> {
    let mut used: Vec<(&str, u32, u32, LedRole)> = Vec::new();
    for (role, span) in leds {
        let key = role.name();
        let Some(stream) = streams.get(&span.stream) else {
            return invalid(format!(
                "leds.{key}: stream `{}` missing from [led_streams]",
                span.stream
            ));
        };
        if span.count == 0 {
            return invalid(format!("leds.{key}: count must be >= 1"));
        }
        let end = u32::from(span.start) + u32::from(span.count);
        if end > u32::from(stream.count) {
            return invalid(format!(
                "leds.{key}: span {}..{end} exceeds stream `{}` ({} LEDs)",
                span.start, span.stream, stream.count
            ));
        }
        if let Some(pct) = span.max_brightness_pct {
            if pct == 0 || pct > 100 {
                return invalid(format!("leds.{key}: max_brightness_pct must be 1..=100"));
            }
        }
        if let Some(order) = span.color_order.as_deref() {
            if !COLOR_ORDERS.contains(&order) {
                return invalid(format!(
                    "leds.{key}: color_order `{order}` must be one of {COLOR_ORDERS:?}"
                ));
            }
        }
        for (s2, a, b, other) in &used {
            if *s2 == span.stream && u32::from(span.start) < *b && *a < end {
                return invalid(format!(
                    "leds.{key} overlaps leds.{} on stream `{}`",
                    other.name(),
                    span.stream
                ));
            }
        }
        used.push((span.stream.as_str(), u32::from(span.start), end, *role));
    }
    Ok(())
}

/// Search order: `boards/prototypes/<id>/config/board.toml`, then `boards/<id>/config/board.toml`.
pub fn find_board_toml(repo_root: &Path, board_id: &str) -> Result<PathBuf, BoardError> {
    let candidates = [
        repo_root
            .join("boards/prototypes")
            .join(board_id)
            .join("config/board.toml"),
        repo_root
            .join("boards")
            .join(board_id)
            .join("config/board.toml"),
    ];
    for path in &candidates {
        if path.is_file() {
            return Ok(path.clone());
        }
    }
    Err(BoardError::NotFound(board_id.into()))
}

/// Resolve monorepo root from this crate's location (dev / tests).
pub fn workspace_root_from_crate() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap_or_else(|_| {
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../..")
                .to_path_buf()
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_plippa_d_poc2_profile() {
        let root = workspace_root_from_crate();
        let profile = BoardProfile::load(&root, "plippa-D-poc2").expect("load poc2");
        assert_eq!(profile.capabilities.board_id, "plippa-D-poc2");
        assert_eq!(profile.model, ProductModel::D);
        assert!(profile.is_model_d_mvp_capable());
        assert!(profile.capabilities.local_display);
        assert!(profile.capabilities.nfc_write);
        assert_eq!(profile.display.width, 240);
        assert_eq!(profile.display.height, 320);
        assert_eq!(profile.display.rotation, 90);
        assert_eq!(profile.display.logical_size(), (320, 240));
        assert_eq!(profile.display.dc_gpio, 24);
        // This panel is a BGR module: driving it as RGB swapped red and blue on the glass
        // (the blue accent drew orange, the red banner drew blue).
        assert_eq!(profile.display.color_order, "bgr");
        assert!(profile.display.bgr_panel());
        assert_eq!(profile.roles.menu_encoder, "SWE5");
        assert_eq!(profile.roles.volume_encoder, "SWE1");
        assert_eq!(profile.roles.back_button, "SW6");
        assert_eq!(profile.roles.play_stop_button.as_deref(), Some("BT_SW3"));
        assert_eq!(profile.roles.assignable_encoder_a.as_deref(), Some("SWE3"));
        assert!(profile.encoders.contains_key("SWE1"));
        assert!(profile.encoders.contains_key("SWE3"));
        assert!(profile.encoders.contains_key("SWE5"));
        assert_eq!(profile.nfc.backend, "pn532_i2c");
        assert_eq!(profile.audio.dac, "pcm5102");
    }

    fn poc2() -> BoardProfile {
        BoardProfile::load(workspace_root_from_crate(), "plippa-D-poc2").expect("load poc2")
    }

    fn minimal() -> BoardProfile {
        BoardProfile::load_file(
            workspace_root_from_crate().join("crates/plippa-board/fixtures/minimal.board.toml"),
        )
        .expect("load minimal fixture")
    }

    fn expander(profile: &BoardProfile, role: ControlRole) -> Option<&str> {
        match profile.button_for(role)? {
            ButtonRef::Expander { expander } => Some(expander.as_str()),
            ButtonRef::Gpio { .. } => None,
        }
    }

    /// Regression: SWPATCH.csv wiring for plippa-D-poc2 (pin ids only appear in tests/profile).
    #[test]
    fn poc2_control_roles_match_swpatch() {
        let p = poc2();
        let want = [
            (ControlRole::MenuEnter, "AW9523_PB4"),
            (ControlRole::MenuBack, "AW9523_PA7"),
            (ControlRole::Power, "AW9523_PB1"),
            (ControlRole::Play, "AW9523_PA2"),
            (ControlRole::PrevTrack, "AW9523_PA0"),
            (ControlRole::Rewind, "AW9523_PA1"),
            (ControlRole::FastForward, "AW9523_PA3"),
            (ControlRole::NextTrack, "AW9523_PA4"),
            (ControlRole::Record, "AW9523_PA5"),
            (ControlRole::PlayMode, "AW9523_PA6"),
            (ControlRole::AssignableAPush, "AW9523_PB2"),
        ];
        assert_eq!(want.len(), ControlRole::ALL.len());
        for (role, pin) in want {
            assert_eq!(expander(&p, role), Some(pin), "{role:?}");
            assert!(p.has(role));
        }
        for role in EncoderRole::ALL {
            assert!(p.has(role), "{role:?}");
        }
        assert_eq!(p.encoder_for(EncoderRole::Menu).unwrap().a_gpio, 9);
        assert_eq!(p.encoder_for(EncoderRole::Volume).unwrap().a_gpio, 17);
        assert_eq!(p.encoder_for(EncoderRole::AssignableA).unwrap().a_gpio, 14);
    }

    /// Regression: LEDPATCH.csv indices for plippa-D-poc2.
    #[test]
    fn poc2_led_roles_match_ledpatch() {
        let p = poc2();
        let want = [
            (LedRole::MenuBackButton, "F_LED_DATA", 0, 1),
            (LedRole::VuLeft, "F_LED_DATA", 1, 10),
            (LedRole::VuRight, "F_LED_DATA", 11, 10),
            (LedRole::PowerLed, "F_LED_DATA", 21, 1),
            (LedRole::PlayModeButton, "F_LED_DATA", 22, 1),
            (LedRole::AssignableBMeter, "F_LED_DATA", 23, 5),
            (LedRole::AssignableAMeter, "F_LED_DATA", 28, 5),
            (LedRole::NfcRing, "BT_LED_DATA", 0, 24),
            (LedRole::PrevTrackButton, "BT_LED_DATA", 24, 1),
            (LedRole::RewindButton, "BT_LED_DATA", 25, 1),
            (LedRole::PlayButton, "BT_LED_DATA", 26, 1),
            (LedRole::FastForwardButton, "BT_LED_DATA", 27, 1),
            (LedRole::NextTrackButton, "BT_LED_DATA", 28, 1),
            (LedRole::RecordButton, "BT_LED_DATA", 29, 1),
        ];
        assert_eq!(want.len(), LedRole::ALL.len());
        for (role, stream, start, count) in want {
            let span = p
                .led_for(role)
                .unwrap_or_else(|| panic!("{role:?} missing"));
            assert_eq!(
                (span.stream.as_str(), span.start, span.count),
                (stream, start, count),
                "{role:?}"
            );
        }
        assert_eq!(p.led_streams["F_LED_DATA"].count, 33);
        assert_eq!(p.led_streams["BT_LED_DATA"].count, 30);
        assert_eq!(
            (
                p.led_streams["F_LED_DATA"].gpio,
                p.led_streams["F_LED_DATA"].channel
            ),
            (13, 1)
        );
        assert_eq!(
            (
                p.led_streams["BT_LED_DATA"].gpio,
                p.led_streams["BT_LED_DATA"].channel
            ),
            (12, 0)
        );
    }

    #[test]
    fn minimal_profile_reports_optional_roles_absent() {
        let p = minimal();
        for role in ControlRole::ALL.into_iter().filter(|r| !r.is_required()) {
            assert!(!p.has(role), "{role:?}");
            assert!(p.button_for(role).is_none());
        }
        assert!(p.has(ControlRole::MenuEnter));
        assert!(p.has(ControlRole::MenuBack));
        assert!(p.has(ControlRole::Power));
        assert!(!p.has(EncoderRole::AssignableA));
        for role in LedRole::ALL {
            assert!(!p.has(role));
            assert!(p.led_for(role).is_none());
        }
        let summary = p.optional_roles_summary();
        assert!(summary.contains("absent [") && summary.contains("control:prev_track"));
    }

    fn poc2_text() -> String {
        fs::read_to_string(
            workspace_root_from_crate().join("boards/prototypes/plippa-D-poc2/config/board.toml"),
        )
        .unwrap()
    }

    fn load_err(text: &str) -> String {
        BoardProfile::from_toml_str(text, PathBuf::from("."))
            .unwrap_err()
            .to_string()
    }

    #[test]
    fn role_pointing_at_undefined_button_is_a_clear_error() {
        let text = poc2_text().replace(
            "prev_track_button = \"BT_SW1\"",
            "prev_track_button = \"NOPE\"",
        );
        let msg = load_err(&text);
        assert!(
            msg.contains("roles.prev_track_button `NOPE` missing from [buttons]"),
            "{msg}"
        );
    }

    #[test]
    fn display_color_order_is_declared_and_validated() {
        let base = poc2_text();
        assert!(poc2().display.bgr_panel(), "POC2 declares bgr");
        let rgb = BoardProfile::from_toml_str(
            &base.replace("color_order = \"bgr\"", "color_order = \"rgb\""),
            PathBuf::from("."),
        )
        .expect("rgb is accepted");
        assert!(!rgb.display.bgr_panel());
        // Absent key: defaults to the standard order, so other boards are unaffected.
        let bare = BoardProfile::from_toml_str(
            &base.replace("color_order = \"bgr\"\n", ""),
            PathBuf::from("."),
        )
        .expect("default applies");
        assert_eq!(bare.display.color_order, "rgb");
        // A typo is a clear error, not a silently swapped panel.
        let msg = load_err(&base.replace("color_order = \"bgr\"", "color_order = \"gbr\""));
        assert!(msg.contains("display.color_order"), "{msg}");
    }

    #[test]
    fn led_role_errors_are_readable() {
        let base = poc2_text();
        let msg = load_err(&base.replace(
            "stream = \"BT_LED_DATA\"\nstart = 26",
            "stream = \"NOPE\"\nstart = 26",
        ));
        assert!(
            msg.contains("leds.play_button: stream `NOPE` missing from [led_streams]"),
            "{msg}"
        );

        let msg = load_err(&base.replace("start = 29", "start = 30"));
        assert!(
            msg.contains("leds.record_button") && msg.contains("exceeds stream"),
            "{msg}"
        );

        let msg = load_err(&base.replace("start = 25", "start = 26"));
        assert!(msg.contains("overlaps"), "{msg}");

        let msg = load_err(&base.replace("[leds.play_button]", "[leds.no_such_led]"));
        assert!(msg.contains("no_such_led"), "{msg}");

        let msg = load_err(&base.replace(
            "[leds.vu_left]      ",
            "[leds.vu_left]\nmax_brightness_pct = 0\n#",
        ));
        assert!(msg.contains("max_brightness_pct"), "{msg}");
    }

    #[test]
    fn menu_encoder_without_push_is_rejected() {
        let text = fs::read_to_string(
            workspace_root_from_crate().join("crates/plippa-board/fixtures/minimal.board.toml"),
        )
        .unwrap()
        .replace("push = \"ENC_MENU_PUSH\"\n", "");
        let msg = load_err(&text);
        assert!(msg.contains("needs a `push` button"), "{msg}");
    }

    /// Profiles written before NB_13 (no new keys) still load.
    #[test]
    fn legacy_profile_without_new_keys_loads() {
        let mut text = poc2_text();
        for key in [
            "prev_track_button",
            "next_track_button",
            "rewind_button",
            "fast_forward_button",
            "record_button",
            "play_mode_button",
        ] {
            text = text
                .lines()
                .filter(|l| !l.starts_with(key))
                .collect::<Vec<_>>()
                .join("\n");
        }
        let cut = text.find("# WS2811 data lines").unwrap();
        let end = text.find("[inputs]").unwrap();
        text.replace_range(cut..end, "");
        let p = BoardProfile::from_toml_str(&text, PathBuf::from(".")).expect("legacy loads");
        assert!(p.has(ControlRole::Play));
        assert!(!p.has(ControlRole::PrevTrack));
        assert!(p.leds.is_empty());
    }

    #[test]
    fn audio_local_sink_values_parse_and_default_to_none() {
        let p = BoardProfile::load(workspace_root_from_crate(), "plippa-D-poc2").expect("load poc2");
        assert_eq!(
            p.audio.local_sink.as_deref(),
            Some("alsa_output.platform-soc_sound.stereo-fallback")
        );
        assert_eq!(
            p.audio.local_sink_match.as_deref(),
            Some("platform-soc_sound")
        );

        // Explicit values parse from text (including a sink whose exact name moved).
        let text = poc2_text()
            .replace(
                "alsa_output.platform-soc_sound.stereo-fallback",
                "alsa_output.platform-soc_audio.stereo-fallback",
            )
            .replace(
                "local_sink_match = \"platform-soc_sound\"",
                "local_sink_match = \"builtin\"",
            );
        let q = BoardProfile::from_toml_str(&text, PathBuf::from(".")).expect("parse audio keys");
        assert_eq!(
            q.audio.local_sink.as_deref(),
            Some("alsa_output.platform-soc_audio.stereo-fallback")
        );
        assert_eq!(q.audio.local_sink_match.as_deref(), Some("builtin"));

        // A profile with no `[audio]` section (the minimal fixture) has neither value.
        let m = minimal();
        assert_eq!(m.audio.local_sink, None);
        assert_eq!(m.audio.local_sink_match, None);
    }

    #[test]
    fn missing_board_is_not_found() {
        let root = workspace_root_from_crate();
        let err = BoardProfile::load(&root, "no-such-board").unwrap_err();
        assert!(matches!(err, BoardError::NotFound(_)));
    }
}
