//! Keeping a photo copied from Apple Photos in step with its asset (issue #1622): whichever app
//! edited it last, that edit shows, and neither is lost.
//!
//! Photos' own edits can't become LightCraft settings, so when Photos shows an edit LightCraft
//! didn't make (or went back to the original), that version of the photo becomes the photo's file
//! in LightCraft, and LightCraft's settings build on it:
//! - settings never saved to Photos stay on top (Photos' version doesn't have them);
//! - settings saved to Photos are already in its version (Photos edits on top of the version
//!   LightCraft saved), so they start again from the default; when they changed since that save,
//!   they are kept as a snapshot first.
//!
//! Photos is asked about the asset's modification time first, and only when it changed since
//! LightCraft last looked is it asked whose edit it shows ([`PhotosAccess::ours`]). A change that
//! isn't a new edit (a favourite, a title, or LightCraft's own save) only moves that time on.
//!
//! The steps, so the app can ask Photos off the UI thread: [`Session::photos_syncs`] says what to
//! check (no file is touched), [`PhotosSyncJob::check`] asks Photos and downloads its version (no
//! session needed), and [`Session::adopt_photos_version`] applies it. Adopting isn't an undo step
//! (as copying a Photos item isn't): it brings in what Photos did.

use std::io::Write;
use std::path::{Path, PathBuf};

use lightcraft_catalog::{Op, PhotoId, PhotosLink, PhotosSync, Version};
use lightcraft_develop::DevelopSettings;

use crate::{EngineError, Result, Session};

/// What Photos says about an asset ([`PhotosAccess::state`]).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct PhotosState {
    /// When the asset last changed, seconds since 1970 (UTC).
    pub modified: Option<f64>,
    /// Photos shows an edited version of it.
    pub edited: bool,
}

/// What syncing asks of Photos about the asset with a UUID.
pub trait PhotosAccess {
    fn state(&mut self, uuid: &str) -> std::result::Result<PhotosState, String>;
    /// Whether the edit Photos shows was saved there by LightCraft.
    fn ours(&mut self, uuid: &str) -> std::result::Result<bool, String>;
    /// Write the version Photos shows (its edit) to `out`; returns the bytes written.
    fn download_current(&mut self, uuid: &str, out: &mut dyn Write) -> std::result::Result<u64, String>;
    /// Write the original to `out`; returns the bytes written.
    fn download_original(&mut self, uuid: &str, out: &mut dyn Write) -> std::result::Result<u64, String>;
}

/// [`PhotosAccess`] where there is no Photos (the web build): every call says so.
pub struct NoPhotos;

impl PhotosAccess for NoPhotos {
    fn state(&mut self, _: &str) -> std::result::Result<PhotosState, String> {
        Err("Apple Photos is only available on macOS".into())
    }
    fn ours(&mut self, _: &str) -> std::result::Result<bool, String> {
        Err("Apple Photos is only available on macOS".into())
    }
    fn download_current(&mut self, _: &str, _: &mut dyn Write) -> std::result::Result<u64, String> {
        Err("Apple Photos is only available on macOS".into())
    }
    fn download_original(&mut self, _: &str, _: &mut dyn Write) -> std::result::Result<u64, String> {
        Err("Apple Photos is only available on macOS".into())
    }
}

/// A hash of develop settings that stays the same across runs and versions (for
/// [`PhotosSync::settings`]).
pub fn settings_hash(settings: &DevelopSettings) -> String {
    lightcraft_preview::hash_bytes(settings.to_json().to_string().as_bytes()).to_string()
}

/// The name a file of these bytes needs for its format to be known, from its first bytes.
fn sniffed_name(stem: &str, bytes: &[u8]) -> Option<String> {
    let ext = match bytes {
        [0xff, 0xd8, 0xff, ..] => "jpeg",
        [0x89, b'P', b'N', b'G', ..] => "png",
        [b'I', b'I', 42, 0, ..] | [b'M', b'M', 0, 42, ..] => "tiff",
        [_, _, _, _, b'f', b't', b'y', b'p', b, ..] if matches!(*b, b'h' | b'm') => "heic",
        _ => return None,
    };
    Some(format!("{stem}.{ext}"))
}

