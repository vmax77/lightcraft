//! Browse an Apple Photos library in the grid (issue #1622, Phase 2): All Photos, Favourites and
//! the user's albums, shown as proxies ([`Source::ApplePhotos`]) in the normal grid.
//!
//! Reading a view ([`read_view`]) opens the library's `Photos.sqlite` read-only and lists the
//! view's assets; it can take a moment, so a front end runs it on a worker. Showing it
//! ([`Session::show_photos`]) makes a proxy for each asset that has none yet (a Local record: not
//! in the library, never written for), updates the original's path of those Photos downloaded or
//! offloaded since, and switches to the [`LibrarySource::ApplePhotos`] view. Assets hidden or in
//! Recently Deleted in Photos are not shown. The `applePhotos.show` command does both at once.

use std::collections::{HashMap, HashSet};

use lightcraft_catalog::{MediaKind, Op, Photo, PhotoId, Source};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{LibrarySource, Result, Session};

/// Which part of a Photos library the [`LibrarySource::ApplePhotos`] view shows.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum PhotosView {
    #[default]
    All,
    Favourites,
    Album {
        uuid: String,
    },
}

impl PhotosView {
    /// The view `{view?: "all" | "favourites", album?: uuid}` names (All Photos by default).
    pub fn from_params(p: &Value) -> std::result::Result<Self, String> {
        if let Some(uuid) = p.get("album").and_then(Value::as_str) {
            return Ok(Self::Album { uuid: uuid.to_string() });
        }
        match p.get("view").and_then(Value::as_str).unwrap_or("all") {
            "all" => Ok(Self::All),
            "favourites" | "favorites" => Ok(Self::Favourites),
            other => Err(format!("unknown view `{other}`: give `all`, `favourites` or an `album`")),
        }
    }
}

/// An album or folder of the Photos library, as the sidebar lists it (parents first).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PhotosAlbumRow {
    pub uuid: String,
    pub title: String,
    pub folder: bool,
    pub parent: Option<String>,
}

/// The Photos library being browsed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PhotosBrowse {
    /// The `.photoslibrary` package.
    pub library: String,
    pub view: PhotosView,
    pub albums: Vec<PhotosAlbumRow>,
    /// The view's proxies.
    pub members: Vec<PhotoId>,
    /// Changes with every [`Session::show_photos`], so the view is worked out again.
    pub generation: u64,
}

/// What a proxy is made from: one asset of a view.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PhotosItem {
    pub uuid: String,
    pub file_name: String,
    pub format: String,
    pub kind: MediaKind,
    pub width: u32,
    pub height: u32,
    /// Capture time as the library records it (`2026-01-01T12:00:00`, local time).
    pub captured: Option<String>,
    pub file_size: u64,
    /// The original inside the package, unless Photos says it is only in iCloud.
    pub original: Option<String>,
    /// What Photos says about it, which its proxy mirrors ([`PhotosItem::mirror`]).
    pub title: String,
    pub caption: String,
    pub keywords: Vec<String>,
    /// `(latitude, longitude)` in degrees.
    pub gps: Option<(f64, f64)>,
    /// Place names Photos worked out from the location: sublocation, city, state, country.
    pub location: String,
    pub city: String,
    pub state: String,
    pub country: String,
    pub favourite: bool,
}

/// The rating Photos' favourite gives a proxy.
pub const FAVOURITE_RATING: u8 = 5;

impl PhotosItem {
    /// `meta` and `rating` with what Photos says about the asset: its title, caption, keywords and
    /// location replace the proxy's (Photos wins), and a favourite is rated
    /// [`FAVOURITE_RATING`]; a proxy no longer a favourite loses that rating, while ratings Photos
    /// has no say on (1–4 stars on a photo that is not a favourite) stay.
    pub fn mirror(&self, meta: &lightcraft_catalog::Meta, rating: u8) -> (lightcraft_catalog::Meta, u8) {
        let mut meta = meta.clone();
        meta.title.clone_from(&self.title);
        meta.caption.clone_from(&self.caption);
        meta.keywords.clone_from(&self.keywords);
        meta.gps = self.gps;
        meta.location.clone_from(&self.location);
        meta.city.clone_from(&self.city);
        meta.state.clone_from(&self.state);
        meta.country.clone_from(&self.country);
        let rating = match (self.favourite, rating) {
            (true, _) => FAVOURITE_RATING,
            (false, FAVOURITE_RATING) => 0,
            (false, r) => r,
        };
        (meta, rating)
    }
}

/// `keywords` trimmed, without empty or repeated ones, in order.
fn clean_keywords(keywords: &[String]) -> Vec<String> {
    let mut seen = HashSet::new();
    keywords.iter().map(|k| k.trim()).filter(|k| !k.is_empty() && seen.insert(*k)).map(str::to_owned).collect()
}

