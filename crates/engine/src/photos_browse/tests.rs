use std::path::Path;

use lightcraft_catalog::Source;
use serde_json::json;

use super::*;
use crate::apple_photos_library::fixture::{self, ALBUM, ASSET_A, ASSET_B, JUNE_2021, Layout, Library, Table, V, t};

const ASSET_D: &str = "0A1B2C3D-0000-4000-8000-000000000004";
const ASSET_E: &str = "0A1B2C3D-0000-4000-8000-000000000005";

/// The fixture library (A: local favourite, B: hidden, C: in Recently Deleted), D, an ordinary
/// photo the library says nothing about, and E, only in iCloud.
fn library() -> Library {
    let mut tables = fixture::photos_tables(Layout::Photos6);
    let assets: &mut Table = tables.iter_mut().find(|t| t.name == "ZASSET").unwrap();
    assets.row(&[
        ("Z_PK", V::Int(4)),
        ("ZUUID", t(ASSET_D)),
        ("ZFILENAME", t(&format!("{ASSET_D}.jpg"))),
        ("ZDIRECTORY", t("0")),
        ("ZDATECREATED", V::Real(JUNE_2021)),
        ("ZUNIFORMTYPEIDENTIFIER", t("public.jpeg")),
        ("ZWIDTH", V::Int(600)),
        ("ZHEIGHT", V::Int(400)),
    ]);
    assets.row(&[("Z_PK", V::Int(5)), ("ZUUID", t(ASSET_E)), ("ZFILENAME", t(&format!("{ASSET_E}.jpg"))), ("ZDIRECTORY", t("0"))]);
    let resources: &mut Table = tables.iter_mut().find(|t| t.name == "ZINTERNALRESOURCE").unwrap();
    resources.row(&[
        ("Z_PK", V::Int(60)),
        ("ZASSET", V::Int(5)),
        ("ZRESOURCETYPE", V::Int(0)),
        ("ZDATASTORESUBTYPE", V::Int(1)),
        ("ZVERSION", V::Int(0)),
        ("ZLOCALAVAILABILITY", V::Int(0)),
    ]);
    Library::new(&tables)
}

fn show(s: &mut Session, library: &Library, params: Value) -> Value {
    let mut p = params;
    p["library"] = json!(library.root.to_string_lossy());
    s.execute("applePhotos.show", &p).unwrap()
}

fn uuids(s: &mut Session) -> Vec<String> {
    let ids = s.visible().to_vec();
    let mut out: Vec<String> = ids
        .iter()
        .map(|id| match &s.catalog.photo(*id).unwrap().source {
            Source::ApplePhotos { uuid, .. } => uuid.clone(),
            other => panic!("not a proxy: {other:?}"),
        })
        .collect();
    out.sort();
    out
}

fn proxy_of(s: &Session, uuid: &str) -> lightcraft_catalog::Photo {
    s.catalog.photos().find(|p| matches!(&p.source, Source::ApplePhotos { uuid: u, .. } if u == uuid)).unwrap().as_ref().clone()
}

/// Every file under `dir`, with its length and modification time.
fn listing(dir: &Path) -> Vec<(std::path::PathBuf, u64, Option<std::time::SystemTime>)> {
    let mut out = Vec::new();
    let mut todo = vec![dir.to_path_buf()];
    while let Some(d) = todo.pop() {
        for entry in std::fs::read_dir(&d).unwrap().flatten() {
            let meta = entry.metadata().unwrap();
            if meta.is_dir() { todo.push(entry.path()) } else { out.push((entry.path(), meta.len(), meta.modified().ok())) }
        }
    }
    out.sort();
    out
}

