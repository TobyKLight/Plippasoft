//! Cover art on the device (ART_08): resolve, load off-thread, cache, paint, fade.
//!
//! Shared by Now Playing (this package), the Library cover viewer (ART_09) and the LED record
//! (ART_10).
//!
//! * **Resolve** ([`resolve_playing_art`]): the effective cover list of the playing item
//!   (`plippa_noubin::covers`, D4) read through the UI's own [`LibrarySource`]. The answer is
//!   cached per `(playable, item position, library generation)` by [`NowPlayingArt`], so it is
//!   never recomputed per frame.
//! * **Load** ([`ArtLoader`]): one worker thread, one decode in flight. It reads the smallest
//!   thumbnail that is at least as large as the wanted side (D5), and decodes an original only
//!   when no such thumbnail exists *and* the original is at most [`ART_ORIGINAL_MAX_PIXELS`]
//!   (a larger file is left for the server's thumbnail backfill). The result is a centre-cropped
//!   square (D2) as RGB565 for the panel and as RGB bytes for the LED sampler. Hostile input is
//!   an error value, never a panic out of the thread.
//! * **Paint** ([`paint_art_square`]) never blocks: the render path only reads finished images.
//! * **Fade** ([`ArtFade`], D10): hides the picture while a scan or jog is under way.

use crate::display::FrameBuffer;
use crate::library::LibrarySource;
use crate::ui::primitives::blend_pixel;
use plippa_image::{decode, scale_square, to_rgb565, ImageError, ImageLimits, Rgb8};
use plippa_noubin::covers::{resolve_thumbnail, CoverRef};
use plippa_noubin::is_safe_relative_path;
use std::collections::VecDeque;
use std::io::Read;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;

/// The cache keeps at most this many decoded images ...
pub const ART_CACHE_MAX_IMAGES: usize = 4;
/// ... and at most this many bytes of pixels.
pub const ART_CACHE_MAX_BYTES: usize = 1 << 20;
/// Side of the RGB copy kept for the LED sampler.
pub const ART_LED_SIDE: u16 = 64;
/// An original without a usable thumbnail is decoded only up to this many pixels (a 1 GB board
/// must not decode a 5000 x 5000 image).
pub const ART_ORIGINAL_MAX_PIXELS: u64 = 2_000_000;
/// Remembered load failures (bounded so a pathological library cannot grow it).
const ART_FAILED_MAX: usize = 32;

/// Fade-out when a scan or jog starts (linear alpha 100 to 0).
pub const ART_FADE_OUT_MS: u64 = 120;
/// Fade-in once the picture may return (linear alpha 0 to 100).
pub const ART_FADE_IN_MS: u64 = 300;
/// How long neither a scan nor a jog may have been active before the picture fades back in.
pub const ART_FADE_HOLD_MS: u64 = 800;

/// What to decode: an image of the library plus the side it will be shown at.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ArtKey {
    /// Index into [`LibrarySource::roots`].
    pub root_idx: usize,
    /// Library-relative path of the image (not of a thumbnail).
    pub image_relpath: String,
    /// Wanted side in pixels.
    pub px: u16,
}

/// A decoded, centre-cropped square.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtImage {
    /// Side of [`Self::rgb565`] (equals the requested `px`).
    pub side: u16,
    /// `side * side` panel pixels, row-major.
    pub rgb565: Vec<u16>,
    /// Side of [`Self::rgb`] ([`ART_LED_SIDE`]).
    pub rgb_side: u16,
    /// `rgb_side * rgb_side * 3` RGB bytes of the same picture, for the LED sampler.
    pub rgb: Vec<u8>,
}

impl ArtImage {
    fn bytes(&self) -> usize {
        self.rgb565.len() * 2 + self.rgb.len()
    }
}

/// `(length, mtime)` of an image file: a changed file is worth another attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FileStamp {
    pub len: u64,
    pub mtime_ns: i64,
}

impl FileStamp {
    pub fn of(path: &Path) -> Option<Self> {
        let meta = std::fs::metadata(path).ok()?;
        let mtime_ns = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(-1, |d| d.as_nanos().min(i64::MAX as u128) as i64);
        Some(Self {
            len: meta.len(),
            mtime_ns,
        })
    }
}

/// Why an image could not be shown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArtErrorKind {
    /// The path is not a safe library-relative path, or the root is gone.
    BadPath,
    /// The file could not be read.
    Io(String),
    /// Refused or undecodable image.
    Image(ImageError),
    /// No thumbnail and the original is over [`ART_ORIGINAL_MAX_PIXELS`].
    TooLargeForDevice,
    /// The decoder panicked (caught at the thread boundary).
    Panicked,
}

/// A failed load, with the stamp of the file it failed on (for the negative cache).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtError {
    pub kind: ArtErrorKind,
    pub stamp: FileStamp,
}

impl ArtError {
    fn new(kind: ArtErrorKind) -> Self {
        Self {
            kind,
            stamp: FileStamp::default(),
        }
    }
}

/// LRU cache of decoded images plus a short list of files that failed to load.
#[derive(Debug, Default)]
pub struct ArtCache {
    /// Most recently used first.
    entries: VecDeque<(ArtKey, Arc<ArtImage>)>,
    failed: VecDeque<(ArtKey, FileStamp)>,
}

impl ArtCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// The image for `key`, marking it most recently used.
    pub fn get(&mut self, key: &ArtKey) -> Option<Arc<ArtImage>> {
        let i = self.entries.iter().position(|(k, _)| k == key)?;
        let entry = self.entries.remove(i)?;
        let img = Arc::clone(&entry.1);
        self.entries.push_front(entry);
        Some(img)
    }

    pub fn insert(&mut self, key: ArtKey, img: Arc<ArtImage>) {
        self.entries.retain(|(k, _)| *k != key);
        self.failed.retain(|(k, _)| *k != key);
        self.entries.push_front((key, img));
        while self.entries.len() > ART_CACHE_MAX_IMAGES
            || (self.entries.len() > 1 && self.bytes() > ART_CACHE_MAX_BYTES)
        {
            self.entries.pop_back();
        }
    }

    /// Remember that `key` failed on a file with `stamp`.
    pub fn insert_failure(&mut self, key: ArtKey, stamp: FileStamp) {
        self.failed.retain(|(k, _)| *k != key);
        self.failed.push_front((key, stamp));
        self.failed.truncate(ART_FAILED_MAX);
    }

    /// True when `key` already failed on a file that still has this `stamp`.
    pub fn is_failed(&self, key: &ArtKey, stamp: FileStamp) -> bool {
        self.failed.iter().any(|(k, s)| k == key && *s == stamp)
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Total pixel bytes held.
    pub fn bytes(&self) -> usize {
        self.entries.iter().map(|(_, i)| i.bytes()).sum()
    }

    /// Forget everything (library reopened).
    pub fn clear(&mut self) {
        self.entries.clear();
        self.failed.clear();
    }
}

