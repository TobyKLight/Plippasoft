//! ART_04: staged cover art of one import session — embedded pictures and dropped image files,
//! deduplicated as they arrive.
//!
//! Every image is written once into `<session>/art/` (inside the session's staging folder, so
//! discarding, expiring or sweeping a session removes it with no extra cleanup code). Candidates
//! are kept for the whole session and [`dedupe_covers`] groups them again after each arrival:
//!
//! 1. **Exact** — equal length and 64-bit digest bucket the files, then a full byte comparison
//!    decides (a digest collision can never merge two different images).
//! 2. **Visual** — 64-bit difference hash of the decoded, square-cropped image within a Hamming
//!    distance of 4 *and* aspect ratios within 2 % (D6). A hash with fewer than
//!    [`MIN_HASH_BITS`] set bits carries no picture information (flat colours), so such an image
//!    only ever merges by exact bytes.
//!
//! The survivor of a group has the most pixels (ties: the larger file; then the earliest); the
//! group sits where its earliest member appeared. A group the user removed stays removed when
//! another copy arrives later (embedded in a track uploaded afterwards); dropping the file again
//! on purpose brings it back.

use super::covers::{cover_limits, image_error_text, kind_ext, kind_name, MAX_COVERS};
use super::imports::ImportError;
use crate::unite::sniff::sniff_image_kind;
use crate::web::download::DownloadLimits;
use plippa_image::{decode, dhash64, hamming, probe, ImageError, ImageKind};
use serde_json::{json, Value};
use std::collections::hash_map::DefaultHasher;
use std::fs;
use std::hash::Hasher;
use std::path::{Path, PathBuf};

/// Distinct image files one session may stage (hostile uploads cannot fill the stick with art).
pub const MAX_SESSION_ART_FILES: usize = 48;
/// Bytes of image files one session may stage.
pub const MAX_SESSION_ART_BYTES: u64 = 128 * 1024 * 1024;
/// Candidates (including exact copies of staged files) one session may hold. Copies cost no disk,
/// but grouping is quadratic in this number.
pub const MAX_SESSION_ART_CANDIDATES: usize = 512;
/// Largest Hamming distance between two difference hashes that still counts as the same picture.
const MAX_HAMMING: u32 = 4;
/// Fewest set bits a difference hash needs to be trusted for visual matching.
const MIN_HASH_BITS: u32 = 8;

/// Where a staged image came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArtOrigin {
    /// Embedded in the audio file with this `file_id`.
    Embedded { file_id: u32 },
    /// An image file the user dropped (its sanitised name).
    Dropped { name: String },
}

impl ArtOrigin {
    /// Embedded art is listed before dropped files.
    fn rank(&self) -> u8 {
        match self {
            ArtOrigin::Embedded { .. } => 0,
            ArtOrigin::Dropped { .. } => 1,
        }
    }
}

/// One staged image.
#[derive(Debug, Clone)]
pub struct StagedArt {
    pub art_id: String,
    pub origin: ArtOrigin,
    pub path: PathBuf,
    pub w: u32,
    pub h: u32,
    pub bytes: u64,
    pub kind: ImageKind,
    pub dhash: u64,
    pub excluded: bool,
    /// 64-bit digest of the file bytes (bucket key only: equality is always confirmed).
    pub digest: u64,
    /// Arrival counter: the order the session saw the images in.
    pub seq: u64,
}

impl StagedArt {
    /// Position key: embedded before dropped, then by arrival.
    fn key(&self) -> (u8, u64) {
        (self.origin.rank(), self.seq)
    }
}

/// An image merged into another.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DupLink {
    pub dropped_art_id: String,
    pub kept_art_id: String,
    /// Byte-for-byte the same file (otherwise: the same picture at another size / encoding).
    pub exact: bool,
}

fn same_bytes(a: &StagedArt, b: &StagedArt) -> bool {
    if a.path == b.path {
        return true;
    }
    if a.digest != b.digest || a.bytes != b.bytes {
        return false;
    }
    match (fs::read(&a.path), fs::read(&b.path)) {
        (Ok(x), Ok(y)) => x == y,
        _ => false,
    }
}

