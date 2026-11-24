//! Saving an edit back to Apple Photos (issue #1622). A photo copied out of Photos to be edited
//! ([`crate::photos_edit`], so it has [`Photo::from_photos`]) is rendered as a full-size JPEG
//! and handed to Photos, which shows it in place of the asset's current version and keeps the
//! original (Photos' Revert to Original brings it back). LightCraft's develop settings go with
//! it as the edit's adjustment data. Only the System Photo Library can be changed (through
//! PhotoKit, in the macOS app); macOS asks the user to allow each change.
//!
//! Two halves, so the app can render and save off the UI thread: [`Session::photos_saves`] sets
//! up the renders (no file is written), [`PhotosSave::run`] renders, writes the JPEG to a
//! temporary file and hands it to a [`Save`] (no session needed).
//!
//! [`Photo::from_photos`]: lightcraft_catalog::Photo::from_photos

use std::path::Path;

use lightcraft_catalog::PhotoId;
use serde_json::json;

use crate::export::{ExportFormat, ExportOptions, MetadataPolicy, PreparedExport};
use crate::{EngineError, Result, Session};

/// The format id of the adjustment data stored with an edit in Photos.
pub const ADJUSTMENT_FORMAT: &str = "ai.storyteller.lightcraft.develop";
/// Its version: the data is `{"app", "version", "develop"}` JSON.
pub const ADJUSTMENT_VERSION: &str = "1";
/// JPEG quality of the photo Photos is given.
pub const QUALITY: u8 = 95;
/// Largest adjustment data stored (as `lightcraft_photokit::MAX_ADJUSTMENT_BYTES`); settings
/// that don't fit are left out and only the app and its version are stored.
pub const MAX_ADJUSTMENT_BYTES: usize = 1 << 20;

/// Hands a rendered JPEG to Photos: `(asset UUID, JPEG file, adjustment data)`.
pub type Save<'a> = dyn FnMut(&str, &Path, &[u8]) -> std::result::Result<(), String> + 'a;

/// One photo's edit to save to Photos.
pub struct PhotosSave {
    pub id: PhotoId,
    /// Photos' id for the asset it goes onto.
    pub uuid: String,
    pub file_name: String,
    /// What is stored with the edit in Photos.
    pub adjustment: Vec<u8>,
    /// What LightCraft last knew of the asset in Photos, to refuse replacing an edit made there
    /// since that LightCraft hasn't brought in ([`crate::photos_sync`]).
    pub sync: Option<lightcraft_catalog::PhotosSync>,
    /// The photo's file (the copy made of the asset), to tell Photos' version of it from a new edit.
    pub file: Option<std::path::PathBuf>,
    export: PreparedExport,
}

impl std::fmt::Debug for PhotosSave {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PhotosSave").field("id", &self.id).field("uuid", &self.uuid).field("file_name", &self.file_name).finish_non_exhaustive()
    }
}

/// A photo [`Session::photos_saves`] leaves out, and why.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Skipped {
    pub id: PhotoId,
    pub why: &'static str,
}

/// How the edit is rendered for Photos: a full-size JPEG in Display P3 (what Photos shows), with
/// the photo's metadata, upright.
pub fn export_options() -> ExportOptions {
    ExportOptions {
        format: ExportFormat::Jpeg,
        quality: QUALITY,
        resize: None,
        metadata: MetadataPolicy::All,
        color_space: lightcraft_pipeline::OutputSpace::DisplayP3,
        hdr: false,
        add_to_photos: false,
        watermark: None,
        ..ExportOptions::default()
    }
}

/// The adjustment data stored with `develop`: the settings when they fit, else only the app.
/// `base_ms`: the settings build on an edited version Photos made then (see
/// [`lightcraft_catalog::PhotosSync::base_ms`]), not on the original.
pub fn adjustment_data(develop: &impl serde::Serialize, base_ms: Option<i64>) -> Vec<u8> {
    let app = env!("CARGO_PKG_VERSION");
    let full = serde_json::to_vec(&json!({"app": "LightCraft", "version": app, "develop": develop, "base": base_ms})).unwrap_or_default();
    if !full.is_empty() && full.len() <= MAX_ADJUSTMENT_BYTES {
        return full;
    }
    serde_json::to_vec(&json!({"app": "LightCraft", "version": app, "base": base_ms})).unwrap_or_default()
}