type LoadFn = dyn Fn(&ArtKey) -> Result<ArtImage, ArtError> + Send + 'static;
type LoadResult = (ArtKey, Result<ArtImage, ArtError>);

#[derive(Default)]
struct Slot {
    wanted: Option<ArtKey>,
    pending: bool,
    shutdown: bool,
}

struct Shared {
    slot: Mutex<Slot>,
    wake: Condvar,
    roots: Mutex<Vec<PathBuf>>,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// One worker thread that decodes the latest wanted image. A request replaces any request that
/// has not started; the result of a request that is no longer the latest is dropped by
/// [`Self::poll`].
pub struct ArtLoader {
    shared: Arc<Shared>,
    results: Receiver<LoadResult>,
    worker: Option<JoinHandle<()>>,
}

impl ArtLoader {
    /// The production loader (reads from the roots given to [`Self::set_roots`]).
    pub fn new() -> Self {
        let shared = Arc::new(Shared {
            slot: Mutex::new(Slot::default()),
            wake: Condvar::new(),
            roots: Mutex::new(Vec::new()),
        });
        let roots = Arc::clone(&shared);
        Self::spawn(shared, move |key| {
            let root = lock(&roots.roots).get(key.root_idx).cloned();
            match root {
                Some(root) => load_art(&root, key),
                None => Err(ArtError::new(ArtErrorKind::BadPath)),
            }
        })
    }

    /// A loader that runs `load` instead of reading files (tests).
    #[cfg(test)]
    pub fn with_loader(
        load: impl Fn(&ArtKey) -> Result<ArtImage, ArtError> + Send + 'static,
    ) -> Self {
        let shared = Arc::new(Shared {
            slot: Mutex::new(Slot::default()),
            wake: Condvar::new(),
            roots: Mutex::new(Vec::new()),
        });
        Self::spawn(shared, load)
    }

    fn spawn(
        shared: Arc<Shared>,
        load: impl Fn(&ArtKey) -> Result<ArtImage, ArtError> + Send + 'static,
    ) -> Self {
        let (tx, results) = channel();
        let worker_shared = Arc::clone(&shared);
        let load: Box<LoadFn> = Box::new(load);
        let worker = std::thread::Builder::new()
            .name("plippa-art".into())
            .spawn(move || worker_loop(&worker_shared, &*load, &tx))
            .ok();
        Self {
            shared,
            results,
            worker,
        }
    }

    /// Library roots the keys' `root_idx` refers to.
    pub fn set_roots(&self, roots: &[PathBuf]) {
        let mut cur = lock(&self.shared.roots);
        if cur.as_slice() != roots {
            *cur = roots.to_vec();
        }
    }

    /// Ask for `key`. Coalesces: only the latest request is decoded.
    pub fn request(&self, key: ArtKey) {
        let mut slot = lock(&self.shared.slot);
        slot.wanted = Some(key);
        slot.pending = true;
        self.shared.wake.notify_one();
    }