fn same_picture(a: &StagedArt, b: &StagedArt) -> bool {
    if a.dhash.count_ones() < MIN_HASH_BITS || b.dhash.count_ones() < MIN_HASH_BITS {
        return false;
    }
    if hamming(a.dhash, b.dhash) > MAX_HAMMING {
        return false;
    }
    // aspect ratios w/h within 2 %, cross-multiplied
    let (x, y) = (u128::from(a.w) * u128::from(b.h), u128::from(b.w) * u128::from(a.h));
    x.abs_diff(y) * 50 <= x.max(y)
}

struct Group {
    /// Indices into the candidate list, earliest first.
    members: Vec<usize>,
    keeper: usize,
}

fn find(parent: &mut [usize], mut i: usize) -> usize {
    while parent[i] != i {
        parent[i] = parent[parent[i]];
        i = parent[i];
    }
    i
}

fn group(cands: &[StagedArt]) -> Vec<Group> {
    let mut parent: Vec<usize> = (0..cands.len()).collect();
    for i in 0..cands.len() {
        for j in 0..i {
            let (ri, rj) = (find(&mut parent, i), find(&mut parent, j));
            if ri != rj && (same_bytes(&cands[i], &cands[j]) || same_picture(&cands[i], &cands[j])) {
                parent[ri] = rj;
            }
        }
    }
    let mut by_root: Vec<(usize, Vec<usize>)> = Vec::new();
    for i in 0..cands.len() {
        let r = find(&mut parent, i);
        match by_root.iter_mut().find(|(root, _)| *root == r) {
            Some((_, m)) => m.push(i),
            None => by_root.push((r, vec![i])),
        }
    }
    let mut groups: Vec<Group> = by_root
        .into_iter()
        .map(|(_, mut members)| {
            members.sort_by_key(|&i| cands[i].key());
            let mut keeper = members[0];
            for &m in &members[1..] {
                let (best, c) = (&cands[keeper], &cands[m]);
                let pixels = |a: &StagedArt| u64::from(a.w) * u64::from(a.h);
                if (pixels(c), c.bytes) > (pixels(best), best.bytes) {
                    keeper = m;
                }
            }
            Group { members, keeper }
        })
        .collect();
    groups.sort_by_key(|g| cands[g.members[0]].key());
    groups
}

/// Merge duplicates (see the module docs). Returns the survivors in display order, each flagged
/// `excluded` when any member of its group was removed, and one [`DupLink`] per merged image.
pub fn dedupe_covers(candidates: Vec<StagedArt>) -> (Vec<StagedArt>, Vec<DupLink>) {
    let groups = group(&candidates);
    let mut kept = Vec::with_capacity(groups.len());
    let mut links = Vec::new();
    for g in &groups {
        let mut k = candidates[g.keeper].clone();
        k.excluded = g.members.iter().any(|&m| candidates[m].excluded);
        for &m in &g.members {
            if m != g.keeper {
                links.push(DupLink {
                    dropped_art_id: candidates[m].art_id.clone(),
                    kept_art_id: k.art_id.clone(),
                    exact: same_bytes(&candidates[m], &candidates[g.keeper]),
                });
            }
        }
        kept.push(k);
    }
    (kept, links)
}

fn default_digest(bytes: &[u8]) -> u64 {
    let mut h = DefaultHasher::new();
    h.write(bytes);
    h.finish()
}

/// What the session shows the user.
pub struct ArtView {
    /// Visible (not removed) images, in the order they will be saved.
    pub kept: Vec<StagedArt>,
    /// Every merge, removed groups included.
    pub links: Vec<DupLink>,
}

/// The staged art of one import session.
pub struct ArtStaging {
    dir: PathBuf,
    cands: Vec<StagedArt>,
    next: u64,
    files: usize,
    bytes: u64,
    /// The order the user chose (ids of any member of a group).
    order: Option<Vec<String>>,
    digest: fn(&[u8]) -> u64,
    /// The session was discarded: nothing may be written into its (deleted) staging folder.
    closed: bool,
}

fn refused(status: u16, code: &'static str, msg: impl Into<String>) -> ImportError {
    ImportError::new(status, code, msg)
}

fn image_refusal(e: &ImageError) -> ImportError {
    let status = match e {
        ImageError::TooLarge { .. } => 413,
        _ => 415,
    };
    refused(status, "bad_image", image_error_text(e))
}

impl ArtStaging {
    /// Images are written into `dir` (created on the first image).
    pub fn new(dir: PathBuf) -> Self {
        Self::with_digest(dir, default_digest)
    }

