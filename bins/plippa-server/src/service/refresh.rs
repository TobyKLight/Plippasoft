//! NB_09: service-side helpers for the unite flows (primary library, index update after a
//! write, and the read context for "refresh metadata"). The network never runs under the
//! service lock: callers read a context, unlock, fetch, lock again and re-read.

use super::{
    list_by_rel, path_relative_posix, paths_same, read_noudata, require_noudata, PlayableSummary,
    ServerError, ServerService,
};
use plippa_noubin::{FileRevision, NoudataDocument};
use std::path::{Path, PathBuf};

/// A playable's `.noudata` as read for a refresh.
pub(crate) struct RefreshCtx {
    pub path: PathBuf,
    pub root: PathBuf,
    /// Library-relative folder of the `.noudata`.
    pub rel_dir: PathBuf,
    pub doc: NoudataDocument,
    pub rev: FileRevision,
}

impl ServerService {
    /// Root of the library new releases go to (first open root, like `add_media`).
    pub(crate) fn primary_root(&self) -> Result<PathBuf, ServerError> {
        self.libraries
            .first()
            .map(|l| l.root.clone())
            .ok_or_else(|| {
                ServerError::Unavailable(
                    self.warning
                        .clone()
                        .unwrap_or_else(|| "No media storage available".into()),
                )
            })
    }

    /// Rescan after unite wrote a release and return its summary.
    pub(crate) fn finish_unite_write(
        &mut self,
        root: &Path,
        noudata_abs: &Path,
    ) -> Result<PlayableSummary, ServerError> {
        let _ = self.lazy_rescan();
        let idx = self
            .libraries
            .iter()
            .position(|l| paths_same(&l.root, root))
            .ok_or_else(|| ServerError::Unavailable("the library is no longer mounted".into()))?;
        let (root, label) = (
            self.libraries[idx].root.clone(),
            self.libraries[idx].label.clone(),
        );
        let multi = self.libraries.len() > 1;
        let rel = path_relative_posix(&root, noudata_abs)?;
        list_by_rel(self, &root, &rel, &label, multi)
    }

    /// Read a playable's `.noudata` with its revision (release documents only).
    pub(crate) fn refresh_context(&mut self, id: u64) -> Result<RefreshCtx, ServerError> {
        let (idx, record) = self.require_record(id)?;
        require_noudata(&record)?;
        let root = self.libraries[idx].root.clone();
        let path = root.join(&record.noudata_relpath);
        let rev = FileRevision::of(&path)?;
        let doc = read_noudata(&path)?;
        if doc.release_data.is_none() {
            return Err(ServerError::Invalid(
                "only releases (albums, mixes, audiobooks, podcasts) have online metadata to refresh"
                    .into(),
            ));
        }
        let rel_dir = Path::new(&record.noudata_relpath)
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_default();
        Ok(RefreshCtx {
            path,
            root,
            rel_dir,
            doc,
            rev,
        })
    }
}
