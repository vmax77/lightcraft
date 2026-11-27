use std::path::Path;

use lightcraft_catalog::{PhotoId, PhotosSync, Source};
use lightcraft_develop::DevelopSettings;
use serde_json::json;

use super::{Adopted, Change, PhotosAccess, PhotosState, SNAPSHOT, pending_change};
use crate::Session;
use crate::photos_edit::tests::{ASSET_D, library, no_fetch, proxy, session};

/// A Photos asset for the tests: what it shows and when it last changed.
#[derive(Debug, Default)]
pub(crate) struct FakePhotos {
    pub modified: Option<f64>,
    pub edited: bool,
    /// The edit it shows was saved by LightCraft.
    pub ours: bool,
    pub current: Vec<u8>,
    pub original: Vec<u8>,
    /// How often it was asked whose edit it shows, and downloaded from.
    pub asked: usize,
    pub downloads: usize,
}

impl PhotosAccess for FakePhotos {
    fn state(&mut self, _: &str) -> Result<PhotosState, String> {
        Ok(PhotosState { modified: self.modified, edited: self.edited })
    }
    fn ours(&mut self, _: &str) -> Result<bool, String> {
        self.asked += 1;
        Ok(self.ours)
    }
    fn download_current(&mut self, _: &str, out: &mut dyn std::io::Write) -> Result<u64, String> {
        self.downloads += 1;
        out.write_all(&self.current).map_err(|e| e.to_string())?;
        Ok(self.current.len() as u64)
    }
    fn download_original(&mut self, _: &str, out: &mut dyn std::io::Write) -> Result<u64, String> {
        self.downloads += 1;
        out.write_all(&self.original).map_err(|e| e.to_string())?;
        Ok(self.original.len() as u64)
    }
}

/// A JPEG of `w`×`h` grey pixels of level `v`.
pub(crate) fn jpeg(w: usize, h: usize, v: u8) -> Vec<u8> {
    let pixels = lightcraft_raster::Rgba8::from_bytes(w, h, &vec![v; w * h * 4]).unwrap();
    crate::export::encode_image(&pixels, &crate::export::ExportOptions::default()).unwrap()
}

fn sync_of(s: &Session, id: PhotoId) -> Option<PhotosSync> {
    s.catalog.photo(id).unwrap().from_photos.as_ref().unwrap().sync.clone()
}

fn file_of(s: &Session, id: PhotoId) -> String {
    let Source::File { path } = &s.catalog.photo(id).unwrap().source else { panic!("not a file") };
    path.clone()
}

/// A session with D copied from Photos and its exposure raised.
fn edited_copy(tag: &str) -> (Session, PhotoId, crate::apple_photos_library::fixture::Library) {
    let library = library();
    let (mut s, _lib) = session(tag, &library);
    let d = proxy(&s, ASSET_D);
    s.execute("library.select", &json!({"ids": [d.0]})).unwrap();
    s.execute("develop.set", &json!({"control": "light.exposure", "value": 0.5})).unwrap();
    s.edit_photos_items(&[d], &mut no_fetch).unwrap();
    (s, d, library)
}

#[test]
fn only_a_new_edit_made_elsewhere_or_a_revert_has_to_be_brought_in() {
    let seen = PhotosSync { photos_modified_ms: 2_000, ..Default::default() };
    let on_photos_edit = PhotosSync { base_ms: Some(2_000), ..seen.clone() };
    let mut photos = FakePhotos { modified: Some(2.0), edited: true, ..Default::default() };
    // not changed since LightCraft looked: Photos isn't even asked whose edit it is
    assert_eq!(pending_change(Some(&seen), "U", &mut photos), Ok((None, Some(2_000))));
    assert_eq!(photos.asked, 0);
    // changed since, and the edit isn't LightCraft's
    photos.modified = Some(3.0);
    assert_eq!(pending_change(Some(&seen), "U", &mut photos), Ok((Some(Change::Edited), Some(3_000))));
    // never looked at: an edit made in Photos is new
    assert_eq!(pending_change(None, "U", &mut photos).unwrap().0, Some(Change::Edited));
    // LightCraft's own save, a favourite or a title: nothing to bring in
    photos.ours = true;
    assert_eq!(pending_change(Some(&seen), "U", &mut photos).unwrap().0, None);
    // unedited in Photos: nothing, unless LightCraft builds on an earlier edit of Photos (a revert)
    photos.edited = false;
    assert_eq!(pending_change(Some(&seen), "U", &mut photos).unwrap().0, None);
    assert_eq!(pending_change(Some(&on_photos_edit), "U", &mut photos).unwrap().0, Some(Change::Reverted));
}