    /// Same with another digest function (tests force collisions).
    pub fn with_digest(dir: PathBuf, digest: fn(&[u8]) -> u64) -> Self {
        Self {
            dir,
            cands: Vec::new(),
            next: 1,
            files: 0,
            bytes: 0,
            order: None,
            digest,
            closed: false,
        }
    }

    /// The session is gone. Later [`add`](Self::add) calls are refused and never touch the disk.
    pub fn close(&mut self) {
        self.closed = true;
    }

    /// Stage one image. Refused (nothing kept) when it is not a JPEG / PNG / WebP within the
    /// cover limits, cannot be decoded, or the session's art budget / the disk is full. A byte
    /// copy of an image already staged costs no disk and no decode. Returns the new `art_id`.
    pub fn add(
        &mut self,
        origin: ArtOrigin,
        bytes: Vec<u8>,
        limits: &DownloadLimits,
    ) -> Result<String, ImportError> {
        if self.closed {
            return Err(refused(404, "unknown_import", "unknown or expired import"));
        }
        // The content decides (never a name or a claimed mime type).
        sniff_image_kind(&bytes)
            .ok_or_else(|| refused(415, "bad_image", "not a JPEG, PNG or WebP image"))?;
        let cover = cover_limits();
        let (kind, w, h) = probe(&bytes, &cover).map_err(|e| image_refusal(&e))?;
        let digest = (self.digest)(&bytes);
        let len = bytes.len() as u64;
        let art_id = format!("{:016x}", rand::random::<u64>());
        let seq = self.next;

        let twin = self
            .cands
            .iter()
            .position(|c| c.digest == digest && c.bytes == len && fs::read(&c.path).is_ok_and(|b| b == bytes));
        if let Some(t) = twin {
            // The same bytes from the same place again (the same file dropped twice, the same
            // picture twice in one tag) add nothing: answer with the candidate already held.
            if self.cands[t].origin == origin {
                let id = self.cands[t].art_id.clone();
                if matches!(origin, ArtOrigin::Dropped { .. }) {
                    self.restore_group_of(&id);
                }
                return Ok(id);
            }
            if self.cands.len() >= MAX_SESSION_ART_CANDIDATES {
                return Err(refused(413, "art_full", "this import holds as many images as it can"));
            }
            let t = self.cands[t].clone();
            self.next += 1;
            let dropped = matches!(origin, ArtOrigin::Dropped { .. });
            self.cands.push(StagedArt {
                art_id: art_id.clone(),
                origin,
                excluded: false,
                seq,
                ..t
            });
            if dropped {
                self.restore_group_of(&art_id);
            }
            return Ok(art_id);
        }

        if self.files >= MAX_SESSION_ART_FILES || self.bytes + len > MAX_SESSION_ART_BYTES {
            return Err(refused(413, "art_full", "this import holds as many images as it can"));
        }
        if let Some(free) = (limits.free_space)(&self.dir_parent()) {
            if len.saturating_add(limits.reserve_for(&self.dir_parent())) > free {
                return Err(refused(507, "no_space", "not enough free space for this image"));
            }
        }
        // The device has to be able to draw it, and the hash needs the pixels.
        let rgb = decode(&bytes, &cover).map_err(|e| image_refusal(&e))?;
        let dhash = dhash64(&rgb);
        drop(rgb);

        fs::create_dir_all(&self.dir).map_err(|e| refused(500, "io", e.to_string()))?;
        let path = self.dir.join(format!("a{seq}.{}", kind_ext(kind)));
        if let Err(e) = fs::write(&path, &bytes) {
            let _ = fs::remove_file(&path);
            return Err(refused(500, "io", e.to_string()));
        }
        self.next += 1;
        self.files += 1;
        self.bytes += len;
        self.cands.push(StagedArt {
            art_id: art_id.clone(),
            origin: origin.clone(),
            path,
            w,
            h,
            bytes: len,
            kind,
            dhash,
            excluded: false,
            digest,
            seq,
        });
        // Dropping a file again on purpose brings a removed picture back.
        if matches!(origin, ArtOrigin::Dropped { .. }) {
            self.restore_group_of(&art_id);
        }
        Ok(art_id)
    }

