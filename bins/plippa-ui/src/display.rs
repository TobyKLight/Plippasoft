//! Framebuffer, packed RGB565 colour and display backends.
//!
//! Text and shapes live in [`crate::ui`]: the framebuffer is deliberately dumb (packed `u16`
//! pixels, clamped writes) so the design system can be changed without touching the display
//! plumbing.

/// Packed RGB565 pixel.
pub type Rgb565 = u16;

pub const fn rgb565(r: u8, g: u8, b: u8) -> Rgb565 {
    (((r as u16) & 0xf8) << 8) | (((g as u16) & 0xfc) << 3) | ((b as u16) >> 3)
}

/// In-memory 16-bit framebuffer.
#[derive(Debug, Clone)]
pub struct FrameBuffer {
    pub width: u16,
    pub height: u16,
    pub pixels: Vec<Rgb565>,
}

impl FrameBuffer {
    pub fn new(width: u16, height: u16) -> Self {
        let len = (width as usize) * (height as usize);
        Self {
            width,
            height,
            pixels: vec![0; len],
        }
    }

    pub fn clear(&mut self, color: Rgb565) {
        self.pixels.fill(color);
    }

    pub fn set_pixel(&mut self, x: u16, y: u16, color: Rgb565) {
        if x < self.width && y < self.height {
            let i = y as usize * self.width as usize + x as usize;
            self.pixels[i] = color;
        }
    }

    pub fn fill_rect(&mut self, x: u16, y: u16, w: u16, h: u16, color: Rgb565) {
        let x1 = x.saturating_add(w).min(self.width);
        let y1 = y.saturating_add(h).min(self.height);
        for yy in y..y1 {
            for xx in x..x1 {
                let i = yy as usize * self.width as usize + xx as usize;
                self.pixels[i] = color;
            }
        }
    }
}

/// Pixel rectangle for partial presents (WAVE_02).
///
/// Coordinates are absolute framebuffer pixels; `w`/`h` of 0 mean "empty" (nothing to
/// present). [`Rect::union`] is the bounding box, used by the display mailbox so a dropped
/// intermediate frame can never leave stale pixels behind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Rect {
    pub x: u16,
    pub y: u16,
    pub w: u16,
    pub h: u16,
}

impl Rect {
    /// Nothing to present.
    pub const EMPTY: Self = Self {
        x: 0,
        y: 0,
        w: 0,
        h: 0,
    };

    pub const fn new(x: u16, y: u16, w: u16, h: u16) -> Self {
        Self { x, y, w, h }
    }

    pub fn is_empty(&self) -> bool {
        self.w == 0 || self.h == 0
    }

    pub fn area(&self) -> u32 {
        u32::from(self.w) * u32::from(self.h)
    }

    /// Bounding box of `self` and `other`; an empty rect is the identity.
    pub fn union(self, other: Rect) -> Rect {
        if self.is_empty() {
            return other;
        }
        if other.is_empty() {
            return self;
        }
        let x = self.x.min(other.x);
        let y = self.y.min(other.y);
        let right = self
            .x
            .saturating_add(self.w)
            .max(other.x.saturating_add(other.w));
        let bottom = self
            .y
            .saturating_add(self.h)
            .max(other.y.saturating_add(other.h));
        Rect::new(x, y, right.saturating_sub(x), bottom.saturating_sub(y))
    }

    /// Clip to a `width`×`height` framebuffer (empty when fully outside).
    pub fn clamped(self, width: u16, height: u16) -> Rect {
        let x = self.x.min(width);
        let y = self.y.min(height);
        Rect::new(
            x,
            y,
            self.w.min(width.saturating_sub(x)),
            self.h.min(height.saturating_sub(y)),
        )
    }
}

/// Display output sink.
pub trait DisplayBackend {
    fn size(&self) -> (u16, u16);

    /// Present the whole frame.
    fn present(&mut self, frame: &FrameBuffer);

    /// Present only `rect` of `frame` (WAVE_02). Backends that cannot window simply present
    /// the whole frame, so every implementation stays correct without changes.
    fn present_rect(&mut self, frame: &FrameBuffer, rect: Rect) {
        let _ = rect;
        self.present(frame);
    }

    /// Black frame / backlight off for clean shutdown (Ctrl-C). Default: no-op.
    fn blank(&mut self) {}
}

/// Records frames for unit tests; never panics.
///
/// `screen` accumulates both full and partial presents (so a sequence of rect presents can be
/// compared against the app's full framebuffer); `last` is the composite after the last
/// present, and `rects` records what each present covered.
#[derive(Debug)]
pub struct MockDisplay {
    pub width: u16,
    pub height: u16,
    pub frames: usize,
    pub last: Option<FrameBuffer>,
    pub rects: Vec<Rect>,
    screen: FrameBuffer,
}

impl MockDisplay {
    pub fn new(width: u16, height: u16) -> Self {
        Self {
            width,
            height,
            frames: 0,
            last: None,
            rects: Vec::new(),
            screen: FrameBuffer::new(width, height),
        }
    }