#[test]
fn photos_views_show_proxies_of_the_shown_assets() {
    let library = library();
    let before = listing(&library.root);
    let mut s = Session::new();

    let r = show(&mut s, &library, json!({}));
    assert_eq!((r["photos"].as_u64(), r["new"].as_u64()), (Some(3), Some(3)), "{r}");
    assert_eq!(s.source, LibrarySource::ApplePhotos);
    assert_eq!(uuids(&mut s), [ASSET_A, ASSET_D, ASSET_E], "hidden and recently deleted assets are left out");

    let a = proxy_of(&s, ASSET_A);
    assert!(a.local && !a.in_library(), "a proxy is not a library photo");
    assert_eq!((a.file_name.as_str(), a.format.as_str(), a.width, a.height), ("IMG_0001.HEIC", "HEIC", 4032, 3024));
    assert_eq!(a.captured.as_deref(), Some("2021-06-15T14:00:00"), "local time where it was taken");
    assert_eq!(a.file_size, 2_345_678);
    let original = library.root.join("originals").join("0").join(format!("{ASSET_A}.heic"));
    assert_eq!(
        a.source,
        Source::ApplePhotos {
            library: library.root.to_string_lossy().into(),
            uuid: ASSET_A.into(),
            original: Some(original.to_string_lossy().into())
        }
    );
    let d = proxy_of(&s, ASSET_D);
    assert_eq!((d.file_name.as_str(), d.format.as_str()), (format!("{ASSET_D}.jpg").as_str(), "JPG"), "no original name: Photos' own");
    let d_original = library.root.join("originals").join("0").join(format!("{ASSET_D}.jpg"));
    assert!(
        matches!(&d.source, Source::ApplePhotos { original: Some(o), .. } if *o == d_original.to_string_lossy()),
        "the library says nothing either way: the original is tried"
    );
    assert!(matches!(proxy_of(&s, ASSET_E).source, Source::ApplePhotos { original: None, .. }), "only in iCloud");

    // what Photos says about A: title, caption, keywords, location, and its favourite as 5 stars
    assert_eq!((a.meta.title.as_str(), a.meta.caption.as_str()), ("Harbour at noon", "Boats, gulls and a lighthouse."));
    assert_eq!(a.meta.keywords, ["boats", "sea"], "each once");
    assert_eq!(a.meta.gps, Some((48.5, -3.25)));
    assert_eq!(a.rating, 5, "a favourite");
    assert_eq!((d.rating, d.meta.gps, d.meta.title.as_str()), (0, None, ""), "D: nothing to carry over");

    let r = show(&mut s, &library, json!({"view": "favourites"}));
    assert_eq!(r["new"], 0, "the proxies are made once");
    assert_eq!(uuids(&mut s), [ASSET_A]);
    show(&mut s, &library, json!({"album": ALBUM}));
    assert_eq!(uuids(&mut s), [ASSET_A], "the album's hidden member is left out");
    let albums = s.execute("applePhotos.albums", &json!({})).unwrap();
    assert_eq!(albums["albums"].as_array().unwrap().len(), 3, "{albums}");
    let r = show(&mut s, &library, json!({}));
    assert_eq!((r["photos"].as_u64(), r["new"].as_u64(), r["updated"].as_u64()), (Some(3), Some(0), Some(0)));

    // the library's own views never show proxies
    s.execute("library.source", &json!({"kind": "all"})).unwrap();
    assert!(s.visible().is_empty());
    s.execute("library.source", &json!({"kind": "picks"})).unwrap();
    assert!(s.visible().is_empty());

    // and a proxy can't be added to the library in place
    let id = a.id;
    let r = s.execute("photo.addToLibrary", &json!({"ids": [id.0]})).unwrap();
    assert_eq!(r["added"], 0);
    assert!(!s.catalog.photo(id).unwrap().in_library());

    assert_eq!(listing(&library.root), before, "nothing was written in the Photos library");
}

