//! Unite flows (NB_09): fetch/upload/download the media of a release, match it to the release
//! metadata (NB_07 engine in `plippa_noubin::unite`), apply the plan to the library and refresh
//! metadata later. Everything that touches the network or untrusted files goes through
//! `crate::web` (NB_08 client, SSRF policy, consent token) and the modules here:
//!
//! * [`sniff`]: content sniffing by magic bytes, image dimensions from headers
//! * [`fsops`]: path-safe, symlink-refusing, never-overwriting moves and writes + rollback
//! * [`images`]: cover/thumbnail URL planning and safe local names
//! * [`hub`]: sessions, uploads, the download worker, proposal/confirm/apply
//! * [`refresh`]: merge fetched metadata into an existing item without touching user data

pub mod fsops;
pub mod hub;
pub mod images;
pub mod refresh;
pub mod sniff;

pub use hub::{UniteError, UniteHub};

#[cfg(test)]
pub(crate) mod tests;
