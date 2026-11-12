//! Read Apple Photos libraries (issue #1622, Phase 1): assets, albums, folders and keywords.
//!
//! [`PhotosSource`] is what the rest of LightCraft sees. [`PhotoKitSource`] reads the System Photo
//! Library through Apple's PhotoKit (with the user's permission). [`SqliteSource`] is the backend for any
//! `.photoslibrary` package: it reads `database/Photos.sqlite` and its committed WAL frames with the
//! clean-room page reader in [`crate::lightroom_sqlite`]. It never writes inside the package: no
//! SQLite runtime, no lock or `-shm` file, no sidecars.
//!
//! `Photos.sqlite` is undocumented and changes with macOS releases. The table and column names here
//! come from observing libraries (schema and aggregate counts only) and public write-ups. Every column
//! is optional: a missing one leaves its field empty. Photos 5 (macOS 10.15) names the asset table
//! `ZGENERICASSET`; Photos 6 and later name it `ZASSET`. A database with neither is refused with
//! [`PhotosError::UnsupportedVersion`].
//!
//! Values that come from the database are treated as hostile: UUIDs, file names and directories are
//! checked before they become paths, dates and coordinates are range-checked, and album parent
//! chains are cut where they loop.

use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use chrono::{DateTime, FixedOffset, Utc};

use crate::lightroom_sqlite::{Database, FileSource, LiveTable, Record, Value, committed_wal_len};

/// File name of the System Photo Library that Photos makes in `~/Pictures`.
pub const SYSTEM_LIBRARY_NAME: &str = "Photos Library.photoslibrary";

/// Most bytes of a WAL read into memory. Photos checkpoints long before this.
const MAX_WAL_BYTES: u64 = 1 << 30;
/// Longest UUID accepted from the database (Photos uses 36-character UUIDs).
const MAX_UUID_LEN: usize = 64;
/// Deepest album folder nesting followed when checking parent chains.
const MAX_FOLDER_DEPTH: usize = 64;
/// Most `.photoslibrary` packages listed from one folder.
const MAX_LISTED_LIBRARIES: usize = 256;
/// Seconds from the Unix epoch to 2001-01-01T00:00:00Z, the Core Data reference date.
const CORE_DATA_EPOCH: i64 = 978_307_200;

/// `ZGENERICALBUM.ZKIND` of a user album.
const ALBUM_KIND_ALBUM: i64 = 2;
/// `ZGENERICALBUM.ZKIND` of a user folder.
const ALBUM_KIND_FOLDER: i64 = 4000;
/// `ZINTERNALRESOURCE.ZDATASTORESUBTYPE` of an original file.
const RESOURCE_ORIGINAL: i64 = 1;
/// `ZINTERNALRESOURCE.ZRESOURCETYPE` of a Live Photo's video (stored as `<UUID>_3.mov`).
const RESOURCE_LIVE_VIDEO: i64 = 3;
/// `ZINTERNALRESOURCE.ZRESOURCETYPE` of the other original of a RAW+JPEG pair (`<UUID>_4.<ext>`).
const RESOURCE_ALTERNATE: i64 = 4;
/// Most entries read from one `originals/<dir>` folder when looking for companions.
const MAX_DIR_ENTRIES: usize = 2_000_000;

/// Why a Photos library could not be read.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PhotosError {
    #[error("{} is not a Photos library: it has no database/Photos.sqlite", .0.display())]
    NotALibrary(PathBuf),
    #[error("macOS did not allow reading {}. Give LightCraft Full Disk Access in System Settings › Privacy & Security", .0.display())]
    PermissionDenied(PathBuf),
    #[error("this Photos library's database is not a version LightCraft can read ({0})")]
    UnsupportedVersion(String),
    #[error("the Photos library changed while it was being read; try again")]
    Changed,
    #[error("reading the Photos library was cancelled")]
    Cancelled,
    /// PhotoKit refused: the user or a policy has not allowed access (System Settings › Privacy & Security › Photos).
    #[error("{0}")]
    NotAllowed(String),
    /// PhotoKit can't be used here (not macOS, or a build without a Photos usage description).
    #[error("{0}")]
    Unavailable(String),
    #[error("cannot read the Photos library: {0}")]
    Unreadable(String),
}

/// Which `Photos.sqlite` layout a library uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SchemaVersion {
    /// Photos 5 (macOS 10.15): assets in `ZGENERICASSET`.
    Photos5,
    /// Photos 6 and later (macOS 11 onwards): assets in `ZASSET`.
    Photos6OrLater,
}

/// Photo or video, from `ZKIND`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AssetKind {
    Photo,
    Video,
    /// A kind this reader does not know, kept as stored.
    Other(i64),
}

/// Whether an asset's original file is on this Mac.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Availability {
    Local,
    /// Only in iCloud: Photos has to download it first.
    CloudOnly,
    /// The library does not say.
    Unknown,
}

/// A file Photos keeps with an asset's original: the other half of a RAW+JPEG pair, or a Live
/// Photo's video. Photos stores it next to the original as `<UUID>_<resource type>.<ext>`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Companion {
    /// The asset has none.
    None,
    /// On this Mac, at this path inside the package.
    Local(PathBuf),
    /// Only in iCloud: Photos has to download it first.
    CloudOnly,
    /// The library does not say, or says it is here but the file was not found.
    Unknown,
}