#[test]
fn an_original_downloaded_or_offloaded_since_is_relinked() {
    let mut s = Session::new();
    let item = |original: Option<&str>| PhotosItem {
        uuid: ASSET_B.into(),
        file_name: "B.JPG".into(),
        format: "JPG".into(),
        kind: MediaKind::Image,
        width: 3,
        height: 2,
        captured: None,
        file_size: 1,
        original: original.map(str::to_string),
        ..Default::default()
    };
    let read = |items| PhotosRead { library: "/L.photoslibrary".into(), view: PhotosView::All, items, albums: Vec::new(), warnings: Vec::new() };
    s.show_photos(read(vec![item(None)])).unwrap();
    let r = s.show_photos(read(vec![item(Some("/L.photoslibrary/originals/B/B.jpg")), item(None)])).unwrap();
    assert_eq!((r["new"].as_u64(), r["updated"].as_u64(), r["photos"].as_u64()), (Some(0), Some(1), Some(1)), "a uuid listed twice is one proxy");
    assert!(matches!(&proxy_of(&s, ASSET_B).source, Source::ApplePhotos { original: Some(o), .. } if o.ends_with("B.jpg")));
    // another library's proxy of the same uuid is another photo, in another view
    let other = PhotosRead { library: "/M.photoslibrary".into(), ..read(vec![item(None)]) };
    assert_eq!(s.show_photos(other).unwrap()["new"], 1);
    assert_eq!(s.visible().len(), 1);
}

/// Photos wins: what it says about an asset replaces what its proxy had, every time the library
/// is read.
#[test]
fn a_proxy_mirrors_what_photos_says() {
    let mut s = Session::new();
    let item = |title: &str, favourite: bool| PhotosItem {
        uuid: ASSET_B.into(),
        file_name: "B.JPG".into(),
        format: "JPG".into(),
        width: 3,
        height: 2,
        title: title.into(),
        keywords: vec!["sea".into()],
        gps: Some((1.0, 2.0)),
        favourite,
        ..Default::default()
    };
    let read = |items| PhotosRead { library: "/L.photoslibrary".into(), view: PhotosView::All, items, albums: Vec::new(), warnings: Vec::new() };
    s.show_photos(read(vec![item("Harbour", true)])).unwrap();
    let id = proxy_of(&s, ASSET_B).id;
    // changed in LightCraft…
    s.execute("photo.setMeta", &json!({"ids": [id.0], "title": "Mine", "keywords": ["mine"]})).unwrap();
    s.execute("photo.rate", &json!({"ids": [id.0], "rating": 3})).unwrap();
    // …then read again: Photos wins
    let r = s.show_photos(read(vec![item("Harbour, renamed", true)])).unwrap();
    assert_eq!(r["updated"], 1, "{r}");
    let p = proxy_of(&s, ASSET_B);
    assert_eq!((p.meta.title.as_str(), p.meta.keywords.as_slice(), p.rating), ("Harbour, renamed", ["sea".to_string()].as_slice(), 5));
    // no longer a favourite: its 5 stars go, other ratings stay
    s.show_photos(read(vec![item("Harbour, renamed", false)])).unwrap();
    assert_eq!(proxy_of(&s, ASSET_B).rating, 0);
    s.execute("photo.rate", &json!({"ids": [id.0], "rating": 2})).unwrap();
    let r = s.show_photos(read(vec![item("Harbour, renamed", false)])).unwrap();
    assert_eq!((r["updated"].as_u64(), proxy_of(&s, ASSET_B).rating), (Some(0), 2), "nothing changed in Photos");
}

#[test]
fn bad_requests_are_errors() {
    let library = library();
    let mut s = Session::new();
    let at = library.root.to_string_lossy().to_string();
    assert!(s.execute("applePhotos.show", &json!({"library": at, "view": "sideways"})).is_err());
    let e = s.execute("applePhotos.show", &json!({"library": at, "album": "nope"})).unwrap_err();
    assert!(e.to_string().contains("no album"), "{e}");
    let e = s.execute("applePhotos.show", &json!({"library": library.dir().join("missing").to_string_lossy()})).unwrap_err();
    assert!(e.to_string().contains("not a Photos library"), "{e}");
    // nothing browsed yet: the view shows nothing
    s.source = LibrarySource::ApplePhotos;
    assert!(s.visible().is_empty());
    assert_eq!(s.execute("applePhotos.albums", &json!({})).unwrap()["albums"], json!([]));
}