/// Settings never saved to Photos aren't in its version: they stay on top of it.
#[test]
fn an_edit_made_in_photos_becomes_the_photo_and_unsaved_settings_stay_on_top() {
    let (mut s, d, library) = edited_copy("photos-sync-unsaved");
    let before = file_of(&s, d);
    let mut photos = FakePhotos { modified: Some(1_800_000_000.0), edited: true, current: jpeg(30, 20, 90), ..Default::default() };
    let done = s.sync_photos_items(&[d], &mut photos).unwrap();
    assert_eq!(done, [(d, Adopted::NewVersion { kept: true, snapshot: None })]);
    let p = s.catalog.photo(d).unwrap().clone();
    let now = file_of(&s, d);
    assert!(now.ends_with("(Photos edit).jpeg"), "{now}");
    assert_eq!(std::fs::read(&now).unwrap(), photos.current, "Photos' version is the photo's file");
    assert_eq!((p.width, p.height, p.format.as_str()), (30, 20, "JPEG"), "its size and format");
    assert_eq!(p.develop.light.exposure, 0.5, "LightCraft's settings stay on top");
    assert!(Path::new(&before).exists(), "the earlier file is kept");
    let sync = sync_of(&s, d).unwrap();
    assert_eq!((sync.photos_modified_ms, sync.base_ms, sync.saved), (1_800_000_000_000, Some(1_800_000_000_000), false));

    // asked again with nothing new in Photos: nothing is downloaded or changed
    let done = s.sync_photos_items(&[d], &mut photos).unwrap();
    assert_eq!(done, [(d, Adopted::InStep)]);
    assert_eq!(photos.downloads, 1);
    assert_eq!(file_of(&s, d), now);
    drop(library);
}

/// Settings saved to Photos are in the version it made on top of them: they start again from the
/// default; changed since that save, they are kept as a snapshot first.
#[test]
fn saved_settings_are_in_photos_version_and_changes_since_are_kept_as_a_snapshot() {
    let (mut s, d, _library) = edited_copy("photos-sync-saved");
    let mut photos = FakePhotos { modified: Some(100.0), edited: true, ours: true, ..Default::default() };
    s.saved_to_photos(d, Some(100.0)).unwrap();
    assert!(sync_of(&s, d).unwrap().saved);
    // then edited in Photos, LightCraft unchanged since its save
    (photos.modified, photos.ours, photos.current) = (Some(200.0), false, jpeg(20, 20, 60));
    let done = s.sync_photos_items(&[d], &mut photos).unwrap();
    assert_eq!(done, [(d, Adopted::NewVersion { kept: false, snapshot: None })]);
    let p = s.catalog.photo(d).unwrap().clone();
    assert_eq!(*p.develop, DevelopSettings::default(), "the saved settings are in Photos' version");
    assert!(p.versions.iter().all(|v| v.name != SNAPSHOT));

    // saved again on top, then changed in LightCraft and edited in Photos: the change is kept
    s.execute("develop.set", &json!({"control": "light.exposure", "value": 0.3})).unwrap();
    s.saved_to_photos(d, Some(300.0)).unwrap();
    s.execute("develop.set", &json!({"control": "light.exposure", "value": 0.9})).unwrap();
    (photos.modified, photos.current) = (Some(400.0), jpeg(20, 20, 30));
    let done = s.sync_photos_items(&[d], &mut photos).unwrap();
    assert_eq!(done, [(d, Adopted::NewVersion { kept: false, snapshot: Some(SNAPSHOT.into()) })]);
    let p = s.catalog.photo(d).unwrap().clone();
    assert_eq!(*p.develop, DevelopSettings::default());
    let kept = p.versions.iter().find(|v| v.name == SNAPSHOT).unwrap();
    assert_eq!(kept.settings.light.exposure, 0.9, "LightCraft's change since its save is kept");
}