/// A point on the Earth, in degrees.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GeoPoint {
    pub latitude: f64,
    pub longitude: f64,
}

/// One photo or video in a Photos library. Photos' own edits are ignored: this is always the original.
#[derive(Clone, Debug, PartialEq)]
pub struct PhotosAsset {
    /// Photos' id for the asset, stable across launches.
    pub uuid: String,
    /// Name of the original inside the library package.
    pub file_name: Option<String>,
    /// Name the file had when it was imported into Photos.
    pub original_file_name: Option<String>,
    /// Where the original is inside the package, when the layout is the usual
    /// `originals/<directory>/<file>`. The file may be missing ([`Self::availability`]).
    pub original_path: Option<PathBuf>,
    /// Capture date, in the time zone Photos recorded (UTC when it recorded none).
    pub created: Option<DateTime<FixedOffset>>,
    pub favourite: bool,
    pub hidden: bool,
    /// In Photos' Recently Deleted.
    pub trashed: bool,
    pub kind: AssetKind,
    /// A Live Photo (its video is a separate resource).
    pub live_photo: bool,
    /// Uniform Type Identifier of the original, for example `public.heic`.
    pub uti: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    /// Size of the original in bytes.
    pub original_size: Option<u64>,
    pub location: Option<GeoPoint>,
    pub title: Option<String>,
    pub caption: Option<String>,
    pub keywords: Vec<String>,
    pub availability: Availability,
    /// The other original of a RAW+JPEG pair. Photos keeps both as one asset; so does LightCraft.
    pub paired_original: Companion,
    /// The video of a Live Photo.
    pub live_video: Companion,
}

impl PhotosAsset {
    /// The original files on this Mac: the main original and, for a RAW+JPEG pair, the other half.
    /// Photos' edits (`.aae` files, rendered versions) are never included.
    pub fn originals(&self) -> Vec<&Path> {
        let paired = match &self.paired_original {
            Companion::Local(path) => Some(path.as_path()),
            _ => None,
        };
        self.original_path.as_deref().into_iter().chain(paired).collect()
    }
}

/// Album or folder.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AlbumKind {
    Album,
    Folder,
}

/// A user album or folder. Smart albums, shared albums and Photos' own collections are not listed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PhotosAlbum {
    pub uuid: String,
    pub title: String,
    pub kind: AlbumKind,
    /// UUID of the folder this is in; `None` at the top level.
    pub parent: Option<String>,
}

/// A source of Photos assets and albums. Assets come in pages, so a backend need not hold them all.
pub trait PhotosSource {
    /// Number of assets, including hidden and trashed ones.
    fn asset_count(&self) -> usize;
    /// Up to `limit` assets starting at `start`, in a stable order.
    fn assets(&self, start: usize, limit: usize) -> Result<Vec<PhotosAsset>, PhotosError>;
    /// One asset by UUID.
    fn asset(&self, uuid: &str) -> Result<Option<PhotosAsset>, PhotosError>;
    /// Albums and folders. A parent is listed before its children.
    fn albums(&self) -> Result<Vec<PhotosAlbum>, PhotosError>;
    /// UUIDs of an album's assets in the album's order; `None` for an unknown album.
    fn album_assets(&self, album_uuid: &str) -> Result<Option<Vec<String>>, PhotosError>;
    /// Things that were skipped while reading, for a log. Empty when everything was read.
    fn warnings(&self) -> &[String];
}

/// A `.photoslibrary` package that has a `database/Photos.sqlite`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PhotosLibrary {
    root: PathBuf,
}

impl PhotosLibrary {
    /// The library at `root`, which must hold a regular `database/Photos.sqlite`.
    pub fn at(root: &Path) -> Result<Self, PhotosError> {
        let database = root.join("database").join("Photos.sqlite");
        match std::fs::metadata(&database) {
            Ok(meta) if meta.is_file() => Ok(Self { root: root.to_path_buf() }),
            Ok(_) => Err(PhotosError::NotALibrary(root.to_path_buf())),
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => Err(PhotosError::PermissionDenied(root.to_path_buf())),
            Err(_) => Err(PhotosError::NotALibrary(root.to_path_buf())),
        }
    }

    /// Where Photos makes the System Photo Library for the user whose home is `home`. The user may
    /// have chosen another library as the System Photo Library; this is only the default place.
    pub fn default_system_path(home: &Path) -> PathBuf {
        home.join("Pictures").join(SYSTEM_LIBRARY_NAME)
    }

    /// `.photoslibrary` packages directly inside `dir`, sorted by path. Unreadable entries are skipped.
    pub fn list_in(dir: &Path) -> Vec<PathBuf> {
        let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
        let mut found: Vec<PathBuf> = entries
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|ext| ext.eq_ignore_ascii_case("photoslibrary")) && path.is_dir())
            .take(MAX_LISTED_LIBRARIES)
            .collect();
        found.sort();
        found
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn database_path(&self) -> PathBuf {
        self.root.join("database").join("Photos.sqlite")
    }
}

