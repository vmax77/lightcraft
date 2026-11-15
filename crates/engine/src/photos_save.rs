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
pub fn adjustment_data(develop: &impl serde::Serialize) -> Vec<u8> {
    let app = env!("CARGO_PKG_VERSION");
    let full = serde_json::to_vec(&json!({"app": "LightCraft", "version": app, "develop": develop})).unwrap_or_default();
    if !full.is_empty() && full.len() <= MAX_ADJUSTMENT_BYTES {
        return full;
    }
    serde_json::to_vec(&json!({"app": "LightCraft", "version": app})).unwrap_or_default()
}

impl PhotosSave {
    /// Render the edit, write it to a temporary file and hand it to `save`. The file is removed
    /// afterwards, whatever happened.
    pub fn run(self, save: &mut Save) -> std::result::Result<(), String> {
        let label = self.file_name.clone();
        let exported = self.export.run().map_err(|e| format!("{label}: {e}"))?;
        let (file, path) = temp_jpeg(self.id).map_err(|e| format!("{label}: {e}"))?;
        let r = (|| {
            use std::io::Write;
            let mut file = file;
            file.write_all(&exported.bytes).map_err(|e| e.to_string())?;
            file.sync_all().map_err(|e| e.to_string())?;
            drop(file);
            save(&self.uuid, &path, &self.adjustment)
        })();
        let _ = std::fs::remove_file(&path);
        r.map_err(|e| format!("{label}: {e}"))
    }
}

/// A new temporary JPEG for photo `id`.
fn temp_jpeg(id: PhotoId) -> std::io::Result<(std::fs::File, std::path::PathBuf)> {
    let dir = std::env::temp_dir();
    for i in 0..1000u32 {
        let path = dir.join(format!("lightcraft-to-photos-{}-{}-{i}.jpg", std::process::id(), id.0));
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
            lightcraft_photokit::SaveError::NotFound => {
                "it is not in the System Photo Library, the only one LightCraft can save edits to".to_string()
            }
            e => e.to_string(),
        })
    }
}

impl Session {
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
            let adjustment = adjustment_data(&p.develop);
            let file_name = p.file_name.clone();
            let export = crate::export::prepare_export(self, id, &options, 1).map_err(|e| EngineError::Other(format!("{file_name}: {e}")))?;
            saves.push(PhotosSave { id, uuid: link.uuid, file_name, adjustment, export });
        }
        Ok((saves, skipped))
    }

    /// Render and save every edit among `ids` with `save`, one at a time (the command's way; the
    /// app does it on a worker). Returns the photos saved and those skipped.
    pub fn save_photos_edits(&mut self, ids: &[PhotoId], save: &mut Save) -> Result<(Vec<PhotoId>, Vec<Skipped>)> {
        let (saves, skipped) = self.photos_saves(ids)?;
        let mut done = Vec::with_capacity(saves.len());
        for one in saves {
            let id = one.id;
            one.run(save).map_err(EngineError::Other)?;
            done.push(id);
        }
        Ok((done, skipped))
    }
}

#[cfg(test)]
mod tests;
