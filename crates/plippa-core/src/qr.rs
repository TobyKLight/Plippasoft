//! QR codes shared by the LAN portal and the device screen.
//!
//! One encoder, two renderings: [`QrMatrix::to_svg`] for the portal (self-contained
//! SVG, no external references) and [`QrMatrix::to_rows`] for the 320x240 screen,
//! which draws the modules itself.
//!
//! `qrcodegen` (Project Nayuki, zero dependencies) does the encoding; this module
//! owns the small vocabulary Plippa uses and keeps the dependency out of the other
//! crates.

use qrcodegen::{QrCode, QrCodeEcc};

/// Error-correction level for a QR symbol.
///
/// Higher levels survive more scanning damage at the cost of a denser symbol. A
/// 320x240 screen showing a join URL at arm's length wants at least [`Self::Medium`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, PartialOrd, Ord)]
pub enum QrErrorLevel {
    /// ~7% recovery.
    Low,
    /// ~15% recovery.
    #[default]
    Medium,
    /// ~25% recovery.
    Quartile,
    /// ~30% recovery.
    High,
}

impl From<QrErrorLevel> for QrCodeEcc {
    fn from(level: QrErrorLevel) -> Self {
        match level {
            QrErrorLevel::Low => QrCodeEcc::Low,
            QrErrorLevel::Medium => QrCodeEcc::Medium,
            QrErrorLevel::Quartile => QrCodeEcc::Quartile,
            QrErrorLevel::High => QrCodeEcc::High,
        }
    }
}

/// Why a QR symbol could not be produced.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum QrError {
    /// The text does not fit in any QR version at the requested error level.
    #[error("text is too long for a QR code at this error-correction level")]
    TooLong,
}

/// An encoded QR symbol.
///
/// `width` is the module count per side (21..=177); modules are row-major from the
/// top left, `true` meaning dark.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QrMatrix {
    /// Modules per side.
    pub width: u32,
    modules: Vec<bool>,
}

impl QrMatrix {
    /// Encode `text` at `level`, choosing the smallest version that fits.
    pub fn encode(text: &str, level: QrErrorLevel) -> Result<Self, QrError> {
        let code = QrCode::encode_text(text, level.into()).map_err(|_| QrError::TooLong)?;
        let width = code.size() as u32;
        let mut modules = Vec::with_capacity((width * width) as usize);
        for y in 0..width as i32 {
            for x in 0..width as i32 {
                modules.push(code.get_module(x, y));
            }
        }
        Ok(Self { width, modules })
    }

    /// `true` when the module at (`x`, `y`) is dark. Out-of-range reads are light.
    pub fn get(&self, x: u32, y: u32) -> bool {
        if x >= self.width || y >= self.width {
            return false;
        }
        self.modules[(y * self.width + x) as usize]
    }

    /// The symbol as rows of booleans for a framebuffer blit.
    pub fn to_rows(&self) -> Vec<Vec<bool>> {
        (0..self.width)
            .map(|y| (0..self.width).map(|x| self.get(x, y)).collect())
            .collect()
    }

