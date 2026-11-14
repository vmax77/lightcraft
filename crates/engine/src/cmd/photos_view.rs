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
    let done = s.edit_photos_items(&ids).map_err(|e| bad(C, e.to_string()))?;
    Ok(json!({"copied": done.iter().map(|(id, path)| json!({"id": id.0, "path": path.to_string_lossy()})).collect::<Vec<_>>()}))
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
            [],
            None,
            "{ids?: photo ids (default: the selection)} — make each Apple Photos item a library photo to edit: its original is copied into the library's Originals/From Apple Photos/ and compared byte for byte, then the photo points at the copy, keeping its ratings and edits; nothing is written in the Photos library. Others are left as they are. An original only in iCloud is an error → {copied: [{id, path}]}",
            always,
            edit
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
