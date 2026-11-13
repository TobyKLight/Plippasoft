//! Navigation stack: menu rotate / enter / back.
//!
//! The root **main menu** is a compositor overlay (not a full-screen [`Route`]).
//! [`NavStack::back`] from Now Playing yields [`NavAction::OpenMainMenu`].
//! Entering Library / System still pushes a route onto the stack; entering
//! **Now Playing** ([`Route::Main`]) resets the stack to `[Main]`.
//!
//! Main menu: **Now Playing**, **Library**, **System**. System submenu: **Network** (IP address,
//! web portal switch, certificate, portal PIN, Wi‑Fi), **Playback settings** (the session play
//! modes on top, then the persisted settings), **Stores**, **Audio**, **Debug**. The old
//! diagnostic Debug / Warnings routes are demoted (force_push / lab only — not on the main menu).
//!
//! Library flow (UI_03 + NB_12): category root → category list → detail →
//! (browse tracks | browse chapters | associate-wait). Associate is not a
//! top-level main-menu item. Pushing a child remembers the parent's focus and
//! [`NavStack::back`] restores it (selection memory per level).
//!
//! UI_16 §7: the encoder reads top-down in every menu-style list (see [`menu_step`]).

/// Screen routes for the MVP local UI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Route {
    Main,
    /// Legacy full-screen menu route — prefer the main-menu overlay.
    /// Kept so older tests / force_push paths still compile; painting is overlay-sized.
    Menu,
    /// System submenu (Network / Playback settings / ...).
    System,
    /// System > Network (SECFIX_02): **IP address**, **Web portal**, **Web portal PIN required**,
    /// **Web portal PIN**, **Wi‑Fi**.
    SystemNetwork,
    /// System > Network > Web portal PIN: the current digits and a keypad for a new PIN.
    SystemNetworkPin,
    /// System > Network > View certificate (SECFIX_03): the fingerprint a browser shows, with
    /// the words for comparing it. The menu itself does not print the fingerprint.
    SystemCertificate,
    /// Scrollable list of all IP addresses (System > Network > IP address).
    SystemIp,
    /// System > Playback settings: the engine's four session play modes first (**Shuffle**,
    /// **Repeat**, **Pause after each**, **Repeat reshuffles**), then **Playback speed** (live,
    /// session-only), speed warp, seek warp, seek outside track, default zoom, fine time and
    /// Art LEDs (WARP_10 / UI_16 / PLAY_04). The old second screen, "Play modes", is folded in
    /// here; the session rows are never persisted.
    SystemPlayMode,
    /// System > Audio (AUDIO_04): **Output devices**, **Delay**, **Waveform vis delay**.
    SystemAudio,
    /// Output devices: the Local only mode plus one tick row per sink.
    SystemAudioOutputs,
    /// Delay: Auto / Manual plus one ms row per enabled output.
    SystemAudioDelay,
    /// System > Audio > Accessories (ACC_03): paired accessories plus `Pair new…`.
    SystemAudioAccessories,
    /// One paired accessory: connect / auto connect / forget.
    SystemAudioAccessory,
    /// Pair new (ACC_03): scan result, pairing prompt, status.
    SystemAudioPairNew,
    Warnings,
    Wifi,
    /// Legacy network info screen (kept for `force_push`; prefer [`Route::SystemIp`]).
    NetworkInfo,
    /// Library root: one row per category (Tracks, Albums, Playlists, …).
    Library,
    /// A-Z list of the items of one category (`ScreenModel::browse_kind`).
    LibraryCategory,
    /// Track detail (title / artist / length / art) with Play + Associate actions.
    LibraryDetail,
    /// Entries of the selected collection; picking one plays from there.
    LibraryTracks,
    /// TRACK_01: what to do with one track of the collection: Play, Cover art, Associate,
    /// Analysis. Pushed by a press on a track row; which track is `ScreenModel::track_menu_pos`.
    LibraryTrackActions,
    /// TRACK_01: what has been analysed for that track, and Re-analyse.
    LibraryTrackAnalysis,
    /// Navigable cues of the selected item; picking one jumps to it.
    LibraryChapters,
    /// The cover images of the selected item or collection, one at a time (ART_09).
    LibraryCover,
    /// Waiting for NFC tap to associate the playable from detail.
    LibraryAssociate,
    /// BCS_07: what a tapped store link means — owned / buy / pick / blocked — and the
    /// download it can lead to. Pushed by an unmatched-tap notice that carried candidate
    /// platform URLs, never from the menu.
    LibraryStoreTap,
    /// BCS_08: System > Stores — whether Bandcamp is connected, with a way to connect it.
    SystemStores,
    /// BCS_08: Library > Stores — the same status screen, reached from the library root so
    /// browsing and downloading are one press from the music.
    LibraryStores,
    /// BCS_08: Connect Bandcamp: a QR code of the portal address plus the address as text,
    /// so a phone can open the portal without a password typed on a 320x240 screen.
    StoreConnect,
    /// BCS_08: the Bandcamp collection, one page at a time.
    StoreReleases,
    /// BCS_08: one release — track list, size, the lossy note, and the download.
    StoreRelease,
    /// BCS_08/BCS_10: Remove from Plippa — the preview and confirm for a downloaded item.
    LibraryRemove,
    /// System > Debug: the debug setting plus the Audio info navigation row (UI_16 §8,
    /// AUDIO_05). Not the legacy diagnostic screen, which stays [`Route::Debug`].
    SystemDebug,
    /// System > Debug > Audio info (AUDIO_05): the read-only audio report.
    SystemDebugAudio,
    /// Legacy diagnostic screen with dummy controls (not on the main menu).
    Debug,
}

