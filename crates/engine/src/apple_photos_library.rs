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

use std::ops::ControlFlow;

use crate::lightroom_sqlite::{Database, FileSource, LiveTable, Record, Scan, Value, committed_wal_len};

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

/// One photo or video in a Photos library: its original, and the version Photos shows when it was
/// edited there ([`Self::edited_path`]).
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
    /// Photos shows an edited version of it (`ZADJUSTMENTSSTATE` above 0 in Photos 11 libraries,
    /// `ZHASADJUSTMENTS` in older ones).
    pub edited: bool,
    /// The place names Photos worked out from the location ([`place_from_archive`]).
    pub place: Option<PhotosPlace>,
    /// The named people Photos found in it, with where their faces are ([`PhotosFace`]).
    pub faces: Vec<PhotosFace>,
    /// That edited version, full size, when it is on this Mac:
    /// `resources/renders/<first character of the UUID>/<UUID>_1_201_a.<heic|jpeg|…>` (observed in
    /// a Photos 11 library on macOS 27.3).
    pub edited_path: Option<PathBuf>,
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
/// Any call may answer [`PhotosError::Changed`] when the library changed since it was opened (a
/// backend that reads pages on demand finds out then): open it again.
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

/// Photos library read from `Photos.sqlite`.
///
/// [`SqliteSource::open`] reads each table once, row by row, and keeps only an index of the assets
/// (UUID and where its rows are) and the small per-asset facts kept in other tables: keywords,
/// which files are on this Mac, album members. Each page of [`PhotosSource::assets`] then reads
/// just its own rows. Memory grows by a couple of hundred bytes per asset, never with the size of
/// the tables, so a library of any size opens.
///
/// The committed WAL is read once, at `open`. When Photos later checkpoints it into the database
/// file, the next read answers [`PhotosError::Changed`]: open the library again.
pub struct SqliteSource {
    library: PhotosLibrary,
    schema: SchemaVersion,
    snapshot: Snapshot,
    tables: AssetTables,
    index: Vec<Entry>,
    by_uuid: HashMap<Box<str>, usize>,
    /// Keyword titles by asset `Z_PK`, sorted and without duplicates.
    keywords: HashMap<i64, Vec<String>>,
    /// Named people's faces by asset `Z_PK` ([`read_faces`]).
    faces: HashMap<i64, Vec<PhotosFace>>,
    resources: Resources,
    companion_files: HashMap<String, PathBuf>,
    albums: Vec<PhotosAlbum>,
    /// Each album's assets, as positions in `index`, in album order.
    album_assets: HashMap<String, Vec<usize>>,
    warnings: Vec<String>,
}

impl std::fmt::Debug for SqliteSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SqliteSource")
            .field("library", &self.library)
            .field("schema", &self.schema)
            .field("assets", &self.index.len())
            .field("albums", &self.albums.len())
            .finish_non_exhaustive()
    }
}

/// One asset of the index: its UUID and where its rows are.
struct Entry {
    uuid: Box<str>,
    pk: i64,
    /// Rowid of its asset (`ZASSET` or `ZGENERICASSET`) row.
    row: i64,
    /// Rowid of its `ZADDITIONALASSETATTRIBUTES` row.
    attributes: Option<i64>,
    /// Rowid of the `ZASSETDESCRIPTION` row holding its caption.
    description: Option<i64>,
}

/// The tables a page of assets is read from.
struct AssetTables {
    asset: Table,
    attributes: Option<Table>,
    descriptions: Option<Table>,
}

/// Asset columns read for each page.
const ASSET_COLUMNS: &[&str] = &[
    "ZUUID",
    "ZFILENAME",
    "ZDIRECTORY",
    "ZKIND",
    "ZDATECREATED",
    "ZFAVORITE",
    "ZHIDDEN",
    "ZTRASHEDSTATE",
    "ZKINDSUBTYPE",
    "ZUNIFORMTYPEIDENTIFIER",
    "ZWIDTH",
    "ZHEIGHT",
    "ZLATITUDE",
    "ZLONGITUDE",
    "ZADJUSTMENTSSTATE",
    "ZHASADJUSTMENTS",
];
/// `ZADDITIONALASSETATTRIBUTES` columns read for each page.
const ATTRIBUTE_COLUMNS: &[&str] =
    &["ZORIGINALFILENAME", "ZORIGINALFILESIZE", "ZTITLE", "ZTIMEZONEOFFSET", "ZREVERSELOCATIONDATA", "ZREVERSELOCATIONDATAISVALID"];
/// Rows read at a time for one page of assets.
const READ_CHUNK: usize = 2048;
/// Rows between two looks at the cancel flag while a table is read.
const CANCEL_CHECK_ROWS: usize = 4096;

impl SqliteSource {
    /// Read the library's index. `cancel` is checked while tables are read. Read-only: the
    /// database and its WAL are opened for reading, and a change while reading gives
    /// [`PhotosError::Changed`].
    pub fn open(library: &PhotosLibrary, cancel: &AtomicBool) -> Result<Self, PhotosError> {
        let snapshot = Snapshot::open(&library.database_path())?;
        let source = read_library(library, snapshot, cancel)?;
        source.snapshot.ensure_unchanged()?;
        Ok(source)
    }