/// Milliseconds since 1970 of a time in seconds.
fn ms(seconds: f64) -> i64 {
    (seconds * 1000.0).round().clamp(i64::MIN as f64, i64::MAX as f64) as i64
}

/// Which version of the asset Photos shows now that LightCraft hasn't brought in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Change {
    /// An edit LightCraft didn't make.
    Edited,
    /// The original, while LightCraft builds on an earlier edit of Photos.
    Reverted,
}

/// What Photos did to the asset since LightCraft last knew (`sync`), if anything LightCraft has to
/// bring in. Also says the asset's modification time, when Photos gave one.
pub fn pending_change(
    sync: Option<&PhotosSync>,
    uuid: &str,
    photos: &mut dyn PhotosAccess,
) -> std::result::Result<(Option<Change>, Option<i64>), String> {
    let state = photos.state(uuid)?;
    let modified = state.modified.map(ms);
    let seen = match (sync, modified) {
        (Some(s), Some(m)) => m <= s.photos_modified_ms,
        _ => false,
    };
    if seen {
        return Ok((None, modified));
    }
    let base_is_photos_edit = sync.is_some_and(|s| s.base_ms.is_some());
    let change = if state.edited { (!photos.ours(uuid)?).then_some(Change::Edited) } else { base_is_photos_edit.then_some(Change::Reverted) };
    Ok((change, modified))
}

/// One photo to check against its asset in Photos.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PhotosSyncJob {
    pub id: PhotoId,
    pub link: PhotosLink,
    /// The photo's file now (to tell a download that is the same version).
    pub file: Option<PathBuf>,
    /// The photo's file name, for a new version's.
    pub file_name: String,
    /// Where a new version goes.
    pub dir: PathBuf,
}

/// What [`PhotosSyncJob::check`] found.
#[derive(Clone, Debug)]
pub enum SyncFound {
    /// Nothing to bring in; the asset's modification time, to remember.
    InStep { modified_ms: Option<i64> },
    /// Photos shows a version LightCraft hasn't brought in: downloaded to `path`.
    NewVersion { change: Change, path: PathBuf, modified_ms: Option<i64>, info: Box<crate::media::ProbeInfo> },
}