impl Route {
    pub fn title(&self) -> &'static str {
        match self {
            Route::Main => "Now Playing",
            Route::Menu => "Menu",
            Route::System => "System",
            Route::SystemNetwork => "Network",
            Route::SystemNetworkPin => "Web portal PIN",
            Route::SystemCertificate => "Certificate",
            Route::SystemIp => "IP address",
            Route::SystemPlayMode => "Playback settings",
            Route::SystemAudio => "Audio",
            Route::SystemAudioOutputs => "Output devices",
            Route::SystemAudioDelay => "Delay",
            Route::SystemAudioAccessories => "Accessories",
            Route::SystemAudioAccessory => "Accessory",
            Route::SystemAudioPairNew => "Pair new",
            Route::Warnings => "Warnings",
            Route::Wifi => "Wi‑Fi",
            Route::NetworkInfo => "Network",
            Route::Library => "Library",
            Route::LibraryCategory => "Category",
            Route::LibraryTracks => "Tracks",
            Route::LibraryTrackActions => "Track",
            Route::LibraryTrackAnalysis => "Analysis",
            Route::LibraryChapters => "Chapters",
            Route::LibraryCover => "Cover art",
            Route::LibraryDetail => "Track",
            Route::LibraryAssociate => "Associate",
            Route::LibraryStoreTap => "Store",
            // UI_14: the Library row is `Distros` (the owner's word for where music comes from);
            // the System row stays `Stores`, which is what the owner asked for when they wanted
            // somewhere to connect a store.
            Route::SystemStores => "Stores",
            Route::LibraryStores => "Distros",
            Route::StoreConnect => "Connect",
            Route::StoreReleases => "Bandcamp",
            Route::StoreRelease => "Release",
            Route::LibraryRemove => "Remove",
            Route::SystemDebug => "Debug",
            Route::SystemDebugAudio => "Audio info",
            Route::Debug => "Debug",
        }
    }
}

/// Result of handling a nav input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NavAction {
    None,
    Entered(Route),
    Popped(Route),
    FocusMoved {
        index: usize,
    },
    /// Back from Now Playing — show the main-menu overlay (stack unchanged).
    OpenMainMenu,
}

/// Focusable menu + stack of routes.
#[derive(Debug, Clone)]
pub struct NavStack {
    stack: Vec<Route>,
    /// Focus index on the current list (main menu, System submenu, Library, …).
    pub focus: usize,
    menu_items: Vec<Route>,
    system_items: Vec<Route>,
    /// `(stack depth after the push, parent focus)` for [`Self::push`]ed children.
    saved_focus: Vec<(usize, usize)>,
}

impl Default for NavStack {
    fn default() -> Self {
        Self::new()
    }
}

/// Encoder delta for a menu-style list (UI_16 §7): the owner wants turning the encoder
/// **left** to move **down** the list and right to move up, which is the inverse of the raw
/// detent sign (`InputEvent::MenuRotate`: negative = up/previous).
///
/// Applied in exactly one place per entry point: [`NavStack::rotate`] covers every menu list
/// (System, IP, Playback settings rows, library routes) and
/// `UiApp::play_mode_edit_rotate` covers the Playback settings value ladder (a value ladder
/// reads top-down like a list, so it follows the same direction). Jog, the Adjust band, the
/// Now Playing overlay cursor, the held-scan speed selector and volume/tempo never call this,
/// so they keep the natural direction. The main menu overlay is the exception: it is a horizontal
/// strip, so `UiApp::on_menu_rotate` pre-negates the delta and it follows the raw detent
/// direction (left = previous item).
pub(crate) fn menu_step(delta: i8) -> i8 {
    // `saturating_neg` so an injected `i8::MIN` in a test cannot overflow.
    delta.saturating_neg()
}