    pub fn library(&self) -> &PhotosLibrary {
        &self.library
    }

    pub fn schema(&self) -> SchemaVersion {
        self.schema
    }

    /// The assets with these UUIDs, by UUID; unknown ones are left out.
    pub(crate) fn assets_by_uuid(&self, uuids: &[String]) -> Result<HashMap<String, PhotosAsset>, PhotosError> {
        let entries: Vec<&Entry> = uuids.iter().filter_map(|u| self.by_uuid.get(u.as_str())).filter_map(|&i| self.index.get(i)).collect();
        Ok(self.read(&entries)?.into_iter().map(|a| (a.uuid.clone(), a)).collect())
    }

    /// The assets of `entries`, in order. A change to the database file since `open` (Photos
    /// checkpointed its WAL) makes every row suspect, so it is checked after reading.
    fn read(&self, entries: &[&Entry]) -> Result<Vec<PhotosAsset>, PhotosError> {
        let mut out = Vec::with_capacity(entries.len());
        let mut result = Ok(());
        for chunk in entries.chunks(READ_CHUNK) {
            match self.read_chunk(chunk) {
                Ok(assets) => out.extend(assets),
                Err(e) => {
                    result = Err(e);
                    break;
                }
            }
        }
        self.snapshot.ensure_database_unchanged()?;
        result.map(|()| out)
    }

    fn read_chunk(&self, entries: &[&Entry]) -> Result<Vec<PhotosAsset>, PhotosError> {
        let db = &self.snapshot.db;
        let tables = &self.tables;
        let assets = rows_by_rowid(db, &tables.asset, ASSET_COLUMNS, entries.iter().map(|e| e.row))?;
        let attributes = match &tables.attributes {
            Some(t) => rows_by_rowid(db, t, ATTRIBUTE_COLUMNS, entries.iter().filter_map(|e| e.attributes))?,
            None => HashMap::new(),
        };
        let descriptions = match &tables.descriptions {
            Some(t) => rows_by_rowid(db, t, &["ZLONGDESCRIPTION"], entries.iter().filter_map(|e| e.description))?,
            None => HashMap::new(),
        };
        entries
            .iter()
            .map(|entry| {
                let row = assets.get(&entry.row).filter(|row| tables.asset.text(row, "ZUUID").as_deref() == Some(&*entry.uuid));
                let Some(row) = row else {
                    return Err(PhotosError::Unreadable(format!("{}: row {} is not where the table said it was", tables.asset.name, entry.row)));
                };
                let attributes = entry.attributes.and_then(|r| attributes.get(&r));
                let description = entry.description.and_then(|r| descriptions.get(&r));
                Ok(self.asset_from(entry, row, attributes, description))
            })
            .collect()
    }

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

    fn asset_from(&self, entry: &Entry, row: &Record, attributes: Option<&Record>, description: Option<&Record>) -> PhotosAsset {
        let (table, pk, uuid) = (&self.tables.asset, entry.pk, entry.uuid.to_string());
        let attr = self.tables.attributes.as_ref().zip(attributes);
        let attr_text = |column: &str| attr.and_then(|(t, r)| t.text(r, column));
        let attr_int = |column: &str| attr.and_then(|(t, r)| t.int(r, column));
        let caption = self.tables.descriptions.as_ref().zip(description).and_then(|(t, r)| t.text(r, "ZLONGDESCRIPTION"));
        let paired_original = self.companion(&self.resources.alternate, pk, &uuid, RESOURCE_ALTERNATE);
        let live_video = self.companion(&self.resources.live_video, pk, &uuid, RESOURCE_LIVE_VIDEO);
        let file_name = table.text(row, "ZFILENAME");
        let original_path =
            table.text(row, "ZDIRECTORY").zip(file_name.as_deref()).and_then(|(dir, file)| original_path(self.library.root(), &dir, file));
        let kind = match table.int(row, "ZKIND") {
            Some(0) | None => AssetKind::Photo,
            Some(1) => AssetKind::Video,
            Some(n) => AssetKind::Other(n),
        };
        let edited = table.int(row, "ZADJUSTMENTSSTATE").is_some_and(|n| n > 0) || table.flag(row, "ZHASADJUSTMENTS");
        let place = attr
            .filter(|(t, r)| t.flag(r, "ZREVERSELOCATIONDATAISVALID"))
            .and_then(|(t, r)| t.blob(r, "ZREVERSELOCATIONDATA"))
            .and_then(place_from_archive);
        let edited_path = if edited { rendered_path(self.library.root(), &uuid) } else { None };
        PhotosAsset {
            edited,
            edited_path,
            place,
            uuid,
            file_name,
            original_file_name: attr_text("ZORIGINALFILENAME"),
            original_path,
            created: table.real(row, "ZDATECREATED").and_then(|secs| core_data_date(secs, attr_int("ZTIMEZONEOFFSET"))),
            favourite: table.flag(row, "ZFAVORITE"),
            hidden: table.flag(row, "ZHIDDEN"),
            trashed: table.flag(row, "ZTRASHEDSTATE"),
            kind,
            live_photo: table.int(row, "ZKINDSUBTYPE") == Some(2),
            uti: table.text(row, "ZUNIFORMTYPEIDENTIFIER"),
            width: table.int(row, "ZWIDTH").and_then(|n| u32::try_from(n).ok()).filter(|&n| n > 0),
            height: table.int(row, "ZHEIGHT").and_then(|n| u32::try_from(n).ok()).filter(|&n| n > 0),
            original_size: attr_int("ZORIGINALFILESIZE").and_then(|n| u64::try_from(n).ok()),
            location: geo_point(table.real(row, "ZLATITUDE"), table.real(row, "ZLONGITUDE")),
            title: attr_text("ZTITLE"),
            caption,
            keywords: self.keywords.get(&pk).cloned().unwrap_or_default(),
            faces: self.faces.get(&pk).cloned().unwrap_or_default(),
            availability: self.resources.original.get(&pk).copied().unwrap_or(Availability::Unknown),
            paired_original,
            live_video,
        }
    }
}

