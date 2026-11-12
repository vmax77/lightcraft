//! Synthetic `Photos.sqlite` databases for tests: a small SQLite file writer (file format §1–2) and
//! the Photos 5 and Photos 6+ tables this reader uses. No real library data: every name, date and
//! place is made up here.

use std::path::{Path, PathBuf};

pub const PAGE: usize = 4096;

#[derive(Clone, Debug)]
pub enum V {
    Null,
    Int(i64),
    Real(f64),
    Text(String),
}

pub fn t(s: &str) -> V {
    V::Text(s.into())
}

#[derive(Clone, Debug)]
pub struct Table {
    pub name: String,
    pub columns: Vec<String>,
    /// `(rowid, values)`; an `INTEGER PRIMARY KEY` column is stored as NULL, as SQLite does.
    pub rows: Vec<(i64, Vec<V>)>,
    pub sql: Option<String>,
}

impl Table {
    pub fn new(name: &str, columns: &[&str]) -> Self {
        Self { name: name.into(), columns: columns.iter().map(|c| (*c).to_string()).collect(), rows: Vec::new(), sql: None }
    }

    pub fn create_sql(&self) -> String {
        if let Some(sql) = &self.sql {
            return sql.clone();
        }
        let cols: Vec<String> =
            self.columns.iter().map(|c| if c == "Z_PK" { "Z_PK INTEGER PRIMARY KEY".into() } else { format!("{c} INTEGER") }).collect();
        format!("CREATE TABLE {} ( {} )", self.name, cols.join(", "))
    }

    /// Add a row with named values; other columns are NULL. `Z_PK` is the rowid.
    pub fn row(&mut self, values: &[(&str, V)]) -> &mut Self {
        for (name, _) in values {
            assert!(self.columns.iter().any(|c| c == name), "{} has no column {name}", self.name);
        }
        let pk = values.iter().find(|(c, _)| *c == "Z_PK").and_then(|(_, v)| if let V::Int(n) = v { Some(*n) } else { None });
        let rowid = pk.unwrap_or(self.rows.len() as i64 + 1);
        let row = self
            .columns
            .iter()
            .map(|c| if c == "Z_PK" { V::Null } else { values.iter().find(|(n, _)| n == c).map_or(V::Null, |(_, v)| v.clone()) })
            .collect();
        self.rows.push((rowid, row));
        self
    }
}

fn varint(mut value: u64) -> Vec<u8> {
    if value > 0x00ff_ffff_ffff_ffff {
        let mut out = Vec::new();
        let last = (value & 0xff) as u8;
        value >>= 8;
        for _ in 0..8 {
            out.push(((value & 0x7f) as u8) | 0x80);
            value >>= 7;
        }
        out.reverse();
        out.push(last);
        return out;
    }
    let mut out = vec![(value & 0x7f) as u8];
    value >>= 7;
    while value != 0 {
        out.push(((value & 0x7f) as u8) | 0x80);
        value >>= 7;
    }
    out.reverse();
    out
}

pub fn record(values: &[V]) -> Vec<u8> {
    let mut header = Vec::new();
    let mut body = Vec::new();
    for v in values {
        match v {
            V::Null => header.push(0),
            V::Int(n) => {
                header.push(6);
                body.extend(n.to_be_bytes());
            }
            V::Real(x) => {
                header.push(7);
                body.extend(x.to_be_bytes());
            }
            V::Text(s) => {
                header.extend(varint(13 + 2 * s.len() as u64));
                body.extend(s.as_bytes());
            }
        }
    }
    // the header length counts itself; one byte is enough for these small headers
    let mut out = varint(header.len() as u64 + 1);
    assert_eq!(out.len(), 1, "fixture record header too long");
    out.extend(header);
    out.extend(body);
    out
}

fn cell(rowid: i64, payload: &[u8]) -> Vec<u8> {
    assert!(payload.len() <= PAGE - 35, "fixture row needs an overflow page");
    let mut c = varint(payload.len() as u64);
    c.extend(varint(rowid as u64));
    c.extend(payload);
    c
}

/// Write a leaf (`0x0d`) at `base` of `page`.
fn write_leaf(page: &mut [u8], base: usize, cells: &[Vec<u8>]) {
    page[base] = 0x0d;
    page[base + 3..base + 5].copy_from_slice(&(cells.len() as u16).to_be_bytes());
    let mut end = PAGE;
    for (i, c) in cells.iter().enumerate() {
        end -= c.len();
        page[end..end + c.len()].copy_from_slice(c);
        let p = base + 8 + i * 2;
        page[p..p + 2].copy_from_slice(&(end as u16).to_be_bytes());
    }
    page[base + 5..base + 7].copy_from_slice(&(end as u16).to_be_bytes());
}

