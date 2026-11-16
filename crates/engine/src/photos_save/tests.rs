use std::path::PathBuf;

use serde_json::{Value, json};

use super::{ADJUSTMENT_FORMAT, MAX_ADJUSTMENT_BYTES, adjustment_data};
use crate::photos_edit::tests::{ASSET_D, library, no_fetch, proxy, session};
use crate::photos_sync::tests::FakePhotos;

/// What a fake [`super::Save`] was given.
#[derive(Debug, Default)]
struct Given {
    uuid: String,
    path: PathBuf,
    jpeg: Vec<u8>,
    adjustment: Value,
}

#[test]
fn an_edited_photos_copy_is_rendered_as_a_jpeg_and_handed_to_photos_with_its_settings() {
    let library = library();
    let (mut s, _lib) = session("photos-save", &library);
    let d = proxy(&s, ASSET_D);
    s.execute("library.select", &json!({"ids": [d.0]})).unwrap();
    s.execute("develop.set", &json!({"control": "light.exposure", "value": 1.0})).unwrap();
    s.edit_photos_items(&[d], &mut no_fetch).unwrap();

    let mut given = Given::default();
    let mut save = |uuid: &str, path: &std::path::Path, data: &[u8]| -> Result<(), String> {
        given = Given {
            uuid: uuid.into(),
            path: path.into(),
            jpeg: std::fs::read(path).map_err(|e| e.to_string())?,
            adjustment: serde_json::from_slice(data).map_err(|e| e.to_string())?,
        };
        Ok(())
    };
    let (done, skipped) = s.save_photos_edits(&[d], &mut save, &mut FakePhotos::default()).unwrap();
    assert_eq!((done, skipped), (vec![d], vec![]));
    assert_eq!(given.uuid, ASSET_D, "onto the asset it was copied from");
    assert!(given.jpeg.starts_with(&[0xff, 0xd8]), "a JPEG");
    let (w, h) = (s.catalog.photo(d).unwrap().width, s.catalog.photo(d).unwrap().height);
    let img = lightcraft_codecs::decode(&given.jpeg, Default::default()).unwrap().to_srgb8();
    assert_eq!((img.width as u32, img.height as u32), (w, h), "full size");
    assert_eq!(given.adjustment["app"], "LightCraft");
    assert_eq!(given.adjustment["develop"]["light"]["exposure"], 1.0, "{}", given.adjustment);
    assert!(!given.path.exists(), "the temporary JPEG is removed");
    assert!(ADJUSTMENT_FORMAT.starts_with("ai.storyteller.lightcraft"));
}

#[test]
fn photos_not_copied_from_photos_or_not_edited_are_skipped() {
    let library = library();
    let (mut s, _lib) = session("photos-save-skip", &library);
    let d = proxy(&s, ASSET_D);
    let mut never = |_: &str, _: &std::path::Path, _: &[u8]| -> Result<(), String> { panic!("nothing to save") };
    // a proxy (not copied yet), then an unedited copy
    let (done, skipped) = s.save_photos_edits(&[d], &mut never, &mut FakePhotos::default()).unwrap();
    assert!(done.is_empty());
    assert_eq!(skipped[0].why, "not copied from Apple Photos");
    s.edit_photos_items(&[d], &mut no_fetch).unwrap();
    let (done, skipped) = s.save_photos_edits(&[d], &mut never, &mut FakePhotos::default()).unwrap();
    assert!(done.is_empty());
    assert_eq!(skipped[0].why, "not edited");
}

#[test]
fn a_refused_save_is_an_error_and_leaves_no_file() {
    let library = library();
    let (mut s, _lib) = session("photos-save-refused", &library);
    let d = proxy(&s, ASSET_D);
    s.execute("library.select", &json!({"ids": [d.0]})).unwrap();
    s.execute("develop.set", &json!({"control": "light.exposure", "value": 0.5})).unwrap();
    s.edit_photos_items(&[d], &mut no_fetch).unwrap();
    let mut path = PathBuf::new();
    let mut refuse = |_: &str, p: &std::path::Path, _: &[u8]| -> Result<(), String> {
        path = p.into();
        Err("not allowed".into())
    };
    let e = s.save_photos_edits(&[d], &mut refuse, &mut FakePhotos::default()).unwrap_err().to_string();
    assert!(e.contains("not allowed"), "{e}");
    assert!(!path.as_os_str().is_empty() && !path.exists(), "the temporary JPEG is removed");
}

#[test]
fn settings_too_large_for_photos_are_left_out() {
    let small = adjustment_data(&json!({"a": 1}), None);
    assert!(serde_json::from_slice::<Value>(&small).unwrap()["develop"].is_object());
    let big = adjustment_data(&json!({"a": "x".repeat(MAX_ADJUSTMENT_BYTES)}), None);
    let big: Value = serde_json::from_slice(&big).unwrap();
    assert_eq!(big["app"], "LightCraft");
    assert!(big.get("develop").is_none());
}

/// Off macOS (and in a binary without Photos access) the command says why, and changes nothing.
#[test]
fn the_command_reports_what_it_skipped_and_why_it_could_not_save() {
    let library = library();
    let (mut s, _lib) = session("photos-save-cmd", &library);
    let d = proxy(&s, ASSET_D);
    let r = s.execute("applePhotos.saveEdit", &json!({"ids": [d.0]})).unwrap();
    assert_eq!(r["saved"], json!([]));
    assert_eq!(r["skipped"][0]["why"], "not copied from Apple Photos", "{r}");
    s.execute("library.select", &json!({"ids": [d.0]})).unwrap();
    s.execute("develop.set", &json!({"control": "light.exposure", "value": 1.0})).unwrap();
    s.edit_photos_items(&[d], &mut no_fetch).unwrap();
    let e = s.execute("applePhotos.saveEdit", &json!({"ids": [d.0]})).unwrap_err().to_string();
    let why = if cfg!(target_os = "macos") { "NSPhotoLibraryUsageDescription" } else { "only available on macOS" };
    assert!(e.contains(why), "{e}");
}
