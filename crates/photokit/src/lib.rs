//! Read the System Photo Library through Apple's PhotoKit (issue #1622, Phase 1 task 0), save an
//! edit back onto one of its photos ([`save_edit`]), and read back what an app stored with the
//! edit Photos shows ([`read_adjustment`]).
//!
//! PhotoKit is the supported way to read the library Photos uses as its System Photo Library. It
//! needs the user's permission (System Settings › Privacy & Security › Photos), and a process that
//! touches it without an `NSPhotoLibraryUsageDescription` in its `Info.plist` is killed by macOS.
//! So every entry point first checks that key in the main bundle and answers
//! [`PhotoKitError::NoUsageDescription`] without calling PhotoKit when it is missing (a bare `cargo
//! run` binary or a test). A packaged app also needs the hardened-runtime entitlement
//! `com.apple.security.personal-information.photos-library` (packaging/macos/entitlements.plist).
//!
//! PhotoKit does not give a title, the original's path, or whether the original is on this Mac,
//! and before macOS 27 it gives no caption or keywords: callers fill those from `Photos.sqlite`
//! (`lightcraft_engine::apple_photos_library`). On macOS 27 and later the caption, keywords and
//! original file name come from `PHAssetExtendedMetadata`, which the `objc2-photos` bindings don't
//! have yet, so it is sent by selector after checking that the asset answers it.
//!
//! Whitespace-only text (some cameras write a caption of 31 spaces) is treated as no text.
//!
//! The Objective-C calls are in the `imp` module, the only place besides `crates/sysmem` and
//! `crates/wincolor` where LightCraft allows `unsafe`. The API is safe and plain data; on other
//! platforms every call answers [`PhotoKitError::NotMacOs`]. Indexes into PhotoKit results are
//! bounded by their counts (an out-of-range index raises an Objective-C exception, which can't be
//! caught here), and folder walks are bounded in depth and size.
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

/// Deepest folder nesting walked when listing albums.
pub const MAX_FOLDER_DEPTH: usize = 64;
/// Most albums and folders listed.
pub const MAX_COLLECTIONS: usize = 100_000;
/// Most asset ids returned for one album.
pub const MAX_ALBUM_ASSETS: usize = 10_000_000;

/// What the user allowed (`PHAuthorizationStatus` for read-write access).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    /// Not asked yet: [`request_access`] shows the prompt.
    NotDetermined,
    /// A policy (parental controls, MDM) forbids access.
    Restricted,
    Denied,
    Authorized,
    /// Only photos the user picked.
    Limited,
    /// A value this crate does not know.
    Other(isize),
}

impl Access {
    /// Whether the library can be read (all of it, or the user's picks).
    pub fn can_read(self) -> bool {
        matches!(self, Self::Authorized | Self::Limited)
    }

    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    fn from_raw(raw: isize) -> Self {
        match raw {
            0 => Self::NotDetermined,
            1 => Self::Restricted,
            2 => Self::Denied,
            3 => Self::Authorized,
            4 => Self::Limited,
            n => Self::Other(n),
        }
    }
}

/// Why PhotoKit can't be used.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PhotoKitError {
    /// PhotoKit exists only on macOS.
    NotMacOs,
    /// The running program has no `NSPhotoLibraryUsageDescription`, so macOS would kill it.
    NoUsageDescription,
    /// The user (or a policy) has not allowed reading the library.
    NotAllowed(Access),
}

impl std::fmt::Display for PhotoKitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotMacOs => f.write_str("Apple Photos is only available on macOS"),
            Self::NoUsageDescription => f.write_str("this build of LightCraft can't ask for access to Photos (no NSPhotoLibraryUsageDescription)"),
            Self::NotAllowed(Access::NotDetermined) => f.write_str("LightCraft has not been given access to Photos yet"),
            Self::NotAllowed(_) => f.write_str("LightCraft is not allowed to read Photos. Allow it in System Settings › Privacy & Security › Photos"),
        }
    }
}

impl std::error::Error for PhotoKitError {}