    /// Finished loads for the key that is currently wanted; results for older keys are dropped.
    pub fn poll(&mut self) -> Vec<(ArtKey, Result<ArtImage, ArtError>)> {
        let wanted = lock(&self.shared.slot).wanted.clone();
        let mut out = Vec::new();
        while let Ok((key, res)) = self.results.try_recv() {
            if wanted.as_ref() == Some(&key) {
                out.push((key, res));
            }
        }
        out
    }
}

impl Default for ArtLoader {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for ArtLoader {
    fn drop(&mut self) {
        {
            let mut slot = lock(&self.shared.slot);
            slot.shutdown = true;
            self.shared.wake.notify_one();
        }
        if let Some(h) = self.worker.take() {
            let _ = h.join();
        }
    }
}

fn worker_loop(shared: &Shared, load: &LoadFn, tx: &Sender<LoadResult>) {
    loop {
        let key = {
            let mut slot = lock(&shared.slot);
            while !slot.pending && !slot.shutdown {
                slot = shared
                    .wake
                    .wait(slot)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
            }
            if slot.shutdown {
                return;
            }
            slot.pending = false;
            match slot.wanted.clone() {
                Some(k) => k,
                None => continue,
            }
        };
        let result = catch_unwind(AssertUnwindSafe(|| load(&key)))
            .unwrap_or_else(|_| Err(ArtError::new(ArtErrorKind::Panicked)));
        if tx.send((key, result)).is_err() {
            return;
        }
    }
}

/// Load one image for the panel: thumbnail first, a small original otherwise.
pub fn load_art(root: &Path, key: &ArtKey) -> Result<ArtImage, ArtError> {
    if !is_safe_relative_path(&key.image_relpath) || key.px == 0 {
        return Err(ArtError::new(ArtErrorKind::BadPath));
    }
    let side = u32::from(key.px);
    let thumb = resolve_thumbnail(root, &key.image_relpath, side);
    // Failures are keyed by the original's stamp: that is the file every caller checks, whichever
    // file (thumbnail or original) actually failed.
    let original_stamp = FileStamp::of(&root.join(&key.image_relpath)).unwrap_or_default();
    // Every file, thumbnail or original, is bounded by the device's pixel cap (a tier is at most
    // 1024 x 1024, well inside it; a planted large file under thumbnails/ is refused).
    let limits = ImageLimits {
        max_pixels: ART_ORIGINAL_MAX_PIXELS,
        ..ImageLimits::default()
    };
    let from_file = |rel: &str| -> Result<ArtImage, ArtError> {
        let img = decode_file(&root.join(rel), &limits)?;
        if img.w == 0 || img.h == 0 {
            return Err(ArtError::new(ArtErrorKind::Image(ImageError::Corrupt(
                "empty image".into(),
            ))));
        }
        Ok(make_art_image(&img, key.px))
    };
    // A thumbnail at least as large as the wanted side is always preferred (D5).
    let usable = thumb.as_ref().filter(|t| t.tier.px() >= side);
    let result = (|| {
        if let Some(t) = usable {
            if let Ok(img) = from_file(&t.relpath) {
                return Ok(img);
            }
            // Unreadable or refused thumbnail: the original, once, below.
        }
        match from_file(&key.image_relpath) {
            Err(e) if matches!(e.kind, ArtErrorKind::Image(ImageError::TooLarge { .. })) => {
                // Over the original-size cap: fall back to the smaller thumbnail, else give up.
                match thumb.as_ref().filter(|_| usable.is_none()) {
                    Some(t) => from_file(&t.relpath),
                    None => Err(ArtError::new(ArtErrorKind::TooLargeForDevice)),
                }
            }
            other => other,
        }
    })();
    result.map_err(|e| ArtError {
        kind: e.kind,
        stamp: original_stamp,
    })
}

fn decode_file(path: &Path, limits: &ImageLimits) -> Result<Rgb8, ArtError> {
    let stamp = FileStamp::of(path).unwrap_or_default();
    let fail = |kind| ArtError { kind, stamp };
    let file = std::fs::File::open(path).map_err(|e| fail(ArtErrorKind::Io(e.to_string())))?;
    if stamp.len > limits.max_bytes {
        return Err(fail(ArtErrorKind::Image(ImageError::TooLarge {
            width: 0,
            height: 0,
            bytes: stamp.len,
        })));
    }
    let mut bytes = Vec::with_capacity(stamp.len.min(limits.max_bytes) as usize);
    file.take(limits.max_bytes + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| fail(ArtErrorKind::Io(e.to_string())))?;
    decode(&bytes, limits).map_err(|e| fail(ArtErrorKind::Image(e)))
}

/// Centre-cropped square of exactly `side` pixels (small sources are enlarged, nearest
/// neighbour: `scale_square` never upscales).
fn fit_square(img: &Rgb8, side: u32) -> Rgb8 {
    let sq = scale_square(img, side);
    if sq.w == side || sq.w == 0 {
        return sq;
    }
    let mut px = Vec::with_capacity((side * side * 3) as usize);
    for y in 0..side {
        let sy = (y * sq.w / side) as usize;
        for x in 0..side {
            let i = (sy * sq.w as usize + (x * sq.w / side) as usize) * 3;
            px.extend_from_slice(&sq.px[i..i + 3]);
        }
    }
    Rgb8 {
        w: side,
        h: side,
        px,
    }
}

/// Both pixel formats from one decoded picture.
fn make_art_image(img: &Rgb8, side: u16) -> ArtImage {
    let panel = fit_square(img, u32::from(side));
    let led = if side == ART_LED_SIDE {
        panel.clone()
    } else {
        fit_square(img, u32::from(ART_LED_SIDE))
    };
    ArtImage {
        side,
        rgb565: to_rgb565(&panel),
        rgb_side: ART_LED_SIDE,
        rgb: led.px,
    }
}

/// What a store cover may be before the device refuses to decode it (BCS_11): the server will
/// send up to 8 MB, but the Pi stays responsive by decoding at most this much and showing the
/// "no art" placeholder for anything larger. Never raise these to match the server's cap.
pub const STORE_COVER_LIMITS: ImageLimits = ImageLimits {
    max_side: 1024,
    max_pixels: 1024 * 1024,
    max_bytes: 4 * 1024 * 1024,
};

/// Decode a store cover and centre-crop it to a `side` square. Runs on the store worker, never
/// the frame loop. A cover over [`STORE_COVER_LIMITS`] or that does not decode is an error the
/// caller turns into the placeholder.
pub fn decode_store_cover(bytes: &[u8], side: u16) -> Result<ArtImage, ImageError> {
    if side == 0 {
        return Err(ImageError::Corrupt("zero side".into()));
    }
    let img = decode(bytes, &STORE_COVER_LIMITS)?;
    if img.w == 0 || img.h == 0 {
        return Err(ImageError::Corrupt("empty image".into()));
    }
    Ok(make_art_image(&img, side))
}

/// Decoded store covers by `(store release id, side)`, bounded like library art: at most
/// [`ART_CACHE_MAX_IMAGES`] pictures and [`ART_CACHE_MAX_BYTES`] of pixels (the newest always
/// stays). Library art is keyed by a library path ([`ArtKey`]), which a store release does not
/// have, so this is the same LRU over the store's own key.
#[derive(Debug, Default)]
pub struct StoreCoverCache {
    /// Most recently used first.
    entries: VecDeque<((String, u16), Arc<ArtImage>)>,
}

impl StoreCoverCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// The picture for `(release_id, side)`, marking it most recently used.
    pub fn get(&mut self, release_id: &str, side: u16) -> Option<Arc<ArtImage>> {
        let i = self
            .entries
            .iter()
            .position(|((id, s), _)| id == release_id && *s == side)?;
        let entry = self.entries.remove(i)?;
        let img = Arc::clone(&entry.1);
        self.entries.push_front(entry);
        Some(img)
    }

    pub fn insert(&mut self, release_id: &str, side: u16, img: Arc<ArtImage>) {
        self.entries
            .retain(|((id, s), _)| !(id == release_id && *s == side));
        self.entries.push_front(((release_id.to_string(), side), img));
        while self.entries.len() > ART_CACHE_MAX_IMAGES
            || (self.entries.len() > 1 && self.bytes() > ART_CACHE_MAX_BYTES)
        {
            self.entries.pop_back();
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Total pixel bytes held.
    pub fn bytes(&self) -> usize {
        self.entries.iter().map(|(_, i)| i.bytes()).sum()
    }
}

/// The effective covers of the playing item and the root they live in.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ArtResolution {
    /// Index into [`LibrarySource::roots`].
    pub root_idx: usize,
    /// The effective list (D4), existing files only, primary first.
    pub covers: Vec<CoverRef>,
}

impl ArtResolution {
    /// Cheap check for "is there anything to show" (ART_09's `Cover art` action).
    pub fn has_art(&self) -> bool {
        !self.covers.is_empty()
    }

    /// The key of the primary image at `px`.
    pub fn primary_key(&self, px: u16) -> Option<ArtKey> {
        self.covers.first().map(|c| ArtKey {
            root_idx: self.root_idx,
            image_relpath: c.relpath.clone(),
            px,
        })
    }
}

/// D4: item, then its release, then the collection being played. `playable_id` is
/// `SessionState::current_item`, `item_position` the 0-based `mediaList` position.
pub fn resolve_playing_art(
    lib: &LibrarySource,
    playable_id: u64,
    item_position: u32,
) -> ArtResolution {
    lib.item_covers(playable_id, item_position)
        .map(|(root_idx, covers)| ArtResolution { root_idx, covers })
        .unwrap_or_default()
}

/// Draw `img` with its top-left corner at `(x, y)`. `alpha_pct` below 100 blends the picture
/// over what is already in the framebuffer (so the waveform shows through while it fades);
/// 0 draws nothing. Pixels outside the framebuffer are clipped.
pub fn paint_art_square(fb: &mut FrameBuffer, x: u16, y: u16, img: &ArtImage, alpha_pct: u8) {
    let side = usize::from(img.side);
    if alpha_pct == 0 || side == 0 || img.rgb565.len() != side * side {
        return;
    }
    let w = side.min(usize::from(fb.width.saturating_sub(x)));
    let h = side.min(usize::from(fb.height.saturating_sub(y)));
    for row in 0..h {
        let src = &img.rgb565[row * side..row * side + w];
        let dst = (usize::from(y) + row) * usize::from(fb.width) + usize::from(x);
        let dst = &mut fb.pixels[dst..dst + w];
        if alpha_pct >= 100 {
            dst.copy_from_slice(src);
        } else {
            for (d, s) in dst.iter_mut().zip(src) {
                *d = blend_pixel(*d, *s, alpha_pct);
            }
        }
    }
}

/// Where the fade is (D10).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FadeState {
    Visible,
    /// Alpha falls linearly from `from` at `started`.
    FadingOut { started: u64, from: u8 },
    Hidden,
    /// Alpha rises linearly from `from` at `started`.
    FadingIn { started: u64, from: u8 },
}

/// The picture hides while a scan or jog is running and returns after
/// [`ART_FADE_HOLD_MS`] of quiet. Driven by an injected clock, so it is deterministic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArtFade {
    state: FadeState,
    last_trigger_ms: u64,
}

