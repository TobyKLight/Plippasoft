//! Release overview shown to the user before anything else happens (§3.5, playback
//! experience A): title, artists, track list, cover URL (**not** fetched), release platforms
//! in the artist's order with the right verb, further links and free-download availability.
//!
//! Built from a document that already went through the whole-document sanitiser; every string
//! here is still rendered with `textContent` by the portal.

use plippa_noubin::{NoudataDocument, NoudataWarning, PlatformType, ReleasePlatform};
use serde::Serialize;

/// Most warnings echoed to the portal.
pub const MAX_OVERVIEW_WARNINGS: usize = 20;

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct OverviewTrack {
    /// 1-based position in `mediaList`.
    pub position: usize,
    pub title: Option<String>,
    pub expected_length_ms: Option<u64>,
    pub free_download: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct OverviewPlatform {
    pub name: String,
    /// `store` | `DRMStore` | `streaming` | `other`.
    pub platform_type: &'static str,
    /// `customVerb`, else `buy` / `stream` / `listen to` (§`releasePlatforms`).
    pub verb: String,
    /// Ready-to-show call to action: `<verb> <title> on <platform>`.
    pub label: String,
    /// Sanitised `https` URL. Shown as text; never fetched by this package.
    pub url: String,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct OverviewLink {
    pub title: Option<String>,
    pub url: String,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct FreeDownload {
    /// At least one item has a `freeDownloadURL`.
    pub available: bool,
    /// Every item has one.
    pub all_items: bool,
    pub item_count: usize,
    /// Distinct URLs that a download would contact (shown for consent in NB_09).
    pub urls: Vec<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ReleaseOverview {
    pub title: Option<String>,
    pub artist: Option<String>,
    /// `music` / `mix` / `audiobook` / `podcast` / `other`.
    pub profile: &'static str,
    pub tracks: Vec<OverviewTrack>,
    pub total_expected_ms: Option<u64>,
    /// Sanitised `https` cover URL, not fetched here.
    pub cover_url: Option<String>,
    /// In the artist's order.
    pub platforms: Vec<OverviewPlatform>,
    pub further_links: Vec<OverviewLink>,
    pub free_download: FreeDownload,
    pub warnings: Vec<String>,
}

fn platform_type_name(t: PlatformType) -> &'static str {
    match t {
        PlatformType::Store => "store",
        PlatformType::DrmStore => "DRMStore",
        PlatformType::Streaming => "streaming",
        PlatformType::Other => "other",
    }
}

/// The verb for a platform: `customVerb` when present, else by type (`other` = "listen to").
pub fn platform_verb(p: &ReleasePlatform) -> String {
    if let Some(v) = p.custom_verb.as_deref().filter(|v| !v.is_empty()) {
        return v.to_string();
    }
    match p.effective_type() {
        PlatformType::Store | PlatformType::DrmStore => "buy",
        PlatformType::Streaming => "stream",
        PlatformType::Other => "listen to",
    }
    .to_string()
}

impl ReleaseOverview {
    pub fn from_document(doc: &NoudataDocument, warnings: &[NoudataWarning]) -> Self {
        let title = doc.playable_item_title.clone();
        let subject = title.clone().unwrap_or_else(|| "this release".to_string());
        let rd = doc.release_data.as_ref();

        let platforms = rd
            .and_then(|r| r.release_platforms.as_ref())
            .map(|v| {
                v.iter()
                    .map(|p| {
                        let verb = platform_verb(p);
                        OverviewPlatform {
                            name: p.platform_name.clone(),
                            platform_type: platform_type_name(p.effective_type()),
                            label: format!("{verb} {subject} on {}", p.platform_name),
                            verb,
                            url: p.platform_url.clone(),
                        }
                    })
                    .collect()
            })
            .unwrap_or_default();

        let further_links = rd
            .and_then(|r| r.further_links.as_ref())
            .map(|v| {
                v.iter()
                    .map(|l| OverviewLink {
                        title: l.link_title.clone(),
                        url: l.link_url.clone(),
                    })
                    .collect()
            })
            .unwrap_or_default();

        let tracks: Vec<OverviewTrack> = doc
            .media_list
            .iter()
            .enumerate()
            .map(|(i, m)| OverviewTrack {
                position: i + 1,
                title: m.media_title.clone(),
                expected_length_ms: m
                    .unite_data
                    .as_ref()
                    .and_then(|u| u.expected_length)
                    .map(|t| t.as_millis()),
                free_download: m
                    .unite_data
                    .as_ref()
                    .is_some_and(|u| u.free_download_url.is_some()),
            })
            .collect();
        let with_len: Vec<u64> = tracks.iter().filter_map(|t| t.expected_length_ms).collect();
        let total_expected_ms = (!with_len.is_empty()).then(|| with_len.iter().sum());

        let mut urls: Vec<String> = Vec::new();
        for m in &doc.media_list {
            if let Some(u) = m
                .unite_data
                .as_ref()
                .and_then(|u| u.free_download_url.clone())
            {
                if !urls.contains(&u) && urls.len() < 500 {
                    urls.push(u);
                }
            }
        }
        let item_count = tracks.iter().filter(|t| t.free_download).count();

        ReleaseOverview {
            title,
            artist: doc.primary_artist_display(),
            profile: profile_name(doc),
            free_download: FreeDownload {
                available: item_count > 0,
                all_items: !tracks.is_empty() && item_count == tracks.len(),
                item_count,
                urls,
            },
            total_expected_ms,
            cover_url: rd
                .and_then(|r| r.cover_image.as_ref())
                .and_then(|c| c.images.iter().find_map(|i| i.image_web_url.clone())),
            tracks,
            platforms,
            further_links,
            warnings: warnings
                .iter()
                .take(MAX_OVERVIEW_WARNINGS)
                .map(|w| w.to_string())
                .collect(),
        }
    }
}

fn profile_name(doc: &NoudataDocument) -> &'static str {
    use plippa_noubin::PlayableItemProfile::*;
    match doc.profile() {
        Music => "music",
        Mix => "mix",
        Audiobook => "audiobook",
        Podcast => "podcast",
        Other => "other",
    }
}
