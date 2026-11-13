//! The Apple Photos section (issue #1622): All Photos, Favourites and the library's albums and
//! folders. A row shows its photos as proxies in the grid (`applePhotos.show`, read on a worker);
//! the albums are listed once the library has been read.

use egui::Sense;
use lightcraft_engine::photos_browse::{PhotosAlbumRow, PhotosView};

use super::rows::{Row, set_tree_open, tree_open, tree_toggle};
use super::{Icon, LibrarySource, LightcraftApp, json};

/// Deepest folder nesting drawn (the engine already cuts loops; this bounds the recursion).
const MAX_DEPTH: usize = 64;

/// How many rows the section has (for its empty hint): All Photos and Favourites, then the
/// top-level albums and folders.
pub(super) fn apple_photos_row_count(app: &LightcraftApp) -> usize {
    2 + app.session.photos_browse.as_ref().map_or(0, |b| b.albums.iter().filter(|a| a.parent.is_none()).count())
}

pub(super) fn apple_photos_rows(app: &mut LightcraftApp, ui: &mut egui::Ui) {
    let shown = (app.session.source == LibrarySource::ApplePhotos)
        .then(|| app.session.photos_browse.as_ref().map(|b| (b.view.clone(), b.members.len())))
        .flatten();
    let reading = app.photos_task.is_some();
    for (id, icon, label, view, params) in [
        ("applePhotos:all", Icon::Photos, "All Photos", PhotosView::All, json!({"view": "all"})),
        ("applePhotos:favourites", Icon::Heart, "Favourites", PhotosView::Favourites, json!({"view": "favourites"})),
    ] {
        let selected = shown.as_ref().is_some_and(|(v, _)| *v == view);
        let count = shown.as_ref().filter(|(v, _)| *v == view && !reading).map(|(_, n)| *n);
        if Row::new(id, icon, crate::i18n::tr(label)).count(count).selected(selected).show(app, ui).clicked() {
            run_show(app, ui, params);
        }
    }
    let albums = app.session.photos_browse.as_ref().map(|b| b.albums.clone()).unwrap_or_default();
    let open_album = shown.and_then(|(v, n)| match v {
        PhotosView::Album { uuid } => Some((uuid, n)),
        _ => None,
    });
    album_rows(app, ui, &albums, None, 0, open_album.as_ref());
}

fn run_show(app: &mut LightcraftApp, ui: &egui::Ui, params: serde_json::Value) {
    if let Err(e) = app.run("applePhotos.show", params) {
        app.toast(ui.ctx(), e);
    }
}

/// The albums and folders inside `parent` (`None`: the top level), in the library's order.
fn album_rows(
    app: &mut LightcraftApp,
    ui: &mut egui::Ui,
    all: &[PhotosAlbumRow],
    parent: Option<&str>,
    depth: usize,
    shown: Option<&(String, usize)>,
) {
    if depth > MAX_DEPTH {
        return;
    }
    let indent = depth as f32 * super::rows::INDENT_STEP;
    for album in all.iter().filter(|a| a.parent.as_deref() == parent) {
        let title = if album.title.is_empty() { crate::i18n::tr("Untitled").to_string() } else { album.title.clone() };
        if album.folder {
            let open_id = egui::Id::unique(("applePhotos-folder-open", album.uuid.as_str()));
            let mut open = tree_open(ui, open_id, false);
            let draw_open = open;
            let resp =
                Row::new(&format!("applePhotos:folder:{}", album.uuid), Icon::Folder, &title).indent(indent).sense(Sense::click()).show(app, ui);
            let has_children = all.iter().any(|a| a.parent.as_deref() == Some(album.uuid.as_str()));
            let mut toggled = false;
            if has_children {
                toggled = tree_toggle(ui, &resp, indent, open_id, &mut open, format!("applePhotosToggle:{}", album.uuid), &title).clicked();
            }
            // a folder is no view of its own: its row folds it, like the library's album folders
            if resp.clicked() && !toggled {
                set_tree_open(ui, open_id, !open);
            }
            if draw_open {
                album_rows(app, ui, all, Some(&album.uuid), depth + 1, shown);
            }
        } else {
            let selected = shown.is_some_and(|(uuid, _)| *uuid == album.uuid);
            let count = shown.filter(|(uuid, _)| *uuid == album.uuid && app.photos_task.is_none()).map(|(_, n)| *n);
            let resp = Row::new(&format!("applePhotos:album:{}", album.uuid), Icon::Album, &title)
                .count(count)
                .selected(selected)
                .indent(indent)
                .show(app, ui);
            if resp.clicked() {
                run_show(app, ui, json!({"album": album.uuid}));
            }
        }
    }
}
