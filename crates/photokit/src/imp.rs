//! The PhotoKit calls (macOS). Every `unsafe` block says why it is sound.
//!
//! The `objc2-photos` bindings mark every PhotoKit method `unsafe` because their signatures have not
//! been audited upstream. The calls here pass only the argument types the headers declare, keep
//! every returned object retained while it is used, and index fetch results only below their
//! `count`.

use std::sync::mpsc;
use std::time::Duration;

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{msg_send, sel};
use objc2_foundation::{NSArray, NSBundle, NSObject, NSObjectProtocol, NSSortDescriptor, NSString, ns_string};
use objc2_photos::{
    PHAccessLevel, PHAsset, PHAssetCollection, PHAssetCollectionSubtype, PHAssetCollectionType, PHAssetMediaSubtype, PHAssetMediaType,
    PHAssetResource, PHAssetResourceType, PHAuthorizationStatus, PHCollection, PHCollectionList, PHFetchOptions, PHFetchResult, PHObject,
    PHPhotoLibrary,
};

use super::{Access, Album, Asset, MAX_ALBUM_ASSETS, MAX_COLLECTIONS, MAX_FOLDER_DEPTH, MediaKind, PhotoKitError};

/// How long [`request_access`] waits for the user to answer the prompt.
const PROMPT_WAIT: Duration = Duration::from_secs(600);

/// The usage description macOS requires before any PhotoKit call.
fn ensure_usage_description() -> Result<(), PhotoKitError> {
    let bundle = NSBundle::mainBundle();
    match bundle.objectForInfoDictionaryKey(ns_string!("NSPhotoLibraryUsageDescription")) {
        Some(_) => Ok(()),
        None => Err(PhotoKitError::NoUsageDescription),
    }
}

pub fn has_extended_metadata() -> bool {
    objc2::available!(macos = 27)
}

pub fn access() -> Result<Access, PhotoKitError> {
    ensure_usage_description()?;
    // SAFETY: a class method taking a PHAccessLevel value and returning an integer status; the usage
    // description is present, so macOS answers instead of terminating the process.
    let status = unsafe { PHPhotoLibrary::authorizationStatusForAccessLevel(PHAccessLevel::ReadWrite) };
    Ok(Access::from_raw(status.0))
}

pub fn request_access() -> Result<Access, PhotoKitError> {
    let now = access()?;
    if now != Access::NotDetermined {
        return Ok(now);
    }
    let (tx, rx) = mpsc::channel::<isize>();
    let handler = RcBlock::new(move |status: PHAuthorizationStatus| {
        // the receiver may be gone after a timeout; nothing to do then
        let _ = tx.send(status.0);
    });
    // SAFETY: the handler has the block signature the header declares (`void (^)(PHAuthorizationStatus)`),
    // PhotoKit copies the block before returning, and the block only sends an integer on a channel.
    unsafe { PHPhotoLibrary::requestAuthorizationForAccessLevel_handler(PHAccessLevel::ReadWrite, &handler) };
    Ok(rx.recv_timeout(PROMPT_WAIT).map_or(Access::NotDetermined, Access::from_raw))
}

/// The System Photo Library: every asset, hidden ones included, oldest first.
#[derive(Debug)]
pub struct Library {
    assets: Retained<PHFetchResult<PHAsset>>,
    count: usize,
}

fn text(s: &NSString) -> Option<String> {
    super::non_blank(s.to_string())
}

fn kind(media: PHAssetMediaType) -> MediaKind {
    match media {
        PHAssetMediaType::Image => MediaKind::Image,
        PHAssetMediaType::Video => MediaKind::Video,
        PHAssetMediaType::Audio => MediaKind::Audio,
        _ => MediaKind::Unknown,
    }
}

/// `result[i]` for every `i` below its count, at most `max`.
fn each<T: objc2::Message>(result: &PHFetchResult<T>, start: usize, max: usize) -> Vec<Retained<T>> {
    // SAFETY: `count` takes no arguments and returns an integer.
    let count = unsafe { result.count() };
    let end = start.saturating_add(max).min(count);
    // SAFETY: every index is below `count`, so `objectAtIndex:` raises no range exception.
    (start..end).map(|i| unsafe { result.objectAtIndex(i) }).collect()
}

fn local_id(object: &PHObject) -> String {
    // SAFETY: `localIdentifier` takes no arguments and returns a non-null NSString.
    unsafe { object.localIdentifier() }.to_string()
}