impl Default for ArtFade {
    fn default() -> Self {
        Self::new()
    }
}

impl ArtFade {
    pub const fn new() -> Self {
        Self {
            state: FadeState::Visible,
            last_trigger_ms: 0,
        }
    }

    pub fn state(&self) -> FadeState {
        self.state
    }

    /// Current opacity in percent.
    pub fn alpha(&self, now_ms: u64) -> u8 {
        match self.state {
            FadeState::Visible => 100,
            FadeState::Hidden => 0,
            FadeState::FadingOut { started, from } => {
                let dt = now_ms.saturating_sub(started);
                let fallen = dt.saturating_mul(100) / ART_FADE_OUT_MS;
                u64::from(from).saturating_sub(fallen) as u8
            }
            FadeState::FadingIn { started, from } => {
                let dt = now_ms.saturating_sub(started);
                let risen = dt.saturating_mul(100) / ART_FADE_IN_MS;
                (u64::from(from) + risen).min(100) as u8
            }
        }
    }

    /// A fade is running, so frames are needed at the animation rate.
    pub fn is_animating(&self) -> bool {
        matches!(
            self.state,
            FadeState::FadingOut { .. } | FadeState::FadingIn { .. }
        )
    }

    /// Advance the state machine. `trigger` is true while a scan or jog is active.
    pub fn tick(&mut self, now_ms: u64, trigger: bool) {
        if trigger {
            self.last_trigger_ms = now_ms;
            if matches!(self.state, FadeState::Visible | FadeState::FadingIn { .. }) {
                self.state = FadeState::FadingOut {
                    started: now_ms,
                    from: self.alpha(now_ms),
                };
            }
            if matches!(self.state, FadeState::FadingOut { .. }) && self.alpha(now_ms) == 0 {
                self.state = FadeState::Hidden;
            }
            return;
        }
        match self.state {
            FadeState::FadingOut { .. } if self.alpha(now_ms) == 0 => self.state = FadeState::Hidden,
            FadeState::FadingIn { .. } if self.alpha(now_ms) >= 100 => {
                self.state = FadeState::Visible;
            }
            _ => {}
        }
        if self.state == FadeState::Hidden
            && now_ms.saturating_sub(self.last_trigger_ms) >= ART_FADE_HOLD_MS
        {
            self.state = FadeState::FadingIn {
                started: self.last_trigger_ms + ART_FADE_HOLD_MS,
                from: 0,
            };
            if self.alpha(now_ms) >= 100 {
                self.state = FadeState::Visible;
            }
        }
    }
}

/// What Now Playing is showing art for: the loaded playable and the playing `mediaList` item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArtSubject {
    pub playable_id: u64,
    pub item_position: u32,
}

/// The Now Playing picture: resolution cache, image cache and loader in one place.
#[derive(Default)]
pub struct NowPlayingArt {
    /// Started on the first request, so an app that never plays anything has no extra thread.
    loader: Option<ArtLoader>,
    cache: ArtCache,
    /// What `resolution` was computed for, and under which library generation.
    resolved_for: Option<(ArtSubject, u64)>,
    generation: u64,
    resolution: ArtResolution,
    wanted: Option<ArtKey>,
    image: Option<Arc<ArtImage>>,
}

impl NowPlayingArt {
    pub fn new() -> Self {
        Self::default()
    }

    #[cfg(test)]
    fn with_loader(loader: ArtLoader) -> Self {
        Self {
            loader: Some(loader),
            ..Self::default()
        }
    }

    /// The library was reopened or its index changed: drop everything derived from it.
    pub fn invalidate(&mut self) {
        self.generation += 1;
        self.cache.clear();
    }

    /// The picture to show right now (ready, square, `side` as requested).
    pub fn image(&self) -> Option<&Arc<ArtImage>> {
        self.image.as_ref()
    }

    /// The resolved cover list of the playing item; see [`ArtResolution::has_art`].
    pub fn resolution(&self) -> &ArtResolution {
        &self.resolution
    }

    /// Resolve the covers when `subject` (or the library) changed and request the primary
    /// image. Does file-system and index work: call from the loop, never from painting.
    /// Returns true when the picture to show changed.
    pub fn service(&mut self, lib: &LibrarySource, subject: Option<ArtSubject>, px: u16) -> bool {
        let Some(subject) = subject else {
            self.resolved_for = None;
            self.resolution = ArtResolution::default();
            self.wanted = None;
            return self.set_image(None);
        };
        if self.resolved_for == Some((subject, self.generation)) {
            return false;
        }
        self.loader
            .get_or_insert_with(ArtLoader::new)
            .set_roots(lib.roots());
        self.resolution = resolve_playing_art(lib, subject.playable_id, subject.item_position);
        self.resolved_for = Some((subject, self.generation));
        self.wanted = self.resolution.primary_key(px);
        let Some(key) = self.wanted.clone() else {
            return self.set_image(None);
        };
        if let Some(img) = self.cache.get(&key) {
            return self.set_image(Some(img));
        }
        let changed = self.set_image(None);
        let stamp = lib
            .roots()
            .get(key.root_idx)
            .and_then(|r| FileStamp::of(&r.join(&key.image_relpath)));
        if stamp.is_some_and(|s| self.cache.is_failed(&key, s)) {
            return changed;
        }
        self.loader.get_or_insert_with(ArtLoader::new).request(key);
        changed
    }

    /// Collect finished loads. Returns true when the picture to show changed.
    pub fn poll(&mut self) -> bool {
        let mut changed = false;
        let Some(loader) = self.loader.as_mut() else {
            return false;
        };
        for (key, res) in loader.poll() {
            match res {
                Ok(img) => {
                    let img = Arc::new(img);
                    self.cache.insert(key.clone(), Arc::clone(&img));
                    if self.wanted.as_ref() == Some(&key) {
                        changed |= self.set_image(Some(img));
                    }
                }
                Err(e) => self.cache.insert_failure(key, e.stamp),
            }
        }
        changed
    }

