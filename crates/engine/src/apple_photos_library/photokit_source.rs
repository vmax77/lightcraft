//! [`PhotoKitSource`]: the System Photo Library through PhotoKit ([`lightcraft_photokit`]), with the
//! fields PhotoKit lacks taken from `Photos.sqlite` when that is readable.
//!
//! PhotoKit gives no title, original path, time zone, size, local/iCloud state or RAW+JPEG and
//! Live Photo companion paths, and before
//! macOS 27 no caption or keywords. Those come from a [`SqliteSource`] of the same library, matched
//! by UUID (the part of PhotoKit's local identifier before the first `/`). Without one they stay
//! empty. Assets in Recently Deleted are not listed by PhotoKit.

use std::collections::HashMap;

use chrono::{DateTime, FixedOffset, Utc};
use lightcraft_photokit as photokit;

use super::{
    AlbumKind, AssetKind, Availability, Companion, PhotosAlbum, PhotosAsset, PhotosError, PhotosSource, SqliteSource, geo_point, valid_uuid,
};

/// Suffix of an asset's PhotoKit local identifier after its UUID, as Photos forms them
/// (`<UUID>/L0/001`; albums end `/L0/040`, folders `/L0/020`).
const ASSET_ID_SUFFIX: &str = "/L0/001";

/// The System Photo Library read through PhotoKit.
#[derive(Debug)]
pub struct PhotoKitSource {
    library: photokit::Library,
    fallback: Option<SqliteSource>,
    albums: Vec<PhotosAlbum>,
    album_ids: HashMap<String, String>,
    warnings: Vec<String>,
}

impl From<photokit::PhotoKitError> for PhotosError {
    fn from(e: photokit::PhotoKitError) -> Self {
        match e {
            photokit::PhotoKitError::NotAllowed(_) => PhotosError::NotAllowed(e.to_string()),
            photokit::PhotoKitError::NotMacOs | photokit::PhotoKitError::NoUsageDescription => PhotosError::Unavailable(e.to_string()),
        }
    }
}

impl PhotoKitSource {
    /// Open the System Photo Library if the user has allowed it (ask first with
    /// [`photokit::request_access`], from a worker thread). `fallback` is a [`SqliteSource`] of the
    /// same library, for the fields PhotoKit lacks.
    pub fn open(fallback: Option<SqliteSource>) -> Result<Self, PhotosError> {
        let library = photokit::Library::open()?;
        let mut warnings = fallback.as_ref().map(|f| f.warnings().to_vec()).unwrap_or_default();
        if fallback.is_none() {
            warnings.push("Photos.sqlite is not readable: titles, file paths and iCloud state are unknown".into());
        }
        let (albums, album_ids) = map_albums(&library.albums());
        Ok(Self { library, fallback, albums, album_ids, warnings })
    }

    /// The same assets read from `Photos.sqlite`, by UUID.
    fn known(&self, uuids: &[String]) -> Result<HashMap<String, PhotosAsset>, PhotosError> {
        self.fallback.as_ref().map_or_else(|| Ok(HashMap::new()), |f| f.assets_by_uuid(uuids))
    }
}

impl PhotosSource for PhotoKitSource {
    fn asset_count(&self) -> usize {
        self.library.asset_count()
    }

    fn assets(&self, start: usize, limit: usize) -> Result<Vec<PhotosAsset>, PhotosError> {
        let page = self.library.assets(start, limit);
        let uuids: Vec<String> = page.iter().filter_map(|a| uuid_of(&a.local_id)).collect();
        let known = self.known(&uuids)?;
        Ok(page
            .into_iter()
            .filter_map(|a| {
                let found = uuid_of(&a.local_id).and_then(|u| known.get(&u));
                map_asset(a, found)
            })
            .collect())
    }

    fn asset(&self, uuid: &str) -> Result<Option<PhotosAsset>, PhotosError> {
        if !valid_uuid(uuid) {
            return Ok(None);
        }
        let Some(found) = self.library.asset(&format!("{uuid}{ASSET_ID_SUFFIX}")) else { return Ok(None) };
        let known = self.known(&[uuid.to_owned()])?;
        Ok(map_asset(found, known.get(uuid)))
    }

    fn albums(&self) -> Result<Vec<PhotosAlbum>, PhotosError> {
        Ok(self.albums.clone())
    }

    fn album_assets(&self, album_uuid: &str) -> Result<Option<Vec<String>>, PhotosError> {
        let Some(id) = self.album_ids.get(album_uuid) else { return Ok(None) };
        Ok(self.library.album_asset_ids(id).map(|ids| ids.iter().filter_map(|i| uuid_of(i)).collect()))
    }