impl Library {
    /// Open the library if the user has allowed it ([`super::request_access`] asks).
    pub fn open() -> Result<Self, PhotoKitError> {
        let access = access()?;
        if !access.can_read() {
            return Err(PhotoKitError::NotAllowed(access));
        }
        // SAFETY: `new` makes an empty PHFetchOptions; the setters take a bool and an array of sort
        // descriptors (the key is a PHAsset property PhotoKit documents as sortable).
        let options = unsafe {
            let options = PHFetchOptions::new();
            options.setIncludeHiddenAssets(true);
            let by_date = NSSortDescriptor::sortDescriptorWithKey_ascending(Some(ns_string!("creationDate")), true);
            options.setSortDescriptors(Some(&NSArray::from_retained_slice(&[by_date])));
            options
        };
        if options.respondsToSelector(sel!(setPrefetchAssetExtendedMetadata:)) {
            // SAFETY: `prefetchAssetExtendedMetadata` is a BOOL property of PHFetchOptions from macOS 27,
            // sent only when the object answers it (checked just above).
            let _: () = unsafe { msg_send![&*options, setPrefetchAssetExtendedMetadata: true] };
        }
        // SAFETY: a class method taking an optional fetch options object; it returns a lazy result.
        let assets = unsafe { PHAsset::fetchAssetsWithOptions(Some(&options)) };
        // SAFETY: `count` takes no arguments and returns an integer.
        let count = unsafe { assets.count() };
        Ok(Self { assets, count })
    }

    pub fn asset_count(&self) -> usize {
        self.count
    }

    /// Up to `limit` assets from `start`, oldest first.
    pub fn assets(&self, start: usize, limit: usize) -> Vec<Asset> {
        each(&self.assets, start, limit).iter().map(|a| describe(a)).collect()
    }

    /// One asset by local identifier.
    pub fn asset(&self, local_id: &str) -> Option<Asset> {
        let ids = NSArray::from_retained_slice(&[NSString::from_str(local_id)]);
        // SAFETY: a class method taking an array of NSString identifiers and optional options.
        let found = unsafe { PHAsset::fetchAssetsWithLocalIdentifiers_options(&ids, None) };
        each(&found, 0, 1).first().map(|a| describe(a))
    }

    /// User albums and folders; a folder comes before what it holds.
    pub fn albums(&self) -> Vec<Album> {
        let mut out = Vec::new();
        // SAFETY: a class method taking optional fetch options.
        let top = unsafe { PHCollection::fetchTopLevelUserCollectionsWithOptions(None) };
        walk(&top, None, 0, &mut out);
        out
    }

    /// Local identifiers of an album's assets in album order; `None` for an unknown album.
    pub fn album_asset_ids(&self, album_id: &str) -> Option<Vec<String>> {
        let ids = NSArray::from_retained_slice(&[NSString::from_str(album_id)]);
        // SAFETY: a class method taking an array of NSString identifiers and optional options.
        let found = unsafe { PHAssetCollection::fetchAssetCollectionsWithLocalIdentifiers_options(&ids, None) };
        let album = each(&found, 0, 1).into_iter().next()?;
        // SAFETY: a class method taking an asset collection and optional options.
        let assets = unsafe { PHAsset::fetchAssetsInAssetCollection_options(&album, None) };
        Some(each(&assets, 0, MAX_ALBUM_ASSETS).iter().map(|a| local_id(a)).collect())
    }
}

fn walk(result: &PHFetchResult<PHCollection>, parent: Option<&str>, depth: usize, out: &mut Vec<Album>) {
    if depth > MAX_FOLDER_DEPTH {
        return;
    }
    for item in each(result, 0, MAX_COLLECTIONS) {
        if out.len() >= MAX_COLLECTIONS {
            return;
        }
        let object: &AnyObject = &item;
        // SAFETY: `localizedTitle` takes no arguments and returns an optional NSString.
        let title = unsafe { item.localizedTitle() }.and_then(|t| text(&t));
        if let Some(album) = object.downcast_ref::<PHAssetCollection>() {
            // SAFETY: both take no arguments and return integer enums.
            let (kind, subtype) = unsafe { (album.assetCollectionType(), album.assetCollectionSubtype()) };
            if kind == PHAssetCollectionType::Album && subtype == PHAssetCollectionSubtype::AlbumRegular {
                out.push(Album { local_id: local_id(&item), title, folder: false, parent: parent.map(str::to_owned) });
            }
        } else if let Some(folder) = object.downcast_ref::<PHCollectionList>() {
            let id = local_id(&item);
            out.push(Album { local_id: id.clone(), title, folder: true, parent: parent.map(str::to_owned) });
            // SAFETY: a class method taking a collection list and optional options.
            let children = unsafe { PHCollection::fetchCollectionsInCollectionList_options(folder, None) };
            walk(&children, Some(&id), depth + 1, out);
        }
    }
}