impl PhotosSyncJob {
    /// Ask Photos what it did since LightCraft last knew, and download the version it shows when
    /// that is new. A download that is the same as the photo's file is dropped.
    pub fn check(&self, photos: &mut dyn PhotosAccess) -> std::result::Result<SyncFound, String> {
        let label = &self.file_name;
        let (change, modified_ms) = pending_change(self.link.sync.as_ref(), &self.link.uuid, photos).map_err(|e| format!("{label}: {e}"))?;
        let Some(change) = change else { return Ok(SyncFound::InStep { modified_ms }) };
        let stem = Path::new(&self.file_name).file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| self.link.uuid.clone());
        let stem = match change {
            Change::Edited => format!("{stem} (Photos edit)"),
            Change::Reverted => format!("{stem} (original)"),
        };
        let name = format!("{stem}.tmp");
        let (file, path) = crate::photos_edit::new_file(&self.dir, &name).map_err(|e| format!("{}: {e}", self.dir.display()))?;
        let fetched = (|| {
            let mut out = std::io::BufWriter::new(&file);
            let n = match change {
                Change::Edited => photos.download_current(&self.link.uuid, &mut out)?,
                Change::Reverted => photos.download_original(&self.link.uuid, &mut out)?,
            };
            out.flush().map_err(|e| e.to_string())?;
            drop(out);
            file.sync_all().map_err(|e| e.to_string())?;
            if n == 0 {
                return Err("Photos sent nothing".to_string());
            }
            std::fs::read(&path).map_err(|e| e.to_string())
        })();
        drop(file);
        let bytes = match fetched {
            Ok(b) => b,
            Err(e) => {
                let _ = std::fs::remove_file(&path);
                return Err(format!("{label}: fetching the version Photos shows failed: {e}"));
            }
        };
        let typed = sniffed_name(&stem, &bytes).ok_or_else(|| "not a photo LightCraft reads".to_string());
        let info = match typed.and_then(|n| crate::files::probe_bytes(&n, &bytes).map(|i| (n, i))) {
            Ok(i) => i,
            Err(e) => {
                let _ = std::fs::remove_file(&path);
                return Err(format!("{label}: the version Photos sent can't be read: {e}"));
            }
        };
        // the same version as the photo's file (a change in Photos that wasn't an edit)
        if self.file.as_ref().and_then(|f| std::fs::read(f).ok()).is_some_and(|now| now == bytes) {
            let _ = std::fs::remove_file(&path);
            return Ok(SyncFound::InStep { modified_ms });
        }
        let (typed, info) = info;
        let path = renamed(&path, &typed).map_err(|e| {
            let _ = std::fs::remove_file(&path);
            format!("{label}: {e}")
        })?;
        Ok(SyncFound::NewVersion { change, path, modified_ms, info: Box::new(info) })
    }
}

