//! Read the System Photo Library through Apple's PhotoKit (issue #1622, Phase 1 task 0).
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