/// Opt-in, against a real library: `LIGHTCRAFT_PHOTOS_LIBRARY=<path to .photoslibrary>`. Prints
/// counts and times only.
#[test]
fn real_library_shows_when_named() {
    let Some(path) = std::env::var_os("LIGHTCRAFT_PHOTOS_LIBRARY") else {
        lightcraft_testkit::skip!(Input, "set LIGHTCRAFT_PHOTOS_LIBRARY=<path to a .photoslibrary>");
        return;
    };
    let mut s = Session::new();
    for view in [json!({}), json!({}), json!({"view": "favourites"})] {
        let mut p = view.clone();
        p["library"] = json!(path.to_string_lossy());
        let started = std::time::Instant::now();
        let r = s.execute("applePhotos.show", &p).unwrap();
        eprintln!("{view}: {} photos, {} new, {} updated in {:?}", r["photos"], r["new"], r["updated"], started.elapsed());
    }
    eprintln!("{} albums", s.photos_browse.as_ref().unwrap().albums.len());
}

/// Photos can keep a JPEG of a photo imported as `IMG_1234.HEIC`: the proxy is named after the
/// file Photos keeps, so its format, and the copy made to edit it, say what the file is.
#[test]
fn a_proxy_is_named_with_the_extension_of_the_file_photos_keeps() {
    assert_eq!(proxy_file_name(Some("IMG_1234.HEIC"), Some("U.jpeg"), "U"), "IMG_1234.jpeg");
    assert_eq!(proxy_file_name(Some("IMG_1234.HEIC"), Some("U.heic"), "U"), "IMG_1234.HEIC", "same type: the imported name");
    assert_eq!(proxy_file_name(Some("Scan"), Some("U.png"), "U"), "Scan.png");
    assert_eq!(proxy_file_name(Some("IMG_1234.HEIC"), None, "U"), "IMG_1234.HEIC");
    assert_eq!(proxy_file_name(None, Some("U.jpeg"), "U"), "U.jpeg");
    assert_eq!(proxy_file_name(None, None, "U"), "U");

    // in a library: F was imported as IMG_0006.HEIC, and Photos keeps a JPEG of it
    const ASSET_F: &str = "0A1B2C3D-0000-4000-8000-000000000006";
    let mut tables = fixture::photos_tables(Layout::Photos6);
    let assets: &mut Table = tables.iter_mut().find(|t| t.name == "ZASSET").unwrap();
    assets.row(&[
        ("Z_PK", V::Int(6)),
        ("ZUUID", t(ASSET_F)),
        ("ZFILENAME", t(&format!("{ASSET_F}.jpeg"))),
        ("ZDIRECTORY", t("0")),
        ("ZUNIFORMTYPEIDENTIFIER", t("public.jpeg")),
        ("ZADDITIONALATTRIBUTES", V::Int(16)),
    ]);
    let attributes: &mut Table = tables.iter_mut().find(|t| t.name == "ZADDITIONALASSETATTRIBUTES").unwrap();
    attributes.row(&[("Z_PK", V::Int(16)), ("ZASSET", V::Int(6)), ("ZORIGINALFILENAME", t("IMG_0006.HEIC"))]);
    let library = Library::new(&tables);
    let mut s = Session::new();
    // a proxy made before names took the kept file's extension
    let stale = PhotosItem {
        uuid: ASSET_F.into(),
        file_name: "IMG_0006.HEIC".into(),
        format: "HEIC".into(),
        kind: MediaKind::Image,
        width: 0,
        height: 0,
        captured: None,
        file_size: 0,
        original: Some(library.root.join("originals").join("0").join(format!("{ASSET_F}.jpeg")).to_string_lossy().into()),
        ..Default::default()
    };
    let at = library.root.to_string_lossy().into_owned();
    s.show_photos(PhotosRead { library: at, view: PhotosView::All, items: vec![stale], albums: Vec::new(), warnings: Vec::new() }).unwrap();
    let r = show(&mut s, &library, json!({}));
    assert_eq!((r["new"].as_u64(), r["updated"].as_u64()), (Some(1), Some(1)), "A is new, F renamed: {r}");
    let f = proxy_of(&s, ASSET_F);
    assert_eq!((f.file_name.as_str(), f.format.as_str()), ("IMG_0006.jpeg", "JPEG"));
    let a = proxy_of(&s, ASSET_A);
    assert_eq!((a.file_name.as_str(), a.format.as_str()), ("IMG_0001.HEIC", "HEIC"), "a HEIC kept as HEIC keeps its name");
}