    fn set_image(&mut self, img: Option<Arc<ArtImage>>) -> bool {
        let same = match (&self.image, &img) {
            (None, None) => true,
            (Some(a), Some(b)) => Arc::ptr_eq(a, b),
            _ => false,
        };
        self.image = img;
        !same
    }
}

/// What [`CoverViewer::sync`] should show: the images of one item at one size.
#[derive(Debug, Clone, Copy)]
pub struct CoverTarget<'a> {
    /// Index into [`LibrarySource::roots`].
    pub root_idx: usize,
    pub covers: &'a [CoverRef],
    /// 0-based image to show (clamped to the list).
    pub index: usize,
    /// Side the image is shown at.
    pub px: u16,
}

/// The Library cover screen and the detail art box (ART_09): shows one image of a list at a
/// time, keeps the previous picture on screen until the next one is decoded, and decodes the
/// neighbour once the current one is ready so scrolling feels instant.
#[derive(Default)]
pub struct CoverViewer {
    /// Started on the first request.
    loader: Option<ArtLoader>,
    cache: ArtCache,
    roots: Vec<PathBuf>,
    root_idx: usize,
    covers: Vec<CoverRef>,
    px: u16,
    index: usize,
    /// Direction of the last move (+1 next, -1 previous): the neighbour that is decoded first.
    heading: i8,
    shown: Option<Arc<ArtImage>>,
    failed: bool,
    open: bool,
    /// The key the loader is working on (so a rotate onto it does not decode it twice).
    in_flight: Option<ArtKey>,
}

impl CoverViewer {
    pub fn new() -> Self {
        Self::default()
    }

    #[cfg(test)]
    fn with_loader(loader: ArtLoader) -> Self {
        Self {
            loader: Some(loader),
            ..Self::default()
        }
    }

    /// The library was reopened or its index changed: forget decoded images.
    pub fn invalidate(&mut self) {
        self.cache.clear();
        self.open = false;
        self.in_flight = None;
    }

    pub fn image(&self) -> Option<&Arc<ArtImage>> {
        self.shown.as_ref()
    }

    /// The image at [`Self::index`] could not be decoded.
    pub fn failed(&self) -> bool {
        self.failed
    }

    pub fn index(&self) -> usize {
        self.index
    }

    pub fn count(&self) -> usize {
        self.covers.len()
    }

    /// Alt text of the current image, else its file name stem. Sanitised for the screen.
    pub fn title(&self) -> String {
        let Some(c) = self.covers.get(self.index) else {
            return String::new();
        };
        let raw = c
            .alt_text
            .as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| {
                let name = c.relpath.rsplit('/').next().unwrap_or(&c.relpath);
                name.rsplit_once('.').map_or(name, |(stem, _)| stem).to_string()
            });
        crate::screens::screen_text(&raw)
    }

    fn key(&self, index: usize) -> Option<ArtKey> {
        self.covers.get(index).map(|c| ArtKey {
            root_idx: self.root_idx,
            image_relpath: c.relpath.clone(),
            px: self.px,
        })
    }

    fn stamp(&self, key: &ArtKey) -> FileStamp {
        self.roots
            .get(key.root_idx)
            .and_then(|r| FileStamp::of(&r.join(&key.image_relpath)))
            .unwrap_or_default()
    }

    /// Point the viewer at `target` (`None` = nothing to show, free the pictures). Idempotent:
    /// call it every loop iteration. Reads file stamps, so it belongs to the loop, not to
    /// painting. Returns true when what the screen shows changed.
    pub fn sync(&mut self, lib: &LibrarySource, target: Option<CoverTarget<'_>>) -> bool {
        let Some(t) = target.filter(|t| !t.covers.is_empty() && t.px > 0) else {
            let was_open = self.open || self.shown.is_some() || self.failed;
            self.open = false;
            self.covers.clear();
            self.shown = None;
            self.failed = false;
            return was_open;
        };
        let index = t.index.min(t.covers.len() - 1);
        let same_set = self.open
            && self.root_idx == t.root_idx
            && self.px == t.px
            && self.covers.len() == t.covers.len()
            && self.covers.iter().zip(t.covers).all(|(a, b)| a.relpath == b.relpath);
        if same_set && self.index == index {
            return false;
        }
        let before = (self.shown.clone(), self.failed, self.index);
        // A different set always changes what the screen says (count, title), even while the
        // picture is still loading.
        let mut changed = !same_set;
        if same_set {
            self.heading = if index > self.index { 1 } else { -1 };
            self.index = index;
        } else {
            // A different set or size: nothing on screen belongs to it.
            self.roots = lib.roots().to_vec();
            if let Some(l) = self.loader.as_ref() {
                l.set_roots(&self.roots);
            }
            self.root_idx = t.root_idx;
            self.covers = t.covers.to_vec();
            self.px = t.px;
            self.index = index;
            self.heading = 1;
            self.shown = None;
            self.failed = false;
            self.open = true;
        }
        self.want_current();
        let after = (&self.shown, self.failed, self.index);
        changed |= !(same_image(&before.0, after.0) && before.1 == after.1 && before.2 == after.2);
        changed
    }

    /// Show the current image from the cache, or ask for it. The previous picture stays on
    /// screen while it loads.
    fn want_current(&mut self) {
        let Some(key) = self.key(self.index) else {
            return;
        };
        if let Some(img) = self.cache.get(&key) {
            self.shown = Some(img);
            self.failed = false;
            self.prefetch();
            return;
        }
        if self.cache.is_failed(&key, self.stamp(&key)) {
            self.shown = None;
            self.failed = true;
            self.prefetch();
            return;
        }
        self.failed = false;
        self.request(key);
    }

    fn request(&mut self, key: ArtKey) {
        if self.in_flight.as_ref() == Some(&key) {
            return;
        }
        self.in_flight = Some(key.clone());
        let roots = self.roots.clone();
        let loader = self.loader.get_or_insert_with(ArtLoader::new);
        loader.set_roots(&roots);
        loader.request(key);
    }

    /// Decode the neighbour in the direction of travel, then the other one. One at a time: a
    /// request replaces the previous one, so this runs only once the current image is settled.
    fn prefetch(&mut self) {
        let heading = if self.heading < 0 { -1i64 } else { 1 };
        for step in [heading, -heading] {
            let Some(i) = usize::try_from(self.index as i64 + step).ok() else {
                continue;
            };
            let Some(key) = self.key(i) else {
                continue;
            };
            if self.cache.get(&key).is_some() || self.cache.is_failed(&key, self.stamp(&key)) {
                continue;
            }
            self.request(key);
            return;
        }
    }

    /// Collect finished decodes. Returns true when the picture to show changed.
    pub fn poll(&mut self) -> bool {
        let Some(loader) = self.loader.as_mut() else {
            return false;
        };
        let done = loader.poll();
        if done.is_empty() {
            return false;
        }
        let current = self.key(self.index);
        let mut changed = false;
        for (key, res) in done {
            if self.in_flight.as_ref() == Some(&key) {
                self.in_flight = None;
            }
            let is_current = current.as_ref() == Some(&key);
            match res {
                Ok(img) => {
                    let img = Arc::new(img);
                    self.cache.insert(key, Arc::clone(&img));
                    if is_current {
                        self.shown = Some(img);
                        self.failed = false;
                        changed = true;
                    }
                }
                Err(e) => {
                    self.cache.insert_failure(key, e.stamp);
                    if is_current {
                        self.shown = None;
                        self.failed = true;
                        changed = true;
                    }
                }
            }
        }
        if self.open {
            self.prefetch();
        }
        changed
    }
}