/// Why an original could not be fetched ([`download_original`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DownloadError {
    /// PhotoKit can't be used (see [`PhotoKitError`]).
    PhotoKit(PhotoKitError),
    /// No asset has this identifier in the System Photo Library.
    NotFound,
    /// The asset has no original photo or video resource.
    NoOriginal,
    /// The asset has no edited version ([`download_current`]).
    NoEdit,
    /// Stopped by the caller.
    Cancelled,
    /// Nothing arrived for [`DOWNLOAD_IDLE`].
    TimedOut,
    /// PhotoKit's reason (offline, iCloud error, …).
    Failed(String),
    /// Writing what arrived failed.
    Write(String),
}

impl std::fmt::Display for DownloadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PhotoKit(e) => e.fmt(f),
            Self::NotFound => f.write_str("the photo is not in the System Photo Library"),
            Self::NoOriginal => f.write_str("Photos has no original for this item"),
            Self::NoEdit => f.write_str("Photos has no edited version of this item"),
            Self::Cancelled => f.write_str("the download was cancelled"),
            Self::TimedOut => f.write_str("the download from iCloud stalled"),
            Self::Failed(why) => write!(f, "Photos could not fetch the original: {why}"),
            Self::Write(why) => write!(f, "the original could not be saved: {why}"),
        }
    }
}

impl std::error::Error for DownloadError {}

impl From<PhotoKitError> for DownloadError {
    fn from(e: PhotoKitError) -> Self {
        Self::PhotoKit(e)
    }
}

/// How long [`download_original`] waits for the next part before giving up.
pub const DOWNLOAD_IDLE: std::time::Duration = std::time::Duration::from_secs(300);

/// Fetch the original photo or video of the asset `local_id` (`<UUID>/L0/001`), from iCloud when it
/// is not on this Mac, writing its bytes to `out` as they arrive. Photos writes nothing for it but
/// its own cache. `progress` is told the fraction downloaded (0–1, on this thread); setting
/// `cancel` stops it. Returns the number of bytes written. Call it from a worker thread.
pub fn download_original(
    local_id: &str,
    out: &mut dyn std::io::Write,
    cancel: &std::sync::atomic::AtomicBool,
    progress: &mut dyn FnMut(f64),
) -> Result<u64, DownloadError> {
    imp::download_original(local_id, out, cancel, progress)
}

/// Largest adjustment data [`save_edit`] stores with an edit (Photos keeps it with the asset).
pub const MAX_ADJUSTMENT_BYTES: usize = 1 << 20;

/// How long [`save_edit`] waits for Photos to prepare the photo (downloading it from iCloud when it
/// is not on this Mac) without hearing from it.
pub const PREPARE_IDLE: std::time::Duration = std::time::Duration::from_secs(300);

/// What is stored with an edit saved to Photos ([`save_edit`]): Photos keeps it with the asset, so
/// the app that made the edit can recognise it later. Other apps see only the rendered photo.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Adjustment<'a> {
    /// Reverse-DNS id of the format (`PHAdjustmentData.formatIdentifier`).
    pub format: &'a str,
    pub version: &'a str,
    /// At most [`MAX_ADJUSTMENT_BYTES`].
    pub data: &'a [u8],
}

/// Why an edit could not be saved to Photos ([`save_edit`]) or read from it ([`read_adjustment`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EditError {
    /// PhotoKit can't be used (see [`PhotoKitError`]).
    PhotoKit(PhotoKitError),
    /// Called on the main thread, where PhotoKit's answers arrive: it would never return.
    MainThread,
    /// No asset has this identifier in the System Photo Library (or, with limited access, the
    /// user did not pick it).
    NotFound,
    /// Photos does not let this asset's content be edited (a video, or a shared or synced photo).
    NotEditable,
    /// The adjustment data (saved or read) is larger than [`MAX_ADJUSTMENT_BYTES`].
    TooLarge,
    /// The rendered photo is missing or empty.
    NoRender,
    /// Stopped by the caller.
    Cancelled,
    /// Photos did not answer for [`PREPARE_IDLE`].
    TimedOut,
    /// Photos could not prepare the photo for editing (offline, iCloud error, …).
    Prepare(String),
    /// Copying the rendered photo to where Photos wants it failed.
    Write(String),
    /// The user did not allow LightCraft to change the photo.
    Declined,
    /// Photos refused the change.
    Failed(String),
}

