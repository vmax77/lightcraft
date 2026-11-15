use std::path::Path;

use lightcraft_catalog::{PhotoId, PhotosLink, Source};
use serde_json::{Value, json};

use crate::Session;
use crate::apple_photos_library::fixture::{self, ASSET_A, JUNE_2021, Layout, Library, Table, V, t};
use crate::tests::xmp::temp_dir;

pub(crate) const ASSET_D: &str = "0A1B2C3D-0000-4000-8000-000000000004";
const ASSET_E: &str = "0A1B2C3D-0000-4000-8000-000000000005";

/// A Photos library (fixture) with A (on this Mac, no file), D (a JPEG original written at
/// `originals/0/<D>.jpg`) and E (only in iCloud).
pub(crate) fn library() -> Library {
    let mut tables = fixture::photos_tables(Layout::Photos6);
    let assets: &mut Table = tables.iter_mut().find(|t| t.name == "ZASSET").unwrap();
    assets.row(&[
        ("Z_PK", V::Int(4)),
        ("ZUUID", t(ASSET_D)),
        ("ZFILENAME", t(&format!("{ASSET_D}.jpg"))),
        ("ZDIRECTORY", t("0")),
        ("ZDATECREATED", V::Real(JUNE_2021)),
        ("ZUNIFORMTYPEIDENTIFIER", t("public.jpeg")),
        ("ZWIDTH", V::Int(40)),
        ("ZHEIGHT", V::Int(24)),
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
    let library = Library::new(&tables);
    let dir = library.root.join("originals").join("0");
    std::fs::create_dir_all(&dir).unwrap();
    crate::tests::xmp::write_png(&dir.join(format!("{ASSET_D}.jpg")), 9);
    library
}

/// A session with a library on disk in `tag`'s temp folder, browsing `library`'s All Photos.
pub(crate) fn session(tag: &str, library: &Library) -> (Session, std::path::PathBuf) {
    let lib = temp_dir(tag);
    let mut s = Session::new().with_fs();
    s.open_library(&lib, false).unwrap();
    s.execute("applePhotos.show", &json!({"library": library.root.to_string_lossy()})).unwrap();
    (s, lib)
}

pub(crate) fn proxy(s: &Session, uuid: &str) -> PhotoId {
    s.catalog.photos().find(|p| matches!(&p.source, Source::ApplePhotos { uuid: u, .. } if u == uuid)).unwrap().id
}

pub(crate) fn no_fetch(_: &str, _: &mut dyn std::io::Write) -> Result<u64, String> {
    panic!("nothing is fetched for an original on this Mac")
}

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
fn editing_a_photos_item_copies_its_original_into_the_library_and_keeps_what_was_done() {
    let library = library();
    let before = listing(&library.root);
    let (mut s, lib) = session("photos-edit", &library);
    let d = proxy(&s, ASSET_D);
    // done to the proxy before the copy: it all stays with the photo
    s.execute("photo.rate", &json!({"ids": [d.0], "rating": 4})).unwrap();
    s.execute("library.select", &json!({"ids": [d.0]})).unwrap();
    s.execute("develop.set", &json!({"control": "light.exposure", "value": 1.0})).unwrap();

    let r = s.execute("applePhotos.edit", &json!({"ids": [d.0]})).unwrap();
    assert_eq!(r["copied"].as_array().map(Vec::len), Some(1), "{r}");
    let p = s.catalog.photo(d).unwrap().clone();
    let Source::File { path } = &p.source else { panic!("not a file: {:?}", p.source) };
    let original = library.root.join("originals").join("0").join(format!("{ASSET_D}.jpg"));
    assert_eq!(Path::new(path).parent(), Some(lib.join("Originals").join("From Apple Photos").as_path()));
    assert_eq!(std::fs::read(path).unwrap(), std::fs::read(&original).unwrap(), "a copy of the original");
    assert!(p.in_library(), "a library photo now");
    assert_eq!(p.from_photos, Some(PhotosLink { library: library.root.to_string_lossy().into(), uuid: ASSET_D.into() }));
    assert_eq!(p.rating, 4);
    assert!(p.is_edited(), "the edit made on the proxy stays");
    assert_eq!(p.file_name, Path::new(path).file_name().unwrap().to_string_lossy());

    // it is in the library's own views now, and in the Photos view in the asset's place
    s.execute("library.source", &json!({"kind": "all"})).unwrap();
    assert_eq!(s.visible().to_vec(), [d]);
    let r = s.execute("applePhotos.show", &json!({"library": library.root.to_string_lossy()})).unwrap();
    assert_eq!(r["new"], 0, "no new proxy of the copied asset: {r}");
    assert!(s.visible().contains(&d));
    assert_eq!(s.visible().len(), 3);

    // undo takes back the edit, not the copy
    s.execute("edit.undo", &json!({})).unwrap();
    assert!(matches!(s.catalog.photo(d).unwrap().source, Source::File { .. }));
    // and editing it again copies nothing
    let r = s.execute("applePhotos.edit", &json!({"ids": [d.0]})).unwrap();
    assert_eq!(r["copied"], json!([]));

    // sidecars and all go beside the copy; the Photos library is as it was
    s.execute("library.xmpPreferences", &json!({"autoWrite": true})).unwrap();
    s.save_sidecar(d).unwrap();
    assert_eq!(listing(&library.root), before, "nothing was written in the Photos library");
}

#[test]
fn a_copy_that_cant_be_made_leaves_the_proxy_as_it_was() {
    let library = library();
    let (mut s, lib) = session("photos-edit-fail", &library);
    let copies = lib.join("Originals").join("From Apple Photos");
    // only in iCloud, and PhotoKit can't be asked here (a test binary has no Photos usage
    // description; off macOS there is none)
    let e = proxy(&s, ASSET_E);
    let err = s.execute("applePhotos.edit", &json!({"ids": [e.0]})).unwrap_err().to_string();
    assert!(err.contains("iCloud"), "{err}");
    // the library says it is on this Mac, but it isn't there
    let a = proxy(&s, ASSET_A);
    let err = s.execute("applePhotos.edit", &json!({"ids": [a.0]})).unwrap_err().to_string();
    assert!(err.contains("the original is untouched"), "{err}");
    for id in [a, e] {
        assert!(matches!(s.catalog.photo(id).unwrap().source, Source::ApplePhotos { .. }));
        assert!(!s.catalog.photo(id).unwrap().in_library());
    }
    assert!(!copies.exists() || listing(&copies).is_empty(), "no partial copy is left");

    // a record that changed while its copy was made is not taken over
    let d = proxy(&s, ASSET_D);
    let plan = s.photos_copies(&[d]).unwrap();
    let made = plan[0].make(&mut no_fetch).unwrap();
    s.adopt_photos_copy(&plan[0], &made).unwrap();
    assert!(s.adopt_photos_copy(&plan[0], &made).is_err(), "already a copy");
}

/// A proxy still named `.HEIC` (made before names took the extension of the file Photos keeps)
/// whose original is a JPEG: the copy is a `.jpg`, so it opens as what it is.
#[test]
fn a_copy_is_named_with_the_extension_of_the_original() {
    let library = library();
    let (mut s, _lib) = session("photos-edit-ext", &library);
    let d = proxy(&s, ASSET_D);
    let Source::ApplePhotos { library: at, uuid, original } = s.catalog.photo(d).unwrap().source.clone() else { panic!("not a proxy") };
    let source = Source::ApplePhotos { library: at, uuid, original };
    s.apply_system(lightcraft_catalog::Op::Relink { id: d, file_name: "IMG_0004.HEIC".into(), source, format: Some("HEIC".into()) }).unwrap();
    let plan = s.photos_copies(&[d]).unwrap();
    assert_eq!(plan[0].file_name, "IMG_0004.jpg");
    let r = s.execute("applePhotos.edit", &json!({"ids": [d.0]})).unwrap();
    assert_eq!(r["copied"].as_array().map(Vec::len), Some(1), "{r}");
    let Source::File { path } = &s.catalog.photo(d).unwrap().source else { panic!("not copied") };
    assert_eq!(Path::new(path).file_name().unwrap(), "IMG_0004.jpg");
}

#[test]
fn editing_needs_a_library_folder_for_the_copy() {
    let library = library();
    let mut s = Session::new();
    s.execute("applePhotos.show", &json!({"library": library.root.to_string_lossy()})).unwrap();
    let d = proxy(&s, ASSET_D);
    let err: Value = json!(s.execute("applePhotos.edit", &json!({"ids": [d.0]})).unwrap_err().to_string());
    assert!(err.as_str().unwrap().contains("open a library"), "{err}");
}

#[test]
fn an_original_only_in_icloud_is_fetched_into_the_copy_and_checked() {
    let library = library();
    let before = listing(&library.root);
    let (mut s, lib) = session("photos-edit-icloud", &library);
    let copies = lib.join("Originals").join("From Apple Photos");
    let e = proxy(&s, ASSET_E);
    let plan = s.photos_copies(&[e]).unwrap();
    assert!(plan[0].in_icloud());
    let bytes = b"the original, as iCloud sends it".to_vec();

    // a fetch that fails, sends nothing, or sends other than Photos' recorded size leaves no file
    let mut failing = |_: &str, out: &mut dyn std::io::Write| -> Result<u64, String> {
        out.write_all(b"part").unwrap();
        Err("offline".into())
    };
    let err = plan[0].make(&mut failing).unwrap_err();
    assert!(err.contains("iCloud") && err.contains("offline"), "{err}");
    let mut empty = |_: &str, _: &mut dyn std::io::Write| -> Result<u64, String> { Ok(0) };
    assert!(plan[0].make(&mut empty).is_err());
    let mut sized = plan[0].clone();
    sized.size = bytes.len() as u64 + 1;
    let mut fetch = |uuid: &str, out: &mut dyn std::io::Write| -> Result<u64, String> {
        assert_eq!(uuid, ASSET_E);
        out.write_all(&bytes).unwrap();
        Ok(bytes.len() as u64)
    };
    let err = sized.make(&mut fetch).unwrap_err();
    assert!(err.contains("bytes"), "{err}");
    assert!(listing(&copies).is_empty(), "no partial copy is left");

    // the bytes that came, under the item's name, and the item becomes that copy
    let made = plan[0].make(&mut fetch).unwrap();
    assert_eq!(std::fs::read(&made).unwrap(), bytes);
    s.adopt_photos_copy(&plan[0], &made).unwrap();
    let p = s.catalog.photo(e).unwrap();
    assert!(p.in_library() && matches!(&p.source, Source::File { path } if Path::new(path) == made));
    assert_eq!(listing(&library.root), before, "nothing was written in the Photos library");
}
