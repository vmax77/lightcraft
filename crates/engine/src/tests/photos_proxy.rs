//! Apple Photos proxies (#1622): a photo whose source is an asset in a Photos library. Nothing is
//! ever written for one: no sidecar (also with auto-write on), no rename, no external-editor copy
//! or merge next to its original. The `.photoslibrary` package is left exactly as it was.

use std::path::{Path, PathBuf};

use lightcraft_catalog::{Op, Photo, PhotoId, Source};
use serde_json::json;

use super::xmp::{temp_dir, write_png};
use crate::Session;

/// Every file under `dir` with its length and modification time, sorted.
fn listing(dir: &Path) -> Vec<(PathBuf, u64, Option<std::time::SystemTime>)> {
    let mut out = Vec::new();
    let mut todo = vec![dir.to_path_buf()];
    while let Some(d) = todo.pop() {
        for entry in std::fs::read_dir(&d).unwrap().flatten() {
            let meta = entry.metadata().unwrap();
            if meta.is_dir() {
                todo.push(entry.path());
            } else {
                out.push((entry.path(), meta.len(), meta.modified().ok()));
            }
        }
    }
    out.sort();
    out
}

fn source(package: &Path, uuid: &str) -> Source {
    let original = package.join("originals").join("0").join(format!("{uuid}.png"));
    Source::ApplePhotos { library: package.to_string_lossy().into(), uuid: uuid.into(), original: Some(original.to_string_lossy().into()) }
}

/// A proxy of a PNG original at `originals/0/<uuid>.png` in `package`.
fn add_proxy(s: &mut Session, package: &Path, uuid: &str) -> PhotoId {
    write_png(&package.join("originals").join("0").join(format!("{uuid}.png")), 5);
    let id = s.catalog.alloc_photo_id();
    let photo = Photo::new(id, source(package, uuid), "IMG_0001.PNG", "PNG", 40, 24, "2026-11-14T00:00:00");
    s.commit("Add", Op::AddPhoto { photo: Box::new(photo) }).unwrap();
    id
}

#[test]
fn nothing_is_ever_written_for_a_proxy() {
    let dir = temp_dir("photos-proxy");
    let package = dir.join("Test.photoslibrary");
    std::fs::create_dir_all(package.join("originals").join("0")).unwrap();
    let mut s = Session::new().with_fs();
    s.open_library(dir.join("lib"), false).unwrap();
    let a = add_proxy(&mut s, &package, "0A1B2C3D-0000-4000-8000-000000000001");
    let b = add_proxy(&mut s, &package, "0A1B2C3D-0000-4000-8000-000000000002");
    let before = listing(&package);

    s.execute("library.xmpPreferences", &json!({"autoWrite": true})).unwrap();
    s.execute("library.select", &json!({"ids": [a.0]})).unwrap();
    s.execute("photo.rate", &json!({"rating": 4})).unwrap();
    s.execute("develop.set", &json!({"control": "light.exposure", "value": 1.0})).unwrap();
    assert!(s.save_sidecar(a).is_err(), "no sidecar beside an original in the package");
    let renamed = s.execute("photo.rename", &json!({"template": "Trip-{seq}", "ids": [a.0, b.0]})).unwrap();
    assert_eq!(renamed["renamed"], 0, "Photos owns the files' names");
    let e = s.execute("photo.editExternal", &json!({})).unwrap_err();
    assert!(e.to_string().contains("Apple Photos"), "{e}");
    let (kind, finish) = crate::merge::parse("merge.hdr", &json!({})).unwrap();
    let e = s.plan_merge(kind, finish, &[a, b], true).err().unwrap();
    assert!(e.to_string().contains("Apple Photos"), "{e}");

    assert_eq!(listing(&package), before, "nothing was written inside the package");
    assert_eq!(s.catalog.photo(a).unwrap().rating, 4, "the change is kept in the library, on the proxy");

    // the library keeps proxies (catalog format 14)
    drop(s);
    let mut s = Session::new().with_fs();
    s.open_library(dir.join("lib"), false).unwrap();
    assert_eq!(s.catalog.photo(a).unwrap().source, source(&package, "0A1B2C3D-0000-4000-8000-000000000001"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_proxy_decodes_its_original_and_one_in_icloud_fails_cleanly() {
    let dir = temp_dir("photos-proxy-decode");
    let package = dir.join("Test.photoslibrary");
    std::fs::create_dir_all(package.join("originals").join("0")).unwrap();
    let mut s = Session::new().with_fs();
    let local = add_proxy(&mut s, &package, "0A1B2C3D-0000-4000-8000-000000000003");
    let cloud = s.catalog.alloc_photo_id();
    let source =
        Source::ApplePhotos { library: package.to_string_lossy().into(), uuid: "0A1B2C3D-0000-4000-8000-000000000004".into(), original: None };
    s.commit("Add", Op::AddPhoto { photo: Box::new(Photo::new(cloud, source, "IMG_0002.HEIC", "HEIC", 40, 24, "2026-11-14T00:00:00")) }).unwrap();
    let load = |s: &mut Session, id: PhotoId| {
        let origin = s.catalog.photo(id).unwrap().source.clone();
        s.media.origin_ref(&origin, 64).load_source().map(|src| (src.image.width, src.image.height))
    };
    assert_eq!(load(&mut s, local).unwrap(), (40, 24));
    assert!(load(&mut s, cloud).is_err(), "only in iCloud: an error, not a panic");
    let keys = [local, cloud].map(|id| crate::media::content_key(s.catalog.photo(id).unwrap()));
    assert_ne!(keys[0], keys[1]);
    assert!(keys[1].starts_with("photos:"), "{}", keys[1]);
    let _ = std::fs::remove_dir_all(&dir);
}