/// Photos library read from `Photos.sqlite`: a snapshot taken by [`SqliteSource::open`].
#[derive(Debug)]
pub struct SqliteSource {
    library: PhotosLibrary,
    schema: SchemaVersion,
    assets: Vec<PhotosAsset>,
    by_uuid: HashMap<String, usize>,
    albums: Vec<PhotosAlbum>,
    album_assets: HashMap<String, Vec<String>>,
    warnings: Vec<String>,
}

impl SqliteSource {
    /// Read the library. `cancel` is checked between tables. Read-only: the database and its WAL
    /// are opened for reading, and a change while reading gives [`PhotosError::Changed`].
    pub fn open(library: &PhotosLibrary, cancel: &AtomicBool) -> Result<Self, PhotosError> {
        let snapshot = Snapshot::open(&library.database_path())?;
        let reader = TableReader { db: &snapshot.db, tables: snapshot.db.live_tables().map_err(PhotosError::Unreadable)?, cancel };
        let source = read_library(library, &reader)?;
        snapshot.ensure_unchanged()?;
        Ok(source)
    }

    pub fn library(&self) -> &PhotosLibrary {
        &self.library
    }

    pub fn schema(&self) -> SchemaVersion {
        self.schema
    }
}

impl PhotosSource for SqliteSource {
    fn asset_count(&self) -> usize {
        self.assets.len()
    }

    fn assets(&self, start: usize, limit: usize) -> Result<Vec<PhotosAsset>, PhotosError> {
        let end = start.saturating_add(limit).min(self.assets.len());
        Ok(self.assets.get(start..end).map(<[PhotosAsset]>::to_vec).unwrap_or_default())
    }

    fn asset(&self, uuid: &str) -> Result<Option<PhotosAsset>, PhotosError> {
        Ok(self.by_uuid.get(uuid).and_then(|&i| self.assets.get(i)).cloned())
    }

    fn albums(&self) -> Result<Vec<PhotosAlbum>, PhotosError> {
        Ok(self.albums.clone())
    }

    fn album_assets(&self, album_uuid: &str) -> Result<Option<Vec<String>>, PhotosError> {
        Ok(self.album_assets.get(album_uuid).cloned())
    }

    fn warnings(&self) -> &[String] {
        &self.warnings
    }
}

/// The database and committed WAL, with what they looked like when opened.
struct Snapshot {
    db: Database,
    database: PathBuf,
    wal: PathBuf,
    database_before: FileStamp,
    wal_before: Option<FileStamp>,
}

#[derive(Debug, PartialEq, Eq)]
struct FileStamp {
    len: u64,
    modified: Option<std::time::SystemTime>,
}

impl FileStamp {
    fn of(path: &Path) -> std::io::Result<Self> {
        let meta = std::fs::metadata(path)?;
        Ok(Self { len: meta.len(), modified: meta.modified().ok() })
    }
}

fn io_error(path: &Path, error: &std::io::Error) -> PhotosError {
    if error.kind() == std::io::ErrorKind::PermissionDenied {
        PhotosError::PermissionDenied(path.to_path_buf())
    } else {
        PhotosError::Unreadable(format!("{}: {error}", path.display()))
    }
}

impl Snapshot {
    fn open(database: &Path) -> Result<Self, PhotosError> {
        let wal = PathBuf::from(format!("{}-wal", database.display()));
        let database_before = FileStamp::of(database).map_err(|e| io_error(database, &e))?;
        let wal_before = optional_stamp(&wal)?;
        let main = FileSource::open(database).map_err(|e| {
            if e.contains("ermission denied") { PhotosError::PermissionDenied(database.to_path_buf()) } else { PhotosError::Unreadable(e) }
        })?;
        let wal_bytes = match wal_before {
            Some(_) => crate::read_file::read_regular(&wal, MAX_WAL_BYTES).map_err(|e| io_error(&wal, &e))?,
            None => Vec::new(),
        };
        let committed = committed_wal_len(main.header(), &wal_bytes).map_err(PhotosError::Unreadable)?;
        let db = Database::open_file_with_wal(main, wal_bytes.get(..committed).unwrap_or_default()).map_err(PhotosError::Unreadable)?;
        let snapshot = Self { db, database: database.to_path_buf(), wal, database_before, wal_before };
        snapshot.ensure_unchanged()?;
        Ok(snapshot)
    }

    fn ensure_unchanged(&self) -> Result<(), PhotosError> {
        let database_now = FileStamp::of(&self.database).map_err(|_| PhotosError::Changed)?;
        let wal_now = optional_stamp(&self.wal).map_err(|_| PhotosError::Changed)?;
        if database_now != self.database_before || wal_now != self.wal_before || self.db.ensure_unchanged().is_err() {
            return Err(PhotosError::Changed);
        }
        Ok(())
    }
}

