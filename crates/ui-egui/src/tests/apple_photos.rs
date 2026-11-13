//! Issue #1622: the Apple Photos section. A library that was read lists All Photos, Favourites and
//! its albums inside their folders; a row reads its view on a worker under an activity row, and
//! a read that fails says why instead of switching the view.

use std::time::Duration;

use lightcraft_catalog::MediaKind;
use lightcraft_engine::LibrarySource;
use lightcraft_engine::photos_browse::{PhotosAlbumRow, PhotosItem, PhotosRead, PhotosView};
use serde_json::json;

use crate::headless::Headless;
use crate::{LightcraftApp, Services};

const T: Duration = Duration::from_secs(60);
const SETTLE: Duration = Duration::from_secs(10);

fn demo() -> Headless {
    let services = Services { png: None, local_places: Some(Vec::new()), ..Default::default() };
    let app = LightcraftApp::new(lightcraft_engine::Session::with_demo(), services);
    let mut h = Headless::new(app, [1200.0, 1400.0], 1.0);
    let r = h.request("ui.set", json!({"view": "photoGrid", "leftPanel": true, "sidebar": ["applePhotos"]}), T);
    assert_eq!(r["ok"], true, "{r}");
    h.settle(SETTLE);
    h
}

fn has(h: &Headless, id: &str) -> bool {
    h.app.widgets.iter().any(|(w, _)| w == id)
}

fn click(h: &mut Headless, id: &str) {
    h.step();
    let r = h.request("ui.clickWidget", json!({"id": id}), T);
    assert_eq!(r["ok"], true, "{id}: {r}");
    for _ in 0..4 {
        h.step();
    }
}

fn album(uuid: &str, title: &str, folder: bool, parent: Option<&str>) -> PhotosAlbumRow {
    PhotosAlbumRow { uuid: uuid.into(), title: title.into(), folder, parent: parent.map(str::to_string) }
}

/// A library (that is nowhere on disk) as if it had been read: two photos, a folder holding an album.
fn shown(h: &mut Headless) {
    let item = |uuid: &str| PhotosItem {
        uuid: uuid.into(),
        file_name: format!("{uuid}.JPG"),
        format: "JPG".into(),
        kind: MediaKind::Image,
        width: 30,
        height: 20,
        captured: Some("2021-06-15T12:00:00".into()),
        file_size: 10,
        original: None,
    };
    let read = PhotosRead {
        library: "/nowhere/Test.photoslibrary".into(),
        view: PhotosView::All,
        items: vec![item("A1"), item("B2")],
        albums: vec![album("F", "Trips", true, None), album("H", "Harbour", false, Some("F")), album("T", "Top", false, None)],
        warnings: Vec::new(),
    };
    h.app.session.show_photos(read).unwrap();
    h.settle(SETTLE);
}

#[test]
fn the_section_lists_views_and_albums_inside_their_folders() {
    if !crate::sidebar::SidebarSection::ApplePhotos.available() {
        return; // macOS only
    }
    let mut h = demo();
    assert!(has(&h, "sidebarSection:applePhotos"));
    assert!(has(&h, "source:applePhotos:all") && has(&h, "source:applePhotos:favourites"));
    shown(&mut h);
    assert_eq!(h.app.session.source, LibrarySource::ApplePhotos);
    assert_eq!(h.app.session.visible().len(), 2, "the proxies are in the grid");
    assert!(has(&h, "source:applePhotos:album:T") && has(&h, "source:applePhotos:folder:F"));
    assert!(!has(&h, "source:applePhotos:album:H"), "folders start folded");
    click(&mut h, "source:applePhotos:folder:F");
    assert!(has(&h, "source:applePhotos:album:H"), "the folder's row opens it");
}

#[test]
fn a_view_is_read_in_the_background_and_a_failed_read_says_why() {
    if !crate::sidebar::SidebarSection::ApplePhotos.available() {
        return; // macOS only
    }
    let mut h = demo();
    shown(&mut h);
    click(&mut h, "source:applePhotos:album:T");
    // the read starts on a worker with a row in the activity stack, or has already ended
    let started = h.app.photos_task.is_some() || h.app.ui.toast.is_some();
    assert!(started, "clicking an album reads it");
    let list = h.app.session.activity.list();
    assert!(h.app.photos_task.is_none() || list.iter().any(|t| t.kind == "applePhotos"), "{list:?}");
    let deadline = std::time::Instant::now() + T;
    while h.app.photos_task.is_some() && std::time::Instant::now() < deadline {
        h.step();
    }
    assert!(h.app.photos_task.is_none(), "the read ended");
    let toast = h.app.ui.toast.as_ref().map(|t| t.0.clone()).unwrap_or_default();
    assert!(toast.contains("not a Photos library"), "{toast:?}");
    assert_eq!(h.app.session.photos_browse.as_ref().map(|b| b.view.clone()), Some(PhotosView::All), "the view shown stays");
    assert!(h.app.session.activity.list().iter().all(|t| t.kind != "applePhotos"), "its activity row went");
}
