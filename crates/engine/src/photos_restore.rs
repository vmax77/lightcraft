//! Reading back the edit LightCraft saved to Apple Photos (issue #1622). Save Edit to Apple Photos
//! ([`crate::photos_save`]) stores the develop settings with the edit as its adjustment data.
//! When the edit Photos shows for an asset is still that one, PhotoKit hands the data back, and
//! the photo (the asset's proxy, or the copy made to edit it) gets those settings again, so
//! editing carries on where it was, on another Mac too or after the copy is gone. An edit made
//! since in Photos or another app is not LightCraft's: nothing is read for it.
//!
//! Three steps, so the app can ask Photos off the UI thread: [`Session::photos_restores`] says
//! which assets to ask about, [`PhotosRestore::read`] asks (no session needed), and
//! [`Session::adopt_restored`] applies the settings as one undo step.

use lightcraft_catalog::{Op, PhotoId, Source};
use lightcraft_develop::DevelopSettings;
use serde_json::Value;

use crate::photos_save::{ADJUSTMENT_VERSION, MAX_ADJUSTMENT_BYTES, Skipped};
use crate::{EngineError, Result, Session};

/// The undo step's label.
pub const LABEL: &str = "Restore Edit from Apple Photos";

/// Asks Photos for the adjustment data LightCraft stored with the edit it shows for the asset
/// with this UUID: `(version, data)`, or `None` when that edit isn't LightCraft's.
pub type Read<'a> = dyn FnMut(&str) -> std::result::Result<Option<(String, Vec<u8>)>, String> + 'a;

/// A photo whose edit is to be read back from Photos.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PhotosRestore {
    pub id: PhotoId,
    /// Photos' id for the asset.
    pub uuid: String,
    pub file_name: String,
}

/// What Photos had for one photo.
#[derive(Clone, Debug, PartialEq)]
pub enum Restored {
    Settings(Box<DevelopSettings>),
    /// The edit Photos shows was not saved by LightCraft (or the photo is unedited there).
    NotLightCraft,
    /// LightCraft saved the edit but its settings were too large to store with it.
    NotStored,
}

/// The settings in adjustment data LightCraft stored (`{"app", "version", "develop"}` JSON,
/// [`crate::photos_save::adjustment_data`]): checked as settings from outside the app are.
/// `Ok(None)` when they were left out for being too large.
pub fn settings_from(version: &str, data: &[u8]) -> std::result::Result<Option<DevelopSettings>, String> {
    if version != ADJUSTMENT_VERSION {
        return Err(format!("the edit was saved by another version of LightCraft (format {version})"));
    }
    if data.len() > MAX_ADJUSTMENT_BYTES {
        return Err("the edit's settings are too large".into());
    }
    let v: Value = serde_json::from_slice(data).map_err(|e| format!("the edit's settings can't be read: {e}"))?;
    if v.get("app").and_then(Value::as_str) != Some("LightCraft") {
        return Err("the edit's settings are not LightCraft's".into());
    }
    let Some(develop) = v.get("develop") else { return Ok(None) };
    DevelopSettings::from_untrusted_json(develop).map(Some).map_err(|e| format!("the edit's settings can't be read: {e}"))
}

impl PhotosRestore {
    /// Ask Photos with `read` and make sense of the answer.
    pub fn read(&self, read: &mut Read) -> std::result::Result<Restored, String> {
        let label = &self.file_name;
        let Some((version, data)) = read(&self.uuid).map_err(|e| format!("{label}: {e}"))? else { return Ok(Restored::NotLightCraft) };
        match settings_from(&version, &data).map_err(|e| format!("{label}: {e}"))? {
            Some(settings) => Ok(Restored::Settings(Box::new(settings))),
            None => Ok(Restored::NotStored),
        }
    }
}