fn optional_stamp(path: &Path) -> Result<Option<FileStamp>, PhotosError> {
    match FileStamp::of(path) {
        Ok(stamp) => Ok(Some(stamp)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(io_error(path, &e)),
    }
}

/// Rows of one table with their column names.
struct Table {
    columns: HashMap<String, usize>,
    rows: Vec<Record>,
}

impl Table {
    fn has(&self, column: &str) -> bool {
        self.columns.contains_key(column)
    }

    fn value<'a>(&self, row: &'a Record, column: &str) -> Option<&'a Value> {
        self.columns.get(column).and_then(|&i| row.values.get(i))
    }

    fn int(&self, row: &Record, column: &str) -> Option<i64> {
        match self.value(row, column)? {
            Value::Integer(n) => Some(*n),
            _ => None,
        }
    }

    fn real(&self, row: &Record, column: &str) -> Option<f64> {
        match self.value(row, column)? {
            Value::Integer(n) => Some(*n as f64),
            Value::Real(x) if x.is_finite() => Some(*x),
            _ => None,
        }
    }

    fn text(&self, row: &Record, column: &str) -> Option<String> {
        match self.value(row, column)? {
            // whitespace-only is no value: some cameras write a caption of 31 spaces
            Value::Text(s) if !s.trim().is_empty() => Some(s.clone()),
            _ => None,
        }
    }

    fn flag(&self, row: &Record, column: &str) -> bool {
        self.int(row, column).is_some_and(|n| n != 0)
    }
}

struct TableReader<'a> {
    db: &'a Database,
    tables: Vec<LiveTable>,
    cancel: &'a AtomicBool,
}

impl TableReader<'_> {
    fn find(&self, name: &str) -> Option<&LiveTable> {
        self.tables.iter().find(|t| t.name.eq_ignore_ascii_case(name))
    }

    fn read(&self, table: &LiveTable) -> Result<Table, PhotosError> {
        if self.cancel.load(Ordering::Relaxed) {
            return Err(PhotosError::Cancelled);
        }
        let names = table.column_names.as_deref().unwrap_or_default();
        let columns = names.iter().enumerate().map(|(i, name)| (name.to_ascii_uppercase(), i)).collect();
        let rows = self.db.read_table(table.rootpage, names.len()).map_err(|e| PhotosError::Unreadable(format!("{}: {e}", table.name)))?;
        Ok(Table { columns, rows })
    }

    /// A table that may be missing or unreadable: either leaves a warning and no rows.
    fn optional(&self, name: &str, warnings: &mut Vec<String>) -> Result<Option<Table>, PhotosError> {
        let Some(table) = self.find(name) else { return Ok(None) };
        self.optional_table(table, warnings)
    }

    fn optional_table(&self, table: &LiveTable, warnings: &mut Vec<String>) -> Result<Option<Table>, PhotosError> {
        match self.read(table) {
            Ok(rows) => Ok(Some(rows)),
            Err(PhotosError::Unreadable(e)) => {
                warnings.push(format!("skipped {e}"));
                Ok(None)
            }
            Err(e) => Err(e),
        }
    }

    /// A Core Data many-to-many table: name `Z_<n><SUFFIX>`, with columns ending `left` and `right`.
    fn join(&self, suffix: &str, left: &str, right: &str) -> Option<(&LiveTable, String, String, Option<String>)> {
        self.tables.iter().filter(|t| join_table_name(&t.name, suffix)).find_map(|t| {
            let names = t.column_names.as_deref()?;
            let upper: Vec<String> = names.iter().map(|n| n.to_ascii_uppercase()).collect();
            let l = upper.iter().find(|c| c.starts_with("Z_") && !c.starts_with("Z_FOK_") && c.ends_with(left))?.clone();
            let r = upper.iter().find(|c| c.starts_with("Z_") && !c.starts_with("Z_FOK_") && c.ends_with(right) && **c != l)?.clone();
            let order = upper.iter().find(|c| c.starts_with("Z_FOK_") && c.ends_with(right)).cloned();
            Some((t, l, r, order))
        })
    }
}

/// `Z_<digits><suffix>`, for example `Z_35ASSETS`.
fn join_table_name(name: &str, suffix: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    let Some(middle) = upper.strip_prefix("Z_").and_then(|rest| rest.strip_suffix(suffix)) else { return false };
    !middle.is_empty() && middle.bytes().all(|b| b.is_ascii_digit())
}

