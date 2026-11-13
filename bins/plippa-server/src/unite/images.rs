//! Cover art and thumbnails for unite / refresh (Noubin §1.27, §4.8, §2 linkhost layout).
//!
//! Planning is pure: which URLs would be fetched (all of them are listed in the consent
//! dialog), and how downloaded/uploaded images are named locally. Downloading is done by
//! [`crate::web::download`]; installing into the library by `hub`.

use plippa_noubin::{parse_fetch_url, safe_path_segment, NoudataDocument, ParsedUrl};

/// Cover entries downloaded per release (more are ignored, not fetched).
pub const MAX_COVER_DOWNLOADS: usize = 4;
/// §1.27 thumbnail tiers.
pub const THUMB_TIERS: [u32; 3] = [128, 512, 1024];
/// §1.27 thumbnail formats.
pub const THUMB_EXTS: [&str; 2] = ["jpg", "webp"];

/// One `coverImage.images[]` entry with a fetchable `imageWebURL`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoverTarget {
    /// Position in `releaseData.coverImage.images`.
    pub entry: usize,
    /// Canonical https URL (already passed the block-list rules).
    pub url: String,
    /// Path of the URL up to and including the last `/` (`/noubin/a/r/`).
    dir_path: String,
    /// Remote file name without extension, still percent-encoded (`cover`).
    stem_enc: String,
    host: plippa_noubin::Host,
    /// Local file stem (sanitised, no extension).
    pub local_stem: String,
}

/// One thumbnail candidate: primary `/thumbnails/` URL and the `/Thumbnails/` fallback.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThumbTarget {
    pub tier: u32,
    pub ext: &'static str,
    pub url: String,
    pub fallback_url: String,
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

impl CoverTarget {
    fn from_url(entry: usize, raw: &str) -> Result<Self, String> {
        let u = parse_fetch_url(raw).map_err(|e| e.to_string())?;
        let path = u
            .path_and_query
            .split('?')
            .next()
            .unwrap_or("/")
            .to_string();
        let (dir_path, file) = match path.rfind('/') {
            Some(i) => (path[..=i].to_string(), path[i + 1..].to_string()),
            None => ("/".to_string(), path.clone()),
        };
        let stem_enc = match file.rsplit_once('.') {
            Some((s, _)) if !s.is_empty() => s.to_string(),
            _ => file.clone(),
        };
        if stem_enc.is_empty() {
            return Err("the image URL has no file name".into());
        }
        let local_stem = safe_path_segment(&percent_decode(&stem_enc), "cover", 100);
        Ok(Self {
            entry,
            url: u.to_fetch_string(),
            dir_path,
            stem_enc,
            host: u.host,
            local_stem,
        })
    }

    fn sibling(&self, folder: &str, tier: u32, ext: &str) -> String {
        ParsedUrl {
            host: self.host.clone(),
            port: None,
            path_and_query: format!("{}{folder}/{}-{tier}.{ext}", self.dir_path, self.stem_enc),
            fragment: None,
        }
        .to_fetch_string()
    }

    /// All thumbnail candidates for this image (§2: `<dir>/thumbnails/<name>-<tier>.<ext>`,
    /// retry `/Thumbnails/`).
    pub fn thumbnails(&self) -> Vec<ThumbTarget> {
        let mut v = Vec::new();
        for tier in THUMB_TIERS {
            for ext in THUMB_EXTS {
                v.push(ThumbTarget {
                    tier,
                    ext,
                    url: self.sibling("thumbnails", tier, ext),
                    fallback_url: self.sibling("Thumbnails", tier, ext),
                });
            }
        }
        v
    }
}

/// Cover images of `doc` that can be downloaded, plus `(url, reason)` for those refused.
pub fn plan_cover_targets(doc: &NoudataDocument) -> (Vec<CoverTarget>, Vec<(String, String)>) {
    let mut ok = Vec::new();
    let mut refused = Vec::new();
    let Some(cover) = doc
        .release_data
        .as_ref()
        .and_then(|r| r.cover_image.as_ref())
    else {
        return (ok, refused);
    };
    for (i, img) in cover.images.iter().enumerate().take(MAX_COVER_DOWNLOADS) {
        let Some(url) = img.image_web_url.as_deref() else {
            continue;
        };
        match CoverTarget::from_url(i, url) {
            Ok(t) => ok.push(t),
            Err(why) => refused.push((url.to_string(), why)),
        }
    }
    (ok, refused)
}

/// Last path segment of a media URL without query, decoded (`My%20Song.mp3` -> `My Song.mp3`).
pub fn url_file_name(url: &str) -> Option<String> {
    let u = parse_fetch_url(url).ok()?;
    let path = u.path_and_query.split('?').next()?;
    let last = path.rsplit('/').next()?;
    let name = percent_decode(last);
    (!name.is_empty()).then_some(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use plippa_noubin::parse_noudata;

    fn doc(url: &str) -> NoudataDocument {
        parse_noudata(
            format!(
                r#"{{"playableItemTitle":"A","releaseData":{{"coverImage":{{"images":[{{"imageWebURL":"{url}"}}]}}}},"mediaList":[{{}}]}}"#
            )
            .as_bytes(),
        )
        .unwrap()
    }

    #[test]
    fn unite_images_thumbnail_urls_follow_the_linkhost_layout() {
        let (t, refused) = plan_cover_targets(&doc(
            "https://Artist.example/noubin/a/release/cover.png?x=1",
        ));
        assert!(refused.is_empty());
        let t = &t[0];
        assert_eq!(
            t.url,
            "https://artist.example/noubin/a/release/cover.png?x=1"
        );
        assert_eq!(t.local_stem, "cover");
        let th = t.thumbnails();
        assert_eq!(th.len(), 6);
        assert_eq!(
            th[0].url,
            "https://artist.example/noubin/a/release/thumbnails/cover-128.jpg"
        );
        assert_eq!(
            th[0].fallback_url,
            "https://artist.example/noubin/a/release/Thumbnails/cover-128.jpg"
        );
        assert_eq!(
            th[5].url,
            "https://artist.example/noubin/a/release/thumbnails/cover-1024.webp"
        );
    }

    #[test]
    fn unite_images_private_and_odd_urls_are_refused_not_fetched() {
        for url in [
            "https://127.0.0.1/c.png",
            "https://192.168.0.5/c.png",
            "https://nas.local/c.png",
            "https://x.example:8443/c.png",
            "https://x.example/",
        ] {
            let (ok, refused) = plan_cover_targets(&doc(url));
            assert!(ok.is_empty(), "{url}");
            assert_eq!(refused.len(), 1, "{url}");
        }
    }

    #[test]
    fn unite_images_local_names_are_safe() {
        let (t, _) = plan_cover_targets(&doc("https://x.example/a/..%2f..%2fetc%2fpasswd.png"));
        assert!(!t[0].local_stem.contains('/') && !t[0].local_stem.starts_with('.'));
        let (t, _) = plan_cover_targets(&doc("https://x.example/a/CON.png"));
        assert_eq!(t[0].local_stem, "_CON");
        assert_eq!(
            url_file_name("https://x.example/d/My%20Song.mp3?t=1").as_deref(),
            Some("My Song.mp3")
        );
        assert!(url_file_name("https://127.0.0.1/x.mp3").is_none());
    }
}