impl PhotosSource for SqliteSource {
    fn asset_count(&self) -> usize {
        self.index.len()
    }

    fn assets(&self, start: usize, limit: usize) -> Result<Vec<PhotosAsset>, PhotosError> {
        let end = start.saturating_add(limit).min(self.index.len());
        let Some(window) = self.index.get(start..end) else { return Ok(Vec::new()) };
        self.read(&window.iter().collect::<Vec<_>>())
    }

    fn asset(&self, uuid: &str) -> Result<Option<PhotosAsset>, PhotosError> {
        let Some(entry) = self.by_uuid.get(uuid).and_then(|&i| self.index.get(i)) else { return Ok(None) };
        Ok(self.read(&[entry])?.into_iter().next())
    }

    fn albums(&self) -> Result<Vec<PhotosAlbum>, PhotosError> {
        Ok(self.albums.clone())
    }

    fn album_assets(&self, album_uuid: &str) -> Result<Option<Vec<String>>, PhotosError> {
        Ok(self.album_assets.get(album_uuid).map(|members| members.iter().filter_map(|&i| self.index.get(i)).map(|e| e.uuid.to_string()).collect()))
    }

    fn warnings(&self) -> &[String] {
        &self.warnings
    }
}

/// Rows of `table` by rowid, with only `columns` decoded. Rowids close together are read in one
/// walk over the leaves between them; scattered ones with one descent each.
fn rows_by_rowid(db: &Database, table: &Table, columns: &[&str], rowids: impl Iterator<Item = i64>) -> Result<HashMap<i64, Record>, PhotosError> {
    let mut wanted: Vec<i64> = rowids.collect();
    wanted.sort_unstable();
    wanted.dedup();
    let (Some(&first), Some(&last)) = (wanted.first(), wanted.last()) else { return Ok(HashMap::new()) };
    let keep = table.keep(columns);
    let scan = Scan { rootpage: table.rootpage, ncols: table.ncols, keep: Some(&keep), from_rowid: first };
    let unreadable = |e: String| PhotosError::Unreadable(format!("{}: {e}", table.name));
    let mut out = HashMap::with_capacity(wanted.len());
    let span = u64::try_from(i128::from(last) - i128::from(first)).unwrap_or(u64::MAX);
    if span <= (wanted.len() as u64).saturating_mul(4).saturating_add(256) {
        let mut set: HashSet<i64> = wanted.iter().copied().collect();
        db.scan(&scan, &mut |rowid, record| {
            if set.remove(&rowid) {
                out.insert(rowid, record);
            }
            if rowid >= last || set.is_empty() { ControlFlow::Break(()) } else { ControlFlow::Continue(()) }
        })
        .map_err(unreadable)?;
    }
    // what the walk did not reach (a damaged tree whose rows are out of order) is looked up alone
    for rowid in wanted {
        if !out.contains_key(&rowid)
            && let Some(record) = db.row(&scan, rowid).map_err(unreadable)?
        {
            out.insert(rowid, record);
        }
    }
    Ok(out)
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

    /// Neither the database nor its WAL changed since `open`.
    fn ensure_unchanged(&self) -> Result<(), PhotosError> {
        let wal_now = optional_stamp(&self.wal).map_err(|_| PhotosError::Changed)?;
        if wal_now != self.wal_before {
            return Err(PhotosError::Changed);
        }
        self.ensure_database_unchanged()
    }

    /// The database file did not change since `open`. The WAL frames were copied at `open`, so
    /// new frames don't matter; a checkpoint writes the database file, and that does.
    fn ensure_database_unchanged(&self) -> Result<(), PhotosError> {
        let database_now = FileStamp::of(&self.database).map_err(|_| PhotosError::Changed)?;
        if database_now != self.database_before || self.db.ensure_unchanged().is_err() {
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

/// A table's place in the database and its column names.
struct Table {
    name: String,
    rootpage: u32,
    ncols: usize,
    columns: HashMap<String, usize>,
}

impl Table {
    fn new(live: &LiveTable) -> Self {
        let names = live.column_names.as_deref().unwrap_or_default();
        let columns = names.iter().enumerate().map(|(i, name)| (name.to_ascii_uppercase(), i)).collect();
        Self { name: live.name.clone(), rootpage: live.rootpage, ncols: names.len(), columns }
    }

    fn has(&self, column: &str) -> bool {
        self.columns.contains_key(column)
    }

    /// Which columns to decode: `names`, where the table has them.
    fn keep(&self, names: &[&str]) -> Vec<bool> {
        let mut keep = vec![false; self.ncols];
        for name in names {
            if let Some(slot) = self.columns.get(*name).and_then(|&i| keep.get_mut(i)) {
                *slot = true;
            }
        }
        keep
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

    fn blob<'a>(&self, row: &'a Record, column: &str) -> Option<&'a [u8]> {
        match self.value(row, column)? {
            Value::Blob(b) => Some(b),
            _ => None,
        }
    }
}

struct TableReader<'a> {
    db: &'a Database,
    tables: Vec<LiveTable>,
    cancel: &'a AtomicBool,
}

impl TableReader<'_> {
    fn find(&self, name: &str) -> Option<Table> {
        self.tables.iter().find(|t| t.name.eq_ignore_ascii_case(name)).map(Table::new)
    }

    /// Visit every row of `table` with only `columns` decoded, looking at the cancel flag as it goes.
    fn scan(&self, table: &Table, columns: &[&str], mut visit: impl FnMut(i64, &Record)) -> Result<(), PhotosError> {
        if self.cancel.load(Ordering::Relaxed) {
            return Err(PhotosError::Cancelled);
        }
        let keep = table.keep(columns);
        let scan = Scan { rootpage: table.rootpage, ncols: table.ncols, keep: Some(&keep), from_rowid: i64::MIN };
        let (mut rows, mut cancelled) = (0usize, false);
        self.db
            .scan(&scan, &mut |rowid, record| {
                rows = rows.wrapping_add(1);
                if rows.is_multiple_of(CANCEL_CHECK_ROWS) && self.cancel.load(Ordering::Relaxed) {
                    cancelled = true;
                    return ControlFlow::Break(());
                }
                visit(rowid, &record);
                ControlFlow::Continue(())
            })
            .map_err(|e| PhotosError::Unreadable(format!("{}: {e}", table.name)))?;
        if cancelled { Err(PhotosError::Cancelled) } else { Ok(()) }
    }

    /// [`Self::scan`] of a table that may be unreadable: that leaves a warning and gives `false`,
    /// and the caller drops what it collected.
    fn optional_scan(
        &self,
        table: &Table,
        columns: &[&str],
        warnings: &mut Vec<String>,
        visit: impl FnMut(i64, &Record),
    ) -> Result<bool, PhotosError> {
        match self.scan(table, columns, visit) {
            Ok(()) => Ok(true),
            Err(PhotosError::Unreadable(e)) => {
                warnings.push(format!("skipped {e}"));
                Ok(false)
            }
            Err(e) => Err(e),
        }
    }

    /// A Core Data many-to-many table: name `Z_<n><SUFFIX>`, with columns ending `left` and `right`.
    fn join(&self, suffix: &str, left: &str, right: &str) -> Option<(Table, String, String, Option<String>)> {
        self.tables.iter().filter(|t| join_table_name(&t.name, suffix)).find_map(|t| {
            let names = t.column_names.as_deref()?;
            let upper: Vec<String> = names.iter().map(|n| n.to_ascii_uppercase()).collect();
            let l = upper.iter().find(|c| c.starts_with("Z_") && !c.starts_with("Z_FOK_") && c.ends_with(left))?.clone();
            let r = upper.iter().find(|c| c.starts_with("Z_") && !c.starts_with("Z_FOK_") && c.ends_with(right) && **c != l)?.clone();
            let order = upper.iter().find(|c| c.starts_with("Z_FOK_") && c.ends_with(right)).cloned();
            Some((Table::new(t), l, r, order))
        })
    }
}

