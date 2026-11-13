//! Build identity shown on `System > Debug` (UI_17).
//!
//! The values come from `build.rs`, which stamps them at compile time. Without the stamp
//! (e.g. a `rustc` invocation that bypassed cargo) every field reads `unknown`; the screen
//! then says so rather than inventing a version.

/// Short commit the binary was built from, `-dirty` when the tree had uncommitted changes.
pub const GIT: &str = match option_env!("PLIPPA_BUILD_GIT") {
    Some(v) => v,
    None => "unknown",
};

/// Build time from the compiling machine, `YYYY-MM-DD HH:MM` **UTC**. UTC because the
/// cross-compile host and the device can sit in different zones, so an unlabelled local time
/// would be the one thing on this screen a human could not interpret.
pub const TIME: &str = match option_env!("PLIPPA_BUILD_TIME") {
    Some(v) => v,
    None => "unknown",
};

/// Cargo profile the binary was built with (`debug` / `release`).
pub const PROFILE: &str = match option_env!("PLIPPA_BUILD_PROFILE") {
    Some(v) => v,
    None => "unknown",
};

/// Crate version from `Cargo.toml`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The one line the Debug screen prints, e.g.
/// `v0.1.0 3f2a1c9 2026-10-21 12:45 UTC release`.
///
/// `Small` (6 px per cell) fits 50 characters across the content width; the worst case here
/// (`<7-hex>-dirty`, an `unknown` time) is 51 with the `UTC` marker, so `line()` is measured
/// in the test below and the screen clips rather than wraps.
pub fn line() -> String {
    format!("v{VERSION} {GIT} {TIME} UTC {PROFILE}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_build_line_names_the_commit_time_and_profile() {
        let l = line();
        assert!(l.starts_with(&format!("v{VERSION} ")), "no version: {l}");
        assert_eq!(l.split(' ').next_back(), Some(PROFILE), "no profile: {l}");
        assert!(!l.contains("  "), "double space: {l}");
    }

    #[test]
    fn the_build_line_fits_the_small_cell_width() {
        // 50 cells at 6 px inside the 304 px content width. The stamp is deterministic apart
        // from a dirty marker, so this pins the real string rather than a guess.
        let l = line();
        assert!(
            l.chars().count() <= 50,
            "too wide ({} chars): {l}",
            l.chars().count()
        );
        assert!(l.contains(" UTC "), "the time must say which zone: {l}");
    }
}
