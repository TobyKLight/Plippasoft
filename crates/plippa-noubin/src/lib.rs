//! Noubin standard helpers: `.noudata`, NDEF key extract/normalise, library paths + SQLite index.

pub mod associate;
pub mod confine;
pub mod covers;
mod cue_paste;
mod discover;
mod error;
mod import_path;
mod index;
mod json_guard;
mod keygen;
mod ndef;
mod ndef_encode;
mod netpolicy;
mod normalise;
mod noudata;
mod noudata_types;
mod paths;
mod playable_id;
mod playlist;
mod safe_name;
mod sanitise;
mod sanitise_doc;
mod timestamp;
pub mod unite;
mod url_norm;

pub use associate::{
    is_clean_relpath, is_release_kind, track_associate_target, TrackTarget,
    MAX_TRACK_NAME_ATTEMPTS, MAX_TRACK_STEM_BYTES,
};
pub use cue_paste::{
    cue_list_to_paste_text, format_hms_ms, parse_cue_paste, sanitise_cue_text, CuePaste,
    CueWarning, CueWarningKind, IgnoredLine, ParsedCue, MAX_CUE_TEXT_BYTES, MAX_PASTE_CUES,
};
pub use discover::{
    default_mount_parents, discover_removable_libraries, discover_under_parents,
    resolve_library_roots, DiscoveredLibrary, MountIdentity,
};
pub use error::{CoverEditError, LibraryError, NdefError, NoubinError, NoudataError};
pub use import_path::{
    audiobook_author_segment, import_placement, ExtractedFields, ImportKind, ImportPlacement,
};
pub use index::{
    cue_entries, index_db_path, library_root_available, normalise_media_relpath,
    view_is_item_level, waveform_hash, waveform_path, waveform_relpath, CueEntry, ItemRecord,
    LibraryKind, LibraryReader, LibraryWriter, LookupHit, OpenWait, LookupResult, MediaCandidate,
    PlayableRecord, ProbeReport, ScanReport, TrackRow, INDEX_DB_FILENAME, MAX_REFERENCE_SCAN_DOCS,
    PLAYER_DB_DIR, SCHEMA_VERSION, WAVEFORM_DIR,
};
pub use json_guard::{
    check_json_object_limits, check_json_object_limits_with, first_significant_byte, JsonLimitError, MAX_JSON_DEPTH,
    MAX_JSON_ELEMENTS,
};
pub use keygen::{generate_local_noubin_key, KeyGenError, LocalKeyParams};
pub use ndef::{extract_noubin_key, extract_uri_records};
pub use ndef_encode::{
    encode_text_record, encode_uri_record, max_text_key_bytes_type2, max_text_key_bytes_type4,
    wrap_type2_ndef_tlv, DEFAULT_TEXT_LANG,
};
pub use netpolicy::{
    block_reason_ip, check_host_allowed, encode_url_component, is_blocked_hostname, is_blocked_ip,
    parse_fetch_url, parse_host, parse_https_url, redact_url_for_log, resolve_redirect,
    sanitise_url, BlockReason, Host, ParsedUrl, UrlError, MAX_URL_BYTES,
};
pub use normalise::normalise_noubin_key;
pub use noudata::{
    classify_type, effective_type, fs_release_resolver, is_dynamic_playlist,
    is_empty_user_playlist, is_minimally_playable, is_playlist, parse_noudata, parse_noudata_str,
    parse_noudata_with_warnings, read_noudata_capped, MediaListItem, NoudataDocument,
    TypeClassification, MAX_LOCAL_NOUDATA_BYTES, MAX_LOCAL_NOUDATA_ELEMENTS, NOUDATA_VERSION, WHAT_AM_I_LOOKING_AT,
};
pub use noudata_types::{
    Contributor, CoverImage, Credits, Cue, CueList, CueListType, CueTextSpan, FurtherLink,
    ImageEntry, ItemLocalPlaybackData, LocalPlaybackData, NoudataType, NoudataWarning,
    OverrideResumeBehaviour, PlatformType, PlayableItemProfile, PrimaryArtist, ReleaseData,
    ReleasePlatform, UniteData, UserNotes,
};
pub use paths::{
    collect_noudata_paths, dir_excluded_from_scan, LibraryRoot, MIN_LIBRARY_SCAN_DEPTH,
};
pub use playable_id::{
    decode_playable_id, display_path_with_library, encode_playable_id, library_display_label,
    library_namespace,
};
pub use playlist::{
    new_playlist_document, playlist_entry, playlist_file_stem, relative_media_path,
    unique_playlist_path, validate_playlist_title, write_noudata_atomic, AtomicWriteError,
    FileRevision, WriteGuard, MAX_PLAYLIST_TITLE_CHARS, PLAYLISTS_DIR,
};
pub use safe_name::{sanitize_filename, sanitize_path_segment};
pub use sanitise::{
    is_safe_relative_path, safe_path_segment, sanitise_key, sanitise_line, sanitise_multiline,
    sanitise_tag, sanitise_title, strip_unsafe_chars, truncate_bytes, truncate_chars,
    MAX_KEY_BYTES, MAX_LONG_TEXT_BYTES, MAX_SEGMENT_BYTES, MAX_TAGS, MAX_TAG_CHARS,
    MAX_TITLE_CHARS,
};
pub use sanitise_doc::{sanitise_document, SanitiseOptions};
pub use timestamp::{NoubinTimestamp, TimestampError};
pub use url_norm::{
    fetch_url_for_key, is_url_noubin_key, normalise_noubin_url, FetchUrlError, NoubinFetchUrl,
    WEB_NOUDATA,
};