    fn dir_parent(&self) -> PathBuf {
        self.dir
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| self.dir.clone())
    }

    fn restore_group_of(&mut self, art_id: &str) {
        let groups = group(&self.cands);
        if let Some(g) = groups
            .iter()
            .find(|g| g.members.iter().any(|&m| self.cands[m].art_id == art_id))
        {
            for &m in &g.members.clone() {
                self.cands[m].excluded = false;
            }
        }
    }

    /// The current state: visible images in save order and the merges.
    pub fn view(&self) -> ArtView {
        let groups = group(&self.cands);
        let (kept, links) = dedupe_covers(self.cands.clone());
        // `kept` is in group order; pair each with the ids of its group for the user's order.
        let mut entries: Vec<(StagedArt, Vec<&str>)> = kept
            .into_iter()
            .zip(&groups)
            .map(|(k, g)| {
                let ids = g.members.iter().map(|&m| self.cands[m].art_id.as_str()).collect();
                (k, ids)
            })
            .collect();
        let mut ordered: Vec<(StagedArt, Vec<&str>)> = Vec::with_capacity(entries.len());
        if let Some(want) = &self.order {
            for id in want {
                if let Some(at) = entries.iter().position(|(_, ids)| ids.contains(&id.as_str())) {
                    ordered.push(entries.remove(at));
                }
            }
        }
        ordered.extend(entries);
        ArtView {
            kept: ordered
                .into_iter()
                .map(|(k, _)| k)
                .filter(|k| !k.excluded)
                .collect(),
            links,
        }
    }

    /// The surviving image of the group `art_id` belongs to (a removed one included: its bytes
    /// are still there).
    pub fn resolve(&self, art_id: &str) -> Option<StagedArt> {
        let groups = group(&self.cands);
        let g = groups
            .iter()
            .find(|g| g.members.iter().any(|&m| self.cands[m].art_id == art_id))?;
        Some(self.cands[g.keeper].clone())
    }

    /// Remove the picture `art_id` belongs to, and every later copy of it.
    pub fn exclude(&mut self, art_id: &str) -> Result<(), ImportError> {
        let groups = group(&self.cands);
        let g = groups
            .iter()
            .find(|g| g.members.iter().any(|&m| self.cands[m].art_id == art_id))
            .ok_or_else(|| refused(404, "unknown_art", "unknown image"))?;
        for &m in &g.members.clone() {
            self.cands[m].excluded = true;
        }
        Ok(())
    }

    /// Forget the pictures that were embedded in audio file `file_id` (the file left the
    /// session). Image files nobody else needs any more are deleted from the staging folder.
    pub fn forget_file(&mut self, file_id: u32) {
        let (gone, kept): (Vec<StagedArt>, Vec<StagedArt>) = std::mem::take(&mut self.cands)
            .into_iter()
            .partition(|c| matches!(c.origin, ArtOrigin::Embedded { file_id: f } if f == file_id));
        self.cands = kept;
        let mut freed: Vec<&StagedArt> = Vec::new();
        for c in &gone {
            let shared = self.cands.iter().any(|k| k.path == c.path);
            if !shared && !freed.iter().any(|f| f.path == c.path) {
                freed.push(c);
            }
        }
        for c in freed {
            let _ = fs::remove_file(&c.path);
            self.files = self.files.saturating_sub(1);
            self.bytes = self.bytes.saturating_sub(c.bytes);
        }
    }

    /// Remember the order the user chose; ids merged into another image count as that image.
    pub fn set_order(&mut self, ids: Vec<String>) -> Result<(), ImportError> {
        for id in &ids {
            if self.resolve(id).is_none() {
                return Err(refused(400, "invalid", format!("unknown image {id}")));
            }
        }
        self.order = Some(ids);
        Ok(())
    }

    /// The images `finish` will save: `requested` (ids, in order) or, when absent, everything
    /// visible except pictures that only audio files outside `files` (the ids being imported)
    /// carry. A removed or unknown image, one listed twice, or more than a collection may
    /// carry is refused.
    pub fn select_for_finish(
        &self,
        requested: Option<&[String]>,
        files: &[u32],
    ) -> Result<Vec<StagedArt>, ImportError> {
        let chosen = match requested {
            None => {
                let groups = group(&self.cands);
                let carried = |k: &StagedArt| {
                    groups
                        .iter()
                        .find(|g| g.members.iter().any(|&m| self.cands[m].art_id == k.art_id))
                        .is_none_or(|g| {
                            g.members.iter().any(|&m| match &self.cands[m].origin {
                                ArtOrigin::Dropped { .. } => true,
                                ArtOrigin::Embedded { file_id } => files.contains(file_id),
                            })
                        })
                };
                self.view().kept.into_iter().filter(|k| carried(k)).collect()
            }
            Some(ids) => {
                let mut out: Vec<StagedArt> = Vec::with_capacity(ids.len());
                for id in ids {
                    let k = self
                        .resolve(id)
                        .ok_or_else(|| refused(400, "invalid", format!("unknown image {id}")))?;
                    if self.view().kept.iter().all(|v| v.art_id != k.art_id) {
                        return Err(refused(400, "invalid", format!("image {id} was removed")));
                    }
                    if out.iter().any(|o| o.art_id == k.art_id) {
                        return Err(refused(400, "invalid", format!("image {id} is listed twice")));
                    }
                    out.push(k);
                }
                out
            }
        };
        if chosen.len() > MAX_COVERS {
            return Err(refused(
                413,
                "too_many_images",
                format!("at most {MAX_COVERS} images per collection; remove some"),
            ));
        }
        Ok(chosen)
    }

    /// `art` / `merged` of the upload answer: the session-wide state after this file.
    pub fn to_json(&self, sid: &str) -> (Value, Value) {
        let v = self.view();
        let art = v.kept.iter().map(|a| art_json(sid, a)).collect::<Vec<_>>();
        let merged = v
            .links
            .iter()
            .map(|l| json!({"dropped": l.dropped_art_id, "kept": l.kept_art_id, "exact": l.exact}))
            .collect::<Vec<_>>();
        (Value::Array(art), Value::Array(merged))
    }
}