/// `Z_<digits><suffix>`, for example `Z_35ASSETS`.
fn join_table_name(name: &str, suffix: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    let Some(middle) = upper.strip_prefix("Z_").and_then(|rest| rest.strip_suffix(suffix)) else { return false };
    !middle.is_empty() && middle.bytes().all(|b| b.is_ascii_digit())
}

/// The asset index and what [`read_library`] learnt while building it.
struct AssetIndex {
    index: Vec<Entry>,
    by_uuid: HashMap<Box<str>, usize>,
    by_pk: HashMap<i64, usize>,
    /// Asset `Z_PK` of each `ZADDITIONALASSETATTRIBUTES` `Z_PK`, from the asset rows.
    asset_of_attributes: HashMap<i64, i64>,
    /// `originals/<dir>` folders of assets whose companions are on this Mac.
    companion_dirs: HashSet<String>,
}

fn read_library(library: &PhotosLibrary, snapshot: Snapshot, cancel: &AtomicBool) -> Result<SqliteSource, PhotosError> {
    let reader = TableReader { db: &snapshot.db, tables: snapshot.db.live_tables().map_err(PhotosError::Unreadable)?, cancel };
    let (schema, asset_table) = if let Some(t) = reader.find("ZASSET") {
        (SchemaVersion::Photos6OrLater, t)
    } else if let Some(t) = reader.find("ZGENERICASSET") {
        (SchemaVersion::Photos5, t)
    } else {
        return Err(PhotosError::UnsupportedVersion("no ZASSET or ZGENERICASSET table; libraries from before Photos 5 are not supported".into()));
    };
    if !asset_table.has("Z_PK") || !asset_table.has("ZUUID") {
        return Err(PhotosError::UnsupportedVersion(format!("{} has no Z_PK or ZUUID column", asset_table.name)));
    }
    let mut warnings = Vec::new();
    let resources = read_resources(&reader, &mut warnings)?;
    let descriptions = read_descriptions(&reader, &mut warnings)?;
    let attributes = read_attributes(&reader, descriptions.as_ref(), &mut warnings)?;
    let mut assets = read_asset_index(library, &reader, &asset_table, attributes.as_ref(), &resources, &mut warnings)?;
    if let Some(attributes) = &attributes {
        assets.asset_of_attributes.extend(attributes.asset_of.iter().map(|(&attr, &asset)| (attr, asset)));
    }
    let keywords = read_keywords(&reader, &assets.asset_of_attributes, &mut warnings)?;
    let faces = read_faces(&reader, &mut warnings)?;
    let companion_files = companion_files(library, &assets.companion_dirs, &mut warnings);
    let (albums, album_pks) = read_albums(&reader, &mut warnings)?;
    let album_assets = read_album_assets(&reader, &album_pks, &assets.by_pk, &mut warnings)?;
    let tables = AssetTables { asset: asset_table, attributes: attributes.map(|a| a.table), descriptions: descriptions.map(|d| d.table) };
    drop(reader);
    Ok(SqliteSource {
        library: library.clone(),
        schema,
        snapshot,
        tables,
        index: assets.index,
        by_uuid: assets.by_uuid,
        keywords,
        faces,
        resources,
        companion_files,
        albums,
        album_assets,
        warnings,
    })
}