/// A change in Photos that isn't a new edit (a favourite, a title, LightCraft's own save) only
/// moves on the time LightCraft last looked; the same version downloaded again isn't taken.
#[test]
fn a_change_that_isnt_a_new_edit_leaves_the_photo_as_it_is() {
    let (mut s, d, _library) = edited_copy("photos-sync-same");
    let file = file_of(&s, d);
    let mut photos = FakePhotos { modified: Some(50.0), edited: true, ours: true, ..Default::default() };
    assert_eq!(s.sync_photos_items(&[d], &mut photos).unwrap(), [(d, Adopted::InStep)]);
    assert_eq!((photos.downloads, sync_of(&s, d).unwrap().photos_modified_ms), (0, 50_000));
    // an edit that isn't LightCraft's, but the same bytes as the photo's file
    (photos.modified, photos.ours, photos.current) = (Some(60.0), false, std::fs::read(&file).unwrap());
    assert_eq!(s.sync_photos_items(&[d], &mut photos).unwrap(), [(d, Adopted::InStep)]);
    assert_eq!(file_of(&s, d), file);
    let dir = Path::new(&file).parent().unwrap();
    assert_eq!(std::fs::read_dir(dir).unwrap().count(), 1, "the download was dropped");
}

/// Reverted to the original in Photos while LightCraft builds on an earlier edit of Photos: the
/// original becomes the photo's file again.
#[test]
fn a_revert_in_photos_brings_back_the_original() {
    let (mut s, d, _library) = edited_copy("photos-sync-revert");
    let mut photos = FakePhotos { modified: Some(10.0), edited: true, current: jpeg(30, 20, 90), original: jpeg(40, 24, 128), ..Default::default() };
    s.sync_photos_items(&[d], &mut photos).unwrap();
    (photos.modified, photos.edited) = (Some(20.0), false);
    let done = s.sync_photos_items(&[d], &mut photos).unwrap();
    assert!(matches!(done[0].1, Adopted::NewVersion { .. }), "{done:?}");
    let now = file_of(&s, d);
    assert!(now.ends_with("(original).jpeg"), "{now}");
    assert_eq!(std::fs::read(&now).unwrap(), photos.original);
    assert_eq!(sync_of(&s, d).unwrap().base_ms, None, "building on the original again");
}

/// Saving would replace an edit made in Photos that LightCraft hasn't brought in: refused.
#[test]
fn a_save_never_replaces_an_edit_made_in_photos_that_lightcraft_hasnt_brought_in() {
    let (mut s, d, _library) = edited_copy("photos-sync-save");
    let mut photos = FakePhotos { modified: Some(10.0), edited: true, current: jpeg(30, 20, 90), ..Default::default() };
    let mut never = |_: &str, _: &Path, _: &[u8]| -> Result<(), String> { panic!("nothing is saved") };
    let e = s.save_photos_edits(&[d], &mut never, &mut photos).unwrap_err().to_string();
    assert!(e.contains(crate::photos_save::NEWER_IN_PHOTOS), "{e}");
    // brought in, then saved: Photos' new time is remembered, so the save isn't taken for an edit
    s.sync_photos_items(&[d], &mut photos).unwrap();
    let mut saved = 0;
    let mut save = |_: &str, _: &Path, data: &[u8]| -> Result<(), String> {
        let v: serde_json::Value = serde_json::from_slice(data).unwrap();
        assert_eq!(v["base"], 10_000, "the settings build on Photos' edit");
        saved += 1;
        Ok(())
    };
    s.save_photos_edits(&[d], &mut save, &mut photos).unwrap();
    assert_eq!(saved, 1);
    let sync = sync_of(&s, d).unwrap();
    assert!(sync.saved && sync.base_ms == Some(10_000));
}

/// A copy of an asset edited in Photos, saved before any sync noted when Photos last changed it:
/// the edit Photos shows is the version it was copied from, no new edit, and the save goes ahead.
/// A version that differs is a new edit, and the save is refused.
#[test]
fn a_save_of_a_copy_of_photos_version_needs_no_sync_first() {
    let (mut s, d, _library) = edited_copy("photos-sync-save-own");
    // as copied from Photos' edited version: building on an edit of Photos, never synced
    let link = s.catalog.photo(d).unwrap().from_photos.clone().unwrap();
    let link = lightcraft_catalog::PhotosLink { sync: Some(PhotosSync { base_ms: Some(0), ..Default::default() }), ..link };
    s.apply_system(lightcraft_catalog::Op::SetFromPhotos { id: d, link: Some(link) }).unwrap();
    let own = std::fs::read(file_of(&s, d)).unwrap();
    let mut photos = FakePhotos { modified: Some(10.0), edited: true, current: own, ..Default::default() };
    let mut saved = 0;
    let mut save = |_: &str, _: &Path, _: &[u8]| -> Result<(), String> {
        saved += 1;
        Ok(())
    };
    s.save_photos_edits(&[d], &mut save, &mut photos).unwrap();
    assert_eq!(saved, 1, "saved without a sync first");
    assert_eq!(sync_of(&s, d).unwrap().photos_modified_ms, 10_000, "Photos' time noted");
    // edited in Photos since (and not LightCraft's): refused
    (photos.modified, photos.current) = (Some(20.0), jpeg(9, 9, 10));
    let mut never = |_: &str, _: &Path, _: &[u8]| -> Result<(), String> { panic!("nothing is saved") };
    let e = s.save_photos_edits(&[d], &mut never, &mut photos).unwrap_err().to_string();
    assert!(e.contains(crate::photos_save::NEWER_IN_PHOTOS), "{e}");
}