/// One image of the `art` list.
pub fn art_json(sid: &str, a: &StagedArt) -> Value {
    let mut v = json!({
        "art_id": a.art_id,
        "url": format!("/api/imports/{sid}/art/{}", a.art_id),
        "w": a.w,
        "h": a.h,
        "bytes": a.bytes,
        "kind": kind_name(a.kind),
    });
    match &a.origin {
        ArtOrigin::Embedded { file_id } => {
            v["origin"] = json!("embedded");
            v["file_id"] = json!(file_id);
        }
        ArtOrigin::Dropped { name } => {
            v["origin"] = json!("dropped");
            v["name"] = json!(name);
        }
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use plippa_image::{encode_jpeg_fit, Rgb8};

    /// A 9 x 8 grid of pseudo-random greys scaled to `w` x `h`: every size of one `seed` hashes
    /// alike, different seeds differ.
    fn blocks(seed: u32, w: u32, h: u32) -> Vec<u8> {
        let mut cell = [0u8; 72];
        let mut s = seed.wrapping_mul(2654435761).wrapping_add(12345);
        for c in cell.iter_mut() {
            s = s.wrapping_mul(1664525).wrapping_add(1013904223);
            *c = (s >> 24) as u8;
        }
        let mut px = Vec::with_capacity((w * h * 3) as usize);
        for y in 0..h {
            for x in 0..w {
                let v = cell[((y * 8 / h) * 9 + x * 9 / w) as usize];
                px.extend_from_slice(&[v, v, v]);
            }
        }
        encode_jpeg_fit(&Rgb8 { w, h, px }, w.max(h)).unwrap()
    }

    fn limits() -> DownloadLimits {
        DownloadLimits::default()
    }

    fn staging() -> (tempfile::TempDir, ArtStaging) {
        let t = tempfile::tempdir().unwrap();
        let s = ArtStaging::new(t.path().join("art"));
        (t, s)
    }

    fn emb(n: u32) -> ArtOrigin {
        ArtOrigin::Embedded { file_id: n }
    }

    #[test]
    fn identical_bytes_are_one_image_stored_once() {
        let (t, mut s) = staging();
        let img = blocks(1, 300, 300);
        for n in 1..=10 {
            s.add(emb(n), img.clone(), &limits()).unwrap();
        }
        let v = s.view();
        assert_eq!(v.kept.len(), 1);
        assert_eq!(v.links.len(), 9);
        assert!(v.links.iter().all(|l| l.exact));
        assert_eq!(fs::read_dir(t.path().join("art")).unwrap().count(), 1);
    }

    #[test]
    fn the_same_picture_at_two_sizes_keeps_the_larger_in_the_earliest_place() {
        let (_t, mut s) = staging();
        s.add(emb(1), blocks(1, 600, 600), &limits()).unwrap();
        s.add(emb(1), blocks(2, 400, 400), &limits()).unwrap();
        s.add(emb(2), blocks(1, 1000, 1000), &limits()).unwrap();
        let v = s.view();
        assert_eq!(v.kept.len(), 2);
        assert_eq!((v.kept[0].w, v.kept[1].w), (1000, 400), "seed 1 first, at 1000 px");
        assert_eq!(v.links.len(), 1);
        assert!(!v.links[0].exact);
    }

    #[test]
    fn different_aspect_ratios_do_not_merge() {
        let (_t, mut s) = staging();
        s.add(emb(1), blocks(3, 600, 600), &limits()).unwrap();
        s.add(emb(2), blocks(3, 600, 400), &limits()).unwrap();
        assert_eq!(s.view().kept.len(), 2);
    }

    #[test]
    fn a_digest_collision_never_merges_different_images() {
        let t = tempfile::tempdir().unwrap();
        let mut s = ArtStaging::with_digest(t.path().join("art"), |_| 7);
        s.add(emb(1), blocks(1, 300, 300), &limits()).unwrap();
        s.add(emb(2), blocks(2, 300, 300), &limits()).unwrap();
        assert_eq!(s.view().kept.len(), 2);
        // and the pure function agrees when handed colliding candidates directly
        let (kept, links) = dedupe_covers(s.cands.clone());
        assert_eq!((kept.len(), links.len()), (2, 0));
        // equal bytes under the same constant digest still merge
        s.add(emb(3), blocks(1, 300, 300), &limits()).unwrap();
        assert_eq!(s.view().kept.len(), 2);
    }

    #[test]
    fn flat_images_only_merge_by_exact_bytes() {
        let (_t, mut s) = staging();
        let flat = |v: u8| {
            encode_jpeg_fit(&Rgb8 { w: 64, h: 64, px: vec![v; 64 * 64 * 3] }, 64).unwrap()
        };
        s.add(emb(1), flat(10), &limits()).unwrap();
        s.add(emb(2), flat(240), &limits()).unwrap();
        assert_eq!(s.view().kept.len(), 2);
    }

    #[test]
    fn dropped_files_follow_embedded_art_and_join_the_dedupe() {
        let (_t, mut s) = staging();
        s.add(ArtOrigin::Dropped { name: "x.jpg".into() }, blocks(5, 300, 300), &limits()).unwrap();
        s.add(emb(1), blocks(6, 300, 300), &limits()).unwrap();
        s.add(ArtOrigin::Dropped { name: "y.jpg".into() }, blocks(5, 300, 300), &limits()).unwrap();
        let v = s.view();
        assert_eq!(v.kept.len(), 2);
        assert!(matches!(v.kept[0].origin, ArtOrigin::Embedded { .. }));
        assert!(matches!(v.kept[1].origin, ArtOrigin::Dropped { .. }));
    }

    #[test]
    fn a_removed_picture_stays_removed_for_later_copies_but_a_drop_restores_it() {
        let (_t, mut s) = staging();
        let first = s.add(emb(1), blocks(1, 600, 600), &limits()).unwrap();
        s.add(emb(1), blocks(2, 600, 600), &limits()).unwrap();
        s.exclude(&first).unwrap();
        assert_eq!(s.view().kept.len(), 1);
        s.add(emb(2), blocks(1, 600, 600), &limits()).unwrap(); // same bytes, later track
        s.add(emb(3), blocks(1, 900, 900), &limits()).unwrap(); // bigger copy, later track
        assert_eq!(s.view().kept.len(), 1);
        s.add(ArtOrigin::Dropped { name: "a.jpg".into() }, blocks(1, 600, 600), &limits()).unwrap();
        assert_eq!(s.view().kept.len(), 2);
    }

    #[test]
    fn the_users_order_survives_a_merge_that_changes_the_survivor() {
        let (_t, mut s) = staging();
        let a = s.add(emb(1), blocks(1, 300, 300), &limits()).unwrap();
        let b = s.add(emb(1), blocks(2, 300, 300), &limits()).unwrap();
        s.set_order(vec![b.clone(), a.clone()]).unwrap();
        s.add(emb(2), blocks(2, 800, 800), &limits()).unwrap(); // b's picture, larger: new survivor
        let v = s.view();
        assert_eq!((v.kept[0].w, v.kept[1].w), (800, 300));
        assert!(s.set_order(vec!["nope".into()]).is_err());
    }

    #[test]
    fn art_carried_only_by_files_left_out_of_the_import_is_not_selected() {
        let (_t, mut s) = staging();
        s.add(emb(1), blocks(1, 300, 300), &limits()).unwrap();
        s.add(emb(2), blocks(2, 300, 300), &limits()).unwrap();
        s.add(emb(2), blocks(1, 300, 300), &limits()).unwrap(); // file 2 also carries picture 1
        s.add(ArtOrigin::Dropped { name: "d.jpg".into() }, blocks(3, 300, 300), &limits()).unwrap();
        assert_eq!(s.select_for_finish(None, &[1, 2]).unwrap().len(), 3);
        assert_eq!(s.select_for_finish(None, &[2]).unwrap().len(), 3, "1 is also in file 2");
        let only_1 = s.select_for_finish(None, &[1]).unwrap();
        assert_eq!(only_1.len(), 2, "picture 2 only came with the omitted file 2");
    }

    #[test]
    fn dropping_one_image_again_and_again_keeps_the_candidates_constant() {
        let (_t, mut s) = staging();
        let img = blocks(1, 400, 400);
        let first = s.add(ArtOrigin::Dropped { name: "a.jpg".into() }, img.clone(), &limits()).unwrap();
        let t = std::time::Instant::now();
        for _ in 0..200 {
            let id = s.add(ArtOrigin::Dropped { name: "a.jpg".into() }, img.clone(), &limits()).unwrap();
            assert_eq!(id, first);
        }
        assert_eq!(s.cands.len(), 1);
        assert_eq!((s.files, s.bytes), (1, img.len() as u64));
        assert_eq!(s.view().kept.len(), 1);
        assert!(t.elapsed() < std::time::Duration::from_secs(5), "{:?}", t.elapsed());
        // The same picture under 200 different names is bounded by the candidate cap.
        let mut refused = 0;
        for i in 0..(MAX_SESSION_ART_CANDIDATES + 20) {
            if s.add(ArtOrigin::Dropped { name: format!("n{i}.jpg") }, img.clone(), &limits()).is_err() {
                refused += 1;
            }
        }
        assert_eq!(s.cands.len(), MAX_SESSION_ART_CANDIDATES);
        assert_eq!(refused, 21, "all beyond the cap refused");
    }

    #[test]
    fn a_closed_staging_refuses_images_and_never_recreates_its_folder() {
        let t = tempfile::tempdir().unwrap();
        let dir = t.path().join("gone").join("art");
        let mut s = ArtStaging::new(dir.clone());
        s.close();
        let e = s.add(emb(1), blocks(1, 300, 300), &limits()).unwrap_err();
        assert_eq!(e.status, 404);
        assert!(!t.path().join("gone").exists());
    }

    #[test]
    fn select_for_finish_refuses_removed_unknown_and_repeated_images() {
        let (_t, mut s) = staging();
        let a = s.add(emb(1), blocks(1, 300, 300), &limits()).unwrap();
        let b = s.add(emb(1), blocks(2, 300, 300), &limits()).unwrap();
        s.exclude(&b).unwrap();
        assert_eq!(s.select_for_finish(None, &[1]).unwrap().len(), 1);
        assert!(s.select_for_finish(Some(&[b]), &[1]).is_err());
        assert!(s.select_for_finish(Some(&["zz".into()]), &[1]).is_err());
        assert!(s.select_for_finish(Some(&[a.clone(), a]), &[1]).is_err());
    }

    #[test]
    fn junk_oversized_and_undecodable_images_are_refused_and_leave_nothing() {
        let (t, mut s) = staging();
        assert_eq!(s.add(emb(1), b"GIF89a....".to_vec(), &limits()).unwrap_err().status, 415);
        let mut cut = blocks(1, 300, 300);
        cut.truncate(cut.len() / 2);
        assert!(s.add(emb(1), cut, &limits()).is_err());
        assert!(s.view().kept.is_empty());
        let dir = t.path().join("art");
        assert!(!dir.exists() || fs::read_dir(dir).unwrap().count() == 0);
    }

    #[test]
    fn the_session_art_budget_is_enforced() {
        let (_t, mut s) = staging();
        for i in 0..MAX_SESSION_ART_FILES as u32 {
            s.add(emb(1), blocks(100 + i, 64, 64), &limits()).unwrap();
        }
        assert_eq!(s.add(emb(1), blocks(999, 64, 64), &limits()).unwrap_err().status, 413);
        // a copy of what is there costs nothing
        s.add(emb(2), blocks(100, 64, 64), &limits()).unwrap();
    }
}