impl std::fmt::Display for EditError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PhotoKit(e) => e.fmt(f),
            Self::MainThread => f.write_str("Photos can't be asked from the main thread"),
            Self::NotFound => f.write_str("the photo is not in the System Photo Library"),
            Self::NotEditable => f.write_str("Photos does not allow this item to be edited"),
            Self::TooLarge => f.write_str("the edit's settings are too large for Photos"),
            Self::NoRender => f.write_str("the edited photo was not rendered"),
            Self::Cancelled => f.write_str("cancelled"),
            Self::TimedOut => f.write_str("Photos did not answer"),
            Self::Prepare(why) => write!(f, "Photos could not prepare the photo: {why}"),
            Self::Write(why) => write!(f, "the edited photo could not be handed to Photos: {why}"),
            Self::Declined => f.write_str("LightCraft was not allowed to change the photo in Photos"),
            Self::Failed(why) => write!(f, "Photos did not save the edit: {why}"),
        }
    }
}

impl std::error::Error for EditError {}

impl From<PhotoKitError> for EditError {
    fn from(e: PhotoKitError) -> Self {
        Self::PhotoKit(e)
    }
}

/// Replace what Photos shows for the asset `local_id` (`<UUID>/L0/001`) with the JPEG at
/// `rendered`, keeping `adjustment` with it. Photos keeps the original: its Revert to Original
/// brings it back. macOS asks the user to allow each change. The JPEG must be upright (EXIF
/// orientation 1). `progress` is told the fraction of an iCloud download Photos needs first;
/// setting `cancel` stops before the change is made. Call it from a worker thread: PhotoKit
/// answers on the main thread.
pub fn save_edit(
    local_id: &str,
    rendered: &std::path::Path,
    adjustment: &Adjustment,
    cancel: &std::sync::atomic::AtomicBool,
    progress: &mut dyn FnMut(f64),
) -> Result<(), EditError> {
    if adjustment.data.len() > MAX_ADJUSTMENT_BYTES {
        return Err(EditError::TooLarge);
    }
    if !std::fs::metadata(rendered).is_ok_and(|m| m.is_file() && m.len() > 0) {
        return Err(EditError::NoRender);
    }
    imp::save_edit(local_id, rendered, adjustment, cancel, progress)
}

/// What an app stored with the edit Photos shows for an asset ([`read_adjustment`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredAdjustment {
    pub version: String,
    pub data: Vec<u8>,
}

/// What `format`'s app stored with the edit Photos shows for the asset `local_id`, or `None` when
/// the asset is unedited or its current edit was made by another app (or in Photos). Photos may
/// have to download the asset from iCloud first: `progress` is told the fraction, setting
/// `cancel` stops it. Nothing is changed. Call it from a worker thread: PhotoKit answers on the
/// main thread.
pub fn read_adjustment(
    local_id: &str,
    format: &str,
    cancel: &std::sync::atomic::AtomicBool,
    progress: &mut dyn FnMut(f64),
) -> Result<Option<StoredAdjustment>, EditError> {
    let found = imp::read_adjustment(local_id, format, cancel, progress)?;
    match found {
        Some(a) if a.data.len() > MAX_ADJUSTMENT_BYTES => Err(EditError::TooLarge),
        found => Ok(found),
    }
}

/// Whether Photos shows an edit of an asset, and when the asset last changed ([`edit_state`]).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct EditState {
    /// `PHAsset.modificationDate`, seconds since 1970-01-01 UTC. Any change counts (an edit, a
    /// favourite, a title), so it says when to look again, not what changed.
    pub modified: Option<f64>,
    /// Photos has an edited version of it (an adjustment data or full-size photo resource).
    pub edited: bool,
}

/// [`EditState`] of the asset `local_id`, without downloading anything.
pub fn edit_state(local_id: &str) -> Result<EditState, EditError> {
    imp::edit_state(local_id)
}

/// Fetch the edited version of the asset `local_id` as Photos shows it (its full-size photo
/// resource), from iCloud when it is not on this Mac, writing its bytes to `out` as they arrive.
/// [`DownloadError::NoEdit`] for an unedited asset. Otherwise as [`download_original`].
pub fn download_current(
    local_id: &str,
    out: &mut dyn std::io::Write,
    cancel: &std::sync::atomic::AtomicBool,
    progress: &mut dyn FnMut(f64),
) -> Result<u64, DownloadError> {
    imp::download_current(local_id, out, cancel, progress)
}