fn read_library(library: &PhotosLibrary, reader: &TableReader<'_>) -> Result<SqliteSource, PhotosError> {
    let (schema, asset_table) = if let Some(t) = reader.find("ZASSET") {
        (SchemaVersion::Photos6OrLater, t)
    } else if let Some(t) = reader.find("ZGENERICASSET") {
        (SchemaVersion::Photos5, t)
    } else {
        return Err(PhotosError::UnsupportedVersion("no ZASSET or ZGENERICASSET table; libraries from before Photos 5 are not supported".into()));
    };
    let mut warnings = Vec::new();
    let table = reader.read(asset_table)?;
    if !table.has("Z_PK") || !table.has("ZUUID") {
        return Err(PhotosError::UnsupportedVersion(format!("{} has no Z_PK or ZUUID column", asset_table.name)));
    }
    let attributes = reader.optional("ZADDITIONALASSETATTRIBUTES", &mut warnings)?;
    let descriptions = reader.optional("ZASSETDESCRIPTION", &mut warnings)?;
    let keywords = read_keywords(reader, attributes.as_ref(), &table, &mut warnings)?;
    let resources = read_resources(reader, &mut warnings)?;
    let companion_files = companion_files(library, &table, &resources, &mut warnings);
    let lookups = Lookups { library, extra: Extras::new(attributes.as_ref(), descriptions.as_ref()), keywords, resources, companion_files };

    let mut assets = Vec::new();
    let mut by_uuid = HashMap::new();
    let mut by_pk = HashMap::new();
    let mut skipped = 0usize;
    for row in &table.rows {
        let (Some(pk), Some(uuid)) = (table.int(row, "Z_PK"), table.text(row, "ZUUID").filter(|u| valid_uuid(u))) else {
            skipped += 1;
            continue;
        };
        if by_uuid.contains_key(&uuid) {
            skipped += 1;
            continue;
        }
        let asset = lookups.asset(&table, row, pk, uuid);
        by_uuid.insert(asset.uuid.clone(), assets.len());
        by_pk.insert(pk, asset.uuid.clone());
        assets.push(asset);
    }
    if skipped > 0 {
        warnings.push(format!("skipped {skipped} assets without a valid, unique UUID"));
    }
    let (albums, album_pks) = read_albums(reader, &mut warnings)?;
    let album_assets = read_album_assets(reader, &album_pks, &by_pk, &mut warnings)?;
    Ok(SqliteSource { library: library.clone(), schema, assets, by_uuid, albums, album_assets, warnings })
}

/// Per-asset values kept in other tables, keyed by asset `Z_PK`.
struct Extras {
    original_name: HashMap<i64, String>,
    size: HashMap<i64, u64>,
    title: HashMap<i64, String>,
    caption: HashMap<i64, String>,
    timezone: HashMap<i64, i64>,
}

impl Extras {
    fn new(attributes: Option<&Table>, descriptions: Option<&Table>) -> Self {
        let mut out =
            Self { original_name: HashMap::new(), size: HashMap::new(), title: HashMap::new(), caption: HashMap::new(), timezone: HashMap::new() };
        let Some(attrs) = attributes else { return out };
        let captions = captions(descriptions);
        for row in &attrs.rows {
            let Some(asset) = attrs.int(row, "ZASSET") else { continue };
            if let Some(name) = attrs.text(row, "ZORIGINALFILENAME") {
                out.original_name.insert(asset, name);
            }
            if let Some(size) = attrs.int(row, "ZORIGINALFILESIZE").and_then(|n| u64::try_from(n).ok()) {
                out.size.insert(asset, size);
            }
            if let Some(title) = attrs.text(row, "ZTITLE") {
                out.title.insert(asset, title);
            }
            if let Some(offset) = attrs.int(row, "ZTIMEZONEOFFSET") {
                out.timezone.insert(asset, offset);
            }
            let attr_pk = attrs.int(row, "Z_PK");
            let caption = attrs
                .int(row, "ZASSETDESCRIPTION")
                .and_then(|d| captions.by_description.get(&d))
                .or_else(|| attr_pk.and_then(|p| captions.by_attributes.get(&p)));
            if let Some(caption) = caption {
                out.caption.insert(asset, caption.clone());
            }
        }
        out
    }
}

struct Captions {
    by_description: HashMap<i64, String>,
    by_attributes: HashMap<i64, String>,
}

fn captions(descriptions: Option<&Table>) -> Captions {
    let mut out = Captions { by_description: HashMap::new(), by_attributes: HashMap::new() };
    let Some(table) = descriptions else { return out };
    for row in &table.rows {
        let Some(text) = table.text(row, "ZLONGDESCRIPTION") else { continue };
        if let Some(pk) = table.int(row, "Z_PK") {
            out.by_description.insert(pk, text.clone());
        }
        if let Some(attr) = table.int(row, "ZASSETATTRIBUTES") {
            out.by_attributes.insert(attr, text);
        }
    }
    out
}

/// Keyword titles by asset `Z_PK`, sorted and without duplicates.
fn read_keywords(
    reader: &TableReader<'_>,
    attributes: Option<&Table>,
    assets: &Table,
    warnings: &mut Vec<String>,
) -> Result<HashMap<i64, Vec<String>>, PhotosError> {
    let mut out: HashMap<i64, Vec<String>> = HashMap::new();
    let Some(keyword_table) = reader.optional("ZKEYWORD", warnings)? else { return Ok(out) };
    let Some((join, attr_col, keyword_col, _)) = reader.join("KEYWORDS", "ASSETATTRIBUTES", "KEYWORDS") else { return Ok(out) };
    let Some(links) = reader.optional_table(join, warnings)? else { return Ok(out) };
    let titles: HashMap<i64, String> =
        keyword_table.rows.iter().filter_map(|r| Some((keyword_table.int(r, "Z_PK")?, keyword_table.text(r, "ZTITLE")?))).collect();
    let mut asset_of_attributes: HashMap<i64, i64> = HashMap::new();
    for row in &assets.rows {
        if let (Some(asset), Some(attr)) = (assets.int(row, "Z_PK"), assets.int(row, "ZADDITIONALATTRIBUTES")) {
            asset_of_attributes.insert(attr, asset);
        }
    }
    if let Some(attrs) = attributes {
        for row in &attrs.rows {
            if let (Some(attr), Some(asset)) = (attrs.int(row, "Z_PK"), attrs.int(row, "ZASSET")) {
                asset_of_attributes.insert(attr, asset);
            }
        }
    }
    for row in &links.rows {
        let asset = links.int(row, &attr_col).and_then(|a| asset_of_attributes.get(&a));
        let title = links.int(row, &keyword_col).and_then(|k| titles.get(&k));
        if let (Some(&asset), Some(title)) = (asset, title) {
            out.entry(asset).or_default().push(title.clone());
        }
    }
    for list in out.values_mut() {
        list.sort();
        list.dedup();
    }
    Ok(out)
}