/// Why a save is refused while Photos shows an edit LightCraft hasn't brought in.
pub const NEWER_IN_PHOTOS: &str = "Photos has an edit of this photo that LightCraft hasn't brought in yet: choose Photo ▸ Apple Photos ▸ Sync with Apple Photos first, so it isn't replaced";

impl PhotosSave {
    /// Check that Photos shows nothing LightCraft hasn't brought in, render the edit, write it to
    /// a temporary file and hand it to `save`. The file is removed afterwards, whatever happened.
    /// Returns the asset's modification time in Photos after the save, when Photos gives one.
    pub fn run(self, save: &mut Save, photos: &mut dyn crate::photos_sync::PhotosAccess) -> std::result::Result<Option<f64>, String> {
        let uuid = self.uuid.clone();
        let rendered = self.render(photos)?;
        let r = save(&rendered.uuid, &rendered.path, &rendered.adjustment);
        let _ = std::fs::remove_file(&rendered.path);
        r.map_err(|e| format!("{}: {e}", rendered.label))?;
        // when it changed now, so this save isn't taken for an edit made in Photos
        Ok(photos.state(&uuid).ok().and_then(|s| s.modified))
    }

    /// Check that Photos shows nothing LightCraft hasn't brought in, render the edit and write
    /// it to a temporary file (the caller removes it).
    fn render(self, photos: &mut dyn crate::photos_sync::PhotosAccess) -> std::result::Result<Rendered, String> {
        let label = self.file_name.clone();
        let (pending, _) = crate::photos_sync::pending_change(self.sync.as_ref(), &self.uuid, photos).map_err(|e| format!("{label}: {e}"))?;
        // an edit of Photos LightCraft hasn't seen: the version this photo was copied from (a copy
        // of an asset edited in Photos, saved before any sync noted when Photos last changed it)
        // is no new edit, and saving goes ahead
        let new_edit = match pending {
            None => false,
            Some(crate::photos_sync::Change::Edited) => !self.is_photos_version(photos),
            Some(crate::photos_sync::Change::Reverted) => true,
        };
        if new_edit {
            return Err(format!("{label}: {NEWER_IN_PHOTOS}"));
        }
        let exported = self.export.run().map_err(|e| format!("{label}: {e}"))?;
        let (file, path) = temp_jpeg(self.id).map_err(|e| format!("{label}: {e}"))?;
        let written = (|| {
            use std::io::Write;
            let mut file = file;
            file.write_all(&exported.bytes)?;
            file.sync_all()
        })();
        if let Err(e) = written {
            let _ = std::fs::remove_file(&path);
            return Err(format!("{label}: {e}"));
        }
        Ok(Rendered { uuid: self.uuid, label, path, adjustment: self.adjustment })
    }
}

/// One edit rendered for Photos ([`PhotosSave::render`]).
struct Rendered {
    uuid: String,
    label: String,
    path: std::path::PathBuf,
    adjustment: Vec<u8>,
}

/// Hands several rendered JPEGs to Photos as one change: `(asset UUID, JPEG file, adjustment
/// data)` each; one result per item, in order.
pub type SaveMany<'a> = dyn FnMut(&[(String, std::path::PathBuf, Vec<u8>)]) -> Vec<std::result::Result<(), String>> + 'a;

/// One photo's outcome in [`run_many`]: the asset's modification time in Photos after the save.
pub type Saved = (PhotoId, String, std::result::Result<Option<f64>, String>);