/// [`Read`] through PhotoKit, from the System Photo Library. `progress` is told the fraction of an
/// iCloud download Photos needs first; `cancel` stops it. Works only in the app bundle (which may
/// ask for access to Photos); elsewhere it answers why not.
#[cfg(not(target_arch = "wasm32"))]
pub fn photokit_read<'a>(
    cancel: &'a std::sync::atomic::AtomicBool,
    progress: &'a mut dyn FnMut(f64),
) -> impl FnMut(&str) -> std::result::Result<Option<(String, Vec<u8>)>, String> + 'a {
    move |uuid| {
        let local_id = format!("{uuid}/L0/001");
        lightcraft_photokit::request_access().map_err(|e| e.to_string())?;
        let found =
            lightcraft_photokit::read_adjustment(&local_id, crate::photos_save::ADJUSTMENT_FORMAT, cancel, progress).map_err(|e| match e {
                lightcraft_photokit::EditError::NotFound => {
                    "it is not in the System Photo Library, the only one LightCraft can read edits from".to_string()
                }
                e => e.to_string(),
            })?;
        Ok(found.map(|a| (a.version, a.data)))
    }
}

impl Session {
    /// The photos among `ids` whose edit can be read back from Photos: proxies of Photos assets
    /// and copies made from them. The rest are returned as skipped.
    pub fn photos_restores(&self, ids: &[PhotoId]) -> Result<(Vec<PhotosRestore>, Vec<Skipped>)> {
        let (mut out, mut skipped) = (Vec::new(), Vec::new());
        for &id in ids {
            let Some(p) = self.catalog.photo(id) else { return Err(EngineError::Other(format!("no photo {}", id.0))) };
            let uuid = match (&p.source, &p.from_photos) {
                (Source::ApplePhotos { uuid, .. }, _) => uuid.clone(),
                (_, Some(link)) => link.uuid.clone(),
                _ => {
                    skipped.push(Skipped { id, why: "not from Apple Photos" });
                    continue;
                }
            };
            out.push(PhotosRestore { id, uuid, file_name: p.file_name.clone() });
        }
        Ok((out, skipped))
    }

    /// Give each photo the settings read back for it, as one undo step. Photos removed meanwhile
    /// are left out; returns the ids changed.
    pub fn adopt_restored(&mut self, found: Vec<(PhotoId, DevelopSettings)>) -> Result<Vec<PhotoId>> {
        let mut ops = Vec::new();
        let mut done = Vec::new();
        for (id, settings) in found {
            if self.catalog.photo(id).is_some_and(|p| *p.develop == settings) {
                done.push(id);
                continue;
            }
            if let Some(op) = self.develop_op(id, settings, LABEL) {
                ops.push(op);
                done.push(id);
            }
        }
        if !ops.is_empty() {
            self.commit(LABEL, Op::Batch { ops })?;
        }
        Ok(done)
    }

    /// Read back and apply the edits of `ids` with `read`, one photo at a time (the command's way;
    /// the app asks Photos on a worker). Returns the photos restored and those skipped.
    pub fn restore_photos_edits(&mut self, ids: &[PhotoId], read: &mut Read) -> Result<(Vec<PhotoId>, Vec<Skipped>)> {
        let (restores, mut skipped) = self.photos_restores(ids)?;
        let mut found = Vec::new();
        for one in &restores {
            match one.read(read).map_err(EngineError::Other)? {
                Restored::Settings(settings) => found.push((one.id, *settings)),
                Restored::NotLightCraft => skipped.push(Skipped { id: one.id, why: NOT_LIGHTCRAFT }),
                Restored::NotStored => skipped.push(Skipped { id: one.id, why: NOT_STORED }),
            }
        }
        Ok((self.adopt_restored(found)?, skipped))
    }
}

/// Why a photo's edit isn't read back: the edit Photos shows isn't LightCraft's.
pub const NOT_LIGHTCRAFT: &str = "its edit in Photos was not saved by LightCraft";
/// Why a photo's edit isn't read back: its settings were too large to store.
pub const NOT_STORED: &str = "its settings were too large to store in Photos";

#[cfg(test)]
mod tests;