/// `path` renamed to `name` in its folder, or `name-1`, `name-2`… when taken.
fn renamed(path: &Path, name: &str) -> std::io::Result<PathBuf> {
    let dir = path.parent().ok_or_else(|| std::io::Error::other("no folder"))?;
    let (stem, ext) = name.rsplit_once('.').unwrap_or((name, ""));
    for i in 0..10_000u32 {
        let to = if i == 0 { dir.join(name) } else { dir.join(format!("{stem}-{i}.{ext}")) };
        // a hard link fails on a taken name, so two downloads never take the same one
        match std::fs::hard_link(path, &to) {
            Ok(()) => {
                std::fs::remove_file(path)?;
                return Ok(to);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::other("no free name"))
}

/// How [`Session::adopt_photos_version`] went.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Adopted {
    /// Nothing new: the photo was already in step.
    InStep,
    /// The photo now builds on the version Photos shows. `kept`: LightCraft's settings stayed on
    /// top; otherwise they started again from the default, and `snapshot` names the snapshot
    /// they were kept in, when they had to be.
    NewVersion { kept: bool, snapshot: Option<String> },
}

/// The name of the snapshot LightCraft's settings are kept in when Photos' version replaces them.
pub const SNAPSHOT: &str = "LightCraft edit before the Photos edit";

impl Session {
    /// The photos among `ids` copied from Apple Photos, to check against their assets.
    pub fn photos_syncs(&self, ids: &[PhotoId]) -> Vec<PhotosSyncJob> {
        let dir = self.library.as_ref().filter(|l| l.on_disk).map(|l| l.originals_dir().join(crate::photos_edit::COPY_FOLDER));
        let Some(dir) = dir else { return Vec::new() };
        ids.iter()
            .filter_map(|&id| {
                let p = self.catalog.photo(id)?;
                let link = p.from_photos.clone()?;
                let file = match &p.source {
                    lightcraft_catalog::Source::File { path } => Some(PathBuf::from(path)),
                    _ => None,
                };
                Some(PhotosSyncJob { id, link, file, file_name: p.file_name.clone(), dir: dir.clone() })
            })
            .collect()
    }

    /// Apply what [`PhotosSyncJob::check`] found. An error when the photo is no longer a copy of
    /// that asset; the caller then removes the file it downloaded.
    pub fn adopt_photos_version(&mut self, job: &PhotosSyncJob, found: SyncFound) -> Result<Adopted> {
        let p = self.catalog.photo(job.id).ok_or_else(|| EngineError::Other(format!("no photo {}", job.id.0)))?;
        let Some(link) = p.from_photos.clone().filter(|l| l.library == job.link.library && l.uuid == job.link.uuid) else {
            return Err(EngineError::Other(format!("photo {} is no longer a copy of the Apple Photos item it was", job.id.0)));
        };
        let (change, path, modified_ms, info) = match found {
            SyncFound::InStep { modified_ms } => {
                let sync = link.sync.clone().unwrap_or_default();
                if let Some(m) = modified_ms.filter(|m| *m > sync.photos_modified_ms) {
                    let link = PhotosLink { sync: Some(PhotosSync { photos_modified_ms: m, ..sync }), ..link };
                    self.apply_system(Op::SetFromPhotos { id: job.id, link: Some(link) })?;
                }
                return Ok(Adopted::InStep);
            }
            SyncFound::NewVersion { change, path, modified_ms, info } => (change, path, modified_ms, info),
        };
        let sync = link.sync.clone().unwrap_or_default();
        // LightCraft's settings are in Photos' version when they were saved there and haven't
        // changed since (an edit or revert in Photos replaces them either way)
        let saved = sync.saved;
        let changed_since = sync.settings.as_deref() != Some(settings_hash(&p.develop).as_str());
        let current = (*p.develop).clone();
        let mut ops = vec![crate::cmd::missing::relink_op(job.id, &path.to_string_lossy())];
        if let Op::Relink { format, .. } = &mut ops[0] {
            *format = Some(info.format.clone());
        }
        if let Some(op) = crate::cmd::convert::content_op(job.id, p, (*info).clone()) {
            ops.push(op);
        }
        // Photos' face boxes were placed on the file this replaces (Photos places faces on the
        // original; an edit may crop it): they go; boxes named or found in LightCraft stay
        let from_photos = |r: &lightcraft_meta::Region| r.description.as_deref() == Some(crate::photos_browse::FROM_PHOTOS);
        if p.meta.regions.iter().any(from_photos) {
            let mut meta = p.meta.clone();
            meta.regions.retain(|r| !from_photos(r));
            ops.push(Op::SetMeta { id: job.id, meta: Box::new(meta) });
        }
        let mut snapshot = None;
        if saved {
            if changed_since && current != DevelopSettings::default() {
                let mut versions = p.versions.clone();
                versions.push(Version { name: SNAPSHOT.into(), created: (self.clock)(), settings: std::sync::Arc::new(current), auto: false });
                ops.push(Op::SetVersions { id: job.id, versions });
                snapshot = Some(SNAPSHOT.to_string());
            }
            let label = match change {
                Change::Edited => "Edit from Apple Photos",
                Change::Reverted => "Original from Apple Photos",
            };
            ops.extend(self.develop_op(job.id, DevelopSettings::default(), label));
        }
        self.apply_system(Op::Batch { ops })?;
        self.media.forget(job.id);
        let settings = self.catalog.photo(job.id).map(|p| settings_hash(&p.develop));
        let base_ms = match change {
            Change::Edited => Some(modified_ms.unwrap_or(sync.photos_modified_ms)),
            Change::Reverted => None,
        };
        let synced = PhotosSync { photos_modified_ms: modified_ms.unwrap_or(sync.photos_modified_ms), saved: false, settings, base_ms };
        self.apply_system(Op::SetFromPhotos { id: job.id, link: Some(PhotosLink { sync: Some(synced), ..link }) })?;
        Ok(Adopted::NewVersion { kept: !saved, snapshot })
    }

    /// Remember that the photo's edit was saved to Photos, where the asset now changed at
    /// `modified` (seconds since 1970).
    pub fn saved_to_photos(&mut self, id: PhotoId, modified: Option<f64>) -> Result<()> {
        let Some(p) = self.catalog.photo(id) else { return Ok(()) };
        let Some(link) = p.from_photos.clone() else { return Ok(()) };
        let before = link.sync.clone().unwrap_or_default();
        let synced = PhotosSync {
            photos_modified_ms: modified.map(ms).unwrap_or(before.photos_modified_ms),
            saved: true,
            settings: Some(settings_hash(&p.develop)),
            base_ms: before.base_ms,
        };
        self.apply_system(Op::SetFromPhotos { id, link: Some(PhotosLink { sync: Some(synced), ..link }) })
    }

    /// Check `ids` against Photos with `photos` and bring in what it shows, one photo at a time
    /// (the command's way; the app asks Photos on a worker).
    pub fn sync_photos_items(&mut self, ids: &[PhotoId], photos: &mut dyn PhotosAccess) -> Result<Vec<(PhotoId, Adopted)>> {
        let mut out = Vec::new();
        for job in self.photos_syncs(ids) {
            let found = job.check(photos).map_err(EngineError::Other)?;
            let made = match &found {
                SyncFound::NewVersion { path, .. } => Some(path.clone()),
                SyncFound::InStep { .. } => None,
            };
            match self.adopt_photos_version(&job, found) {
                Ok(a) => out.push((job.id, a)),
                Err(e) => {
                    if let Some(p) = made {
                        let _ = std::fs::remove_file(p);
                    }
                    return Err(e);
                }
            }
        }
        Ok(out)
    }
}

/// [`PhotosAccess`] through PhotoKit, for the System Photo Library. `progress` is told the fraction
/// of a download; `cancel` stops it. Works only in the app bundle (which may ask for access to
/// Photos); elsewhere every call answers why not.
#[cfg(not(target_arch = "wasm32"))]
pub struct PhotoKitAccess<'a> {
    pub cancel: &'a std::sync::atomic::AtomicBool,
    pub progress: &'a mut dyn FnMut(f64),
}