    fn warnings(&self) -> &[String] {
        &self.warnings
    }
}

/// The UUID at the start of a PhotoKit local identifier, if it is a plausible one.
pub(super) fn uuid_of(local_id: &str) -> Option<String> {
    let uuid = local_id.split('/').next()?;
    valid_uuid(uuid).then(|| uuid.to_owned())
}

/// A PhotoKit asset as a [`PhotosAsset`], completed from `known` (the same asset read from
/// `Photos.sqlite`). `None` when the identifier has no usable UUID.
pub(super) fn map_asset(asset: photokit::Asset, known: Option<&PhotosAsset>) -> Option<PhotosAsset> {
    let uuid = uuid_of(&asset.local_id)?;
    let offset = known.and_then(|k| k.created).map(|c| *c.offset());
    let created = asset.created.and_then(|secs| unix_date(secs, offset));
    let kind = match asset.kind {
        Some(photokit::MediaKind::Image) => AssetKind::Photo,
        Some(photokit::MediaKind::Video) => AssetKind::Video,
        Some(photokit::MediaKind::Audio) => AssetKind::Other(3),
        Some(photokit::MediaKind::Unknown) | None => AssetKind::Other(0),
    };
    let dimension = |n: u64| u32::try_from(n).ok().filter(|&n| n > 0);
    let from_known = |f: fn(&PhotosAsset) -> Option<String>| known.and_then(f);
    Some(PhotosAsset {
        uuid,
        file_name: from_known(|k| k.file_name.clone()),
        original_file_name: asset.original_file_name.or_else(|| from_known(|k| k.original_file_name.clone())),
        original_path: known.and_then(|k| k.original_path.clone()),
        created: created.or_else(|| known.and_then(|k| k.created)),
        favourite: asset.favourite,
        hidden: asset.hidden,
        trashed: false,
        kind,
        live_photo: asset.live_photo,
        uti: asset.uti.or_else(|| from_known(|k| k.uti.clone())),
        width: dimension(asset.width),
        height: dimension(asset.height),
        original_size: known.and_then(|k| k.original_size),
        location: asset.location.and_then(|(la, lo)| geo_point(Some(la), Some(lo))),
        title: from_known(|k| k.title.clone()),
        caption: if asset.keywords.is_some() || asset.caption.is_some() { asset.caption } else { from_known(|k| k.caption.clone()) },
        keywords: asset.keywords.unwrap_or_else(|| known.map(|k| k.keywords.clone()).unwrap_or_default()),
        availability: known.map_or(Availability::Unknown, |k| k.availability),
        paired_original: known.map_or(Companion::Unknown, |k| k.paired_original.clone()),
        live_video: known.map_or(Companion::Unknown, |k| k.live_video.clone()),
        edited: known.is_some_and(|k| k.edited),
        place: known.and_then(|k| k.place.clone()),
        edited_path: known.and_then(|k| k.edited_path.clone()),
    })
}

/// Seconds since 1970 as a date in `offset` (UTC when unknown), if it is a real date.
fn unix_date(seconds: f64, offset: Option<FixedOffset>) -> Option<DateTime<FixedOffset>> {
    if !seconds.is_finite() || seconds.abs() > 1e11 {
        return None;
    }
    let whole = seconds.floor();
    let nanos = ((seconds - whole) * 1e9).round().clamp(0.0, 999_999_999.0) as u32;
    let utc: DateTime<Utc> = DateTime::from_timestamp(whole as i64, nanos)?;
    Some(utc.with_timezone(&offset.unwrap_or(FixedOffset::east_opt(0)?)))
}

/// Albums and folders by UUID, parents first, and each album's PhotoKit local identifier.
pub(super) fn map_albums(albums: &[photokit::Album]) -> (Vec<PhotosAlbum>, HashMap<String, String>) {
    let mut out = Vec::new();
    let mut ids = HashMap::new();
    for album in albums {
        let Some(uuid) = uuid_of(&album.local_id) else { continue };
        if ids.contains_key(&uuid) {
            continue;
        }
        let parent = album.parent.as_deref().and_then(uuid_of).filter(|p| ids.contains_key(p));
        let kind = if album.folder { AlbumKind::Folder } else { AlbumKind::Album };
        ids.insert(uuid.clone(), album.local_id.clone());
        out.push(PhotosAlbum { uuid, title: album.title.clone().unwrap_or_default(), kind, parent });
    }
    (out, ids)
}
