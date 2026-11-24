//! `applePhotos.*`: browse an Apple Photos library in the grid (issue #1622; see
//! [`crate::photos_browse`]). macOS libraries are read in place, read only; the web build has none.

use serde_json::{Value, json};

use super::{CommandSpec, always, bad, cmd};
use crate::Session;
use crate::photos_browse::PhotosView;

fn view_param(p: &Value) -> Result<PhotosView, crate::EngineError> {
    PhotosView::from_params(p).map_err(|e| bad("applePhotos.show", e))
}

#[cfg(not(target_arch = "wasm32"))]
fn show(s: &mut Session, p: &Value) -> crate::Result<Value> {
    const C: &str = "applePhotos.show";
    let view = view_param(p)?;
    let library = crate::photos_browse::library_param(s, p).map_err(|e| bad(C, e))?;
    let read = crate::photos_browse::read_view(&library, &view, &std::sync::atomic::AtomicBool::new(false)).map_err(|e| bad(C, e.to_string()))?;
    s.show_photos(read)
}

#[cfg(target_arch = "wasm32")]
fn show(_: &mut Session, p: &Value) -> crate::Result<Value> {
    view_param(p)?;
    Err(bad("applePhotos.show", "Apple Photos libraries can be browsed on macOS only"))
}

#[cfg(not(target_arch = "wasm32"))]
fn libraries(_: &mut Session, _: &Value) -> crate::Result<Value> {
    let home = crate::library::home_folder();
    let pictures = home.as_ref().map(|h| h.join("Pictures"));
    let found = pictures.as_deref().map(crate::apple_photos_library::PhotosLibrary::list_in).unwrap_or_default();
    let default = home.as_deref().and_then(crate::photos_browse::default_library);
    Ok(json!({
        "libraries": found.iter().map(|p| p.to_string_lossy()).collect::<Vec<_>>(),
        "default": default.map(|p| p.to_string_lossy().into_owned()),
    }))
}

#[cfg(target_arch = "wasm32")]
fn libraries(_: &mut Session, _: &Value) -> crate::Result<Value> {
    Ok(json!({"libraries": [], "default": null}))
}

/// Make each Apple Photos item among the targets a library photo of its own, from a verified
/// copy of its original (see [`crate::photos_edit`]).
fn edit(s: &mut Session, p: &Value) -> crate::Result<Value> {
    const C: &str = "applePhotos.edit";
    let ids = s.targets(p).map_err(|e| bad(C, e.to_string()))?;
    #[cfg(not(target_arch = "wasm32"))]
    let (cancel, mut progress) = (std::sync::atomic::AtomicBool::new(false), |_: f64| {});
    #[cfg(not(target_arch = "wasm32"))]
    let mut fetch = crate::photos_edit::photokit_fetch(&cancel, &mut progress);
    #[cfg(target_arch = "wasm32")]
    let mut fetch = |_: &str, _: &mut dyn std::io::Write| -> Result<u64, String> { Err("Apple Photos is only available on macOS".into()) };
    let done = s.edit_photos_items(&ids, &mut fetch).map_err(|e| bad(C, e.to_string()))?;
    Ok(json!({"copied": done.iter().map(|(id, path)| json!({"id": id.0, "path": path.to_string_lossy()})).collect::<Vec<_>>()}))
}

/// Save the edit of each target photo copied from Photos back onto its asset in the System Photo
/// Library (see [`crate::photos_save`]).
fn save_edit(s: &mut Session, p: &Value) -> crate::Result<Value> {
    const C: &str = "applePhotos.saveEdit";
    let ids = s.targets(p).map_err(|e| bad(C, e.to_string()))?;
    #[cfg(not(target_arch = "wasm32"))]
    let (cancel, mut progress) = (std::sync::atomic::AtomicBool::new(false), |_: f64| {});
    #[cfg(not(target_arch = "wasm32"))]
    let (check_cancel, mut check_progress) = (std::sync::atomic::AtomicBool::new(false), |_: f64| {});
    #[cfg(not(target_arch = "wasm32"))]
    let mut photos = crate::photos_sync::PhotoKitAccess { cancel: &check_cancel, progress: &mut check_progress };
    #[cfg(not(target_arch = "wasm32"))]
    let mut save = crate::photos_save::photokit_save(&cancel, &mut progress);
    #[cfg(target_arch = "wasm32")]
    let mut photos = crate::photos_sync::NoPhotos;
    #[cfg(target_arch = "wasm32")]
    let mut save = |_: &str, _: &std::path::Path, _: &[u8]| -> Result<(), String> { Err("Apple Photos is only available on macOS".into()) };
    let (saved, skipped) = s.save_photos_edits(&ids, &mut save, &mut photos).map_err(|e| bad(C, e.to_string()))?;
    Ok(json!({
        "saved": saved.iter().map(|id| id.0).collect::<Vec<_>>(),
        "skipped": skipped.iter().map(|k| json!({"id": k.id.0, "why": k.why})).collect::<Vec<_>>(),
    }))
}

