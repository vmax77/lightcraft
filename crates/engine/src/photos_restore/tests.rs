use lightcraft_develop::DevelopSettings;
use serde_json::json;

use super::{LABEL, NOT_LIGHTCRAFT, NOT_STORED, settings_from};
use crate::Session;
use crate::photos_edit::tests::{ASSET_D, library, no_fetch, proxy, session};
use crate::photos_save::{ADJUSTMENT_VERSION, adjustment_data};

fn with_exposure(ev: f64) -> DevelopSettings {
    let mut d = DevelopSettings::default();
    d.light.exposure = ev;
    d
}

#[test]
fn what_save_stores_reads_back_as_the_same_settings() {
    let d = with_exposure(0.7);
    let back = settings_from(ADJUSTMENT_VERSION, &adjustment_data(&d, None)).unwrap();
    assert_eq!(back, Some(d));
}

/// Adjustment data comes from outside the app: anything another app (or version) stored under
/// LightCraft's format is checked, and values are kept in range.
#[test]
fn stored_settings_are_checked_like_any_from_outside() {
    let data = adjustment_data(&with_exposure(0.7), None);
    assert!(settings_from("2", &data).unwrap_err().contains("another version"));
    assert!(settings_from(ADJUSTMENT_VERSION, b"not json").unwrap_err().contains("can't be read"));
    assert!(settings_from(ADJUSTMENT_VERSION, br#"{"app": "Other", "develop": {}}"#).unwrap_err().contains("not LightCraft's"));
    assert_eq!(settings_from(ADJUSTMENT_VERSION, br#"{"app": "LightCraft", "version": "0.1"}"#), Ok(None), "settings too large to store");
    let wild = json!({"app": "LightCraft", "develop": {"light": {"exposure": 1e9}}}).to_string();
    let back = settings_from(ADJUSTMENT_VERSION, wild.as_bytes()).unwrap().unwrap();
    assert!(back.light.exposure.is_finite() && back.light.exposure < 1e9, "kept in range: {}", back.light.exposure);
    let big = vec![b' '; crate::photos_save::MAX_ADJUSTMENT_BYTES + 1];
    assert!(settings_from(ADJUSTMENT_VERSION, &big).is_err());
}

#[test]
fn a_photos_item_gets_back_the_settings_lightcraft_saved_as_one_undo_step() {
    let library = library();
    let (mut s, _lib) = session("photos-restore", &library);
    let d = proxy(&s, ASSET_D);
    let stored = adjustment_data(&with_exposure(0.7), None);
    let asked = std::cell::RefCell::new(Vec::new());
    let mut read = |uuid: &str| -> Result<Option<(String, Vec<u8>)>, String> {
        asked.borrow_mut().push(uuid.to_string());
        Ok(Some((ADJUSTMENT_VERSION.into(), stored.clone())))
    };
    let (restored, skipped) = s.restore_photos_edits(&[d], &mut read).unwrap();
    assert_eq!((restored, skipped), (vec![d], vec![]));
    assert_eq!(*asked.borrow(), [ASSET_D], "asked about the asset the proxy shows");
    assert_eq!(s.catalog.photo(d).unwrap().develop.light.exposure, 0.7);
    assert_eq!(s.undo.last().map(|u| u.label.as_str()), Some(LABEL));
    s.execute("edit.undo", &json!({})).unwrap();
    assert_eq!(s.catalog.photo(d).unwrap().develop.light.exposure, 0.0, "one undo step takes it back");

    // the copy made to edit it asks about the same asset
    s.edit_photos_items(&[d], &mut no_fetch).unwrap();
    let (restored, _) = s.restore_photos_edits(&[d], &mut read).unwrap();
    assert_eq!(restored, [d]);
    assert_eq!(*asked.borrow(), [ASSET_D, ASSET_D]);
    assert_eq!(s.catalog.photo(d).unwrap().develop.light.exposure, 0.7);
}

#[test]
fn edits_lightcraft_did_not_save_and_other_photos_are_skipped_and_said_why() {
    let library = library();
    let (mut s, _lib) = session("photos-restore-skip", &library);
    let d = proxy(&s, ASSET_D);
    let mut nothing = |_: &str| -> Result<Option<(String, Vec<u8>)>, String> { Ok(None) };
    let (restored, skipped) = s.restore_photos_edits(&[d], &mut nothing).unwrap();
    assert!(restored.is_empty());
    assert_eq!(skipped[0].why, NOT_LIGHTCRAFT);
    assert!(!s.catalog.photo(d).unwrap().is_edited(), "nothing changed");
    let mut too_large = |_: &str| -> Result<Option<(String, Vec<u8>)>, String> {
        Ok(Some((ADJUSTMENT_VERSION.into(), br#"{"app": "LightCraft", "version": "0.1"}"#.to_vec())))
    };
    let (_, skipped) = s.restore_photos_edits(&[d], &mut too_large).unwrap();
    assert_eq!(skipped[0].why, NOT_STORED);
    let mut failing = |_: &str| -> Result<Option<(String, Vec<u8>)>, String> { Err("offline".into()) };
    assert!(s.restore_photos_edits(&[d], &mut failing).unwrap_err().to_string().contains("offline"));

    let demo = Session::with_demo();
    let first = demo.catalog.photos().next().unwrap().id;
    let (restores, skipped) = demo.photos_restores(&[first]).unwrap();
    assert!(restores.is_empty());
    assert_eq!(skipped[0].why, "not from Apple Photos");
}

/// Off macOS (and in a binary without Photos access) the command says why, and changes nothing.
#[test]
fn the_command_says_why_it_could_not_read() {
    let library = library();
    let (mut s, _lib) = session("photos-restore-cmd", &library);
    let d = proxy(&s, ASSET_D);
    let e = s.execute("applePhotos.restoreEdit", &json!({"ids": [d.0]})).unwrap_err().to_string();
    let why = if cfg!(target_os = "macos") { "NSPhotoLibraryUsageDescription" } else { "only available on macOS" };
    assert!(e.contains(why), "{e}");
    assert!(!s.catalog.photo(d).unwrap().is_edited());
}

/// Settings build on one version of the photo: saved on the original, they aren't put on a photo
/// that now builds on an edit Photos made, and the other way round.
#[test]
fn settings_are_restored_only_onto_the_version_they_build_on() {
    use crate::photos_sync::tests::{FakePhotos, jpeg};
    let library = library();
    let (mut s, _lib) = session("photos-restore-base", &library);
    let d = proxy(&s, ASSET_D);
    s.edit_photos_items(&[d], &mut no_fetch).unwrap();
    let mut photos = FakePhotos { modified: Some(7.0), edited: true, current: jpeg(30, 20, 90), ..Default::default() };
    s.sync_photos_items(&[d], &mut photos).unwrap();
    let on = |base: Option<i64>| adjustment_data(&with_exposure(0.4), base);
    let read = |data: Vec<u8>| move |_: &str| -> Result<Option<(String, Vec<u8>)>, String> { Ok(Some((ADJUSTMENT_VERSION.into(), data.clone()))) };
    let (restored, skipped) = s.restore_photos_edits(&[d], &mut read(on(None))).unwrap();
    assert!(restored.is_empty());
    assert_eq!(skipped[0].why, super::OTHER_BASE);
    let (restored, _) = s.restore_photos_edits(&[d], &mut read(on(Some(7_000)))).unwrap();
    assert_eq!(restored, [d]);
    assert_eq!(s.catalog.photo(d).unwrap().develop.light.exposure, 0.4);
}