    /// A self-contained SVG rendering.
    ///
    /// `module_px` is clamped to `1..=64` and `quiet_modules` to `0..=16`, so a bad
    /// caller cannot produce a zero-sized or gigantic document. The background is a
    /// white rectangle covering the quiet zone, there are no external references and
    /// no scripts, and the viewBox makes the output scale to any container.
    pub fn to_svg(&self, module_px: u32, quiet_modules: u32) -> String {
        let module_px = module_px.clamp(1, 64);
        let quiet = quiet_modules.min(16);
        let span = self.width + quiet * 2;
        let px = span * module_px;

        let mut path = String::new();
        for y in 0..self.width {
            for x in 0..self.width {
                if self.get(x, y) {
                    let mx = (x + quiet) * module_px;
                    let my = (y + quiet) * module_px;
                    // One subpath per module keeps this readable and diffable.
                    use std::fmt::Write as _;
                    // Writing into a String cannot fail.
                    let _ = write!(
                        path,
                        "M{mx} {my}h{module_px}v{module_px}h-{module_px}z"
                    );
                }
            }
        }

        format!(
            concat!(
                r##"<svg xmlns="http://www.w3.org/2000/svg" version="1.1" "##,
                r##"viewBox="0 0 {px} {px}" width="{px}" height="{px}" "##,
                r##"shape-rendering="crispEdges">"##,
                r##"<rect width="{px}" height="{px}" fill="#ffffff"/>"##,
                r##"<path d="{path}" fill="#000000"/>"##,
                r##"</svg>"##
            ),
            px = px,
            path = path
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const JOIN_URL: &str = "http://192.168.1.24:8420/portal/stores/bandcamp/connect?token=abc123";

    #[test]
    fn a_url_encodes_to_a_valid_square_matrix() {
        let matrix = QrMatrix::encode(JOIN_URL, QrErrorLevel::Medium).expect("fits");
        assert!(
            (21..=177).contains(&matrix.width),
            "width {} is not a QR version",
            matrix.width
        );
        assert_eq!((matrix.width - 17) % 4, 0);
        assert_eq!(matrix.modules.len(), (matrix.width * matrix.width) as usize);

        // Every QR symbol has a dark finder module at (0, 0) and (width-1, 0).
        assert!(matrix.get(0, 0));
        assert!(matrix.get(matrix.width - 1, 0));
        assert!(!matrix.get(matrix.width, 0), "out of range reads light");

        let rows = matrix.to_rows();
        assert_eq!(rows.len(), matrix.width as usize);
        assert!(rows.iter().all(|row| row.len() == matrix.width as usize));
        assert_eq!(rows[0][0], matrix.get(0, 0));

        // A denser symbol for the same payload at a higher error level, and a
        // shorter payload in a sparser one.
        let high = QrMatrix::encode(JOIN_URL, QrErrorLevel::High).expect("fits");
        assert!(high.width >= matrix.width);
        let short = QrMatrix::encode("https://x.example", QrErrorLevel::Low).expect("fits");
        assert!(short.width <= matrix.width);
    }

    #[test]
    fn svg_is_self_contained_and_has_a_quiet_zone() {
        let matrix = QrMatrix::encode(JOIN_URL, QrErrorLevel::Medium).expect("fits");
        let svg = matrix.to_svg(4, 4);

        assert!(svg.starts_with("<svg"), "{svg}");
        assert!(svg.ends_with("</svg>"), "{svg}");
        assert!(svg.contains("xmlns=\"http://www.w3.org/2000/svg\""), "{svg}");
        assert!(svg.contains("viewBox=\"0 0 "), "{svg}");
        assert!(!svg.contains("<script"), "{svg}");
        assert!(!svg.contains("<image"), "{svg}");
        assert!(!svg.contains("<use"), "{svg}");
        assert!(!svg.contains("href"), "{svg}");
        assert!(!svg.contains("http://192.168"), "the payload is not echoed");

        // Background covers the whole span, including the quiet zone.
        let span = (matrix.width + 8) * 4;
        assert!(
            svg.contains(&format!("<rect width=\"{span}\" height=\"{span}\" fill=\"#ffffff\"/>")),
            "{svg}"
        );
        // The first dark module starts after the quiet zone.
        let quiet_px = 4 * 4;
        assert!(
            svg.contains(&format!("M{quiet_px} {quiet_px}h4v4h-4z")),
            "{svg}"
        );

        // Clamping keeps the document sane.
        let zero = matrix.to_svg(0, 0);
        let expected = matrix.width;
        assert!(
            zero.contains(&format!("viewBox=\"0 0 {expected} {expected}\"")),
            "{zero}"
        );
    }

    #[test]
    fn too_long_input_is_an_error_not_a_panic() {
        let huge = "x".repeat(8000);
        assert_eq!(
            QrMatrix::encode(&huge, QrErrorLevel::Low),
            Err(QrError::TooLong)
        );
        assert_eq!(
            QrMatrix::encode(&huge, QrErrorLevel::High),
            Err(QrError::TooLong)
        );
        assert_eq!(QrError::TooLong.to_string(), "text is too long for a QR code at this error-correction level");
    }

    #[test]
    fn empty_text_still_produces_a_symbol() {
        let matrix = QrMatrix::encode("", QrErrorLevel::Low).expect("empty fits");
        assert!(matrix.width >= 21);
        let svg = matrix.to_svg(2, 2);
        assert!(svg.starts_with("<svg"));
    }
}
