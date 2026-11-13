//! NB_04: long-form resume write-through.
//!
//! The engine never writes `.noudata`; it sends a [`ResumeWriteback`] when a `podcast` /
//! `audiobook` / `mix` stops (or finishes). The server applies it here: sets
//! `localPlaybackData.resumeAtItem` (1-indexed) and the matching item's
//! `itemLocalPlaybackData.resumeFromTime`, or clears both. Same write path as playlists:
//! read → modify → atomic temp+rename guarded by the file's mtime+size.

use super::{ServerError, ServerService};
use plippa_ipc_proto::ResumeWriteback;
use plippa_noubin::{
    parse_noudata, write_noudata_atomic, AtomicWriteError, FileRevision, ItemLocalPlaybackData,
    NoubinTimestamp, NoudataDocument, WriteGuard,
};

/// Apply `wb` to `doc`. Returns whether anything changed.
///
/// One resume point per playable: setting it clears `resumeFromTime` on every other item.
/// Empty containers this leaves behind are removed so files stay tidy.
pub(crate) fn apply_resume(doc: &mut NoudataDocument, wb: &ResumeWriteback) -> bool {
    let before = doc.clone();
    match (wb.resume_at_item, wb.resume_from_ms) {
        (Some(item), Some(ms)) if item >= 1 && (item as usize) <= doc.media_list.len() => {
            doc.local_playback_data
                .get_or_insert_with(Default::default)
                .resume_at_item = Some(u64::from(item));
            for (i, m) in doc.media_list.iter_mut().enumerate() {
                if i + 1 == item as usize {
                    m.item_local_playback_data
                        .get_or_insert_with(Default::default)
                        .resume_from_time = Some(NoubinTimestamp::from_millis(ms));
                } else {
                    clear_item_resume(&mut m.item_local_playback_data);
                }
            }
        }
        _ => {
            if let Some(l) = doc.local_playback_data.as_mut() {
                l.resume_at_item = None;
                if *l == Default::default() {
                    doc.local_playback_data = None;
                }
            }
            for m in doc.media_list.iter_mut() {
                clear_item_resume(&mut m.item_local_playback_data);
            }
        }
    }
    *doc != before
}

fn clear_item_resume(data: &mut Option<ItemLocalPlaybackData>) {
    if let Some(d) = data {
        d.resume_from_time = None;
        if *d == ItemLocalPlaybackData::default() {
            *data = None;
        }
    }
}

impl ServerService {
    /// Write one resume request through to the playable's `.noudata`.
    /// `Ok(false)`: nothing to do (orphan without a file, or already up to date).
    pub fn apply_resume_writeback(&mut self, wb: &ResumeWriteback) -> Result<bool, ServerError> {
        let (lib_idx, rec) = self.require_record(wb.playable_id.as_u64())?;
        if rec.is_orphan || rec.noudata_relpath.is_empty() {
            return Ok(false);
        }
        let path = self.libraries[lib_idx].root.join(&rec.noudata_relpath);
        for _ in 0..3 {
            let rev = FileRevision::of(&path)?;
            let mut doc = parse_noudata(&plippa_noubin::read_noudata_capped(&path)?)?;
            if !apply_resume(&mut doc, wb) {
                return Ok(false);
            }
            match write_noudata_atomic(&doc, &path, WriteGuard::Unchanged(rev)) {
                Ok(_) => {
                    let _ = self.lazy_rescan();
                    return Ok(true);
                }
                Err(AtomicWriteError::Conflict) => continue, // edited meanwhile: re-read, retry
                Err(AtomicWriteError::Io(e)) => return Err(ServerError::Io(e)),
                Err(AtomicWriteError::Noudata(e)) => return Err(ServerError::Noudata(e)),
            }
        }
        Err(ServerError::Conflict(
            "the .noudata kept changing; resume point not saved".into(),
        ))
    }

    /// Drain the engine's pending resume requests and write them. Returns how many
    /// files were changed. Failures are logged, never fatal (resume is best effort).
    pub fn apply_resume_writebacks(&mut self) -> usize {
        let pending = self.with_ipc(|ipc| {
            ipc.poll_inbound();
            ipc.take_resume_writebacks()
        });
        let mut changed = 0;
        for wb in pending {
            match self.apply_resume_writeback(&wb) {
                Ok(true) => changed += 1,
                Ok(false) => {}
                Err(e) => eprintln!(
                    "plippa-server: resume write-back for {} failed: {e}",
                    wb.playable_id.as_u64()
                ),
            }
        }
        changed
    }
}