/// Write an interior (`0x05`) page at `base`: `(child, max rowid)` cells and the right child.
fn write_interior(page: &mut [u8], base: usize, children: &[(u32, i64)], right: u32) {
    page[base] = 0x05;
    page[base + 3..base + 5].copy_from_slice(&(children.len() as u16).to_be_bytes());
    page[base + 8..base + 12].copy_from_slice(&right.to_be_bytes());
    let mut end = PAGE;
    for (i, (child, key)) in children.iter().enumerate() {
        let mut c = child.to_be_bytes().to_vec();
        c.extend(varint(*key as u64));
        end -= c.len();
        page[end..end + c.len()].copy_from_slice(&c);
        let p = base + 12 + i * 2;
        page[p..p + 2].copy_from_slice(&(end as u16).to_be_bytes());
    }
    page[base + 5..base + 7].copy_from_slice(&(end as u16).to_be_bytes());
}

/// Split cells into leaves that fit a page with a `base`-byte prefix and an 8-byte header.
fn leaves(cells: Vec<(i64, Vec<u8>)>, base: usize) -> Vec<Vec<(i64, Vec<u8>)>> {
    let mut out = vec![Vec::new()];
    let mut used = base + 8;
    for (rowid, c) in cells {
        if used + c.len() + 2 > PAGE && !out.last().is_some_and(Vec::is_empty) {
            out.push(Vec::new());
            used = 8;
        }
        used += c.len() + 2;
        if let Some(last) = out.last_mut() {
            last.push((rowid, c));
        }
    }
    out
}

struct Pages(Vec<Vec<u8>>);

impl Pages {
    fn alloc(&mut self) -> u32 {
        self.0.push(vec![0; PAGE]);
        self.0.len() as u32
    }

    fn get(&mut self, id: u32) -> &mut Vec<u8> {
        &mut self.0[id as usize - 1]
    }

    /// Store a table's rows in a B-tree rooted at `root` (allocated by the caller).
    fn btree(&mut self, root: u32, mut cells: Vec<(i64, Vec<u8>)>) {
        cells.sort_by_key(|(rowid, _)| *rowid);
        let base = if root == 1 { 100 } else { 0 };
        let groups = leaves(cells.clone(), base);
        if groups.len() == 1 {
            let cs: Vec<Vec<u8>> = groups[0].iter().map(|(_, c)| c.clone()).collect();
            write_leaf(self.get(root), base, &cs);
            return;
        }
        let groups = leaves(cells, 0);
        let mut children = Vec::new();
        for group in &groups {
            let id = self.alloc();
            let cs: Vec<Vec<u8>> = group.iter().map(|(_, c)| c.clone()).collect();
            write_leaf(self.get(id), 0, &cs);
            children.push((id, group.last().map_or(0, |(r, _)| *r)));
        }
        let (right, _) = children.pop().expect("at least two leaves");
        write_interior(self.get(root), base, &children, right);
    }
}

/// A SQLite database image holding `tables`.
pub fn database(tables: &[Table]) -> Vec<u8> {
    let mut pages = Pages(Vec::new());
    pages.alloc();
    let mut schema = Vec::new();
    for (i, table) in tables.iter().enumerate() {
        let root = pages.alloc();
        let sql = table.create_sql();
        let row = [t("table"), t(&table.name), t(&table.name), V::Int(i64::from(root)), V::Text(sql)];
        schema.push((i as i64 + 1, cell(i as i64 + 1, &record(&row))));
        let cells = table.rows.iter().map(|(rowid, values)| (*rowid, cell(*rowid, &record(values)))).collect();
        pages.btree(root, cells);
    }
    pages.btree(1, schema);
    let count = pages.0.len() as u32;
    let page1 = pages.get(1);
    page1[..16].copy_from_slice(b"SQLite format 3\0");
    page1[16..18].copy_from_slice(&(PAGE as u16).to_be_bytes());
    page1[18] = 1;
    page1[19] = 1;
    page1[21] = 64;
    page1[22] = 32;
    page1[23] = 32;
    page1[24..28].copy_from_slice(&1u32.to_be_bytes());
    page1[28..32].copy_from_slice(&count.to_be_bytes());
    page1[44..48].copy_from_slice(&4u32.to_be_bytes());
    page1[56..60].copy_from_slice(&1u32.to_be_bytes());
    page1[92..96].copy_from_slice(&1u32.to_be_bytes());
    page1[96..100].copy_from_slice(&3_045_000u32.to_be_bytes());
    pages.0.concat()
}