fn describe(asset: &PHAsset) -> Asset {
    // SAFETY: each getter takes no arguments and returns the type the header declares; the optional
    // ones (dates, location) are checked for nil by the binding.
    let (media, subtypes, width, height, created, location, hidden, favourite) = unsafe {
        (
            asset.mediaType(),
            asset.mediaSubtypes(),
            asset.pixelWidth(),
            asset.pixelHeight(),
            asset.creationDate().map(|d| d.timeIntervalSince1970()),
            asset.location().map(|l| l.coordinate()),
            asset.isHidden(),
            asset.isFavorite(),
        )
    };
    let mut out = Asset {
        local_id: local_id(asset),
        kind: Some(kind(media)),
        live_photo: subtypes.contains(PHAssetMediaSubtype::PhotoLive),
        width: width as u64,
        height: height as u64,
        created: created.filter(|s| s.is_finite()),
        location: location.map(|c| (c.latitude, c.longitude)).filter(|(la, lo)| la.is_finite() && lo.is_finite()),
        hidden,
        favourite,
        ..Asset::default()
    };
    extended_metadata(asset, &mut out);
    content_type(asset, &mut out);
    // listing resources costs a fetch per asset: only when the newer properties left a gap
    if out.original_file_name.is_none() || out.uti.is_none() {
        original_resource(asset, &mut out);
    }
    out
}

/// Type of the original from `PHAsset.contentType` (macOS 26+), without listing resources.
fn content_type(asset: &PHAsset, out: &mut Asset) {
    if !asset.respondsToSelector(sel!(contentType)) {
        return;
    }
    // SAFETY: `contentType` is a PHAsset property from macOS 26 (checked above) returning a non-null
    // UTType; `identifier` is a UTType property returning a non-null NSString. Each is sent only when
    // the object answers it.
    unsafe {
        let kind: Option<Retained<NSObject>> = msg_send![asset, contentType];
        let Some(kind) = kind else { return };
        if kind.respondsToSelector(sel!(identifier)) {
            let id: Option<Retained<NSString>> = msg_send![&*kind, identifier];
            out.uti = id.and_then(|i| text(&i));
        }
    }
}

/// File name and type of the original photo or video, from the asset's resources. Fills only what
/// is still missing.
fn original_resource(asset: &PHAsset, out: &mut Asset) {
    // SAFETY: a class method taking an asset; it returns an array of resources (no fetch result).
    let resources = unsafe { PHAssetResource::assetResourcesForAsset(asset) };
    for resource in resources.iter() {
        // SAFETY: getters without arguments returning an integer enum and non-null NSStrings.
        // `uniformTypeIdentifier` is deprecated for `contentType` (a UTType, which needs another
        // binding crate) but still answers on every macOS LightCraft runs on.
        #[allow(deprecated)]
        let (kind, name, uti) = unsafe { (resource.r#type(), resource.originalFilename(), resource.uniformTypeIdentifier()) };
        if kind == PHAssetResourceType::Photo || kind == PHAssetResourceType::Video {
            if out.original_file_name.is_none() {
                out.original_file_name = text(&name);
            }
            if out.uti.is_none() {
                out.uti = text(&uti);
            }
            return;
        }
    }
}

/// Caption, keywords and original file name from `PHAssetExtendedMetadata` (macOS 27+).
fn extended_metadata(asset: &PHAsset, out: &mut Asset) {
    if !asset.respondsToSelector(sel!(extendedMetadata)) {
        return;
    }
    // SAFETY: `extendedMetadata` is a PHAsset property from macOS 27 (checked above) returning a
    // PHAssetExtendedMetadata; its `caption` and `originalFilename` are nullable NSStrings and
    // `keywords` a non-null NSArray<NSString>, per PHAssetExtendedMetadata.h. Each is sent only when
    // the object answers it.
    unsafe {
        let meta: Option<Retained<NSObject>> = msg_send![asset, extendedMetadata];
        let Some(meta) = meta else { return };
        if meta.respondsToSelector(sel!(caption)) {
            let caption: Option<Retained<NSString>> = msg_send![&*meta, caption];
            out.caption = caption.and_then(|c| text(&c));
        }
        if meta.respondsToSelector(sel!(keywords)) {
            let keywords: Option<Retained<NSArray<NSString>>> = msg_send![&*meta, keywords];
            out.keywords = Some(keywords.map(|k| k.iter().filter_map(|w| text(&w)).collect()).unwrap_or_default());
        }
        if meta.respondsToSelector(sel!(originalFilename)) {
            let name: Option<Retained<NSString>> = msg_send![&*meta, originalFilename];
            if let Some(name) = name.and_then(|n| text(&n)) {
                out.original_file_name = Some(name);
            }
        }
    }
}