/// Read back the settings LightCraft stored with the edit Photos shows for each target photo (see
/// [`crate::photos_restore`]).
fn restore_edit(s: &mut Session, p: &Value) -> crate::Result<Value> {
    const C: &str = "applePhotos.restoreEdit";
    let ids = s.targets(p).map_err(|e| bad(C, e.to_string()))?;
    #[cfg(not(target_arch = "wasm32"))]
    let (cancel, mut progress) = (std::sync::atomic::AtomicBool::new(false), |_: f64| {});
    #[cfg(not(target_arch = "wasm32"))]
    let mut read = crate::photos_restore::photokit_read(&cancel, &mut progress);
    #[cfg(target_arch = "wasm32")]
    let mut read = |_: &str| -> Result<Option<(String, Vec<u8>)>, String> { Err("Apple Photos is only available on macOS".into()) };
    let (restored, skipped) = s.restore_photos_edits(&ids, &mut read).map_err(|e| bad(C, e.to_string()))?;
    Ok(json!({
        "restored": restored.iter().map(|id| id.0).collect::<Vec<_>>(),
        "skipped": skipped.iter().map(|k| json!({"id": k.id.0, "why": k.why})).collect::<Vec<_>>(),
    }))
}

/// Bring in the version Photos shows of each target photo copied from it, when Photos edited it
/// (or went back to the original) since LightCraft last looked (see [`crate::photos_sync`]).
fn sync(s: &mut Session, p: &Value) -> crate::Result<Value> {
    const C: &str = "applePhotos.sync";
    let ids = s.targets(p).map_err(|e| bad(C, e.to_string()))?;
    #[cfg(not(target_arch = "wasm32"))]
    let (cancel, mut progress) = (std::sync::atomic::AtomicBool::new(false), |_: f64| {});
    #[cfg(not(target_arch = "wasm32"))]
    let mut photos = crate::photos_sync::PhotoKitAccess { cancel: &cancel, progress: &mut progress };
    #[cfg(target_arch = "wasm32")]
    let mut photos = crate::photos_sync::NoPhotos;
    let done = s.sync_photos_items(&ids, &mut photos).map_err(|e| bad(C, e.to_string()))?;
    let row = |(id, a): &(lightcraft_catalog::PhotoId, crate::photos_sync::Adopted)| match a {
        crate::photos_sync::Adopted::InStep => json!({"id": id.0, "changed": false}),
        crate::photos_sync::Adopted::NewVersion { kept, snapshot } => {
            json!({"id": id.0, "changed": true, "keptSettings": kept, "snapshot": snapshot})
        }
    };
    Ok(json!({"photos": done.iter().map(row).collect::<Vec<_>>()}))
}

/// Save every LightCraft edit not saved to Photos yet, library-wide, as one change (see
/// [`crate::photos_save::run_many`]).
fn save_all_edits(s: &mut Session, _: &Value) -> crate::Result<Value> {
    const C: &str = "applePhotos.saveAllEdits";
    #[cfg(not(target_arch = "wasm32"))]
    let (cancel, mut progress, mut checked) = (std::sync::atomic::AtomicBool::new(false), |_: f64| {}, |_: f64| {});
    #[cfg(not(target_arch = "wasm32"))]
    let mut photos = crate::photos_sync::PhotoKitAccess { cancel: &cancel, progress: &mut checked };
    #[cfg(not(target_arch = "wasm32"))]
    let mut save = crate::photos_save::photokit_save_many(&cancel, &mut progress);
    #[cfg(target_arch = "wasm32")]
    let mut photos = crate::photos_sync::NoPhotos;
    #[cfg(target_arch = "wasm32")]
    let mut save = |items: &[(String, std::path::PathBuf, Vec<u8>)]| -> Vec<Result<(), String>> {
        items.iter().map(|_| Err("Apple Photos is only available on macOS".to_string())).collect()
    };
    let (done, skipped) = s.save_all_photos_edits(&mut save, &mut photos).map_err(|e| bad(C, e.to_string()))?;
    Ok(json!({
        "saved": done.iter().filter(|d| d.2.is_ok()).map(|d| d.0.0).collect::<Vec<_>>(),
        "failed": done.iter().filter_map(|d| d.2.as_ref().err().map(|e| json!({"id": d.0.0, "why": e}))).collect::<Vec<_>>(),
        "skipped": skipped.iter().map(|k| json!({"id": k.id.0, "why": k.why})).collect::<Vec<_>>(),
    }))
}