/// One view of a Photos library, read by [`read_view`].
#[derive(Clone, Debug, PartialEq)]
pub struct PhotosRead {
    pub library: String,
    pub view: PhotosView,
    pub items: Vec<PhotosItem>,
    pub albums: Vec<PhotosAlbumRow>,
    pub warnings: Vec<String>,
}

/// The name a proxy shows: the name the file had when it was imported into Photos (`imported`),
/// with the extension of the file Photos keeps (`stored`). They can differ: Photos may keep a
/// JPEG of a photo imported as `IMG_1234.HEIC`, and the name has to say what the file is, or the
/// copy made to edit it is a JPEG named `.HEIC`. Photos' own name when there is no imported one,
/// and `uuid` when there is neither.
pub(crate) fn proxy_file_name(imported: Option<&str>, stored: Option<&str>, uuid: &str) -> String {
    let Some(imported) = imported else { return stored.unwrap_or(uuid).to_string() };
    let path = std::path::Path::new(imported);
    match (path.extension(), stored.and_then(|s| std::path::Path::new(s).extension())) {
        (have, Some(kept)) if have.is_none_or(|h| !h.eq_ignore_ascii_case(kept)) => path.with_extension(kept).to_string_lossy().into_owned(),
        _ => imported.to_string(),
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub use native::{default_library, library_param, read_view};

#[cfg(not(target_arch = "wasm32"))]
mod native {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicBool, Ordering};

    use lightcraft_catalog::MediaKind;

    use super::{PhotosAlbumRow, PhotosItem, PhotosRead, PhotosView};
    use crate::EngineError;
    use crate::apple_photos_library::{AlbumKind, AssetKind, Availability, PhotosAsset, PhotosLibrary, PhotosSource, SqliteSource};

    /// Assets read at a time.
    const PAGE: usize = 2000;

    fn other(e: impl std::fmt::Display) -> EngineError {
        EngineError::Other(e.to_string())
    }

    /// The System Photo Library's usual place for the user whose home is `home`, if it is there.
    pub fn default_library(home: &Path) -> Option<PathBuf> {
        let path = PhotosLibrary::default_system_path(home);
        PhotosLibrary::at(&path).is_ok().then_some(path)
    }

    /// The library `{library?: path}` names: else the one being browsed, else the usual System
    /// Photo Library.
    pub fn library_param(s: &crate::Session, p: &serde_json::Value) -> Result<PathBuf, String> {
        if let Some(path) = p.get("library").and_then(serde_json::Value::as_str) {
            return Ok(PathBuf::from(path));
        }
        s.photos_browse
            .as_ref()
            .map(|b| PathBuf::from(&b.library))
            .or_else(|| crate::library::home_folder().and_then(|h| default_library(&h)))
            .ok_or_else(|| "no Photos library found: give `library`".to_string())
    }

    /// Read `view` of the library at `library`: its shown assets (not hidden, not in Recently
    /// Deleted) and the library's albums. Read only; `cancel` is looked at between pages.
    pub fn read_view(library: &Path, view: &PhotosView, cancel: &AtomicBool) -> crate::Result<PhotosRead> {
        let lib = PhotosLibrary::at(library).map_err(other)?;
        let source = SqliteSource::open(&lib, cancel).map_err(other)?;
        let shown = |a: &PhotosAsset| !a.hidden && !a.trashed && (*view != PhotosView::Favourites || a.favourite);
        let mut items = Vec::new();
        let mut take = |assets: Vec<PhotosAsset>| items.extend(assets.iter().filter(|a| shown(a)).map(item));
        match view {
            PhotosView::All | PhotosView::Favourites => {
                for start in (0..source.asset_count()).step_by(PAGE) {
                    if cancel.load(Ordering::Relaxed) {
                        return Err(other("reading the Photos library was cancelled"));
                    }
                    take(source.assets(start, PAGE).map_err(other)?);
                }
            }
            PhotosView::Album { uuid } => {
                let members = source.album_assets(uuid).map_err(other)?.ok_or_else(|| other(format!("no album {uuid} in this Photos library")))?;
                for chunk in members.chunks(PAGE) {
                    if cancel.load(Ordering::Relaxed) {
                        return Err(other("reading the Photos library was cancelled"));
                    }
                    let mut found = source.assets_by_uuid(chunk).map_err(other)?;
                    take(chunk.iter().filter_map(|u| found.remove(u)).collect());
                }
            }
        }
        let albums = source
            .albums()
            .map_err(other)?
            .into_iter()
            .map(|a| PhotosAlbumRow { uuid: a.uuid, title: a.title, folder: a.kind == AlbumKind::Folder, parent: a.parent })
            .collect();
        Ok(PhotosRead { library: library.to_string_lossy().into_owned(), view: view.clone(), items, albums, warnings: source.warnings().to_vec() })
    }

    /// What a proxy shows of `a`.
    fn item(a: &PhotosAsset) -> PhotosItem {
        // edited in Photos: the version Photos shows, when it is on this Mac (whichever app edited
        // last, that edit shows; editing it in LightCraft builds on it)
        let edited = a.edited_path.as_ref().filter(|_| a.kind != AssetKind::Video).map(|p| p.to_string_lossy().into_owned());
        let stored = match &edited {
            Some(p) => Path::new(p).file_name().map(|n| n.to_string_lossy().into_owned()),
            None => a.file_name.clone(),
        };
        let file_name = super::proxy_file_name(a.original_file_name.as_deref(), stored.as_deref(), &a.uuid);
        let format = Path::new(&file_name).extension().map(|e| e.to_string_lossy().to_uppercase()).unwrap_or_default();
        let raw = edited.is_none() && a.uti.as_deref().is_some_and(|u| u.ends_with("raw-image"));
        let kind = match a.kind {
            AssetKind::Video => MediaKind::Video,
            _ if raw => MediaKind::Raw,
            _ => MediaKind::Image,
        };
        // tried unless Photos says it is only in iCloud (a library may not say either way: a file
        // that turns out to be missing falls back to Photos' preview when it is shown)
        let original = edited.or_else(|| {
            (a.availability != Availability::CloudOnly).then(|| a.original_path.as_ref().map(|p| p.to_string_lossy().into_owned())).flatten()
        });
        PhotosItem {
            uuid: a.uuid.clone(),
            file_name,
            format,
            kind,
            width: a.width.unwrap_or(0),
            height: a.height.unwrap_or(0),
            captured: a.created.map(|d| d.naive_local().format("%Y-%m-%dT%H:%M:%S").to_string()),
            file_size: a.original_size.unwrap_or(0),
            original,
            title: a.title.as_deref().map(str::trim).unwrap_or_default().to_owned(),
            caption: a.caption.as_deref().map(str::trim).unwrap_or_default().to_owned(),
            keywords: super::clean_keywords(&a.keywords),
            gps: a.location.map(|g| (g.latitude, g.longitude)),
            location: a.place.as_ref().and_then(|p| p.sublocality.clone()).unwrap_or_default(),
            city: a.place.as_ref().and_then(|p| p.city.clone()).unwrap_or_default(),
            state: a.place.as_ref().and_then(|p| p.state.clone()).unwrap_or_default(),
            country: a.place.as_ref().and_then(|p| p.country.clone()).unwrap_or_default(),
            favourite: a.favourite,
        }
    }
}

/// Whether `path` is a full-size edited version Photos keeps ([`crate::apple_photos_library::rendered_path`]), not an original.
pub fn is_rendered_path(path: &str) -> bool {
    let p = std::path::Path::new(path);
    let mut parents = p.ancestors().skip(1);
    let _letter = parents.next();
    parents.next().and_then(std::path::Path::file_name).is_some_and(|n| n == "renders")
        && parents.next().and_then(std::path::Path::file_name).is_some_and(|n| n == "resources")
        && p.file_name().is_some_and(|n| n.to_string_lossy().contains("_1_201_a."))
}

/// Photos' own small preview of an asset's unedited original, about 480 pixels long:
/// `resources/derivatives/masters/<first character of the UUID>/<UUID>_4_5005_c.jpeg` (observed in
/// Photos 11 libraries; a library may lack it). `None` for a UUID that can't name a file.
pub fn preview_path(library: &str, uuid: &str) -> Option<String> {
    let first = uuid.chars().next()?;
    let plain = uuid.len() <= 64 && uuid.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-');
    plain.then(|| {
        std::path::Path::new(library)
            .join("resources")
            .join("derivatives")
            .join("masters")
            .join(first.to_string())
            .join(format!("{uuid}_4_5005_c.jpeg"))
            .to_string_lossy()
            .into_owned()
    })
}

/// Ops applied at a time when proxies are made.
const OPS_PER_BATCH: usize = 2000;

impl Session {
    /// Show `read` (from [`read_view`]): make the proxies it lacks, update the originals' paths
    /// that changed, and switch to the [`LibrarySource::ApplePhotos`] view. Proxies are Local
    /// records, journaled but not an undo step, like a browsed folder's.
    pub fn show_photos(&mut self, read: PhotosRead) -> Result<Value> {
        // by asset: its proxy, the original it was linked to and the name it shows, or (the
        // original's path being `None` there) the copy made of it to edit it, which is shown in
        // its place
        let mut known: HashMap<String, (PhotoId, Option<String>, String)> = HashMap::new();
        let mut copied: HashMap<String, PhotoId> = HashMap::new();
        for p in self.catalog.photos() {
            match (&p.source, &p.from_photos) {
                (Source::ApplePhotos { library, uuid, original }, _) if *library == read.library => {
                    known.insert(uuid.clone(), (p.id, original.clone(), p.file_name.clone()));
                }
                // a copy in Recently Deleted gives way to the asset's proxy
                (_, Some(link)) if link.library == read.library && !p.deleted => {
                    copied.insert(link.uuid.clone(), p.id);
                }
                _ => {}
            }
        }
        let now = (self.clock)();
        let mut ops = Vec::new();
        let mut members = Vec::with_capacity(read.items.len());
        let mut seen = HashSet::new();
        let (mut added, mut updated) = (0usize, 0usize);
        for item in read.items.iter().filter(|i| seen.insert(i.uuid.as_str())) {
            if let Some(id) = copied.get(&item.uuid) {
                members.push(*id);
                continue;
            }
            let source = Source::ApplePhotos { library: read.library.clone(), uuid: item.uuid.clone(), original: item.original.clone() };
            match known.get(&item.uuid) {
                Some((id, original, file_name)) => {
                    let mut changed = false;
                    // also a proxy named before its name took the stored file's extension
                    if *original != item.original || *file_name != item.file_name {
                        ops.push(Op::Relink { id: *id, file_name: item.file_name.clone(), source, format: Some(item.format.clone()) });
                        changed = true;
                    }
                    // what Photos says about it wins over what the proxy had
                    if let Some(p) = self.catalog.photo(*id) {
                        let (meta, rating) = item.mirror(&p.meta, p.rating);
                        if meta != p.meta {
                            ops.push(Op::SetMeta { id: *id, meta: Box::new(meta) });
                            changed = true;
                        }
                        if rating != p.rating {
                            ops.push(Op::SetRating { id: *id, rating });
                            changed = true;
                        }
                    }
                    updated += usize::from(changed);
                    members.push(*id);
                }
                None => {
                    let id = self.catalog.alloc_photo_id();
                    let mut photo = Photo::new(id, source, &item.file_name, &item.format, item.width, item.height, &now);
                    photo.kind = item.kind;
                    photo.captured = item.captured.clone();
                    photo.file_size = item.file_size;
                    photo.local = true;
                    (photo.meta, photo.rating) = item.mirror(&photo.meta, photo.rating);
                    ops.push(Op::AddPhoto { photo: Box::new(photo) });
                    added += 1;
                    members.push(id);
                }
            }
        }
        // a proxy whose file changed (the original downloaded or offloaded, an edit made or reverted
        // in Photos) drops what was decoded of the old file: a view opened before this read
        // (a restored loupe at launch) would go on showing it
        let relinked: Vec<PhotoId> = ops.iter().filter_map(|op| if let Op::Relink { id, .. } = op { Some(*id) } else { None }).collect();
        for chunk in ops.chunks(OPS_PER_BATCH) {
            let op = Op::Batch { ops: chunk.to_vec() };
            self.catalog.apply(op.clone())?;
            self.pending_log.push(op);
        }
        for id in relinked {
            self.media.forget(id);
        }
        let generation = self.photos_browse.as_ref().map_or(0, |b| b.generation.wrapping_add(1));
        self.photos_browse = Some(PhotosBrowse { library: read.library.clone(), view: read.view.clone(), albums: read.albums, members, generation });
        self.source = LibrarySource::ApplePhotos;
        self.keep_active_in_view();
        let shown = self.visible().len();
        Ok(json!({
            "library": read.library,
            "view": read.view,
            "photos": shown,
            "new": added,
            "updated": updated,
            "warnings": read.warnings,
        }))
    }

    /// The filter of the [`LibrarySource::ApplePhotos`] view: the proxies of the view's assets.
    pub(crate) fn photos_filter(&self, f: &mut lightcraft_catalog::Filter) {
        let Some(browse) = &self.photos_browse else {
            // nothing browsed yet: no library is named, and nothing matches
            f.apple_photos = Some(String::new());
            f.only_none = true;
            return;
        };
        f.apple_photos = Some(browse.library.clone());
        if f.only.is_empty() {
            f.only = browse.members.clone();
        } else {
            let members: HashSet<PhotoId> = browse.members.iter().copied().collect();
            f.only.retain(|id| members.contains(id));
        }
        if f.only.is_empty() {
            f.only_none = true;
        }
    }
}

#[cfg(test)]
mod tests;