#[cfg(not(target_arch = "wasm32"))]
fn not_found(e: impl std::fmt::Display, not_found: bool) -> String {
    if not_found { "it is not in the System Photo Library, the only one LightCraft can sync with".to_string() } else { e.to_string() }
}

#[cfg(not(target_arch = "wasm32"))]
impl PhotosAccess for PhotoKitAccess<'_> {
    fn state(&mut self, uuid: &str) -> std::result::Result<PhotosState, String> {
        lightcraft_photokit::request_access().map_err(|e| e.to_string())?;
        let s =
            lightcraft_photokit::edit_state(&format!("{uuid}/L0/001")).map_err(|e| not_found(&e, e == lightcraft_photokit::EditError::NotFound))?;
        Ok(PhotosState { modified: s.modified, edited: s.edited })
    }

    fn ours(&mut self, uuid: &str) -> std::result::Result<bool, String> {
        let found =
            lightcraft_photokit::read_adjustment(&format!("{uuid}/L0/001"), crate::photos_save::ADJUSTMENT_FORMAT, self.cancel, &mut *self.progress)
                .map_err(|e| not_found(&e, e == lightcraft_photokit::EditError::NotFound))?;
        Ok(found.is_some())
    }

    fn download_current(&mut self, uuid: &str, out: &mut dyn Write) -> std::result::Result<u64, String> {
        lightcraft_photokit::download_current(&format!("{uuid}/L0/001"), out, self.cancel, &mut *self.progress)
            .map_err(|e| not_found(&e, e == lightcraft_photokit::DownloadError::NotFound))
    }

    fn download_original(&mut self, uuid: &str, out: &mut dyn Write) -> std::result::Result<u64, String> {
        lightcraft_photokit::download_original(&format!("{uuid}/L0/001"), out, self.cancel, &mut *self.progress)
            .map_err(|e| not_found(&e, e == lightcraft_photokit::DownloadError::NotFound))
    }
}

#[cfg(test)]
pub(crate) mod tests;