/// Save All: every photo whose LightCraft edit isn't in Photos yet, all handed to Photos at once
/// (macOS asks once); then nothing is left to save.
#[test]
fn save_all_hands_every_unsaved_edit_to_photos_at_once() {
    let (mut s, d, _library) = edited_copy("photos-sync-save-all");
    assert_eq!(s.unsaved_photos_edits(), [d], "edited, never saved");
    let mut photos = FakePhotos { modified: Some(5.0), ..Default::default() };
    let mut calls = Vec::new();
    let mut save = |items: &[(String, std::path::PathBuf, Vec<u8>)]| -> Vec<Result<(), String>> {
        calls.push(items.iter().map(|(uuid, path, _)| (uuid.clone(), path.exists())).collect::<Vec<_>>());
        items.iter().map(|_| Ok(())).collect()
    };
    let (done, skipped) = s.save_all_photos_edits(&mut save, &mut photos).unwrap();
    assert_eq!(calls, [vec![(crate::photos_edit::tests::ASSET_D.to_string(), true)]], "one change, its JPEG written");
    assert!(skipped.is_empty());
    assert_eq!(done.len(), 1);
    assert_eq!(done[0].2, Ok(Some(5.0)));
    assert!(s.unsaved_photos_edits().is_empty(), "saved: nothing left");
    // changed again in LightCraft: unsaved again
    s.execute("develop.set", &json!({"control": "light.exposure", "value": 0.2})).unwrap();
    assert_eq!(s.unsaved_photos_edits(), [d]);
}

/// A photo that can't be rendered fails alone; the others go to Photos together.
#[test]
fn one_save_that_cant_be_rendered_fails_alone() {
    use crate::photos_save::run_many;
    let (mut s, d, _library) = edited_copy("photos-sync-save-many");
    // Photos shows an edit LightCraft hasn't brought in: this one is refused before rendering
    let mut photos = FakePhotos { modified: Some(9.0), edited: true, current: jpeg(5, 5, 1), ..Default::default() };
    let (saves, _) = s.photos_saves(&[d]).unwrap();
    let mut never = |_: &[(String, std::path::PathBuf, Vec<u8>)]| -> Vec<Result<(), String>> { panic!("nothing to hand over") };
    let done = run_many(saves, &mut never, &mut photos, &mut |_| {});
    assert!(done[0].2.as_ref().unwrap_err().contains(crate::photos_save::NEWER_IN_PHOTOS));
}

/// Photos' face boxes are placed on the file Photos placed them on: when the photo's file becomes
/// another version (an edit made in Photos), they go; boxes from LightCraft stay.
#[test]
fn photos_face_boxes_go_when_the_photos_file_changes() {
    use lightcraft_meta::{Region, RegionKind};
    let (mut s, d, _library) = edited_copy("photos-sync-faces");
    let rect = lightcraft_geom::Rect { x0: 0.1, y0: 0.1, x1: 0.2, y1: 0.2 };
    let from_photos =
        Region { rect, kind: RegionKind::Face, name: Some("Ada Example".into()), description: Some(crate::photos_browse::FROM_PHOTOS.into()) };
    let own = Region { rect, kind: RegionKind::Face, name: Some("Mine".into()), description: None };
    let mut meta = s.catalog.photo(d).unwrap().meta.clone();
    meta.regions = vec![from_photos, own.clone()];
    s.apply_system(lightcraft_catalog::Op::SetMeta { id: d, meta: Box::new(meta) }).unwrap();
    let mut photos = FakePhotos { modified: Some(10.0), edited: true, current: jpeg(30, 20, 90), ..Default::default() };
    s.sync_photos_items(&[d], &mut photos).unwrap();
    assert_eq!(s.catalog.photo(d).unwrap().meta.regions, [own], "only LightCraft's box stays");
}
