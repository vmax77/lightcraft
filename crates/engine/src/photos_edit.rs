//! Editing an Apple Photos item (issue #1622): its proxy ([`Source::ApplePhotos`]) becomes a
//! library photo of its own. The original is copied out of the `.photoslibrary` package into the
//! library's `Originals/From Apple Photos/`, the copy is read back and compared byte for byte, and
//! only then does the record point at it ([`Op::Relink`]), join the library ([`Op::SetLocal`]) and
//! keep the asset it came from ([`Op::SetFromPhotos`]). The record keeps its id, so whatever was
//! done to it while the copy was made (ratings, develop settings, history) stays with it.
//! Nothing is ever written inside the package.
//!
//! The copy is two halves so the app can make it off the UI thread: [`Session::photos_copies`]
//! says what to copy (no file is touched), [`PhotosCopy::make`] copies (no session needed), and
//! [`Session::adopt_photos_copy`] links the record to the copy.

use std::path::{Path, PathBuf};

use lightcraft_catalog::{Op, PhotoId, PhotosLink, Source};

use crate::{EngineError, Result, Session};

/// The folder in the library's `Originals/` that copies of Photos originals go to.
pub const COPY_FOLDER: &str = "From Apple Photos";

/// A proxy's original to copy before the photo can be edited.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PhotosCopy {
    pub id: PhotoId,
    pub link: PhotosLink,
    /// The original inside the package (read only).
    pub original: PathBuf,
    /// The name the copy is given (made unique in `dir`).
    pub file_name: String,
    /// Where the copy goes.
    pub dir: PathBuf,
}

impl PhotosCopy {
    /// Copy the original into [`Self::dir`] under a free name, sync it and compare it byte for
    /// byte with the original; a bad copy is removed. Returns the copy's path.
    pub fn make(&self) -> std::result::Result<PathBuf, String> {
        crate::import_move::copy_new(&self.original, &self.dir, &self.file_name, None)
    }
}

/// A file name that is safe to make in a folder: the last part of `name`, without characters
/// that name folders or are invalid on common file systems; `fallback` when nothing is left.
fn safe_name(name: &str, fallback: &str) -> String {
    let last = name.rsplit(['/', '\\']).next().unwrap_or("");
    let clean: String = last.chars().filter(|c| !c.is_control() && !matches!(c, ':' | '*' | '?' | '"' | '<' | '>' | '|')).collect();
    let clean = clean.trim().trim_start_matches('.').to_string();
    if clean.is_empty() { fallback.to_string() } else { clean }
}

impl Session {
    /// The copies `ids` need before they can be edited: one per proxy. Photos that are not
    /// proxies need none and are left out. A proxy whose original is only in iCloud is an error
    /// naming it, as is a session without a library folder for the copies.
    pub fn photos_copies(&self, ids: &[PhotoId]) -> Result<Vec<PhotosCopy>> {
        let mut out = Vec::new();
        for &id in ids {
            let Some(p) = self.catalog.photo(id) else { return Err(EngineError::Other(format!("no photo {}", id.0))) };
            let Source::ApplePhotos { library, uuid, original } = &p.source else { continue };
            let Some(original) = original else {
                return Err(EngineError::Other(format!("{}: the original is in iCloud, not on this Mac; download it in Photos first", p.file_name)));
            };
            let Some(lib) = self.library.as_ref().filter(|l| l.on_disk) else {
                return Err(EngineError::Other("editing an Apple Photos item copies it into the library: open a library first".into()));
            };
            // the original's own name (what Photos shows can differ, e.g. after an edit there)
            let fallback = Path::new(original).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| format!("{uuid}.jpg"));
            out.push(PhotosCopy {
                id,
                link: PhotosLink { library: library.clone(), uuid: uuid.clone() },
                original: PathBuf::from(original),
                file_name: safe_name(&p.file_name, &fallback),
                dir: lib.originals_dir().join(COPY_FOLDER),
            });
        }
        Ok(out)
    }

    /// Make the proxy `copy` was planned for a library photo of the file at `path` (made by
    /// [`PhotosCopy::make`]). Journaled, not an undo step: undo can't take a copy back into
    /// Photos. An error when the record is no longer that proxy (removed, or made a copy of
    /// meanwhile); the caller then removes the file it made.
    pub fn adopt_photos_copy(&mut self, copy: &PhotosCopy, path: &Path) -> Result<()> {
        let still = self.catalog.photo(copy.id).is_some_and(
            |p| matches!(&p.source, Source::ApplePhotos { library, uuid, .. } if *library == copy.link.library && *uuid == copy.link.uuid),
        );
        if !still {
            return Err(EngineError::Other(format!("photo {} is no longer the Apple Photos item it was", copy.id.0)));
        }
        let mut ops = vec![crate::cmd::missing::relink_op(copy.id, &path.to_string_lossy())];
        ops.push(Op::SetLocal { id: copy.id, local: false });
        ops.push(Op::SetFromPhotos { id: copy.id, link: Some(copy.link.clone()) });
        self.apply_system(Op::Batch { ops })?;
        self.media.forget(copy.id);
        Ok(())
    }

    /// Copy every proxy among `ids` and adopt the copies, one at a time (the command's way; the
    /// app does the copying on a worker). Returns the copies' paths by photo id.
    pub fn edit_photos_items(&mut self, ids: &[PhotoId]) -> Result<Vec<(PhotoId, PathBuf)>> {
        let copies = self.photos_copies(ids)?;
        let mut done = Vec::with_capacity(copies.len());
        for copy in copies {
            let path = copy.make().map_err(EngineError::Other)?;
            if let Err(e) = self.adopt_photos_copy(&copy, &path) {
                let _ = std::fs::remove_file(&path);
                return Err(e);
            }
            done.push((copy.id, path));
        }
        Ok(done)
    }
}

#[cfg(test)]
mod tests;