/// Render every edit of `saves`, then hand them all to Photos with `save` as one change (macOS
/// asks the user once, not once a photo). A photo that can't be rendered fails alone. The
/// temporary files are removed afterwards, whatever happened. `progress` is told the fraction
/// of the edits rendered.
pub fn run_many(
    saves: Vec<PhotosSave>,
    save: &mut SaveMany,
    photos: &mut dyn crate::photos_sync::PhotosAccess,
    progress: &mut dyn FnMut(f64),
) -> Vec<Saved> {
    let n = saves.len().max(1) as f64;
    let mut out: Vec<Saved> = Vec::with_capacity(saves.len());
    let mut rendered = Vec::new();
    for (i, one) in saves.into_iter().enumerate() {
        let (id, name) = (one.id, one.file_name.clone());
        match one.render(photos) {
            Ok(r) => {
                rendered.push((out.len(), r));
                out.push((id, name, Ok(None)));
            }
            Err(e) => out.push((id, name, Err(e))),
        }
        progress((i as f64 + 1.0) / n);
    }
    if rendered.is_empty() {
        return out;
    }
    let items: Vec<(String, std::path::PathBuf, Vec<u8>)> =
        rendered.iter().map(|(_, r)| (r.uuid.clone(), r.path.clone(), r.adjustment.clone())).collect();
    let results = save(&items);
    for (k, (at, r)) in rendered.into_iter().enumerate() {
        let _ = std::fs::remove_file(&r.path);
        let outcome = match results.get(k) {
            Some(Ok(())) => Ok(photos.state(&r.uuid).ok().and_then(|s| s.modified)),
            Some(Err(e)) => Err(format!("{}: {e}", r.label)),
            None => Err(format!("{}: Photos gave no answer", r.label)),
        };
        if let Some(slot) = out.get_mut(at) {
            slot.2 = outcome;
        }
    }
    out
}

impl PhotosSave {
    /// Whether the version Photos shows is, byte for byte, this photo's file.
    fn is_photos_version(&self, photos: &mut dyn crate::photos_sync::PhotosAccess) -> bool {
        let Some(file) = &self.file else { return false };
        let mut current = Vec::new();
        photos.download_current(&self.uuid, &mut current).is_ok() && std::fs::read(file).is_ok_and(|own| own == current)
    }
}

/// A new temporary JPEG for photo `id`, under a name this process never gives twice (a name freed
/// by one save is not taken by the next, so a caller can check its file is gone).
fn temp_jpeg(id: PhotoId) -> std::io::Result<(std::fs::File, std::path::PathBuf)> {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let dir = std::env::temp_dir();
    for _ in 0..1000u32 {
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = dir.join(format!("lightcraft-to-photos-{}-{}-{n}.jpg", std::process::id(), id.0));
        match std::fs::OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(f) => return Ok((f, path)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::other("no free temporary file name"))
}

/// [`Save`] through PhotoKit, onto the asset of the System Photo Library with this UUID.
/// `progress` is told the fraction of an iCloud download Photos needs first; `cancel` stops it.
/// Works only in the app bundle (which may ask for access to Photos); elsewhere it answers why not.
#[cfg(not(target_arch = "wasm32"))]
pub fn photokit_save<'a>(
    cancel: &'a std::sync::atomic::AtomicBool,
    progress: &'a mut dyn FnMut(f64),
) -> impl FnMut(&str, &Path, &[u8]) -> std::result::Result<(), String> + 'a {
    move |uuid, jpeg, data| {
        let local_id = format!("{uuid}/L0/001");
        lightcraft_photokit::request_access().map_err(|e| e.to_string())?;
        let adjustment = lightcraft_photokit::Adjustment { format: ADJUSTMENT_FORMAT, version: ADJUSTMENT_VERSION, data };
        lightcraft_photokit::save_edit(&local_id, jpeg, &adjustment, cancel, progress).map_err(|e| match e {
            lightcraft_photokit::EditError::NotFound => {
                "it is not in the System Photo Library, the only one LightCraft can save edits to".to_string()
            }
            e => e.to_string(),
        })
    }
}

/// [`SaveMany`] through PhotoKit, onto the System Photo Library, as one change. Otherwise as
/// [`photokit_save`].
#[cfg(not(target_arch = "wasm32"))]
pub fn photokit_save_many<'a>(
    cancel: &'a std::sync::atomic::AtomicBool,
    progress: &'a mut dyn FnMut(f64),
) -> impl FnMut(&[(String, std::path::PathBuf, Vec<u8>)]) -> Vec<std::result::Result<(), String>> + 'a {
    move |items| {
        if let Err(e) = lightcraft_photokit::request_access() {
            return items.iter().map(|_| Err(e.to_string())).collect();
        }
        let ids: Vec<String> = items.iter().map(|(uuid, _, _)| format!("{uuid}/L0/001")).collect();
        let edits: Vec<lightcraft_photokit::EditItem> = items
            .iter()
            .zip(&ids)
            .map(|((_, jpeg, data), id)| lightcraft_photokit::EditItem {
                local_id: id,
                rendered: jpeg,
                adjustment: lightcraft_photokit::Adjustment { format: ADJUSTMENT_FORMAT, version: ADJUSTMENT_VERSION, data },
            })
            .collect();
        lightcraft_photokit::save_edits(&edits, cancel, progress)
            .into_iter()
            .map(|r| {
                r.map_err(|e| match e {
                    lightcraft_photokit::EditError::NotFound => {
                        "it is not in the System Photo Library, the only one LightCraft can save edits to".to_string()
                    }
                    e => e.to_string(),
                })
            })
            .collect()
    }
}