impl NavStack {
    pub fn new() -> Self {
        Self {
            stack: vec![Route::Main],
            focus: 0,
            // Now Playing + Library + System. Debug/Warnings demoted off the main menu.
            menu_items: vec![Route::Main, Route::Library, Route::System],
            system_items: vec![
                // SECFIX_02: IP address, the web portal and Wi-Fi live under Network.
                Route::SystemNetwork,
                // The session play modes are the top rows of this one screen.
                Route::SystemPlayMode,
                // BCS_08: connect Bandcamp / check the connection.
                Route::SystemStores,
                // AUDIO_04: after Playback settings; Debug stays last.
                Route::SystemAudio,
                Route::SystemDebug,
            ],
            saved_focus: Vec::new(),
        }
    }

    pub fn current(&self) -> &Route {
        self.stack.last().unwrap_or(&Route::Main)
    }

    pub fn depth(&self) -> usize {
        self.stack.len()
    }

    pub fn menu_items(&self) -> &[Route] {
        &self.menu_items
    }

    pub fn system_items(&self) -> &[Route] {
        &self.system_items
    }

    pub fn rotate(&mut self, delta: i8, list_len: usize) -> NavAction {
        if list_len == 0 {
            return NavAction::None;
        }
        let len = list_len as isize;
        let mut idx = self.focus as isize + menu_step(delta) as isize;
        while idx < 0 {
            idx += len;
        }
        idx %= len;
        self.focus = idx as usize;
        NavAction::FocusMoved { index: self.focus }
    }

    /// Move the cursor in a bounded read-only list: clamped at both ends, never wrapped
    /// (AUDIO_05's Audio info report). The direction is the same top-down one as [`Self::rotate`].
    pub fn scroll(&mut self, delta: i8, list_len: usize) -> NavAction {
        if list_len == 0 {
            self.focus = 0;
            return NavAction::None;
        }
        let last = list_len - 1;
        let idx = (self.focus.min(last) as isize + menu_step(delta) as isize)
            .clamp(0, last as isize);
        self.focus = idx as usize;
        NavAction::FocusMoved { index: self.focus }
    }

    /// Push-to-enter on current route.
    ///
    /// From [`Route::Main`] this is a no-op for the stack — the main menu is an
    /// overlay opened via [`NavAction::OpenMainMenu`] / Back.
    pub fn enter(&mut self) -> NavAction {
        match self.current().clone() {
            Route::Main => NavAction::None,
            Route::Menu => self.enter_focused_menu_item(),
            Route::System => self.enter_focused_system_item(),
            // Leaf screens: enter is handled by the screen model (play / associate).
            _ => NavAction::None,
        }
    }

    /// Enter the focused root-menu item (overlay or legacy [`Route::Menu`]).
    ///
    /// Selecting [`Route::Main`] (Now Playing) resets the stack to `[Main]`
    /// instead of pushing a duplicate Main on top of Library/System.
    pub fn enter_focused_menu_item(&mut self) -> NavAction {
        let Some(next) = self.menu_items.get(self.focus).cloned() else {
            return NavAction::None;
        };
        // If we somehow still have legacy Menu on the stack, replace it.
        if *self.current() == Route::Menu {
            self.stack.pop();
        }
        self.focus = 0;
        if next == Route::Main {
            self.saved_focus.clear();
            self.stack.clear();
            self.stack.push(Route::Main);
            return NavAction::Entered(Route::Main);
        }
        self.stack.push(next.clone());
        NavAction::Entered(next)
    }

    /// Enter the focused System submenu item.
    pub fn enter_focused_system_item(&mut self) -> NavAction {
        let Some(next) = self.system_items.get(self.focus).cloned() else {
            return NavAction::None;
        };
        self.focus = 0;
        self.stack.push(next.clone());
        NavAction::Entered(next)
    }

    /// Push a child route, resetting focus to 0.
    pub fn push(&mut self, route: Route) -> NavAction {
        self.stack.push(route.clone());
        self.saved_focus.push((self.stack.len(), self.focus));
        self.focus = 0;
        NavAction::Entered(route)
    }