fn read_asset_index(
    library: &PhotosLibrary,
    reader: &TableReader<'_>,
    table: &Table,
    attributes: Option<&Attributes>,
    resources: &Resources,
    warnings: &mut Vec<String>,
) -> Result<AssetIndex, PhotosError> {
    let mut out = AssetIndex {
        index: Vec::new(),
        by_uuid: HashMap::new(),
        by_pk: HashMap::new(),
        asset_of_attributes: HashMap::new(),
        companion_dirs: HashSet::new(),
    };
    let wants_companions = |pk: i64| resources.alternate.get(&pk) == Some(&true) || resources.live_video.get(&pk) == Some(&true);
    let mut skipped = 0usize;
    reader.scan(table, &["Z_PK", "ZUUID", "ZADDITIONALATTRIBUTES", "ZDIRECTORY"], |rowid, row| {
        if let (Some(asset), Some(attr)) = (table.int(row, "Z_PK"), table.int(row, "ZADDITIONALATTRIBUTES")) {
            out.asset_of_attributes.insert(attr, asset);
        }
        let (Some(pk), Some(uuid)) = (table.int(row, "Z_PK"), table.text(row, "ZUUID").filter(|u| valid_uuid(u))) else {
            skipped += 1;
            return;
        };
        if out.by_uuid.contains_key(uuid.as_str()) {
            skipped += 1;
            return;
        }
        if wants_companions(pk)
            && let Some(dir) = table.text(row, "ZDIRECTORY").filter(|d| original_path(library.root(), d, "x").is_some())
        {
            out.companion_dirs.insert(dir);
        }
        let rows = attributes.and_then(|a| a.by_asset.get(&pk));
        let uuid: Box<str> = uuid.into();
        out.by_uuid.insert(uuid.clone(), out.index.len());
        out.by_pk.entry(pk).or_insert(out.index.len());
        out.index.push(Entry { uuid, pk, row: rowid, attributes: rows.map(|r| r.0), description: rows.and_then(|r| r.1) });
    })?;
    if skipped > 0 {
        warnings.push(format!("skipped {skipped} assets without a valid, unique UUID"));
    }
    Ok(out)
}

/// `ZASSETDESCRIPTION` rows that hold a caption, by their `Z_PK` and by the `Z_PK` of the
/// `ZADDITIONALASSETATTRIBUTES` row they belong to.
struct Descriptions {
    table: Table,
    by_pk: HashMap<i64, i64>,
    by_attributes: HashMap<i64, i64>,
}

fn read_descriptions(reader: &TableReader<'_>, warnings: &mut Vec<String>) -> Result<Option<Descriptions>, PhotosError> {
    let Some(table) = reader.find("ZASSETDESCRIPTION") else { return Ok(None) };
    let (mut by_pk, mut by_attributes) = (HashMap::new(), HashMap::new());
    let read = reader.optional_scan(&table, &["Z_PK", "ZASSETATTRIBUTES", "ZLONGDESCRIPTION"], warnings, |rowid, row| {
        if table.text(row, "ZLONGDESCRIPTION").is_none() {
            return;
        }
        if let Some(pk) = table.int(row, "Z_PK") {
            by_pk.insert(pk, rowid);
        }
        if let Some(attr) = table.int(row, "ZASSETATTRIBUTES") {
            by_attributes.insert(attr, rowid);
        }
    })?;
    Ok(read.then_some(Descriptions { table, by_pk, by_attributes }))
}