/// Photo, video or something else.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MediaKind {
    Image,
    Video,
    Audio,
    Unknown,
}

/// One asset as PhotoKit describes it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Asset {
    /// PhotoKit's local identifier, `<UUID>/L0/001`.
    pub local_id: String,
    pub kind: Option<MediaKind>,
    pub live_photo: bool,
    pub width: u64,
    pub height: u64,
    /// Capture date, seconds since 1970-01-01 UTC.
    pub created: Option<f64>,
    /// `(latitude, longitude)` in degrees.
    pub location: Option<(f64, f64)>,
    pub hidden: bool,
    pub favourite: bool,
    /// Uniform Type Identifier of the original, when PhotoKit lists its resources.
    pub uti: Option<String>,
    pub original_file_name: Option<String>,
    /// From `PHAssetExtendedMetadata` (macOS 27+); `None` where PhotoKit can't say.
    pub caption: Option<String>,
    /// From `PHAssetExtendedMetadata` (macOS 27+); `None` where PhotoKit can't say.
    pub keywords: Option<Vec<String>>,
}

/// A user album or folder.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Album {
    /// PhotoKit's local identifier, `<UUID>/L0/040`.
    pub local_id: String,
    pub title: Option<String>,
    pub folder: bool,
    /// Local identifier of the folder it is in; `None` at the top level.
    pub parent: Option<String>,
}

/// `s` unless it is empty or only whitespace (some cameras write a caption of 31 spaces).
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn non_blank(s: String) -> Option<String> {
    (!s.trim().is_empty()).then_some(s)
}

/// Whether this macOS has `PHAssetExtendedMetadata` (caption, keywords).
pub fn has_extended_metadata() -> bool {
    imp::has_extended_metadata()
}

/// What the user has allowed, without asking.
pub fn access() -> Result<Access, PhotoKitError> {
    imp::access()
}

/// Ask for read-write access if the user has not answered yet, and wait for the answer. macOS shows
/// its prompt only once; afterwards this answers at once. Call it from a worker thread.
pub fn request_access() -> Result<Access, PhotoKitError> {
    imp::request_access()
}

pub use imp::Library;

#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
mod imp;

#[cfg(not(target_os = "macos"))]
mod imp {
    use super::{Access, Album, Asset, PhotoKitError};

    pub fn has_extended_metadata() -> bool {
        false
    }

    pub fn access() -> Result<Access, PhotoKitError> {
        Err(PhotoKitError::NotMacOs)
    }

    pub fn request_access() -> Result<Access, PhotoKitError> {
        Err(PhotoKitError::NotMacOs)
    }

    pub fn download_original(
        _: &str,
        _: &mut dyn std::io::Write,
        _: &std::sync::atomic::AtomicBool,
        _: &mut dyn FnMut(f64),
    ) -> Result<u64, super::DownloadError> {
        Err(PhotoKitError::NotMacOs.into())
    }

    pub fn save_edit(
        _: &str,
        _: &std::path::Path,
        _: &super::Adjustment,
        _: &std::sync::atomic::AtomicBool,
        _: &mut dyn FnMut(f64),
    ) -> Result<(), super::EditError> {
        Err(PhotoKitError::NotMacOs.into())
    }

    pub fn edit_state(_: &str) -> Result<super::EditState, super::EditError> {
        Err(PhotoKitError::NotMacOs.into())
    }

    pub fn download_current(
        _: &str,
        _: &mut dyn std::io::Write,
        _: &std::sync::atomic::AtomicBool,
        _: &mut dyn FnMut(f64),
    ) -> Result<u64, super::DownloadError> {
        Err(PhotoKitError::NotMacOs.into())
    }

    pub fn read_adjustment(
        _: &str,
        _: &str,
        _: &std::sync::atomic::AtomicBool,
        _: &mut dyn FnMut(f64),
    ) -> Result<Option<super::StoredAdjustment>, super::EditError> {
        Err(PhotoKitError::NotMacOs.into())
    }

    /// The System Photo Library; never opens off macOS.
    #[derive(Debug)]
    pub struct Library(());