    pub fn back(&mut self) -> NavAction {
        if self.stack.len() <= 1 {
            return NavAction::OpenMainMenu;
        }
        let depth = self.stack.len();
        let popped = self.stack.pop().unwrap();
        self.focus = match self.saved_focus.last() {
            Some(&(d, f)) if d == depth => {
                self.saved_focus.pop();
                f
            }
            _ => 0,
        };
        NavAction::Popped(popped)
    }

    /// Test / harness: jump to a route without walking the menu.
    pub fn force_push(&mut self, route: Route) {
        if *self.current() != route {
            self.stack.push(route);
            self.focus = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn back_from_main_opens_menu_enter_library() {
        let mut nav = NavStack::new();
        assert_eq!(*nav.current(), Route::Main);
        assert!(matches!(nav.back(), NavAction::OpenMainMenu));
        assert_eq!(*nav.current(), Route::Main);
        let len = nav.menu_items().len();
        // UI_16 §7: turning left (negative detent) moves down the list.
        assert!(matches!(
            nav.rotate(-1, len),
            NavAction::FocusMoved { index: 1 }
        ));
        assert!(matches!(
            nav.enter_focused_menu_item(),
            NavAction::Entered(Route::Library)
        ));
        assert_eq!(*nav.current(), Route::Library);
        assert!(matches!(nav.back(), NavAction::Popped(Route::Library)));
        assert_eq!(*nav.current(), Route::Main);
    }

    /// UI_16 §7: the inversion lives here, once, for every menu list. A positive raw detent
    /// (right) now walks **up** and a negative one (left) walks down, wrapping at both ends.
    #[test]
    fn menu_rotate_reads_top_down_and_wraps() {
        let mut nav = NavStack::new();
        let len = nav.menu_items().len(); // Now Playing, Library, System
        assert_eq!(menu_step(1), -1);
        assert_eq!(menu_step(-1), 1);
        assert_eq!(menu_step(0), 0);
        // Right (positive) from the first row wraps up to the last.
        assert!(matches!(
            nav.rotate(1, len),
            NavAction::FocusMoved { index: 2 }
        ));
        // Left (negative) from the last row wraps down to the first.
        assert!(matches!(
            nav.rotate(-1, len),
            NavAction::FocusMoved { index: 0 }
        ));
        // Multiple detents in one event keep the new direction.
        assert!(matches!(
            nav.rotate(-2, len),
            NavAction::FocusMoved { index: 2 }
        ));
    }

    #[test]
    fn main_menu_has_system_not_standalone_wifi() {
        let nav = NavStack::new();
        assert_eq!(
            nav.menu_items(),
            &[Route::Main, Route::Library, Route::System]
        );
        assert!(!nav
            .menu_items()
            .iter()
            .any(|r| matches!(r, Route::LibraryAssociate | Route::Wifi | Route::Debug)));
        assert_eq!(
            nav.system_items(),
            &[
                Route::SystemNetwork,
                Route::SystemPlayMode,
                Route::SystemStores,
                Route::SystemAudio,
                Route::SystemDebug
            ]
        );
        assert_eq!(Route::SystemPlayMode.title(), "Playback settings");
        assert_eq!(Route::SystemAudio.title(), "Audio");
        assert_eq!(Route::SystemAudioOutputs.title(), "Output devices");
        assert_eq!(Route::SystemAudioDelay.title(), "Delay");
        assert_eq!(Route::SystemDebug.title(), "Debug");
        assert_eq!(Route::SystemCertificate.title(), "Certificate");
        // AUDIO_04: Audio sits immediately before Debug, which stays last.
        let a = nav
            .system_items()
            .iter()
            .position(|r| *r == Route::SystemAudio)
            .unwrap();
        assert_eq!(nav.system_items()[a + 1], Route::SystemDebug);
    }

    #[test]
    fn enter_now_playing_from_menu_resets_stack() {
        let mut nav = NavStack::new();
        nav.force_push(Route::Library);
        nav.push(Route::LibraryDetail);
        assert_eq!(nav.depth(), 3);
        nav.focus = 0; // Now Playing
        assert!(matches!(
            nav.enter_focused_menu_item(),
            NavAction::Entered(Route::Main)
        ));
        assert_eq!(*nav.current(), Route::Main);
        assert_eq!(nav.depth(), 1);
    }

    #[test]
    fn system_submenu_enter_network_then_wifi_one_level_deeper() {
        let mut nav = NavStack::new();
        nav.force_push(Route::System);
        nav.focus = 0;
        assert!(matches!(
            nav.enter_focused_system_item(),
            NavAction::Entered(Route::SystemNetwork)
        ));
        assert_eq!(*nav.current(), Route::SystemNetwork);
        // Wi-Fi is a child of Network: Back from it lands on Network, then System.
        nav.focus = 4;
        assert!(matches!(nav.push(Route::Wifi), NavAction::Entered(Route::Wifi)));
        assert!(matches!(nav.back(), NavAction::Popped(Route::Wifi)));
        assert_eq!(*nav.current(), Route::SystemNetwork);
        assert_eq!(nav.focus, 4, "the Wi-Fi row is still focused");
        assert!(matches!(nav.back(), NavAction::Popped(Route::SystemNetwork)));
        assert_eq!(*nav.current(), Route::System);
    }

    #[test]
    fn the_system_menu_has_network_and_no_top_level_ip_or_wifi() {
        let nav = NavStack::new();
        assert_eq!(nav.system_items()[0], Route::SystemNetwork);
        assert!(!nav
            .system_items()
            .iter()
            .any(|r| matches!(r, Route::SystemIp | Route::Wifi)));
        assert_eq!(Route::SystemNetwork.title(), "Network");
        assert_eq!(Route::SystemIp.title(), "IP address");
    }

    #[test]
    fn enter_back_encoder_move_legacy_menu_route() {
        let mut nav = NavStack::new();
        nav.force_push(Route::Menu);
        assert_eq!(*nav.current(), Route::Menu);
        let len = nav.menu_items().len();
        // Main=0, Library=1, System=2. UI_16 §7: left (negative) = down.
        assert!(matches!(
            nav.rotate(-2, len),
            NavAction::FocusMoved { index: 2 }
        ));
        assert!(matches!(nav.enter(), NavAction::Entered(Route::System)));
        assert_eq!(*nav.current(), Route::System);
        assert!(matches!(nav.back(), NavAction::Popped(Route::System)));
        assert_eq!(*nav.current(), Route::Main);
    }

    #[test]
    fn library_list_detail_associate_stack() {
        let mut nav = NavStack::new();
        nav.force_push(Route::Library);
        assert!(matches!(
            nav.push(Route::LibraryDetail),
            NavAction::Entered(Route::LibraryDetail)
        ));
        assert!(matches!(
            nav.push(Route::LibraryAssociate),
            NavAction::Entered(Route::LibraryAssociate)
        ));
        assert!(matches!(
            nav.back(),
            NavAction::Popped(Route::LibraryAssociate)
        ));
        assert_eq!(*nav.current(), Route::LibraryDetail);
        assert!(matches!(
            nav.back(),
            NavAction::Popped(Route::LibraryDetail)
        ));
        assert_eq!(*nav.current(), Route::Library);
    }

    #[test]
    fn push_remembers_parent_focus_per_level() {
        let mut nav = NavStack::new();
        nav.force_push(Route::Library);
        nav.focus = 3;
        nav.push(Route::LibraryCategory);
        assert_eq!(nav.focus, 0);
        nav.focus = 17;
        nav.push(Route::LibraryDetail);
        nav.focus = 2;
        nav.push(Route::LibraryTracks);
        nav.focus = 9;
        assert!(matches!(
            nav.back(),
            NavAction::Popped(Route::LibraryTracks)
        ));
        assert_eq!(nav.focus, 2, "detail action focus restored");
        assert!(matches!(
            nav.back(),
            NavAction::Popped(Route::LibraryDetail)
        ));
        assert_eq!(nav.focus, 17, "category row restored");
        assert!(matches!(
            nav.back(),
            NavAction::Popped(Route::LibraryCategory)
        ));
        assert_eq!(nav.focus, 3, "root row restored");
        // Popping a force-pushed route (no saved focus) still resets to 0.
        assert!(matches!(nav.back(), NavAction::Popped(Route::Library)));
        assert_eq!(nav.focus, 0);
    }

    #[test]
    fn entering_now_playing_drops_saved_focus() {
        let mut nav = NavStack::new();
        nav.force_push(Route::Library);
        nav.focus = 2;
        nav.push(Route::LibraryCategory);
        nav.focus = 0;
        assert!(matches!(
            nav.enter_focused_menu_item(),
            NavAction::Entered(Route::Main)
        ));
        nav.force_push(Route::Library);
        nav.push(Route::LibraryCategory);
        nav.back();
        assert_eq!(nav.focus, 0, "no stale focus from the earlier visit");
    }
}