/// `ZADDITIONALASSETATTRIBUTES` rows by asset `Z_PK`: the row's rowid and the rowid of the
/// `ZASSETDESCRIPTION` row with its caption.
struct Attributes {
    table: Table,
    by_asset: HashMap<i64, (i64, Option<i64>)>,
    /// Asset `Z_PK` of each attributes `Z_PK`.
    asset_of: HashMap<i64, i64>,
}

fn read_attributes(
    reader: &TableReader<'_>,
    descriptions: Option<&Descriptions>,
    warnings: &mut Vec<String>,
) -> Result<Option<Attributes>, PhotosError> {
    let Some(table) = reader.find("ZADDITIONALASSETATTRIBUTES") else { return Ok(None) };
    let (mut by_asset, mut asset_of) = (HashMap::new(), HashMap::new());
    let read = reader.optional_scan(&table, &["Z_PK", "ZASSET", "ZASSETDESCRIPTION"], warnings, |rowid, row| {
        let Some(asset) = table.int(row, "ZASSET") else { return };
        let pk = table.int(row, "Z_PK");
        let description = descriptions.and_then(|d| {
            table.int(row, "ZASSETDESCRIPTION").and_then(|p| d.by_pk.get(&p)).or_else(|| pk.and_then(|p| d.by_attributes.get(&p))).copied()
        });
        by_asset.insert(asset, (rowid, description));
        if let Some(pk) = pk {
            asset_of.insert(pk, asset);
        }
    })?;
    Ok(read.then_some(Attributes { table, by_asset, asset_of }))
}

/// Keyword titles by asset `Z_PK`, sorted and without duplicates.
/// Named people's faces by asset `Z_PK`, from `ZPERSON` and `ZDETECTEDFACE` (Photos 11 on macOS
/// 27.3: a face's asset in `ZASSETFORFACE`, its person in `ZPERSONFORFACE`). A person merged into
/// another (`ZMERGETARGETPERSON`) is that other. Faces without a named person, hidden, in the
/// trash or of an asset not shown are left out.
fn read_faces(reader: &TableReader<'_>, warnings: &mut Vec<String>) -> Result<HashMap<i64, Vec<PhotosFace>>, PhotosError> {
    let (Some(people), Some(faces)) = (reader.find("ZPERSON"), reader.find("ZDETECTEDFACE")) else { return Ok(HashMap::new()) };
    if !faces.has("ZASSETFORFACE") || !faces.has("ZPERSONFORFACE") || !faces.has("ZCENTERX") || !faces.has("ZCENTERY") {
        return Ok(HashMap::new());
    }
    let mut names: HashMap<i64, String> = HashMap::new();
    let mut merged_into: HashMap<i64, i64> = HashMap::new();
    let read = reader.optional_scan(&people, &["Z_PK", "ZFULLNAME", "ZDISPLAYNAME", "ZMERGETARGETPERSON"], warnings, |_, row| {
        let Some(pk) = people.int(row, "Z_PK") else { return };
        if let Some(name) = people.text(row, "ZFULLNAME").or_else(|| people.text(row, "ZDISPLAYNAME")) {
            names.insert(pk, name.trim().to_string());
        }
        if let Some(target) = people.int(row, "ZMERGETARGETPERSON").filter(|&t| t != pk) {
            merged_into.insert(pk, target);
        }
    })?;
    if !read {
        return Ok(HashMap::new());
    }
    let name_of = |mut pk: i64| {
        for _ in 0..MAX_MERGE_DEPTH {
            match merged_into.get(&pk) {
                Some(&next) => pk = next,
                None => break,
            }
        }
        names.get(&pk).filter(|n| !n.is_empty()).cloned()
    };
    let columns = ["ZASSETFORFACE", "ZPERSONFORFACE", "ZCENTERX", "ZCENTERY", "ZSIZE", "ZHIDDEN", "ZISINTRASH", "ZASSETVISIBLE"];
    let mut out: HashMap<i64, Vec<PhotosFace>> = HashMap::new();
    let read = reader.optional_scan(&faces, &columns, warnings, |_, row| {
        let shown = !faces.flag(row, "ZHIDDEN") && !faces.flag(row, "ZISINTRASH") && faces.int(row, "ZASSETVISIBLE").is_none_or(|v| v != 0);
        let (Some(asset), Some(person)) = (faces.int(row, "ZASSETFORFACE"), faces.int(row, "ZPERSONFORFACE")) else { return };
        let fraction = |c: &str| faces.real(row, c).filter(|v| (0.0..=1.0).contains(v));
        let (Some(center_x), Some(center_y)) = (fraction("ZCENTERX"), fraction("ZCENTERY")) else { return };
        let Some(name) = name_of(person).filter(|_| shown) else { return };
        let list = out.entry(asset).or_default();
        if list.len() < MAX_FACES_PER_ASSET {
            list.push(PhotosFace { name, center_x, center_y, size: fraction("ZSIZE").unwrap_or(0.0) });
        }
    })?;
    if !read {
        return Ok(HashMap::new());
    }
    Ok(out)
}