    impl Library {
        pub fn open() -> Result<Self, PhotoKitError> {
            Err(PhotoKitError::NotMacOs)
        }
        pub fn asset_count(&self) -> usize {
            0
        }
        pub fn assets(&self, _start: usize, _limit: usize) -> Vec<Asset> {
            Vec::new()
        }
        pub fn asset(&self, _local_id: &str) -> Option<Asset> {
            None
        }
        pub fn albums(&self) -> Vec<Album> {
            Vec::new()
        }
        pub fn album_asset_ids(&self, _local_id: &str) -> Option<Vec<String>> {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn access_values_map() {
        assert_eq!(Access::from_raw(3), Access::Authorized);
        assert_eq!(Access::from_raw(4), Access::Limited);
        assert_eq!(Access::from_raw(99), Access::Other(99));
        assert!(Access::Limited.can_read() && Access::Authorized.can_read());
        assert!(!Access::Denied.can_read() && !Access::NotDetermined.can_read() && !Access::Other(9).can_read());
    }

    /// A test binary has no Info.plist: PhotoKit must not be touched (macOS would kill the process).
    #[test]
    fn a_binary_without_a_usage_description_never_calls_photokit() {
        let expected = if cfg!(target_os = "macos") { PhotoKitError::NoUsageDescription } else { PhotoKitError::NotMacOs };
        assert_eq!(access(), Err(expected));
        assert_eq!(request_access(), Err(expected));
        assert_eq!(Library::open().err(), Some(expected));
        let mut out = Vec::new();
        let r = download_original("A/L0/001", &mut out, &std::sync::atomic::AtomicBool::new(false), &mut |_| {});
        assert_eq!(r, Err(DownloadError::PhotoKit(expected)));
        assert!(out.is_empty());
    }

    /// Checked before PhotoKit is touched, on every platform.
    #[test]
    fn a_save_checks_its_inputs_first() {
        let dir = std::env::temp_dir().join(format!("lightcraft-photokit-save-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (cancel, mut progress) = (std::sync::atomic::AtomicBool::new(false), |_: f64| {});
        let big = vec![0u8; MAX_ADJUSTMENT_BYTES + 1];
        let too_big = Adjustment { format: "x", version: "1", data: &big };
        let jpeg = dir.join("a.jpg");
        std::fs::write(&jpeg, b"\xff\xd8").unwrap();
        assert_eq!(save_edit("A/L0/001", &jpeg, &too_big, &cancel, &mut progress), Err(EditError::TooLarge));
        let small = Adjustment { format: "x", version: "1", data: b"{}" };
        let empty = dir.join("empty.jpg");
        std::fs::write(&empty, b"").unwrap();
        assert_eq!(save_edit("A/L0/001", &empty, &small, &cancel, &mut progress), Err(EditError::NoRender));
        assert_eq!(save_edit("A/L0/001", &dir.join("missing.jpg"), &small, &cancel, &mut progress), Err(EditError::NoRender));
        // a test binary has no usage description: PhotoKit is never reached
        let expected = if cfg!(target_os = "macos") { PhotoKitError::NoUsageDescription } else { PhotoKitError::NotMacOs };
        assert_eq!(save_edit("A/L0/001", &jpeg, &small, &cancel, &mut progress), Err(EditError::PhotoKit(expected)));
        assert_eq!(read_adjustment("A/L0/001", "x", &cancel, &mut progress), Err(EditError::PhotoKit(expected)));
        assert_eq!(edit_state("A/L0/001"), Err(EditError::PhotoKit(expected)));
        let mut out = Vec::new();
        assert_eq!(download_current("A/L0/001", &mut out, &cancel, &mut progress), Err(DownloadError::PhotoKit(expected)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn blank_text_is_no_text() {
        assert_eq!(non_blank(" ".repeat(31)), None);
        assert_eq!(non_blank("\t\n".into()), None);
        assert_eq!(non_blank(String::new()), None);
        assert_eq!(non_blank(" Harbour ".into()).as_deref(), Some(" Harbour "));
    }

    #[test]
    fn errors_read_as_sentences() {
        assert!(PhotoKitError::NotAllowed(Access::Denied).to_string().contains("System Settings"));
        assert!(PhotoKitError::NoUsageDescription.to_string().contains("NSPhotoLibraryUsageDescription"));
    }
}