/// What `ZINTERNALRESOURCE` says about each asset's files, by asset `Z_PK`.
#[derive(Default)]
struct Resources {
    /// The table was read: an asset missing from the maps below has no such file.
    known: bool,
    original: HashMap<i64, Availability>,
    /// Whether the other original of a RAW+JPEG pair is local.
    alternate: HashMap<i64, bool>,
    /// Whether a Live Photo's video is local.
    live_video: HashMap<i64, bool>,
}

/// Local wins: an asset with any local copy of a resource has it here.
fn note(map: &mut HashMap<i64, bool>, asset: i64, local: bool) {
    *map.entry(asset).or_insert(local) |= local;
}

fn read_resources(reader: &TableReader<'_>, warnings: &mut Vec<String>) -> Result<Resources, PhotosError> {
    let mut out = Resources::default();
    let Some(table) = reader.optional("ZINTERNALRESOURCE", warnings)? else { return Ok(out) };
    if !table.has("ZASSET") || !table.has("ZLOCALAVAILABILITY") {
        return Ok(out);
    }
    out.known = table.has("ZRESOURCETYPE");
    for row in &table.rows {
        let Some(asset) = table.int(row, "ZASSET") else { continue };
        let current = table.int(row, "ZVERSION").is_none_or(|n| n == 0);
        let local = table.int(row, "ZLOCALAVAILABILITY") == Some(1);
        let kind = table.int(row, "ZRESOURCETYPE");
        if current && table.int(row, "ZDATASTORESUBTYPE").is_none_or(|n| n == RESOURCE_ORIGINAL) && kind.is_none_or(|n| n == 0 || n == 1) {
            let here = if local { Availability::Local } else { Availability::CloudOnly };
            let entry = out.original.entry(asset).or_insert(here);
            if here == Availability::Local {
                *entry = Availability::Local;
            }
            continue;
        }
        // companions of the original, not of an edit (an edit has a recipe)
        if !current || table.int(row, "ZRECIPEID").is_some_and(|n| n != 0) {
            continue;
        }
        match kind {
            Some(RESOURCE_ALTERNATE) => note(&mut out.alternate, asset, local),
            Some(RESOURCE_LIVE_VIDEO) => note(&mut out.live_video, asset, local),
            _ => {}
        }
    }
    Ok(out)
}

/// Companion files on disk, by `<UUID>_<resource type>`, from the `originals/<dir>` folders of
/// assets whose companions the library says are local. Only regular files named exactly
/// `<UUID>_<3 or 4>.<ext>` are taken; links, folders and anything else are ignored.
fn companion_files(library: &PhotosLibrary, assets: &Table, resources: &Resources, warnings: &mut Vec<String>) -> HashMap<String, PathBuf> {
    let wanted = |pk: i64| resources.alternate.get(&pk) == Some(&true) || resources.live_video.get(&pk) == Some(&true);
    let mut dirs = HashSet::new();
    for row in &assets.rows {
        if assets.int(row, "Z_PK").is_some_and(wanted)
            && let Some(dir) = assets.text(row, "ZDIRECTORY").filter(|d| original_path(library.root(), d, "x").is_some())
        {
            dirs.insert(dir);
        }
    }
    let mut out = HashMap::new();
    let mut sorted: Vec<String> = dirs.into_iter().collect();
    sorted.sort();
    for dir in sorted {
        let folder = library.root().join("originals").join(&dir);
        let entries = match std::fs::read_dir(&folder) {
            Ok(entries) => entries,
            Err(e) => {
                warnings.push(format!("cannot list originals/{dir}: {e}"));
                continue;
            }
        };
        for entry in entries.take(MAX_DIR_ENTRIES).filter_map(Result::ok) {
            if !entry.file_type().is_ok_and(|t| t.is_file()) {
                continue;
            }
            let Ok(name) = entry.file_name().into_string() else { continue };
            if let Some(stem) = companion_stem(&name) {
                out.insert(stem.to_owned(), entry.path());
            }
        }
    }
    out
}

/// `<UUID>_<3 or 4>` of a companion file name `<UUID>_<3 or 4>.<ext>`, if it is one.
fn companion_stem(name: &str) -> Option<&str> {
    let (stem, ext) = name.split_once('.')?;
    let (uuid, kind) = stem.rsplit_once('_')?;
    let kind_ok = kind.parse::<i64>().is_ok_and(|k| k == RESOURCE_ALTERNATE || k == RESOURCE_LIVE_VIDEO);
    let ext_ok = !ext.is_empty() && ext.bytes().all(|b| b.is_ascii_alphanumeric());
    (kind_ok && ext_ok && valid_uuid(uuid)).then_some(stem)
}

