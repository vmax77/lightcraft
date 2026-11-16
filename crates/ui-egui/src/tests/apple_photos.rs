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
        ..Default::default()
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
    // (neither original is on this Mac: each cell says the photo is in iCloud)
    let ids = h.app.session.visible().to_vec();
    for id in ids {
        assert!(has(&h, &format!("badge:icloud:{}", id.0)), "no iCloud badge on {id:?}");
    }
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

#[test]
fn the_first_edit_of_a_photos_item_copies_it_into_the_library() {
    let dir = std::env::temp_dir().join(format!("lc-ui-photos-edit-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let package = dir.join("Test.photoslibrary");
    let original = package.join("originals").join("0").join("U1.jpg");
    std::fs::create_dir_all(original.parent().unwrap()).unwrap();
    std::fs::write(&original, b"not really a jpeg, but copied byte for byte").unwrap();
    let mut session = lightcraft_engine::Session::new().with_fs();
    session.open_library(dir.join("lib"), false).unwrap();
    let services = Services { png: None, local_places: Some(Vec::new()), ..Default::default() };
    let mut h = Headless::new(LightcraftApp::new(session, services), [1200.0, 1000.0], 1.0);
    let item = |uuid: &str, original: Option<String>| PhotosItem {
        uuid: uuid.into(),
        file_name: format!("{uuid}.JPG"),
        format: "JPG".into(),
        kind: MediaKind::Image,
        width: 30,
        height: 20,
        captured: None,
        file_size: 10,
        original,
        ..Default::default()
    };
    let library = package.to_string_lossy().into_owned();
    let read = PhotosRead {
        library: library.clone(),
        view: PhotosView::All,
        items: vec![item("U1", Some(original.to_string_lossy().into())), item("U2", None)],
        albums: Vec::new(),
        warnings: Vec::new(),
    };
    h.app.session.show_photos(read).unwrap();
    let id_of = |h: &Headless, uuid: &str| {
        h.app.session.catalog.photos().find(|p| matches!(&p.source, lightcraft_catalog::Source::ApplePhotos { uuid: u, .. } if u == uuid)).unwrap().id
    };
    let (u1, u2) = (id_of(&h, "U1"), id_of(&h, "U2"));

    // browsing a Photos item copies nothing
    h.app.session.execute("library.select", &json!({"ids": [u1.0]})).unwrap();
    h.request("ui.set", json!({"view": "detail"}), T);
    h.settle(SETTLE);
    assert!(!h.app.photos_edit.busy());
    // nor does looking at it in the Edit tools
    h.app.ui.right = crate::state::RightPanel::Edit;
    h.settle(SETTLE);
    assert!(!h.app.photos_edit.busy());
    assert!(matches!(h.app.session.catalog.photo(u1).unwrap().source, lightcraft_catalog::Source::ApplePhotos { .. }), "viewed, not copied");
    // its first edit: copied in the background, with a row in the activity stack
    h.app.session.execute("develop.set", &json!({"control": "light.exposure", "value": 1.0})).unwrap();
    h.step();
    let list = h.app.session.activity.list();
    assert!(!h.app.photos_edit.busy() || list.iter().any(|t| t.kind == "applePhotosCopy"), "{list:?}");
    h.settle(SETTLE);
    let p = h.app.session.catalog.photo(u1).unwrap().clone();
    let lightcraft_catalog::Source::File { path } = &p.source else { panic!("not copied: {:?}", p.source) };
    assert_eq!(std::fs::read(path).unwrap(), std::fs::read(&original).unwrap());
    assert!(p.in_library() && p.from_photos.as_ref().is_some_and(|l| l.uuid == "U1" && l.library == library));
    assert!(p.is_edited(), "the edit stays with the copy");
    assert!(h.app.session.activity.list().iter().all(|t| t.kind != "applePhotosCopy"), "its row went");

    // one only in iCloud, once edited, says so once, and stays as it is
    h.app.session.execute("library.select", &json!({"ids": [u2.0]})).unwrap();
    h.settle(SETTLE);
    assert!(h.app.ui.toast.is_none(), "viewing it asks for nothing");
    h.app.session.execute("develop.set", &json!({"control": "light.exposure", "value": 1.0})).unwrap();
    h.settle(SETTLE);
    let toast = h.app.ui.toast.as_ref().map(|t| t.0.clone()).unwrap_or_default();
    assert!(toast.contains("iCloud"), "{toast:?}");
    assert!(!h.app.session.catalog.photo(u2).unwrap().in_library());
    let _ = std::fs::remove_dir_all(&dir);
}

/// Saving an edit back to Photos runs on a worker under a row in the activity stack, and says
/// how it went (here, off macOS or without Photos access, why it couldn't).
#[test]
fn saving_an_edit_to_photos_runs_in_the_background_and_says_how_it_went() {
    let dir = std::env::temp_dir().join(format!("lc-ui-photos-save-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let package = dir.join("Test.photoslibrary");
    let original = package.join("originals").join("0").join("U1.jpg");
    std::fs::create_dir_all(original.parent().unwrap()).unwrap();
    let pixels = lightcraft_raster::Rgba8::from_bytes(4, 3, &[128u8; 4 * 3 * 4]).unwrap();
    let jpeg = lightcraft_engine::export::encode_image(&pixels, &lightcraft_engine::export::ExportOptions::default()).unwrap();
    std::fs::write(&original, &jpeg).unwrap();
    let mut session = lightcraft_engine::Session::new().with_fs();
    session.open_library(dir.join("lib"), false).unwrap();
    let services = Services { png: None, local_places: Some(Vec::new()), ..Default::default() };
    let mut h = Headless::new(LightcraftApp::new(session, services), [1200.0, 1000.0], 1.0);
    let read = PhotosRead {
        library: package.to_string_lossy().into_owned(),
        view: PhotosView::All,
        items: vec![PhotosItem {
            uuid: "U1".into(),
            file_name: "U1.JPG".into(),
            format: "JPG".into(),
            kind: MediaKind::Image,
            width: 4,
            height: 3,
            captured: None,
            file_size: jpeg.len() as u64,
            original: Some(original.to_string_lossy().into()),
            ..Default::default()
        }],
        albums: Vec::new(),
        warnings: Vec::new(),
    };
    h.app.session.show_photos(read).unwrap();
    let u1 = h.app.session.catalog.photos().next().unwrap().id;

    // nothing to save before it has been copied and edited
    assert!(h.app.run("applePhotos.saveEdit", json!({"ids": [u1.0]})).unwrap_err().contains("not copied from Apple Photos"));
    assert!(!h.app.photos_save.busy());

    h.app.session.execute("library.select", &json!({"ids": [u1.0]})).unwrap();
    h.app.session.execute("develop.set", &json!({"control": "light.exposure", "value": 1.0})).unwrap();
    let mut no_fetch = |_: &str, _: &mut dyn std::io::Write| -> Result<u64, String> { Err("on this Mac".into()) };
    h.app.session.edit_photos_items(&[u1], &mut no_fetch).unwrap();

    let r = h.app.run("applePhotos.saveEdit", json!({"ids": [u1.0]})).unwrap();
    assert_eq!(r["queued"], json!([u1.0]), "{r}");
    let list = h.app.session.activity.list();
    assert!(list.iter().any(|t| t.kind == "applePhotosSave"), "{list:?}");
    h.settle(SETTLE);
    assert!(!h.app.photos_save.busy());
    assert!(h.app.session.activity.list().iter().all(|t| t.kind != "applePhotosSave"), "its row went");
    let toast = h.app.ui.toast.as_ref().map(|t| t.0.clone()).unwrap_or_default();
    let why = if cfg!(target_os = "macos") { "NSPhotoLibraryUsageDescription" } else { "only available on macOS" };
    assert!(toast.contains(why), "{toast}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Reading edits back from Photos runs on a worker under a row in the activity stack, and says how
/// it went (here, off macOS or without Photos access, why it couldn't); nothing changes then.
#[test]
fn reading_edits_from_photos_runs_in_the_background_and_says_how_it_went() {
    let mut h = Headless::new(
        LightcraftApp::new(lightcraft_engine::Session::new(), Services { png: None, local_places: Some(Vec::new()), ..Default::default() }),
        [1200.0, 1000.0],
        1.0,
    );
    let read = PhotosRead {
        library: "/T.photoslibrary".into(),
        view: PhotosView::All,
        items: vec![PhotosItem { uuid: "U1".into(), file_name: "U1.JPG".into(), format: "JPG".into(), width: 4, height: 3, ..Default::default() }],
        albums: Vec::new(),
        warnings: Vec::new(),
    };
    h.app.session.show_photos(read).unwrap();
    let u1 = h.app.session.catalog.photos().next().unwrap().id;
    let r = h.app.run("applePhotos.restoreEdit", json!({"ids": [u1.0]})).unwrap();
    assert_eq!(r["asked"], json!([u1.0]), "{r}");
    assert!(h.app.run("applePhotos.restoreEdit", json!({"ids": [u1.0]})).is_err(), "one read at a time");
    let list = h.app.session.activity.list();
    assert!(list.iter().any(|t| t.kind == "applePhotosRestore"), "{list:?}");
    h.settle(SETTLE);
    assert!(h.app.photos_restore.is_none());
    assert!(h.app.session.activity.list().iter().all(|t| t.kind != "applePhotosRestore"), "its row went");
    let toast = h.app.ui.toast.as_ref().map(|t| t.0.clone()).unwrap_or_default();
    let why = if cfg!(target_os = "macos") { "NSPhotoLibraryUsageDescription" } else { "only available on macOS" };
    assert!(toast.contains(why), "{toast}");
    assert!(!h.app.session.catalog.photo(u1).unwrap().is_edited());
}

/// A photo copied from Photos is checked against Photos once a session when it is open in the Edit
/// tools, on a worker under a row in the activity stack, saying nothing when Photos can't be asked
/// (here, off macOS or without Photos access); Sync with Apple Photos asks again and says why.
#[test]
fn a_photo_from_photos_is_checked_once_in_the_edit_tools_and_sync_says_how_it_went() {
    let dir = std::env::temp_dir().join(format!("lc-ui-photos-sync-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let original = dir.join("Test.photoslibrary").join("originals").join("0").join("U1.jpg");
    std::fs::create_dir_all(original.parent().unwrap()).unwrap();
    let pixels = lightcraft_raster::Rgba8::from_bytes(4, 3, &[128u8; 4 * 3 * 4]).unwrap();
    std::fs::write(&original, lightcraft_engine::export::encode_image(&pixels, &lightcraft_engine::export::ExportOptions::default()).unwrap())
        .unwrap();
    let mut session = lightcraft_engine::Session::new().with_fs();
    session.open_library(dir.join("lib"), false).unwrap();
    let services = Services { png: None, local_places: Some(Vec::new()), ..Default::default() };
    let mut h = Headless::new(LightcraftApp::new(session, services), [1200.0, 1000.0], 1.0);
    let item = PhotosItem {
        uuid: "U1".into(),
        file_name: "U1.JPG".into(),
        format: "JPG".into(),
        width: 4,
        height: 3,
        original: Some(original.to_string_lossy().into()),
        ..Default::default()
    };
    let library = dir.join("Test.photoslibrary").to_string_lossy().into_owned();
    h.app.session.show_photos(PhotosRead { library, view: PhotosView::All, items: vec![item], albums: Vec::new(), warnings: Vec::new() }).unwrap();
    let u1 = h.app.session.catalog.photos().next().unwrap().id;
    let mut no_fetch = |_: &str, _: &mut dyn std::io::Write| -> Result<u64, String> { Err("on this Mac".into()) };
    h.app.session.edit_photos_items(&[u1], &mut no_fetch).unwrap();
    h.app.session.execute("library.select", &json!({"ids": [u1.0]})).unwrap();

    // opened in the Edit tools: checked by itself, once
    h.request("ui.set", json!({"view": "detail"}), T);
    h.app.ui.right = crate::state::RightPanel::Edit;
    h.step();
    let list = h.app.session.activity.list();
    assert!(!h.app.photos_sync.busy() || list.iter().any(|t| t.kind == "applePhotosSync"), "{list:?}");
    h.settle(SETTLE);
    assert!(!h.app.photos_sync.busy());
    assert!(h.app.session.activity.list().iter().all(|t| t.kind != "applePhotosSync"), "its row went");
    assert!(h.app.ui.toast.is_none(), "a check made by itself that can't ask Photos says nothing");
    h.step();
    assert!(!h.app.photos_sync.busy(), "not checked again this session");

    // asked for: says why it couldn't
    let r = h.app.run("applePhotos.sync", json!({"ids": [u1.0]})).unwrap();
    assert_eq!(r["queued"], json!([u1.0]), "{r}");
    h.settle(SETTLE);
    let toast = h.app.ui.toast.as_ref().map(|t| t.0.clone()).unwrap_or_default();
    let why = if cfg!(target_os = "macos") { "NSPhotoLibraryUsageDescription" } else { "only available on macOS" };
    assert!(toast.contains(why), "{toast}");
    let _ = std::fs::remove_dir_all(&dir);
}
