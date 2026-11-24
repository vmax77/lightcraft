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

/// What the resource request's handlers pass to the waiting thread.
enum Part {
    Data(Vec<u8>),
    Progress(f64),
    /// The request ended: `None` when it succeeded, else PhotoKit's reason.
    Done(Option<String>),
}

/// The original photo or video resource of `asset`.
fn original_of(asset: &PHAsset) -> Option<Retained<PHAssetResource>> {
    // SAFETY: a class method taking an asset; it returns an array of resources (no fetch result).
    let resources = unsafe { PHAssetResource::assetResourcesForAsset(asset) };
    resources.iter().find(|r| {
        // SAFETY: a getter without arguments returning an integer enum.
        let kind = unsafe { r.r#type() };
        kind == PHAssetResourceType::Photo || kind == PHAssetResourceType::Video
    })
}

/// The resource of `asset` whose type is one of `kinds`, in that order of preference.
fn resource_of(asset: &PHAsset, kinds: &[PHAssetResourceType]) -> Option<Retained<PHAssetResource>> {
    // SAFETY: a class method taking an asset; it returns an array of resources (no fetch result).
    let resources = unsafe { PHAssetResource::assetResourcesForAsset(asset) };
    kinds.iter().find_map(|kind| {
        resources.iter().find(|r| {
            // SAFETY: a getter without arguments returning an integer enum.
            let t = unsafe { r.r#type() };
            t == *kind
        })
    })
}

/// The asset `local_id` once read access has been checked.
fn readable_asset(local_id: &str) -> Result<Option<Retained<PHAsset>>, PhotoKitError> {
    let access = access()?;
    if !access.can_read() {
        return Err(PhotoKitError::NotAllowed(access));
    }
    let ids = NSArray::from_retained_slice(&[NSString::from_str(local_id)]);
    // SAFETY: a class method taking an array of NSString identifiers and optional options.
    let found = unsafe { PHAsset::fetchAssetsWithLocalIdentifiers_options(&ids, None) };
    Ok(each(&found, 0, 1).into_iter().next())
}

pub fn edit_state(local_id: &str) -> Result<super::EditState, super::EditError> {
    let asset = readable_asset(local_id)?.ok_or(super::EditError::NotFound)?;
    // SAFETY: a getter without arguments returning an optional NSDate.
    let modified = unsafe { asset.modificationDate() }.map(|d| d.timeIntervalSince1970()).filter(|s| s.is_finite());
    let edited = resource_of(&asset, &[PHAssetResourceType::FullSizePhoto, PHAssetResourceType::AdjustmentData]).is_some();
    Ok(super::EditState { modified, edited })
}

pub fn download_original(
    local_id: &str,
    out: &mut dyn std::io::Write,
    cancel: &std::sync::atomic::AtomicBool,
    progress: &mut dyn FnMut(f64),
) -> Result<u64, super::DownloadError> {
    let asset = readable_asset(local_id)?.ok_or(super::DownloadError::NotFound)?;
    let resource = original_of(&asset).ok_or(super::DownloadError::NoOriginal)?;
    download(&resource, out, cancel, progress)
}

pub fn download_current(
    local_id: &str,
    out: &mut dyn std::io::Write,
    cancel: &std::sync::atomic::AtomicBool,
    progress: &mut dyn FnMut(f64),
) -> Result<u64, super::DownloadError> {
    let asset = readable_asset(local_id)?.ok_or(super::DownloadError::NotFound)?;
    let resource = resource_of(&asset, &[PHAssetResourceType::FullSizePhoto]).ok_or(super::DownloadError::NoEdit)?;
    download(&resource, out, cancel, progress)
}

/// Write the bytes of `resource` to `out` as they arrive (from iCloud when needed).
fn download(
    resource: &PHAssetResource,
    out: &mut dyn std::io::Write,
    cancel: &std::sync::atomic::AtomicBool,
    progress: &mut dyn FnMut(f64),
) -> Result<u64, super::DownloadError> {
    use super::DownloadError;
    use std::sync::atomic::Ordering;

    let (tx, rx) = mpsc::channel::<Part>();
    let (on_data, on_progress, on_done) = (tx.clone(), tx.clone(), tx);
    let data = RcBlock::new(move |chunk: std::ptr::NonNull<objc2_foundation::NSData>| {
        // SAFETY: PhotoKit passes a valid NSData that lives for the call; it is copied out at once.
        let bytes = unsafe { chunk.as_ref() }.to_vec();
        let _ = on_data.send(Part::Data(bytes));
    });
    let done = RcBlock::new(move |error: *mut objc2_foundation::NSError| {
        // SAFETY: the error is nil on success, else a valid NSError that lives for the call.
        let why = unsafe { error.as_ref() }.map(|e| e.localizedDescription().to_string());
        let _ = on_done.send(Part::Done(why));
    });
    let progressed = RcBlock::new(move |fraction: std::ffi::c_double| {
        let _ = on_progress.send(Part::Progress(fraction));
    });
    // SAFETY: `new` makes default options; the setters take a bool and a progress block of the
    // declared type (`void (^)(double)`), which the options object copies and keeps.
    let options = unsafe {
        let options = objc2_photos::PHAssetResourceRequestOptions::new();
        options.setNetworkAccessAllowed(true);
        options.setProgressHandler(RcBlock::as_ptr(&progressed));
        options
    };
    // SAFETY: the manager is PhotoKit's shared one; the handlers have the block signatures the
    // header declares (`void (^)(NSData *)`, `void (^)(NSError *)`), PhotoKit copies them before
    // returning, and they only send owned values on a channel (a send to a dropped receiver is ignored).
    let manager = unsafe { objc2_photos::PHAssetResourceManager::defaultManager() };
    let request =
        unsafe { manager.requestDataForAssetResource_options_dataReceivedHandler_completionHandler(resource, Some(&options), &data, &done) };

    let mut written: u64 = 0;
    let mut failure: Option<DownloadError> = None;
    let mut idle = Duration::ZERO;
    const TICK: Duration = Duration::from_millis(200);
    loop {
        if failure.is_none() && cancel.load(Ordering::Relaxed) {
            failure = Some(DownloadError::Cancelled);
            // SAFETY: cancels the request this call made, by the id PhotoKit gave it.
            unsafe { manager.cancelDataRequest(request) };
        }
        match rx.recv_timeout(TICK) {
            Ok(Part::Data(bytes)) => {
                idle = Duration::ZERO;
                if failure.is_none() {
                    match out.write_all(&bytes) {
                        Ok(()) => written = written.saturating_add(bytes.len() as u64),
                        Err(e) => {
                            failure = Some(DownloadError::Write(e.to_string()));
                            // SAFETY: as above.
                            unsafe { manager.cancelDataRequest(request) };
                        }
                    }
                }
            }
            Ok(Part::Progress(fraction)) => {
                idle = Duration::ZERO;
                if fraction.is_finite() {
                    progress(fraction.clamp(0.0, 1.0));
                }
            }
            Ok(Part::Done(why)) => {
                return match (failure, why) {
                    (Some(f), _) => Err(f),
                    (None, Some(why)) => Err(DownloadError::Failed(why)),
                    (None, None) => Ok(written),
                };
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                idle += TICK;
                // after a cancel the completion is waited for a few seconds at most
                let limit = if failure.is_some() { Duration::from_secs(5) } else { super::DOWNLOAD_IDLE };
                if idle >= limit {
                    // SAFETY: as above.
                    unsafe { manager.cancelDataRequest(request) };
                    return Err(failure.unwrap_or(DownloadError::TimedOut));
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => return Err(failure.unwrap_or(DownloadError::Failed("PhotoKit ended the request".into()))),
        }
    }
}

/// A content editing output handed from PhotoKit's completion handler to the waiting thread.
struct SendOutput(Retained<objc2_photos::PHContentEditingOutput>);

// SAFETY: the output is made in the completion handler and then used by one thread only (the one
// waiting in `save_edit`); Apple's documentation has apps fill an output on a background queue.
unsafe impl Send for SendOutput {}

/// What the editing-input request's handlers pass to the waiting thread.
enum Prepared<T> {
    Progress(f64),
    Done(Result<T, String>),
}

/// PhotoKit's reason in a content-editing-input `info` dictionary.
fn input_error(info: &objc2_foundation::NSDictionary) -> String {
    // SAFETY: the key is the NSString constant PhotoKit declares for this dictionary; the value
    // under it, when present, is an NSError (checked by the downcast).
    let error = unsafe { info.objectForKey(objc2_photos::PHContentEditingInputErrorKey) };
    error
        .as_deref()
        .and_then(|e| e.downcast_ref::<objc2_foundation::NSError>())
        .map(|e| e.localizedDescription().to_string())
        .unwrap_or_else(|| "no reason given".into())
}

/// The image asset `local_id`, once access, the thread and the asset have been checked. With
/// `edit`, its content must be one Photos lets apps edit.
fn image_asset(local_id: &str, edit: bool) -> Result<Retained<PHAsset>, super::EditError> {
    use super::EditError;
    let access = access()?;
    if !access.can_read() {
        return Err(PhotoKitError::NotAllowed(access).into());
    }
    if objc2::MainThreadMarker::new().is_some() {
        return Err(EditError::MainThread);
    }
    let ids = NSArray::from_retained_slice(&[NSString::from_str(local_id)]);
    // SAFETY: a class method taking an array of NSString identifiers and optional options.
    let found = unsafe { PHAsset::fetchAssetsWithLocalIdentifiers_options(&ids, None) };
    let asset = each(&found, 0, 1).into_iter().next().ok_or(EditError::NotFound)?;
    // SAFETY: getters taking no arguments / an integer enum, returning an enum and a bool.
    let ok = unsafe {
        asset.mediaType() == PHAssetMediaType::Image && (!edit || asset.canPerformEditOperation(objc2_photos::PHAssetEditOperation::Content))
    };
    if !ok {
        return Err(EditError::NotEditable);
    }
    Ok(asset)
}

/// Ask Photos for the content editing input of `asset` (downloading it from iCloud when needed)
/// and wait for it. `can_handle` names the adjustment format the caller reads, so Photos hands
/// over the data stored with an edit in that format. `take` runs in PhotoKit's handler with the
/// input and makes what is sent back.
fn with_input<T: Send + 'static>(
    asset: &PHAsset,
    can_handle: Option<String>,
    cancel: &std::sync::atomic::AtomicBool,
    progress: &mut dyn FnMut(f64),
    take: impl Fn(&objc2_photos::PHContentEditingInput) -> T + 'static,
) -> Result<T, super::EditError> {
    use super::EditError;
    use objc2_photos::{PHAdjustmentData, PHContentEditingInput, PHContentEditingInputRequestOptions};
    use std::sync::atomic::Ordering;

    let (tx, rx) = mpsc::channel::<Prepared<T>>();
    let on_progress = tx.clone();
    let progressed = RcBlock::new(move |fraction: std::ffi::c_double, _stop: std::ptr::NonNull<objc2::runtime::Bool>| {
        let _ = on_progress.send(Prepared::Progress(fraction));
    });
    let handler = RcBlock::new(move |input: *mut PHContentEditingInput, info: std::ptr::NonNull<objc2_foundation::NSDictionary>| {
        // SAFETY: PhotoKit passes a valid input (or nil) and a valid dictionary, both living for the call.
        let result = match unsafe { input.as_ref() } {
            Some(input) => Ok(take(input)),
            None => Err(input_error(unsafe { info.as_ref() })),
        };
        let _ = tx.send(Prepared::Done(result));
    });
    let handles = can_handle.map(|format| {
        RcBlock::new(move |data: std::ptr::NonNull<PHAdjustmentData>| {
            // SAFETY: PhotoKit passes a valid adjustment data object living for the call; its
            // `formatIdentifier` takes no arguments and returns a non-null NSString.
            let id = unsafe { data.as_ref().formatIdentifier() };
            objc2::runtime::Bool::new(id.to_string() == format)
        })
    });
    // SAFETY: `new` makes default options; the setters take a bool and blocks of the declared types
    // (`void (^)(double, BOOL *)`, `BOOL (^)(PHAdjustmentData *)`), which the options object copies
    // and keeps.
    let options = unsafe {
        let options = PHContentEditingInputRequestOptions::new();
        options.setNetworkAccessAllowed(true);
        options.setProgressHandler(Some(&progressed));
        if let Some(handles) = &handles {
            options.setCanHandleAdjustmentData(handles);
        }
        options
    };
    // SAFETY: the handler has the block signature the header declares
    // (`void (^)(PHContentEditingInput *, NSDictionary *)`), PhotoKit copies it before returning,
    // and it only sends owned values on a channel (a send to a dropped receiver is ignored).
    let request = unsafe { asset.requestContentEditingInputWithOptions_completionHandler(Some(&options), &handler) };

    const TICK: Duration = Duration::from_millis(200);
    let mut idle = Duration::ZERO;
    loop {
        if cancel.load(Ordering::Relaxed) {
            // SAFETY: cancels the request this call made, by the id PhotoKit gave it.
            unsafe { asset.cancelContentEditingInputRequest(request) };
            return Err(EditError::Cancelled);
        }
        match rx.recv_timeout(TICK) {
            Ok(Prepared::Progress(fraction)) => {
                idle = Duration::ZERO;
                if fraction.is_finite() {
                    progress(fraction.clamp(0.0, 1.0));
                }
            }
            Ok(Prepared::Done(Ok(t))) => return Ok(t),
            Ok(Prepared::Done(Err(why))) => return Err(EditError::Prepare(why)),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                idle += TICK;
                if idle >= super::PREPARE_IDLE {
                    // SAFETY: as above.
                    unsafe { asset.cancelContentEditingInputRequest(request) };
                    return Err(EditError::TimedOut);
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => return Err(EditError::Prepare("PhotoKit ended the request".into())),
        }
    }
}

pub fn read_adjustment(
    local_id: &str,
    format: &str,
    cancel: &std::sync::atomic::AtomicBool,
    progress: &mut dyn FnMut(f64),
) -> Result<Option<super::StoredAdjustment>, super::EditError> {
    let asset = image_asset(local_id, false)?;
    let wanted = format.to_string();
    with_input(&asset, Some(format.to_string()), cancel, progress, move |input| {
        // SAFETY: `adjustmentData` takes no arguments and returns an optional object; its getters
        // take no arguments and return non-null NSStrings and NSData, copied out at once.
        let data = unsafe { input.adjustmentData() }?;
        let (id, version, bytes) = unsafe { (data.formatIdentifier().to_string(), data.formatVersion().to_string(), data.data().to_vec()) };
        // Photos hands it over only when `can_handle` said yes; checked again all the same
        (id == wanted).then_some(super::StoredAdjustment { version, data: bytes })
    })
}

/// Prepare the edit of `item` for a change: the asset's editing output, holding the rendered
/// photo and the adjustment data.
fn prepare_edit(
    item: &super::EditItem,
    cancel: &std::sync::atomic::AtomicBool,
    progress: &mut dyn FnMut(f64),
) -> Result<(Retained<PHAsset>, Retained<objc2_photos::PHContentEditingOutput>), super::EditError> {
    use super::EditError;
    use objc2::AllocAnyThread;
    use objc2_photos::{PHAdjustmentData, PHContentEditingOutput};

    let asset = image_asset(item.local_id, true)?;
    let output = with_input(&asset, None, cancel, progress, |input| {
        // SAFETY: an initializer taking the valid input PhotoKit passed; the output is retained.
        SendOutput(unsafe { PHContentEditingOutput::initWithContentEditingInput(PHContentEditingOutput::alloc(), input) })
    })?
    .0;
    // SAFETY: a getter without arguments returning a non-null file URL inside Photos' editing area.
    let url = unsafe { output.renderedContentURL() };
    let target =
        url.path().map(|p| std::path::PathBuf::from(p.to_string())).ok_or_else(|| EditError::Write("Photos gave no file to write to".into()))?;
    std::fs::copy(item.rendered, &target).map_err(|e| EditError::Write(e.to_string()))?;
    // SAFETY: an initializer taking two NSStrings and NSData (copied by the object); the setter takes
    // the adjustment data the output keeps.
    unsafe {
        let data = objc2_foundation::NSData::with_bytes(item.adjustment.data);
        let adjustment = PHAdjustmentData::initWithFormatIdentifier_formatVersion_data(
            PHAdjustmentData::alloc(),
            &NSString::from_str(item.adjustment.format),
            &NSString::from_str(item.adjustment.version),
            &data,
        );
        output.setAdjustmentData(Some(&adjustment));
    }
    Ok((asset, output))
}

pub fn save_edits(
    items: &[super::EditItem],
    checked: Vec<Result<(), super::EditError>>,
    cancel: &std::sync::atomic::AtomicBool,
    progress: &mut dyn FnMut(f64),
) -> Vec<Result<(), super::EditError>> {
    use super::EditError;
    use objc2_photos::PHAssetChangeRequest;
    use std::sync::atomic::Ordering;

    let n = items.len().max(1) as f64;
    let mut results = checked;
    let mut prepared = Vec::new();
    for (i, item) in items.iter().enumerate() {
        if results.get(i).is_some_and(Result::is_err) {
            continue;
        }
        let mut one = |f: f64| progress((i as f64 + f.clamp(0.0, 1.0)) / n);
        match prepare_edit(item, cancel, &mut one) {
            Ok(p) => prepared.push((i, p)),
            Err(e) => {
                if let Some(r) = results.get_mut(i) {
                    *r = Err(e);
                }
            }
        }
        progress((i as f64 + 1.0) / n);
    }
    if prepared.is_empty() {
        return results;
    }
    let outcome = if cancel.load(Ordering::Relaxed) {
        Err(EditError::Cancelled)
    } else {
        let edits: Vec<_> = prepared.iter().map(|(_, p)| p.clone()).collect();
        let change = RcBlock::new(move || {
            for (asset, output) in &edits {
                // SAFETY: runs inside `performChangesAndWait:`, the only place a change request may
                // be made; the assets and outputs are retained by the block.
                unsafe { PHAssetChangeRequest::changeRequestForAsset(asset).setContentEditingOutput(Some(output)) };
            }
        });
        // SAFETY: the change block has the declared type (`void (^)(void)`) and is copied by
        // PhotoKit; this waits on a worker thread (checked when the assets were prepared), as the
        // method requires.
        let done = unsafe { PHPhotoLibrary::sharedPhotoLibrary().performChangesAndWait_error(RcBlock::as_ptr(&change)) };
        done.map_err(|e| {
            if e.code() == objc2_photos::PHPhotosError::UserCancelled.0 {
                EditError::Declined
            } else {
                EditError::Failed(e.localizedDescription().to_string())
            }
        })
    };
    for (i, _) in prepared {
        if let Some(r) = results.get_mut(i) {
            *r = outcome.clone();
        }
    }
    results
}