/// What [`Lookups::asset`] needs besides the asset row.
struct Lookups<'a> {
    library: &'a PhotosLibrary,
    extra: Extras,
    keywords: HashMap<i64, Vec<String>>,
    resources: Resources,
    companion_files: HashMap<String, PathBuf>,
}

impl Lookups<'_> {
    /// A companion of asset `pk` from `map` (local or not), found on disk as `<uuid>_<kind>.*`.
    fn companion(&self, map: &HashMap<i64, bool>, pk: i64, uuid: &str, kind: i64) -> Companion {
        if !self.resources.known {
            return Companion::Unknown;
        }
        match map.get(&pk) {
            None => Companion::None,
            Some(false) => Companion::CloudOnly,
            Some(true) => self.companion_files.get(&format!("{uuid}_{kind}")).map_or(Companion::Unknown, |p| Companion::Local(p.clone())),
        }
    }

    fn asset(&self, table: &Table, row: &Record, pk: i64, uuid: String) -> PhotosAsset {
        let (library, extra, keywords) = (self.library, &self.extra, &self.keywords);
        let paired_original = self.companion(&self.resources.alternate, pk, &uuid, RESOURCE_ALTERNATE);
        let live_video = self.companion(&self.resources.live_video, pk, &uuid, RESOURCE_LIVE_VIDEO);
        let file_name = table.text(row, "ZFILENAME");
        let original_path = table.text(row, "ZDIRECTORY").zip(file_name.as_deref()).and_then(|(dir, file)| original_path(library.root(), &dir, file));
        let kind = match table.int(row, "ZKIND") {
            Some(0) | None => AssetKind::Photo,
            Some(1) => AssetKind::Video,
            Some(n) => AssetKind::Other(n),
        };
        let offset = extra.timezone.get(&pk).copied();
        PhotosAsset {
            uuid,
            file_name,
            original_file_name: extra.original_name.get(&pk).cloned(),
            original_path,
            created: table.real(row, "ZDATECREATED").and_then(|secs| core_data_date(secs, offset)),
            favourite: table.flag(row, "ZFAVORITE"),
            hidden: table.flag(row, "ZHIDDEN"),
            trashed: table.flag(row, "ZTRASHEDSTATE"),
            kind,
            live_photo: table.int(row, "ZKINDSUBTYPE") == Some(2),
            uti: table.text(row, "ZUNIFORMTYPEIDENTIFIER"),
            width: table.int(row, "ZWIDTH").and_then(|n| u32::try_from(n).ok()).filter(|&n| n > 0),
            height: table.int(row, "ZHEIGHT").and_then(|n| u32::try_from(n).ok()).filter(|&n| n > 0),
            original_size: extra.size.get(&pk).copied(),
            location: geo_point(table.real(row, "ZLATITUDE"), table.real(row, "ZLONGITUDE")),
            title: extra.title.get(&pk).cloned(),
            caption: extra.caption.get(&pk).cloned(),
            keywords: keywords.get(&pk).cloned().unwrap_or_default(),
            availability: self.resources.original.get(&pk).copied().unwrap_or(Availability::Unknown),
            paired_original,
            live_video,
        }
    }
}