/// A WAL that replaces whole pages, one commit at the end, with valid checksums.
pub fn wal(pages: &[(u32, &[u8])], db_pages: u32) -> Vec<u8> {
    use crate::lightroom_sqlite::wal_checksum;
    let mut out = vec![0u8; 32];
    out[..4].copy_from_slice(&0x377f_0682u32.to_be_bytes());
    out[4..8].copy_from_slice(&3_007_000u32.to_be_bytes());
    out[8..12].copy_from_slice(&(PAGE as u32).to_be_bytes());
    out[16..20].copy_from_slice(&0x1234_5678u32.to_be_bytes());
    out[20..24].copy_from_slice(&0x9abc_def0u32.to_be_bytes());
    let mut sum = wal_checksum(&out[..24], false, (0, 0));
    out[24..28].copy_from_slice(&sum.0.to_be_bytes());
    out[28..32].copy_from_slice(&sum.1.to_be_bytes());
    for (i, (id, data)) in pages.iter().enumerate() {
        let mut frame = vec![0u8; 24];
        frame[..4].copy_from_slice(&id.to_be_bytes());
        let commit = if i + 1 == pages.len() { db_pages } else { 0 };
        frame[4..8].copy_from_slice(&commit.to_be_bytes());
        let salt = [out[16], out[17], out[18], out[19], out[20], out[21], out[22], out[23]];
        frame[8..16].copy_from_slice(&salt);
        sum = wal_checksum(&frame[..8], false, sum);
        sum = wal_checksum(data, false, sum);
        frame[16..20].copy_from_slice(&sum.0.to_be_bytes());
        frame[20..24].copy_from_slice(&sum.1.to_be_bytes());
        out.extend(frame);
        out.extend_from_slice(data);
    }
    out
}

/// Which Photos layout to make.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Layout {
    Photos5,
    Photos6,
}

/// 2021-06-15T12:00:00Z as Core Data seconds.
pub const JUNE_2021: f64 = 645_451_200.0;

pub const ASSET_A: &str = "0A1B2C3D-0000-4000-8000-000000000001";
pub const ASSET_B: &str = "0A1B2C3D-0000-4000-8000-000000000002";
pub const ASSET_C: &str = "0A1B2C3D-0000-4000-8000-000000000003";
pub const FOLDER: &str = "F0000000-0000-4000-8000-000000000001";
pub const ALBUM: &str = "A0000000-0000-4000-8000-000000000001";
pub const ROOT_ALBUM: &str = "A0000000-0000-4000-8000-000000000002";