impl Session {
    /// Photos from Apple Photos with LightCraft edits not saved there: edited proxies, and edited
    /// copies whose settings differ from those last saved (or that were never saved). Photos in
    /// Recently Deleted are left out.
    pub fn unsaved_photos_edits(&self) -> Vec<PhotoId> {
        self.catalog
            .photos()
            .filter(|p| !p.deleted && p.is_edited())
            .filter(|p| match (&p.source, &p.from_photos) {
                (lightcraft_catalog::Source::ApplePhotos { .. }, _) => true,
                (_, Some(link)) => {
                    let saved = link.sync.as_ref().filter(|s| s.saved).and_then(|s| s.settings.as_deref());
                    saved != Some(crate::photos_sync::settings_hash(&p.develop).as_str())
                }
                _ => false,
            })
            .map(|p| p.id)
            .collect()
    }

    /// The saves `ids` need: one per photo copied from Photos that has been edited. The rest are
    /// returned as skipped, with the reason.
    pub fn photos_saves(&mut self, ids: &[PhotoId]) -> Result<(Vec<PhotosSave>, Vec<Skipped>)> {
        let options = export_options();
        let (mut saves, mut skipped) = (Vec::new(), Vec::new());
        for &id in ids {
            let Some(p) = self.catalog.photo(id) else { return Err(EngineError::Other(format!("no photo {}", id.0))) };
            let Some(link) = p.from_photos.clone() else {
                skipped.push(Skipped { id, why: "not copied from Apple Photos" });
                continue;
            };
            if !p.is_edited() {
                skipped.push(Skipped { id, why: "not edited" });
                continue;
            }
            let adjustment = adjustment_data(&p.develop, link.sync.as_ref().and_then(|s| s.base_ms));
            let file_name = p.file_name.clone();
            let file = match &p.source {
                lightcraft_catalog::Source::File { path } => Some(std::path::PathBuf::from(path)),
                _ => None,
            };
            let export = crate::export::prepare_export(self, id, &options, 1).map_err(|e| EngineError::Other(format!("{file_name}: {e}")))?;
            saves.push(PhotosSave { id, uuid: link.uuid, file_name, adjustment, sync: link.sync, file, export });
        }
        Ok((saves, skipped))
    }

    /// Save every edit not saved to Photos yet ([`Self::unsaved_photos_edits`]) as one change with
    /// `save` (the command's way: proxies not copied yet are skipped, as copying them is the
    /// app's). Returns the outcomes and the photos skipped.
    pub fn save_all_photos_edits(
        &mut self,
        save: &mut SaveMany,
        photos: &mut dyn crate::photos_sync::PhotosAccess,
    ) -> Result<(Vec<Saved>, Vec<Skipped>)> {
        let ids = self.unsaved_photos_edits();
        let (saves, skipped) = self.photos_saves(&ids)?;
        let done = run_many(saves, save, photos, &mut |_| {});
        for (id, _, r) in &done {
            if let Ok(modified) = r {
                self.saved_to_photos(*id, *modified)?;
            }
        }
        Ok((done, skipped))
    }

    /// Render and save every edit among `ids` with `save`, one at a time (the command's way; the
    /// app does it on a worker). Returns the photos saved and those skipped.
    pub fn save_photos_edits(
        &mut self,
        ids: &[PhotoId],
        save: &mut Save,
        photos: &mut dyn crate::photos_sync::PhotosAccess,
    ) -> Result<(Vec<PhotoId>, Vec<Skipped>)> {
        let (saves, skipped) = self.photos_saves(ids)?;
        let mut done = Vec::with_capacity(saves.len());
        for one in saves {
            let id = one.id;
            let modified = one.run(save, photos).map_err(EngineError::Other)?;
            self.saved_to_photos(id, modified)?;
            done.push(id);
        }
        Ok((done, skipped))
    }
}

#[cfg(test)]
mod tests;