fn albums(s: &mut Session, _: &Value) -> crate::Result<Value> {
    let browse = s.photos_browse.as_ref();
    Ok(json!({
        "library": browse.map(|b| b.library.clone()),
        "view": browse.map(|b| b.view.clone()),
        "albums": browse.map(|b| b.albums.clone()).unwrap_or_default(),
    }))
}

pub fn specs() -> Vec<CommandSpec> {
    vec![
        cmd!(
            query "applePhotos.libraries",
            "Apple Photos Libraries",
            [],
            None,
            "{} — the Photos libraries in ~/Pictures and the usual System Photo Library, if there (macOS) → {libraries: [path], default: path|null}",
            always,
            libraries
        ),
        cmd!(
            "applePhotos.show",
            "Show Apple Photos",
            [],
            None,
            "{library?: path to a .photoslibrary (default: the one browsed, else ~/Pictures/Photos Library.photoslibrary), view?: `all` | `favourites`, album?: uuid} — show the library's photos in the grid without adding them: read only, nothing is written in the library; hidden and recently deleted ones are left out → {library, view, photos, new, updated, warnings}",
            always,
            show
        ),
        cmd!(
            "applePhotos.edit",
            "Edit Apple Photos Item",
            ["Photo", "Apple Photos"],
            None,
            "{ids?: photo ids (default: the selection)} — make each Apple Photos item a library photo to edit: its original is copied into the library's Originals/From Apple Photos/ and compared byte for byte, then the photo points at the copy, keeping its ratings and edits; nothing is written in the Photos library. An original only in iCloud is downloaded first (System Photo Library, macOS app only). Others are left as they are → {copied: [{id, path}]}",
            always,
            edit
        ),
        cmd!(
            "applePhotos.saveEdit",
            "Save Edit to Apple Photos",
            ["Photo", "Apple Photos"],
            Some("Cmd+Alt+Shift+P"),
            "{ids?: photo ids (default: the selection)} — for each edited photo that was copied from Apple Photos to be edited, render it (full-size JPEG, Display P3) and put it on the asset it came from, in place of what Photos showed; Photos keeps the original (Revert to Original) and LightCraft's settings go with the edit. System Photo Library only, macOS app only; macOS asks to allow each change. Others are skipped → {saved: [id], skipped: [{id, why}]}",
            always,
            save_edit
        ),
        cmd!(
            "applePhotos.saveAllEdits",
            "Save All Edits to Apple Photos",
            ["Photo", "Apple Photos"],
            None,
            "{} — library-wide: every photo from Apple Photos with LightCraft edits not saved there yet (edited, and its settings not the ones last saved), rendered and put on its asset as Save Edit to Apple Photos does, all as one change, so macOS asks once. In the app, items not copied yet are copied first; here they are skipped. System Photo Library only, macOS app only → {saved: [id], failed: [{id, why}], skipped: [{id, why}]}",
            always,
            save_all_edits
        ),
        cmd!(
            "applePhotos.restoreEdit",
            "Restore Edit from Apple Photos",
            ["Photo", "Apple Photos"],
            None,
            "{ids?: photo ids (default: the selection)} — for each Apple Photos item (or copy made of one) whose edit in Photos was saved there by LightCraft (Save Edit to Apple Photos), read back the develop settings stored with it and apply them, as one undo step; an edit made since in Photos or another app is not LightCraft's and is skipped. System Photo Library only, macOS app only; nothing is changed in Photos → {restored: [id], skipped: [{id, why}]}",
            always,
            restore_edit
        ),
        cmd!(
            "applePhotos.sync",
            "Sync with Apple Photos",
            ["Photo", "Apple Photos"],
            None,
            "{ids?: photo ids (default: the selection)} — for each photo copied from Apple Photos, ask Photos whether it shows an edit LightCraft didn't make (or went back to the original) since LightCraft last looked; if so, that version becomes the photo's file and LightCraft's settings build on it: settings never saved to Photos stay on top, settings saved there (already in Photos' version) start again from the default, kept as a snapshot when they changed since. Not an undo step. System Photo Library only, macOS app only → {photos: [{id, changed, keptSettings?, snapshot?}]}",
            always,
            sync
        ),
        cmd!(
            query "applePhotos.albums",
            "Apple Photos Albums",
            [],
            None,
            "{} — the albums and folders of the Photos library being browsed, parents first → {library, view, albums: [{uuid, title, folder, parent}]}",
            always,
            albums
        ),
    ]
}