/// The tables of a small library: three assets (a local HEIC with all metadata, a cloud-only
/// RAW, a trashed video), a folder holding one album, a top-level album and two keywords.
pub fn photos_tables(layout: Layout) -> Vec<Table> {
    let (asset_table, asset_ent, album_join, album_col, asset_col) = match layout {
        Layout::Photos5 => ("ZGENERICASSET", 34, "Z_26ASSETS", "Z_26ALBUMS", "Z_34ASSETS"),
        Layout::Photos6 => ("ZASSET", 3, "Z_35ASSETS", "Z_35ALBUMS", "Z_3ASSETS"),
    };
    let fok = format!("Z_FOK_{}", asset_col.trim_start_matches("Z_"));
    let mut assets = Table::new(
        asset_table,
        &[
            "Z_PK",
            "Z_ENT",
            "ZUUID",
            "ZFILENAME",
            "ZDIRECTORY",
            "ZDATECREATED",
            "ZFAVORITE",
            "ZHIDDEN",
            "ZTRASHEDSTATE",
            "ZKIND",
            "ZKINDSUBTYPE",
            "ZUNIFORMTYPEIDENTIFIER",
            "ZWIDTH",
            "ZHEIGHT",
            "ZLATITUDE",
            "ZLONGITUDE",
            "ZADDITIONALATTRIBUTES",
        ],
    );
    assets
        .row(&[
            ("Z_PK", V::Int(1)),
            ("Z_ENT", V::Int(asset_ent)),
            ("ZUUID", t(ASSET_A)),
            ("ZFILENAME", t(&format!("{ASSET_A}.heic"))),
            ("ZDIRECTORY", t("0")),
            ("ZDATECREATED", V::Real(JUNE_2021 + 0.5)),
            ("ZFAVORITE", V::Int(1)),
            ("ZKIND", V::Int(0)),
            ("ZKINDSUBTYPE", V::Int(2)),
            ("ZUNIFORMTYPEIDENTIFIER", t("public.heic")),
            ("ZWIDTH", V::Int(4032)),
            ("ZHEIGHT", V::Int(3024)),
            ("ZLATITUDE", V::Real(48.5)),
            ("ZLONGITUDE", V::Real(-3.25)),
            ("ZADDITIONALATTRIBUTES", V::Int(11)),
        ])
        .row(&[
            ("Z_PK", V::Int(2)),
            ("ZUUID", t(ASSET_B)),
            ("ZFILENAME", t(&format!("{ASSET_B}.dng"))),
            ("ZDIRECTORY", t("0")),
            ("ZDATECREATED", V::Int(JUNE_2021 as i64 + 60)),
            ("ZHIDDEN", V::Int(1)),
            ("ZKIND", V::Int(0)),
            ("ZUNIFORMTYPEIDENTIFIER", t("com.adobe.raw-image")),
            ("ZLATITUDE", V::Real(-180.0)),
            ("ZLONGITUDE", V::Real(-180.0)),
            ("ZADDITIONALATTRIBUTES", V::Int(12)),
        ])
        .row(&[
            ("Z_PK", V::Int(3)),
            ("ZUUID", t(ASSET_C)),
            ("ZFILENAME", t(&format!("{ASSET_C}.mov"))),
            ("ZDIRECTORY", t("0")),
            ("ZTRASHEDSTATE", V::Int(1)),
            ("ZKIND", V::Int(1)),
            ("ZUNIFORMTYPEIDENTIFIER", t("com.apple.quicktime-movie")),
        ]);
    let mut attrs = Table::new(
        "ZADDITIONALASSETATTRIBUTES",
        &["Z_PK", "ZASSET", "ZORIGINALFILENAME", "ZORIGINALFILESIZE", "ZTITLE", "ZTIMEZONEOFFSET", "ZASSETDESCRIPTION"],
    );
    attrs
        .row(&[
            ("Z_PK", V::Int(11)),
            ("ZASSET", V::Int(1)),
            ("ZORIGINALFILENAME", t("IMG_0001.HEIC")),
            ("ZORIGINALFILESIZE", V::Int(2_345_678)),
            ("ZTITLE", t("Harbour at noon")),
            ("ZTIMEZONEOFFSET", V::Int(7200)),
            ("ZASSETDESCRIPTION", V::Int(21)),
        ])
        .row(&[("Z_PK", V::Int(12)), ("ZASSET", V::Int(2)), ("ZORIGINALFILENAME", t("DSC_0002.DNG"))]);
    let mut descriptions = Table::new("ZASSETDESCRIPTION", &["Z_PK", "ZASSETATTRIBUTES", "ZLONGDESCRIPTION"]);
    descriptions.row(&[("Z_PK", V::Int(21)), ("ZASSETATTRIBUTES", V::Int(11)), ("ZLONGDESCRIPTION", t("Boats, gulls and a lighthouse."))]);
    let mut keywords = Table::new("ZKEYWORD", &["Z_PK", "ZTITLE", "ZUUID"]);
    keywords.row(&[("Z_PK", V::Int(31)), ("ZTITLE", t("sea"))]).row(&[("Z_PK", V::Int(32)), ("ZTITLE", t("boats"))]);
    let keyword_col = if layout == Layout::Photos5 { "Z_37KEYWORDS" } else { "Z_54KEYWORDS" };
    let mut keyword_join = Table::new("Z_1KEYWORDS", &["Z_1ASSETATTRIBUTES", keyword_col]);
    keyword_join
        .row(&[("Z_1ASSETATTRIBUTES", V::Int(11)), (keyword_col, V::Int(31))])
        .row(&[("Z_1ASSETATTRIBUTES", V::Int(11)), (keyword_col, V::Int(32))])
        .row(&[("Z_1ASSETATTRIBUTES", V::Int(11)), (keyword_col, V::Int(32))]);
    let mut albums = Table::new("ZGENERICALBUM", &["Z_PK", "ZUUID", "ZTITLE", "ZKIND", "ZPARENTFOLDER", "ZTRASHEDSTATE"]);
    albums
        .row(&[("Z_PK", V::Int(40)), ("ZUUID", t("R0000000-0000-4000-8000-000000000000")), ("ZKIND", V::Int(3999))])
        .row(&[("Z_PK", V::Int(41)), ("ZUUID", t(ALBUM)), ("ZTITLE", t("Harbour")), ("ZKIND", V::Int(2)), ("ZPARENTFOLDER", V::Int(42))])
        .row(&[("Z_PK", V::Int(42)), ("ZUUID", t(FOLDER)), ("ZTITLE", t("Trips")), ("ZKIND", V::Int(4000)), ("ZPARENTFOLDER", V::Int(40))])
        .row(&[("Z_PK", V::Int(43)), ("ZUUID", t(ROOT_ALBUM)), ("ZTITLE", t("Family")), ("ZKIND", V::Int(2)), ("ZPARENTFOLDER", V::Int(40))])
        .row(&[
            ("Z_PK", V::Int(44)),
            ("ZUUID", t("A0000000-0000-4000-8000-0000000000ff")),
            ("ZTITLE", t("Old")),
            ("ZKIND", V::Int(2)),
            ("ZTRASHEDSTATE", V::Int(1)),
        ])
        .row(&[("Z_PK", V::Int(45)), ("ZUUID", t("S0000000-0000-4000-8000-000000000000")), ("ZTITLE", t("Smart")), ("ZKIND", V::Int(1507))]);
    let mut album_assets = Table::new(album_join, &[album_col, asset_col, &fok]);
    album_assets
        .row(&[(album_col, V::Int(41)), (asset_col, V::Int(2)), (fok.as_str(), V::Int(2048))])
        .row(&[(album_col, V::Int(41)), (asset_col, V::Int(1)), (fok.as_str(), V::Int(1024))])
        .row(&[(album_col, V::Int(43)), (asset_col, V::Int(3)), (fok.as_str(), V::Int(1024))]);
    let mut resources =
        Table::new("ZINTERNALRESOURCE", &["Z_PK", "ZASSET", "ZRESOURCETYPE", "ZDATASTORESUBTYPE", "ZVERSION", "ZRECIPEID", "ZLOCALAVAILABILITY"]);
    resources
        .row(&[
            ("Z_PK", V::Int(51)),
            ("ZASSET", V::Int(1)),
            ("ZRESOURCETYPE", V::Int(0)),
            ("ZDATASTORESUBTYPE", V::Int(1)),
            ("ZVERSION", V::Int(0)),
            ("ZLOCALAVAILABILITY", V::Int(1)),
        ])
        // a derivative that is local does not make the cloud-only original local
        .row(&[
            ("Z_PK", V::Int(52)),
            ("ZASSET", V::Int(2)),
            ("ZRESOURCETYPE", V::Int(0)),
            ("ZDATASTORESUBTYPE", V::Int(1)),
            ("ZVERSION", V::Int(0)),
            ("ZLOCALAVAILABILITY", V::Int(-1)),
        ])
        .row(&[
            ("Z_PK", V::Int(53)),
            ("ZASSET", V::Int(2)),
            ("ZRESOURCETYPE", V::Int(0)),
            ("ZDATASTORESUBTYPE", V::Int(5)),
            ("ZVERSION", V::Int(0)),
            ("ZLOCALAVAILABILITY", V::Int(1)),
        ]);
    let mut metadata = Table::new("Z_METADATA", &["Z_VERSION", "Z_UUID", "Z_PLIST"]);
    metadata.row(&[("Z_VERSION", V::Int(1))]);
    vec![metadata, assets, attrs, descriptions, keywords, keyword_join, albums, album_assets, resources]
}

/// A library folder holding a database made from `tables` (and `wal`, if any). Removed on drop.
pub struct Library {
    pub root: PathBuf,
    dir: PathBuf,
}

impl Library {
    pub fn new(tables: &[Table]) -> Self {
        Self::from_bytes(&database(tables), None)
    }

    pub fn from_bytes(db: &[u8], wal: Option<&[u8]>) -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("lightcraft-photos-{}-{n}", std::process::id()));
        let root = dir.join("Test.photoslibrary");
        std::fs::create_dir_all(root.join("database")).unwrap();
        std::fs::write(root.join("database/Photos.sqlite"), db).unwrap();
        if let Some(wal) = wal {
            std::fs::write(root.join("database/Photos.sqlite-wal"), wal).unwrap();
        }
        Self { root, dir }
    }

    pub fn database(&self) -> PathBuf {
        self.root.join("database/Photos.sqlite")
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }
}

impl Drop for Library {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}