    /// The composite of every present so far.
    pub fn screen(&self) -> &FrameBuffer {
        &self.screen
    }
}

impl DisplayBackend for MockDisplay {
    fn size(&self) -> (u16, u16) {
        (self.width, self.height)
    }

    fn present(&mut self, frame: &FrameBuffer) {
        self.frames += 1;
        self.screen = frame.clone();
        self.rects
            .push(Rect::new(0, 0, frame.width, frame.height));
        self.last = Some(self.screen.clone());
    }

    fn present_rect(&mut self, frame: &FrameBuffer, rect: Rect) {
        let r = rect.clamped(self.width, self.height);
        if r.is_empty() {
            return;
        }
        self.frames += 1;
        for y in r.y..r.y.saturating_add(r.h) {
            let src = y as usize * frame.width as usize;
            let dst = y as usize * self.width as usize;
            for x in r.x..r.x.saturating_add(r.w) {
                self.screen.pixels[dst + x as usize] = frame.pixels[src + x as usize];
            }
        }
        self.rects.push(r);
        self.last = Some(self.screen.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pack_and_clamp_writes() {
        // RGB565 packing: 5/6/5 bits, low bits dropped.
        assert_eq!(rgb565(255, 255, 255), 0xFFFF);
        assert_eq!(rgb565(0, 0, 0), 0x0000);
        assert_eq!(rgb565(255, 0, 0), 0xF800);
        assert_eq!(rgb565(0, 255, 0), 0x07E0);
        assert_eq!(rgb565(0, 0, 255), 0x001F);

        let mut fb = FrameBuffer::new(4, 3);
        fb.clear(0);
        fb.fill_rect(2, 1, 10, 10, 0x1234);
        assert_eq!(fb.pixels[1 * 4 + 2], 0x1234);
        assert_eq!(fb.pixels.last().copied(), Some(0x1234));
        // Out-of-range writes are ignored, never a panic.
        fb.set_pixel(4, 3, 0xFFFF);
        fb.set_pixel(u16::MAX, u16::MAX, 0xFFFF);
        fb.fill_rect(u16::MAX - 1, u16::MAX - 1, 4, 4, 0xFFFF);
        assert!(fb.pixels.iter().all(|&p| p == 0 || p == 0x1234));
    }

    #[test]
    fn rect_union_area_and_clamp() {
        let a = Rect::new(2, 3, 4, 5);
        let b = Rect::new(10, 1, 2, 2);
        assert_eq!(a.area(), 20);
        assert!(Rect::EMPTY.is_empty());
        assert!(!a.is_empty());
        assert_eq!(a.union(Rect::EMPTY), a);
        assert_eq!(Rect::EMPTY.union(a), a);
        assert_eq!(a.union(b), Rect::new(2, 1, 10, 7));
        // Clipping never invents pixels outside the framebuffer.
        assert_eq!(Rect::new(6, 6, 8, 8).clamped(10, 10), Rect::new(6, 6, 4, 4));
        assert!(Rect::new(10, 0, 4, 4).clamped(10, 10).is_empty());
    }

    #[test]
    fn mock_present_rect_patches_the_composite() {
        let mut d = MockDisplay::new(8, 4);
        let mut full = FrameBuffer::new(8, 4);
        full.clear(1);
        d.present(&full);
        let mut patch = full.clone();
        patch.fill_rect(2, 1, 3, 2, 7);
        d.present_rect(&patch, Rect::new(2, 1, 3, 2));
        assert_eq!(d.frames, 2);
        assert_eq!(d.rects.len(), 2);
        assert_eq!(d.rects[0], Rect::new(0, 0, 8, 4));
        assert_eq!(d.rects[1], Rect::new(2, 1, 3, 2));
        // The composite contains the patch and nothing else changed.
        let screen = d.screen();
        assert_eq!(screen.pixels[1 * 8 + 2], 7);
        assert_eq!(screen.pixels[0], 1);
        assert_eq!(d.last.as_ref().unwrap().pixels, screen.pixels);
        // A rect outside the panel changes nothing and is not recorded.
        d.present_rect(&patch, Rect::new(8, 0, 2, 2));
        assert_eq!(d.frames, 2);
    }

    #[test]
    fn default_present_rect_falls_back_to_a_full_present() {
        struct Whole(MockDisplay);
        impl DisplayBackend for Whole {
            fn size(&self) -> (u16, u16) {
                self.0.size()
            }
            fn present(&mut self, frame: &FrameBuffer) {
                self.0.present(frame);
            }
        }
        let mut w = Whole(MockDisplay::new(4, 4));
        let fb = FrameBuffer::new(4, 4);
        w.present_rect(&fb, Rect::new(1, 1, 1, 1));
        assert_eq!(w.0.rects, vec![Rect::new(0, 0, 4, 4)]);
    }
}
