//! Editing an Apple Photos item (issue #1622): its proxy ([`Source::ApplePhotos`]) becomes a
//! library photo of its own. The original is copied out of the `.photoslibrary` package into the
//! library's `Originals/From Apple Photos/`, the copy is read back and compared byte for byte, and
//! only then does the record point at it ([`Op::Relink`]), join the library ([`Op::SetLocal`]) and
//! keep the asset it came from ([`Op::SetFromPhotos`]). The record keeps its id, so whatever was
//! done to it while the copy was made (ratings, develop settings, history) stays with it.
//! Nothing is ever written inside the package.
//!
//! An original Photos keeps only in iCloud is fetched instead (on macOS through PhotoKit,
//! [`photokit_fetch`], for the System Photo Library): its bytes are written straight into the new
//! file in `From Apple Photos/`, synced, and checked against the size Photos records for it.
//!
//! The copy is two halves so the app can make it off the UI thread: [`Session::photos_copies`]
//! says what to copy (no file is touched), [`PhotosCopy::make`] copies or fetches (no session
//! needed), and [`Session::adopt_photos_copy`] links the record to the copy.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use lightcraft_catalog::{Op, PhotoId, PhotosLink, Source};

use crate::{EngineError, Result, Session};

/// The folder in the library's `Originals/` that copies of Photos originals go to.
pub const COPY_FOLDER: &str = "From Apple Photos";

/// A proxy's original to copy before the photo can be edited.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PhotosCopy {
    pub id: PhotoId,
    pub link: PhotosLink,
    /// The original inside the package (read only); `None` when it is only in iCloud.
    pub original: Option<PathBuf>,
    /// The original's size as Photos records it (0: unknown), which a fetched copy must have.
    pub size: u64,
    /// The name the copy is given (made unique in `dir`).
    pub file_name: String,
    /// Where the copy goes.
    pub dir: PathBuf,
}

/// Fetches an original that is only in iCloud: writes the bytes of the asset with this UUID to
/// the writer and returns how many it wrote.
pub type Fetch<'a> = dyn FnMut(&str, &mut dyn Write) -> std::result::Result<u64, String> + 'a;

impl PhotosCopy {
    /// Whether the original has to be fetched from iCloud.
    pub fn in_icloud(&self) -> bool {
        self.original.is_none()
    }

    /// Copy the original into [`Self::dir`] under a free name, sync it and compare it byte for
    /// byte with the original; or, when it is only in iCloud, fetch it with `fetch` into that new
    /// file. A bad copy is removed. Returns the copy's path.
    pub fn make(&self, fetch: &mut Fetch) -> std::result::Result<PathBuf, String> {
        match &self.original {
            Some(original) => crate::import_move::copy_new(original, &self.dir, &self.file_name, None),
            None => self.fetch(fetch),
        }
    }

    fn fetch(&self, fetch: &mut Fetch) -> std::result::Result<PathBuf, String> {
        let (file, path) = new_file(&self.dir, &self.file_name).map_err(|e| format!("{}: {e}", self.dir.display()))?;
        let r = (|| {
            let mut out = io::BufWriter::new(&file);
            let n = fetch(&self.link.uuid, &mut out)?;
            out.flush().map_err(|e| e.to_string())?;
            drop(out);
            file.sync_all().map_err(|e| e.to_string())?;
            let on_disk = file.metadata().map_err(|e| e.to_string())?.len();
            if on_disk != n || n == 0 {
                return Err(format!("{n} bytes came from Photos but {on_disk} were saved"));
            }
            if self.size > 0 && n != self.size {
                return Err(format!("Photos says the original is {} bytes, but {n} came", self.size));
            }
            Ok(())
        })();
        drop(file);
        match r {
            Ok(()) => Ok(path),
            Err(e) => {
                let _ = std::fs::remove_file(&path);
                Err(format!("{}: fetching the original from iCloud failed: {e}", self.file_name))
            }
        }
    }
}

/// A new file named `name` in `dir` (made if needed), or `name-1`, `name-2`… when taken.
fn new_file(dir: &Path, name: &str) -> io::Result<(File, PathBuf)> {
    std::fs::create_dir_all(dir)?;
    let (stem, ext) = match name.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() => (s.to_string(), format!(".{e}")),
        _ => (name.to_string(), String::new()),
    };
    for i in 0..100_000u32 {
        let path = if i == 0 { dir.join(name) } else { dir.join(format!("{stem}-{i}{ext}")) };
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(f) => return Ok((f, path)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::other("no free name"))
}

/// [`Fetch`] through PhotoKit: the asset of the System Photo Library with this UUID, downloaded
/// from iCloud. `progress` is told the fraction done; `cancel` stops it. Works only in the app
/// bundle (which may ask for access to Photos); elsewhere it answers why not.
#[cfg(not(target_arch = "wasm32"))]
pub fn photokit_fetch<'a>(
    cancel: &'a AtomicBool,
    progress: &'a mut dyn FnMut(f64),
) -> impl FnMut(&str, &mut dyn Write) -> std::result::Result<u64, String> + 'a {
    move |uuid, out| {
        let local_id = format!("{uuid}/L0/001");
        // asks once if the user hasn't answered yet (the app only; elsewhere this says why not)
        lightcraft_photokit::request_access().map_err(|e| e.to_string())?;
        lightcraft_photokit::download_original(&local_id, out, cancel, progress).map_err(|e| match e {
            lightcraft_photokit::DownloadError::NotFound => {
                "it is not in the System Photo Library, the only one LightCraft can fetch from iCloud; download it in Photos first".to_string()
            }
            e => e.to_string(),
        })
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
    /// proxies need none and are left out. A session without a library folder for the copies is
    /// an error.
    pub fn photos_copies(&self, ids: &[PhotoId]) -> Result<Vec<PhotosCopy>> {
        let mut out = Vec::new();
        for &id in ids {
            let Some(p) = self.catalog.photo(id) else { return Err(EngineError::Other(format!("no photo {}", id.0))) };
            let Source::ApplePhotos { library, uuid, original } = &p.source else { continue };
            let Some(lib) = self.library.as_ref().filter(|l| l.on_disk) else {
                return Err(EngineError::Other("editing an Apple Photos item copies it into the library: open a library first".into()));
            };
            // the original's own name (what Photos shows can differ, e.g. after an edit there)
            let fallback = original
                .as_deref()
                .and_then(|o| Path::new(o).file_name())
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| format!("{uuid}.jpg"));
            out.push(PhotosCopy {
                id,
                link: PhotosLink { library: library.clone(), uuid: uuid.clone() },
                original: original.as_deref().map(PathBuf::from),
                size: p.file_size,
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

    /// Copy (or fetch with `fetch`) every proxy among `ids` and adopt the copies, one at a time
    /// (the command's way; the app does the copying on a worker). Returns the copies' paths by
    /// photo id.
    pub fn edit_photos_items(&mut self, ids: &[PhotoId], fetch: &mut Fetch) -> Result<Vec<(PhotoId, PathBuf)>> {
        let copies = self.photos_copies(ids)?;
        let mut done = Vec::with_capacity(copies.len());
        for copy in copies {
            let path = copy.make(fetch).map_err(EngineError::Other)?;
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