fn read_keywords(
    reader: &TableReader<'_>,
    asset_of_attributes: &HashMap<i64, i64>,
    warnings: &mut Vec<String>,
) -> Result<HashMap<i64, Vec<String>>, PhotosError> {
    let Some(keyword_table) = reader.find("ZKEYWORD") else { return Ok(HashMap::new()) };
    let mut titles: HashMap<i64, String> = HashMap::new();
    let read = reader.optional_scan(&keyword_table, &["Z_PK", "ZTITLE"], warnings, |_, row| {
        if let (Some(pk), Some(title)) = (keyword_table.int(row, "Z_PK"), keyword_table.text(row, "ZTITLE")) {
            titles.insert(pk, title);
        }
    })?;
    let Some((links, attr_col, keyword_col, _)) = reader.join("KEYWORDS", "ASSETATTRIBUTES", "KEYWORDS") else { return Ok(HashMap::new()) };
    if !read {
        return Ok(HashMap::new());
    }
    let mut out: HashMap<i64, Vec<String>> = HashMap::new();
    let read = reader.optional_scan(&links, &[&attr_col, &keyword_col], warnings, |_, row| {
        let asset = links.int(row, &attr_col).and_then(|a| asset_of_attributes.get(&a));
        let title = links.int(row, &keyword_col).and_then(|k| titles.get(&k));
        if let (Some(&asset), Some(title)) = (asset, title) {
            out.entry(asset).or_default().push(title.clone());
        }
    })?;
    if !read {
        return Ok(HashMap::new());
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
    let Some(table) = reader.find("ZINTERNALRESOURCE") else { return Ok(out) };
    if !table.has("ZASSET") || !table.has("ZLOCALAVAILABILITY") {
        return Ok(out);
    }
    let columns = ["ZASSET", "ZVERSION", "ZLOCALAVAILABILITY", "ZRESOURCETYPE", "ZDATASTORESUBTYPE", "ZRECIPEID"];
    let read = reader.optional_scan(&table, &columns, warnings, |_, row| {
        let Some(asset) = table.int(row, "ZASSET") else { return };
        let current = table.int(row, "ZVERSION").is_none_or(|n| n == 0);
        let local = table.int(row, "ZLOCALAVAILABILITY") == Some(1);
        let kind = table.int(row, "ZRESOURCETYPE");
        if current && table.int(row, "ZDATASTORESUBTYPE").is_none_or(|n| n == RESOURCE_ORIGINAL) && kind.is_none_or(|n| n == 0 || n == 1) {
            let here = if local { Availability::Local } else { Availability::CloudOnly };
            let entry = out.original.entry(asset).or_insert(here);
            if here == Availability::Local {
                *entry = Availability::Local;
            }
            return;
        }
        // companions of the original, not of an edit (an edit has a recipe)
        if !current || table.int(row, "ZRECIPEID").is_some_and(|n| n != 0) {
            return;
        }
        match kind {
            Some(RESOURCE_ALTERNATE) => note(&mut out.alternate, asset, local),
            Some(RESOURCE_LIVE_VIDEO) => note(&mut out.live_video, asset, local),
            _ => {}
        }
    })?;
    if !read {
        return Ok(Resources::default());
    }
    out.known = table.has("ZRESOURCETYPE");
    Ok(out)
}

/// Companion files on disk, by `<UUID>_<resource type>`, from the `originals/<dir>` folders of
/// assets whose companions the library says are local. Only regular files named exactly
/// `<UUID>_<3 or 4>.<ext>` are taken; links, folders and anything else are ignored.
fn companion_files(library: &PhotosLibrary, dirs: &HashSet<String>, warnings: &mut Vec<String>) -> HashMap<String, PathBuf> {
    let mut out = HashMap::new();
    let mut sorted: Vec<&String> = dirs.iter().collect();
    sorted.sort();
    for dir in sorted {
        let folder = library.root().join("originals").join(dir);
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

/// Place names Photos worked out from an asset's location: the parts of its postal address a
/// photo's metadata has fields for (the street and postal code are left out).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PhotosPlace {
    /// A neighbourhood or district (Photos often has none).
    pub sublocality: Option<String>,
    pub city: Option<String>,
    pub state: Option<String>,
    pub country: Option<String>,
}

pub use crate::photos_browse::PhotosFace;

/// Deepest chain of merged people (`ZMERGETARGETPERSON`) followed to the person they became.
const MAX_MERGE_DEPTH: usize = 16;
/// Most faces kept for one photo.
const MAX_FACES_PER_ASSET: usize = 256;

/// Largest stored place record read (Photos' are about 3 KB).
const MAX_PLACE_BYTES: usize = 256 * 1024;

/// The place names in Photos' stored place record for an asset
/// (`ZADDITIONALASSETATTRIBUTES.ZREVERSELOCATIONDATA`): a binary property list written by
/// NSKeyedArchiver whose root object has a `postalAddress` with `_subLocality`, `_city`,
/// `_state` and `_country` (observed in a Photos 11 library on macOS 27.3). `None` when it can't
/// be read or names nothing.
pub fn place_from_archive(bytes: &[u8]) -> Option<PhotosPlace> {
    if bytes.len() > MAX_PLACE_BYTES {
        return None;
    }
    let archive = plist::Value::from_reader(std::io::Cursor::new(bytes)).ok()?;
    let archive = archive.as_dictionary()?;
    let objects = archive.get("$objects")?.as_array()?;
    let uid = |v: &plist::Value| v.as_uid().and_then(|u| usize::try_from(u.get()).ok());
    let root = uid(archive.get("$top")?.as_dictionary()?.get("root")?)?;
    let root = objects.get(root)?.as_dictionary()?;
    let address = objects.get(uid(root.get("postalAddress")?)?)?.as_dictionary()?;
    // object 0 is the archive's `$null`
    let text = |key: &str| {
        let at = address.get(key).and_then(uid).filter(|&i| i != 0)?;
        let s = objects.get(at)?.as_string()?.trim();
        (!s.is_empty()).then(|| s.to_string())
    };
    let place = PhotosPlace { sublocality: text("_subLocality"), city: text("_city"), state: text("_state"), country: text("_country") };
    (place != PhotosPlace::default()).then_some(place)
}

/// Extensions Photos' full-size edited version has been seen with, most likely first.
const RENDER_EXTENSIONS: &[&str] = &["heic", "jpeg", "jpg", "png", "tiff"];

/// The full-size edited version Photos keeps of asset `uuid`, if it is there (see
/// [`PhotosAsset::edited_path`]). Looks at the disk: call it off the UI thread.
pub fn rendered_path(root: &Path, uuid: &str) -> Option<PathBuf> {
    let first = uuid.chars().next()?;
    let plain = uuid.len() <= 64 && uuid.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-');
    if !plain {
        return None;
    }
    let dir = root.join("resources").join("renders").join(first.to_string());
    RENDER_EXTENSIONS.iter().map(|ext| dir.join(format!("{uuid}_1_201_a.{ext}"))).find(|p| p.is_file())
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
    let none = || (Vec::new(), HashMap::new());
    let Some(table) = reader.find("ZGENERICALBUM") else { return Ok(none()) };
    struct Raw {
        pk: i64,
        uuid: String,
        title: String,
        kind: AlbumKind,
        parent_pk: Option<i64>,
    }
    let mut raw = Vec::new();
    let mut seen = HashSet::new();
    let columns = ["Z_PK", "ZUUID", "ZTITLE", "ZKIND", "ZPARENTFOLDER", "ZTRASHEDSTATE"];
    let read = reader.optional_scan(&table, &columns, warnings, |_, row| {
        let kind = match table.int(row, "ZKIND") {
            Some(ALBUM_KIND_ALBUM) => AlbumKind::Album,
            Some(ALBUM_KIND_FOLDER) => AlbumKind::Folder,
            _ => return,
        };
        if table.flag(row, "ZTRASHEDSTATE") {
            return;
        }
        let (Some(pk), Some(uuid)) = (table.int(row, "Z_PK"), table.text(row, "ZUUID").filter(|u| valid_uuid(u))) else { return };
        if !seen.insert(uuid.clone()) {
            return;
        }
        let title = table.text(row, "ZTITLE").unwrap_or_default();
        raw.push(Raw { pk, uuid, title, kind, parent_pk: table.int(row, "ZPARENTFOLDER") });
    })?;
    if !read {
        return Ok(none());
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

/// Each album's assets, as positions in the asset index, in album order, from the
/// `Z_<n>ASSETS` join table.
fn read_album_assets(
    reader: &TableReader<'_>,
    albums: &HashMap<i64, String>,
    assets: &HashMap<i64, usize>,
    warnings: &mut Vec<String>,
) -> Result<HashMap<String, Vec<usize>>, PhotosError> {
    let mut out: HashMap<String, Vec<usize>> = HashMap::new();
    let Some((links, album_col, asset_col, order_col)) = reader.join("ASSETS", "ALBUMS", "ASSETS") else { return Ok(out) };
    let mut ordered: HashMap<&String, Vec<(i64, i64)>> = HashMap::new();
    let mut columns = vec![album_col.as_str(), asset_col.as_str()];
    columns.extend(order_col.as_deref());
    let read = reader.optional_scan(&links, &columns, warnings, |_, row| {
        let (Some(album), Some(asset)) = (links.int(row, &album_col), links.int(row, &asset_col)) else { return };
        let Some(album_uuid) = albums.get(&album) else { return };
        let order = order_col.as_deref().and_then(|c| links.int(row, c)).unwrap_or(i64::MAX);
        ordered.entry(album_uuid).or_default().push((order, asset));
    })?;
    if !read {
        return Ok(out);
    }
    for (album, mut members) in ordered {
        members.sort();
        let mut seen = HashSet::new();
        let positions = members.into_iter().filter(|(_, asset)| seen.insert(*asset)).filter_map(|(_, asset)| assets.get(&asset).copied()).collect();
        out.insert(album.clone(), positions);
    }
    Ok(out)
}

mod photokit_source;
pub use photokit_source::PhotoKitSource;

#[cfg(test)]
pub(crate) mod fixture;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_scale;