fn same_image(a: &Option<Arc<ArtImage>>, b: &Option<Arc<ArtImage>>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => Arc::ptr_eq(a, b),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn key(name: &str) -> ArtKey {
        ArtKey {
            root_idx: 0,
            image_relpath: name.into(),
            px: 4,
        }
    }

    fn tiny() -> ArtImage {
        ArtImage {
            side: 4,
            rgb565: vec![0x1234; 16],
            rgb_side: 2,
            rgb: vec![9; 12],
        }
    }

    fn wait_for(loader: &mut ArtLoader) -> Vec<(ArtKey, Result<ArtImage, ArtError>)> {
        let t0 = Instant::now();
        loop {
            let got = loader.poll();
            if !got.is_empty() || t0.elapsed() > Duration::from_secs(10) {
                return got;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    #[test]
    fn cache_evicts_least_recently_used_at_four_images() {
        let mut c = ArtCache::new();
        for n in 0..4 {
            c.insert(key(&format!("{n}")), Arc::new(tiny()));
        }
        assert!(c.get(&key("0")).is_some(), "touch 0 so 1 is the oldest");
        c.insert(key("4"), Arc::new(tiny()));
        assert_eq!(c.len(), ART_CACHE_MAX_IMAGES);
        assert!(c.get(&key("1")).is_none());
        assert!(c.get(&key("0")).is_some());
    }

    #[test]
    fn cache_respects_the_byte_budget() {
        let mut c = ArtCache::new();
        let big = ArtImage {
            side: 600,
            rgb565: vec![0; 600 * 600],
            rgb_side: 64,
            rgb: vec![0; 64 * 64 * 3],
        };
        c.insert(key("a"), Arc::new(big.clone()));
        c.insert(key("b"), Arc::new(big));
        assert_eq!(c.len(), 1, "two 730 kB images do not fit in 1 MiB: {}", c.bytes());
        assert!(c.bytes() <= ART_CACHE_MAX_BYTES * 2);
    }

    #[test]
    fn failures_are_remembered_per_file_stamp() {
        let mut c = ArtCache::new();
        let s1 = FileStamp { len: 3, mtime_ns: 1 };
        c.insert_failure(key("x"), s1);
        assert!(c.is_failed(&key("x"), s1));
        assert!(!c.is_failed(&key("x"), FileStamp { len: 3, mtime_ns: 2 }), "changed file retries");
        c.clear();
        assert!(!c.is_failed(&key("x"), s1));
    }

    #[test]
    fn ten_requests_decode_only_the_last_and_stale_results_drop() {
        let (gate_tx, gate_rx) = std::sync::mpsc::channel::<()>();
        let (started_tx, started_rx) = std::sync::mpsc::channel::<()>();
        let gate_rx = Mutex::new(gate_rx);
        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let seen_in = Arc::clone(&seen);
        let mut loader = ArtLoader::with_loader(move |k| {
            seen_in.lock().unwrap().push(k.image_relpath.clone());
            if k.image_relpath == "first" {
                started_tx.send(()).unwrap();
                gate_rx.lock().unwrap().recv().unwrap();
            }
            Ok(tiny())
        });
        loader.request(key("first"));
        started_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        for n in 0..10 {
            loader.request(key(&format!("r{n}")));
        }
        gate_tx.send(()).unwrap();
        let got = wait_for(&mut loader);
        assert_eq!(got.len(), 1, "the stale first result is dropped");
        assert_eq!(got[0].0, key("r9"));
        assert_eq!(*seen.lock().unwrap(), vec!["first".to_string(), "r9".to_string()]);
    }

    #[test]
    fn a_panicking_decoder_is_an_error_not_a_crash() {
        let mut loader = ArtLoader::with_loader(|_| panic!("boom"));
        loader.request(key("p"));
        let got = wait_for(&mut loader);
        assert_eq!(got[0].1.as_ref().unwrap_err().kind, ArtErrorKind::Panicked);
        // The thread survived: it serves the next request.
        loader.request(key("q"));
        assert_eq!(wait_for(&mut loader)[0].0, key("q"));
    }

    #[test]
    fn fade_out_takes_120_ms_and_hold_is_800_then_in_over_300() {
        let mut f = ArtFade::new();
        assert_eq!(f.alpha(0), 100);
        f.tick(1_000, true);
        assert!(f.is_animating());
        assert_eq!(f.alpha(1_060), 50);
        f.tick(1_120, true);
        assert_eq!(f.alpha(1_120), 0);
        // Released at 2 000: quiet starts there.
        f.tick(2_000, true);
        for t in [2_033, 2_500, 2_799] {
            f.tick(t, false);
            assert_eq!(f.alpha(t), 0, "still hidden at {t}");
            assert!(!f.is_animating());
        }
        f.tick(2_800, false);
        assert!(f.is_animating());
        assert_eq!(f.alpha(2_800), 0);
        assert_eq!(f.alpha(2_950), 50);
        f.tick(3_100, false);
        assert_eq!(f.alpha(3_100), 100);
        assert_eq!(f.state(), FadeState::Visible);
    }

    #[test]
    fn retrigger_during_fade_in_reverses_from_the_current_alpha() {
        let mut f = ArtFade::new();
        f.tick(0, true);
        f.tick(200, true);
        f.tick(200, false);
        f.tick(1_000, false); // fade in starts at 1 000 (hold from the last trigger at 200)
        f.tick(1_150, false);
        let a = f.alpha(1_150);
        assert_eq!(a, 50);
        f.tick(1_150, true);
        assert_eq!(f.alpha(1_150), a, "no snap");
        assert!(f.alpha(1_180) < a, "falls from where it was");
        f.tick(1_300, true);
        assert_eq!(f.alpha(1_300), 0);
    }

    #[test]
    fn retrigger_during_fade_out_keeps_falling() {
        let mut f = ArtFade::new();
        f.tick(0, true);
        f.tick(60, true);
        let a = f.alpha(60);
        f.tick(60, true);
        assert_eq!(f.alpha(60), a);
    }

    #[test]
    fn paint_clips_and_blends_toward_the_underlying_pixels() {
        let img = ArtImage {
            side: 2,
            rgb565: vec![0xFFFF; 4],
            rgb_side: 2,
            rgb: vec![255; 12],
        };
        let mut fb = FrameBuffer::new(3, 3);
        fb.clear(0);
        paint_art_square(&mut fb, 2, 2, &img, 100);
        assert_eq!(fb.pixels.iter().filter(|p| **p == 0xFFFF).count(), 1, "clipped to the corner");
        let mut fb = FrameBuffer::new(3, 3);
        fb.clear(0);
        paint_art_square(&mut fb, 0, 0, &img, 0);
        assert!(fb.pixels.iter().all(|p| *p == 0));
        paint_art_square(&mut fb, 0, 0, &img, 50);
        assert_eq!(fb.pixels[0], blend_pixel(0, 0xFFFF, 50));
        assert_eq!(fb.pixels[8], 0);
    }

    #[test]
    fn fit_square_returns_exact_sides_for_small_and_wide_sources() {
        let small = Rgb8 {
            w: 2,
            h: 2,
            px: vec![10; 12],
        };
        let a = make_art_image(&small, 6);
        assert_eq!((a.side, a.rgb565.len()), (6, 36));
        assert_eq!(a.rgb.len(), 64 * 64 * 3);
    }

    #[test]
    fn a_file_that_failed_is_not_retried_until_it_changes() {
        use plippa_noubin::{encode_playable_id, LibraryRoot, LibraryWriter};
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::write(root.join("a.wav"), b"RIFF").unwrap();
        std::fs::write(root.join("b.wav"), b"RIFF").unwrap();
        std::fs::write(root.join("cover.jpg"), b"not a jpeg").unwrap();
        std::fs::write(
            root.join("_a.noudata"),
            br#"{"noubinKey":"a.k","noubinKeyNormalised":"a.k","playableItemTitle":"A",
                "mediaList":[{"localFilePaths":["a.wav"]},{"localFilePaths":["b.wav"]}]}"#,
        )
        .unwrap();
        LibraryWriter::open(&LibraryRoot::new(root)).unwrap().full_scan().unwrap();
        let lib = LibrarySource::open(Some(root));
        let local = lib.list_items()[0].id;
        let subject = |position| ArtSubject {
            playable_id: encode_playable_id(root, local),
            item_position: position,
        };

        let attempts = Arc::new(Mutex::new(0usize));
        let counted = Arc::clone(&attempts);
        let file = root.join("cover.jpg");
        let loader = ArtLoader::with_loader(move |_| {
            *counted.lock().unwrap() += 1;
            Err(ArtError {
                kind: ArtErrorKind::Image(ImageError::Corrupt("bad".into())),
                stamp: FileStamp::of(&file).unwrap(),
            })
        });
        let mut art = NowPlayingArt::with_loader(loader);
        let wait_attempts = |n: usize, art: &mut NowPlayingArt| {
            let t0 = Instant::now();
            while *attempts.lock().unwrap() < n && t0.elapsed() < Duration::from_secs(10) {
                std::thread::sleep(Duration::from_millis(2));
            }
            std::thread::sleep(Duration::from_millis(20));
            art.poll();
        };
        art.service(&lib, Some(subject(0)), 96);
        wait_attempts(1, &mut art);
        assert!(art.image().is_none());
        // Moving to another item that shows the same file and back does not try again.
        for position in [1, 0, 1, 0] {
            art.service(&lib, Some(subject(position)), 96);
            std::thread::sleep(Duration::from_millis(20));
            art.poll();
        }
        assert_eq!(*attempts.lock().unwrap(), 1, "one attempt only");
        assert!(art.resolution().has_art(), "the file is still listed as the cover");

        // The file changes (new length): worth another attempt.
        std::fs::write(root.join("cover.jpg"), b"still not a jpeg, but different").unwrap();
        art.service(&lib, Some(subject(1)), 96);
        wait_attempts(2, &mut art);
        assert_eq!(*attempts.lock().unwrap(), 2);
    }

    fn jpeg(w: u32, h: u32, c: [u8; 3]) -> Vec<u8> {
        let px = (0..w * h).flat_map(|_| c).collect();
        plippa_image::encode_jpeg_fit(&Rgb8 { w, h, px }, w.max(h)).unwrap()
    }

    fn put(root: &Path, rel: &str, bytes: &[u8]) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, bytes).unwrap();
    }

    fn art_key(rel: &str) -> ArtKey {
        ArtKey {
            root_idx: 0,
            image_relpath: rel.into(),
            px: 96,
        }
    }

    const BAD_THUMB: &str = "thumbnails/{}-128.jpg";

    fn thumb(stem: &str) -> String {
        BAD_THUMB.replace("{}", stem)
    }

    #[test]
    fn a_corrupt_thumbnail_falls_back_to_the_good_original() {
        let tmp = tempfile::tempdir().unwrap();
        put(tmp.path(), &thumb("a"), b"not a jpeg");
        put(tmp.path(), "a.jpg", &jpeg(64, 64, [200, 10, 10]));
        let img = load_art(tmp.path(), &art_key("a.jpg")).expect("original used");
        let mid = img.rgb565[usize::from(img.side) * usize::from(img.side) / 2];
        assert!(mid >> 11 > 20, "red original shown, got {mid:#06x}");
    }

    #[test]
    fn a_failure_carries_the_originals_stamp_even_when_the_thumbnail_failed_first() {
        let tmp = tempfile::tempdir().unwrap();
        put(tmp.path(), &thumb("a"), b"not a jpeg");
        put(tmp.path(), "a.jpg", &jpeg(1500, 1500, [1, 2, 3])); // 2.25 MP: over the cap
        let err = load_art(tmp.path(), &art_key("a.jpg")).unwrap_err();
        assert_eq!(err.kind, ArtErrorKind::TooLargeForDevice);
        assert_eq!(err.stamp, FileStamp::of(&tmp.path().join("a.jpg")).unwrap());
        put(tmp.path(), "b.jpg", b"also corrupt");
        put(tmp.path(), &thumb("b"), b"corrupt too");
        let err = load_art(tmp.path(), &art_key("b.jpg")).unwrap_err();
        assert_eq!(err.stamp, FileStamp::of(&tmp.path().join("b.jpg")).unwrap());
    }

    #[test]
    fn a_planted_oversize_thumbnail_does_not_bypass_the_device_cap() {
        let tmp = tempfile::tempdir().unwrap();
        put(tmp.path(), &thumb("a"), &jpeg(1500, 1500, [0, 0, 250]));
        put(tmp.path(), "a.jpg", &jpeg(64, 64, [250, 0, 0]));
        let img = load_art(tmp.path(), &art_key("a.jpg")).expect("falls back to the original");
        let mid = img.rgb565[usize::from(img.side) * usize::from(img.side) / 2];
        assert!(mid >> 11 > 20 && mid & 0x1f < 4, "original (red), not the planted blue: {mid:#06x}");
        // A legitimate 1024 x 1024 tier (~1 MP) is still accepted.
        put(tmp.path(), "b.jpg", &jpeg(64, 64, [250, 0, 0]));
        put(tmp.path(), &thumb("b").replace("-128", "-1024"), &jpeg(1024, 1024, [0, 0, 250]));
        let img = load_art(tmp.path(), &art_key("b.jpg")).unwrap();
        let mid = img.rgb565[usize::from(img.side) * usize::from(img.side) / 2];
        assert!(mid & 0x1f > 20, "the 1024 tier is used: {mid:#06x}");
    }

    #[test]
    fn a_neighbour_that_cannot_load_is_requested_once_not_forever() {
        use plippa_noubin::covers::CoverSource;
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        put(&root, "a.jpg", &jpeg(64, 64, [10, 200, 10]));
        put(&root, "b.jpg", b"corrupt original");
        put(&root, &thumb("b"), b"corrupt thumbnail");
        let covers: Vec<CoverRef> = ["a.jpg", "b.jpg"]
            .iter()
            .map(|r| CoverRef {
                relpath: (*r).into(),
                source: CoverSource::Sibling,
                alt_text: None,
                is_image_of_noubin: false,
            })
            .collect();
        let requests = Arc::new(Mutex::new(0usize));
        let counted = Arc::clone(&requests);
        let load_root = root.clone();
        let loader = ArtLoader::with_loader(move |key| {
            *counted.lock().unwrap() += 1;
            load_art(&load_root, key)
        });
        let lib = LibrarySource::open(Some(&root));
        let mut viewer = CoverViewer::with_loader(loader);
        viewer.sync(
            &lib,
            Some(CoverTarget {
                root_idx: 0,
                covers: &covers,
                index: 0,
                px: 96,
            }),
        );
        let t0 = Instant::now();
        while viewer.image().is_none() && t0.elapsed() < Duration::from_secs(10) {
            std::thread::sleep(Duration::from_millis(2));
            viewer.poll();
        }
        assert!(viewer.image().is_some(), "the good image shows");
        // The neighbour (b) is decoded, fails, and must then be left alone.
        for _ in 0..40 {
            std::thread::sleep(Duration::from_millis(5));
            viewer.poll();
        }
        let settled = *requests.lock().unwrap();
        assert_eq!(settled, 2, "one load for a, one for the broken b");
        for _ in 0..40 {
            std::thread::sleep(Duration::from_millis(5));
            viewer.poll();
        }
        assert_eq!(*requests.lock().unwrap(), settled, "no re-request on later polls");
    }

    // ---- BCS_11: store covers ------------------------------------------------------------

    #[test]
    fn a_store_cover_is_decoded_and_centre_cropped_to_the_wanted_side() {
        // A wide 200 x 100 image of one colour: cropped to a square, scaled to 48 px.
        let bytes = jpeg(200, 100, [200, 30, 30]);
        let img = decode_store_cover(&bytes, 48).expect("decodes");
        assert_eq!(img.side, 48);
        assert_eq!(img.rgb565.len(), 48 * 48);
        let centre = img.rgb565[24 * 48 + 24];
        let red = plippa_image::to_rgb565(&Rgb8 { w: 1, h: 1, px: vec![200, 30, 30] })[0];
        let (r, g, b) = (
            (centre >> 11) & 0x1F,
            (centre >> 5) & 0x3F,
            centre & 0x1F,
        );
        let (er, eg, eb) = ((red >> 11) & 0x1F, (red >> 5) & 0x3F, red & 0x1F);
        assert!(r.abs_diff(er) <= 2 && g.abs_diff(eg) <= 3 && b.abs_diff(eb) <= 2, "{centre:04x} vs {red:04x}");
    }

    #[test]
    fn a_store_cover_over_the_device_limits_or_not_an_image_is_refused() {
        // The server may send 8 MB; the device decodes at most 1024 px a side.
        let too_wide = jpeg(1100, 100, [10, 10, 10]);
        assert!(
            matches!(decode_store_cover(&too_wide, 48), Err(ImageError::TooLarge { .. })),
            "a side over the limit is refused, not downscaled after a full decode"
        );
        let too_many_pixels = jpeg(1024, 1024, [10, 10, 10]);
        // 1024 x 1024 is exactly the pixel limit: allowed.
        assert!(decode_store_cover(&too_many_pixels, 48).is_ok());
        assert!(decode_store_cover(b"not an image at all", 48).is_err());
        assert!(decode_store_cover(&[], 48).is_err());
        assert!(decode_store_cover(&jpeg(10, 10, [1, 2, 3]), 0).is_err());
        assert_eq!(STORE_COVER_LIMITS.max_side, 1024);
        assert_eq!(STORE_COVER_LIMITS.max_bytes, 4 * 1024 * 1024);
        // Bytes over the encoded cap are refused by size alone.
        let huge = vec![0u8; 4 * 1024 * 1024 + 1];
        assert!(decode_store_cover(&huge, 48).is_err());
    }

    #[test]
    fn the_store_cover_cache_is_bounded_like_library_art() {
        let pic = |side: u16| {
            Arc::new(ArtImage {
                side,
                rgb565: vec![0; usize::from(side) * usize::from(side)],
                rgb_side: 0,
                rgb: Vec::new(),
            })
        };
        let mut cache = StoreCoverCache::new();
        assert!(cache.is_empty());
        for n in 0..(ART_CACHE_MAX_IMAGES + 3) {
            cache.insert(&format!("r{n}"), 48, pic(48));
        }
        assert_eq!(cache.len(), ART_CACHE_MAX_IMAGES, "oldest pictures fall out");
        assert!(cache.get("r0", 48).is_none());
        assert!(cache.get(&format!("r{}", ART_CACHE_MAX_IMAGES + 2), 48).is_some());
        // The side is part of the key.
        assert!(cache.get(&format!("r{}", ART_CACHE_MAX_IMAGES + 2), 64).is_none());
        // Re-inserting a key replaces it rather than growing the cache.
        let before = cache.len();
        cache.insert(&format!("r{}", ART_CACHE_MAX_IMAGES + 2), 48, pic(48));
        assert_eq!(cache.len(), before);
        // The byte bound applies too, and the newest picture always stays.
        let mut big = StoreCoverCache::new();
        big.insert("a", 512, pic(512));
        big.insert("b", 512, pic(512));
        assert!(big.bytes() <= ART_CACHE_MAX_BYTES || big.len() == 1);
        assert!(big.get("b", 512).is_some());
    }
}