/// UUIDs become map keys and later file names: ASCII letters, digits and `-` only.
fn valid_uuid(uuid: &str) -> bool {
    !uuid.is_empty() && uuid.len() <= MAX_UUID_LEN && uuid.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

/// `<root>/originals/<directory>/<file>` when both are single, plain path components. Anything else
/// (an absolute path, `..`, a separator) would point outside the package, so it gives `None`.
fn original_path(root: &Path, directory: &str, file: &str) -> Option<PathBuf> {
    let plain = |part: &str| {
        let mut components = Path::new(part).components();
        matches!(components.next(), Some(Component::Normal(_))) && components.next().is_none() && !part.contains(['/', '\\', '\0'])
    };
    (plain(directory) && plain(file)).then(|| root.join("originals").join(directory).join(file))
}

/// A Core Data date (seconds since 2001-01-01 UTC) in the recorded time zone, if it is a real date.
fn core_data_date(seconds: f64, offset_seconds: Option<i64>) -> Option<DateTime<FixedOffset>> {
    if !seconds.is_finite() || seconds.abs() > 1e11 {
        return None;
    }
    let whole = seconds.floor();
    let nanos = ((seconds - whole) * 1e9).round().clamp(0.0, 999_999_999.0) as u32;
    let utc: DateTime<Utc> = DateTime::from_timestamp(CORE_DATA_EPOCH.checked_add(whole as i64)?, nanos)?;
    let offset = offset_seconds.and_then(|s| i32::try_from(s).ok()).and_then(FixedOffset::east_opt).unwrap_or(FixedOffset::east_opt(0)?);
    Some(utc.with_timezone(&offset))
}

/// A location, unless it is out of range or Photos' "no location" value (-180, -180).
fn geo_point(latitude: Option<f64>, longitude: Option<f64>) -> Option<GeoPoint> {
    let (latitude, longitude) = (latitude?, longitude?);
    let valid = (-90.0..=90.0).contains(&latitude) && (-180.0..=180.0).contains(&longitude);
    valid.then_some(GeoPoint { latitude, longitude })
}

/// User albums and folders, parents first, and each one's `Z_PK`.
fn read_albums(reader: &TableReader<'_>, warnings: &mut Vec<String>) -> Result<(Vec<PhotosAlbum>, HashMap<i64, String>), PhotosError> {
    let Some(table) = reader.optional("ZGENERICALBUM", warnings)? else { return Ok((Vec::new(), HashMap::new())) };
    struct Raw {
        pk: i64,
        uuid: String,
        title: String,
        kind: AlbumKind,
        parent_pk: Option<i64>,
    }
    let mut raw = Vec::new();
    let mut seen = HashSet::new();
    for row in &table.rows {
        let kind = match table.int(row, "ZKIND") {
            Some(ALBUM_KIND_ALBUM) => AlbumKind::Album,
            Some(ALBUM_KIND_FOLDER) => AlbumKind::Folder,
            _ => continue,
        };
        if table.flag(row, "ZTRASHEDSTATE") {
            continue;
        }
        let (Some(pk), Some(uuid)) = (table.int(row, "Z_PK"), table.text(row, "ZUUID").filter(|u| valid_uuid(u))) else { continue };
        if !seen.insert(uuid.clone()) {
            continue;
        }
        let title = table.text(row, "ZTITLE").unwrap_or_default();
        raw.push(Raw { pk, uuid, title, kind, parent_pk: table.int(row, "ZPARENTFOLDER") });
    }
    let folders: HashMap<i64, (String, Option<i64>)> =
        raw.iter().filter(|a| a.kind == AlbumKind::Folder).map(|a| (a.pk, (a.uuid.clone(), a.parent_pk))).collect();
    let depth_of = |start: Option<i64>| -> Option<usize> {
        let mut depth = 0;
        let mut at = start;
        let mut visited = HashSet::new();
        while let Some(pk) = at {
            let Some((_, parent)) = folders.get(&pk) else { return Some(depth) };
            if !visited.insert(pk) || depth >= MAX_FOLDER_DEPTH {
                return None;
            }
            depth += 1;
            at = *parent;
        }
        Some(depth)
    };
    let mut ordered: Vec<(usize, PhotosAlbum, i64)> = Vec::new();
    let mut cut = 0usize;
    for album in raw {
        let parent_uuid = album.parent_pk.and_then(|p| folders.get(&p)).map(|(uuid, _)| uuid.clone());
        let (depth, parent) = match depth_of(album.parent_pk) {
            Some(d) if album.parent_pk != Some(album.pk) => (d, parent_uuid),
            _ => {
                cut += 1;
                (0, None)
            }
        };
        ordered.push((depth, PhotosAlbum { uuid: album.uuid, title: album.title, kind: album.kind, parent }, album.pk));
    }
    if cut > 0 {
        warnings.push(format!("{cut} albums or folders had a looping parent chain and are shown at the top level"));
    }
    ordered.sort_by_key(|(depth, _, _)| *depth);
    let pks = ordered.iter().map(|(_, album, pk)| (*pk, album.uuid.clone())).collect();
    Ok((ordered.into_iter().map(|(_, album, _)| album).collect(), pks))
}

/// Asset UUIDs of each album, in album order, from the `Z_<n>ASSETS` join table.
fn read_album_assets(
    reader: &TableReader<'_>,
    albums: &HashMap<i64, String>,
    assets: &HashMap<i64, String>,
    warnings: &mut Vec<String>,
) -> Result<HashMap<String, Vec<String>>, PhotosError> {
    let mut out: HashMap<String, Vec<String>> = HashMap::new();
    let Some((join, album_col, asset_col, order_col)) = reader.join("ASSETS", "ALBUMS", "ASSETS") else { return Ok(out) };
    let Some(links) = reader.optional_table(join, warnings)? else { return Ok(out) };
    let mut ordered: HashMap<&String, Vec<(i64, i64)>> = HashMap::new();
    for row in &links.rows {
        let (Some(album), Some(asset)) = (links.int(row, &album_col), links.int(row, &asset_col)) else { continue };
        let Some(album_uuid) = albums.get(&album) else { continue };
        let order = order_col.as_deref().and_then(|c| links.int(row, c)).unwrap_or(i64::MAX);
        ordered.entry(album_uuid).or_default().push((order, asset));
    }
    for (album, mut members) in ordered {
        members.sort();
        let mut seen = HashSet::new();
        let uuids = members.into_iter().filter(|(_, asset)| seen.insert(*asset)).filter_map(|(_, asset)| assets.get(&asset).cloned()).collect();
        out.insert(album.clone(), uuids);
    }
    Ok(out)
}

mod photokit_source;
pub use photokit_source::PhotoKitSource;

#[cfg(test)]
mod fixture;
#[cfg(test)]
mod tests;
