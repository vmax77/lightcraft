use super::fixture::{self, ALBUM, ASSET_A, ASSET_B, ASSET_C, FOLDER, JUNE_2021, Layout, Library, ROOT_ALBUM, Table, V, t};
use super::*;

fn open(library: &Library) -> Result<SqliteSource, PhotosError> {
    SqliteSource::open(&PhotosLibrary::at(&library.root)?, &AtomicBool::new(false))
}

fn open_tables(tables: &[Table]) -> Result<SqliteSource, PhotosError> {
    open(&Library::new(tables))
}

fn asset(source: &SqliteSource, uuid: &str) -> PhotosAsset {
    source.asset(uuid).unwrap().unwrap()
}

fn table<'a>(tables: &'a mut [Table], name: &str) -> &'a mut Table {
    tables.iter_mut().find(|t| t.name == name).unwrap()
}

fn check_small_library(source: &SqliteSource, root: &Path) {
    assert_eq!(source.asset_count(), 3);
    let a = asset(source, ASSET_A);
    assert_eq!(a.file_name.as_deref(), Some(&*format!("{ASSET_A}.heic")));
    assert_eq!(a.original_file_name.as_deref(), Some("IMG_0001.HEIC"));
    assert_eq!(a.original_path, Some(root.join("originals").join("0").join(format!("{ASSET_A}.heic"))));
    let created = a.created.unwrap();
    assert_eq!(created.to_rfc3339(), "2021-06-15T14:00:00.500+02:00");
    assert!(a.favourite && !a.hidden && !a.trashed && a.live_photo);
    assert_eq!(a.kind, AssetKind::Photo);
    assert_eq!(a.uti.as_deref(), Some("public.heic"));
    assert_eq!((a.width, a.height, a.original_size), (Some(4032), Some(3024), Some(2_345_678)));
    assert_eq!(a.location, Some(GeoPoint { latitude: 48.5, longitude: -3.25 }));
    assert_eq!(a.title.as_deref(), Some("Harbour at noon"));
    assert_eq!(a.caption.as_deref(), Some("Boats, gulls and a lighthouse."));
    assert_eq!(a.keywords, ["boats", "sea"]);
    assert_eq!(a.availability, Availability::Local);

    let b = asset(source, ASSET_B);
    assert!(b.hidden && !b.favourite && !b.live_photo);
    assert_eq!(b.created.unwrap().to_rfc3339(), "2021-06-15T12:01:00+00:00");
    assert_eq!(b.location, None, "-180/-180 means no location");
    assert_eq!(b.availability, Availability::CloudOnly, "a local derivative does not make the original local");
    assert_eq!((b.title.as_ref(), b.caption.as_ref(), b.keywords.len()), (None, None, 0));

    let c = asset(source, ASSET_C);
    assert!(c.trashed);
    assert_eq!(c.kind, AssetKind::Video);
    assert_eq!((c.created, c.availability, c.original_file_name.as_ref()), (None, Availability::Unknown, None));

    let albums = source.albums().unwrap();
    let names: Vec<_> = albums.iter().map(|a| (a.title.as_str(), a.kind, a.parent.as_deref())).collect();
    assert_eq!(names, [("Trips", AlbumKind::Folder, None), ("Family", AlbumKind::Album, None), ("Harbour", AlbumKind::Album, Some(FOLDER))]);
    assert_eq!(source.album_assets(ALBUM).unwrap().unwrap(), [ASSET_A, ASSET_B], "album order follows Z_FOK");
    assert_eq!(source.album_assets(ROOT_ALBUM).unwrap().unwrap(), [ASSET_C]);
    assert_eq!(source.album_assets(FOLDER).unwrap(), None);
    assert_eq!(source.album_assets("nope").unwrap(), None);
    assert!(source.warnings().is_empty(), "{:?}", source.warnings());
}

#[test]
fn photos6_library_maps_assets_albums_and_keywords() {
    let library = Library::new(&fixture::photos_tables(Layout::Photos6));
    let source = open(&library).unwrap();
    assert_eq!(source.schema(), SchemaVersion::Photos6OrLater);
    check_small_library(&source, &library.root);
}

#[test]
fn photos5_library_reads_the_same_way() {
    let library = Library::new(&fixture::photos_tables(Layout::Photos5));
    let source = open(&library).unwrap();
    assert_eq!(source.schema(), SchemaVersion::Photos5);
    check_small_library(&source, &library.root);
}

#[test]
fn assets_come_in_pages() {
    let source = open_tables(&fixture::photos_tables(Layout::Photos6)).unwrap();
    let all: Vec<String> = source.assets(0, usize::MAX).unwrap().into_iter().map(|a| a.uuid).collect();
    assert_eq!(all, [ASSET_A, ASSET_B, ASSET_C]);
    let pages: Vec<String> = [source.assets(0, 2).unwrap(), source.assets(2, 2).unwrap()].concat().into_iter().map(|a| a.uuid).collect();
    assert_eq!(pages, all);
    assert!(source.assets(3, 1).unwrap().is_empty());
    assert!(source.assets(usize::MAX, usize::MAX).unwrap().is_empty());
    assert!(source.assets(1, 0).unwrap().is_empty());
}

#[test]
fn many_assets_span_several_btree_pages() {
    let mut tables = fixture::photos_tables(Layout::Photos6);
    let assets = table(&mut tables, "ZASSET");
    for pk in 100..2100 {
        assets.row(&[("Z_PK", V::Int(pk)), ("ZUUID", t(&format!("B0000000-0000-4000-8000-{pk:012}"))), ("ZDATECREATED", V::Real(JUNE_2021))]);
    }
    let source = open_tables(&tables).unwrap();
    assert_eq!(source.asset_count(), 2003);
    assert_eq!(source.assets(1000, 500).unwrap().len(), 500);
    assert!(source.asset("B0000000-0000-4000-8000-000000002099").unwrap().is_some());
}

/// Page 3 is `ZASSET` in [`fixture::photos_tables`] (page 1 is the schema, page 2 `Z_METADATA`).
const ASSET_PAGE: u32 = 3;

fn page(image: &[u8], id: u32) -> &[u8] {
    let start = (id as usize - 1) * fixture::PAGE;
    &image[start..start + fixture::PAGE]
}

#[test]
fn committed_wal_frames_are_read_and_broken_ones_ignored() {
    let tables = fixture::photos_tables(Layout::Photos6);
    let main = fixture::database(&tables);
    let mut changed = tables.clone();
    let row = &mut table(&mut changed, "ZASSET").rows[0].1;
    row[6] = V::Int(0); // ZFAVORITE of asset A
    let newer = fixture::database(&changed);
    let pages = (main.len() / fixture::PAGE) as u32;
    let wal = fixture::wal(&[(ASSET_PAGE, page(&newer, ASSET_PAGE))], pages);

    let library = Library::from_bytes(&main, Some(&wal));
    assert!(!asset(&open(&library).unwrap(), ASSET_A).favourite, "the committed WAL frame wins");

    let mut torn = wal.clone();
    let last = torn.len() - 1;
    torn[last] ^= 0xff; // checksum no longer matches: the frame was never committed
    let library = Library::from_bytes(&main, Some(&torn));
    assert!(asset(&open(&library).unwrap(), ASSET_A).favourite);

    let library = Library::from_bytes(&main, Some(b"not a wal at all, just bytes that are long enough"));
    assert!(matches!(open(&library), Err(PhotosError::Unreadable(_))));
}

#[test]
fn library_lookup_and_listing() {
    let library = Library::new(&fixture::photos_tables(Layout::Photos6));
    assert_eq!(PhotosLibrary::at(&library.root).unwrap().database_path(), library.database());
    assert_eq!(PhotosLibrary::list_in(library.dir()), std::slice::from_ref(&library.root));
    assert!(PhotosLibrary::list_in(&library.dir().join("missing")).is_empty());
    assert_eq!(PhotosLibrary::default_system_path(Path::new("/Users/x")), Path::new("/Users/x/Pictures/Photos Library.photoslibrary"));

    let not_library = library.dir().join("Empty.photoslibrary");
    std::fs::create_dir_all(not_library.join("database/Photos.sqlite")).unwrap();
    assert_eq!(PhotosLibrary::at(&not_library), Err(PhotosError::NotALibrary(not_library.clone())), "a directory is not a database");
    assert!(matches!(PhotosLibrary::at(&library.dir().join("nowhere")), Err(PhotosError::NotALibrary(_))));
}

#[test]
fn unknown_schema_is_a_clear_error() {
    let mut tables = fixture::photos_tables(Layout::Photos6);
    tables.retain(|t| t.name != "ZASSET");
    let err = open_tables(&tables).unwrap_err();
    assert!(matches!(&err, PhotosError::UnsupportedVersion(m) if m.contains("before Photos 5")), "{err}");

    let mut tables = fixture::photos_tables(Layout::Photos6);
    let assets = table(&mut tables, "ZASSET");
    assets.columns[2] = "ZSOMETHINGELSE".into();
    assert!(matches!(open_tables(&tables), Err(PhotosError::UnsupportedVersion(_))));
}

#[test]
fn every_optional_table_may_be_missing() {
    for missing in [
        "ZADDITIONALASSETATTRIBUTES",
        "ZASSETDESCRIPTION",
        "ZKEYWORD",
        "Z_1KEYWORDS",
        "ZGENERICALBUM",
        "Z_35ASSETS",
        "ZINTERNALRESOURCE",
        "Z_METADATA",
    ] {
        let mut tables = fixture::photos_tables(Layout::Photos6);
        tables.retain(|t| t.name != missing);
        let source = open_tables(&tables).unwrap_or_else(|e| panic!("{missing}: {e}"));
        assert_eq!(source.asset_count(), 3, "{missing}");
    }
    let mut tables = fixture::photos_tables(Layout::Photos6);
    tables.retain(|t| t.name == "ZASSET");
    let source = open_tables(&tables).unwrap();
    let a = asset(&source, ASSET_A);
    assert_eq!((a.title, a.caption, a.keywords.len(), a.availability), (None, None, 0, Availability::Unknown));
    assert_eq!(a.created.unwrap().offset().local_minus_utc(), 0, "no recorded time zone: UTC");
    assert!(source.albums().unwrap().is_empty());
}

#[test]
fn hostile_values_are_skipped_not_trusted() {
    let mut tables = fixture::photos_tables(Layout::Photos6);
    let assets = table(&mut tables, "ZASSET");
    assets
        .row(&[("Z_PK", V::Int(7)), ("ZUUID", t("../../etc"))])
        .row(&[("Z_PK", V::Int(8)), ("ZUUID", t(ASSET_A))])
        .row(&[("Z_PK", V::Int(9)), ("ZUUID", t(&"A".repeat(200)))])
        .row(&[("Z_PK", V::Int(10)), ("ZUUID", V::Int(5))])
        .row(&[
            ("Z_PK", V::Int(11)),
            ("ZUUID", t("H1")),
            ("ZDIRECTORY", t("..")),
            ("ZFILENAME", t("x.jpg")),
            ("ZDATECREATED", V::Real(1e300)),
            ("ZLATITUDE", V::Real(91.0)),
            ("ZLONGITUDE", V::Real(0.0)),
            ("ZWIDTH", V::Int(-4)),
            ("ZHEIGHT", V::Int(i64::MAX)),
            ("ZKIND", V::Int(99)),
            ("ZFAVORITE", t("yes")),
            ("ZADDITIONALATTRIBUTES", V::Int(13)),
        ])
        .row(&[("Z_PK", V::Int(12)), ("ZUUID", t("H2")), ("ZDIRECTORY", t("0")), ("ZFILENAME", t("../../x.jpg")), ("ZDATECREATED", V::Real(-1e12))])
        .row(&[("Z_PK", V::Int(13)), ("ZUUID", t("H3")), ("ZDIRECTORY", t("/tmp")), ("ZFILENAME", t("x.jpg"))])
        .row(&[("Z_PK", V::Int(14)), ("ZUUID", t("H4")), ("ZDIRECTORY", t("0")), ("ZFILENAME", t("a\\b.jpg"))]);
    table(&mut tables, "ZADDITIONALASSETATTRIBUTES").row(&[
        ("Z_PK", V::Int(13)),
        ("ZASSET", V::Int(11)),
        ("ZORIGINALFILESIZE", V::Int(-1)),
        ("ZTIMEZONEOFFSET", V::Int(1_000_000_000)),
        ("ZASSETDESCRIPTION", V::Int(999)),
    ]);
    table(&mut tables, "Z_1KEYWORDS").row(&[("Z_1ASSETATTRIBUTES", V::Int(13)), ("Z_54KEYWORDS", V::Int(404))]);
    table(&mut tables, "Z_35ASSETS")
        .row(&[("Z_35ALBUMS", V::Int(41)), ("Z_3ASSETS", V::Int(404))])
        .row(&[("Z_35ALBUMS", V::Int(404)), ("Z_3ASSETS", V::Int(1))]);
    let source = open_tables(&tables).unwrap();
    assert_eq!(source.asset_count(), 3 + 4, "bad, duplicate and over-long UUIDs are skipped");
    assert!(source.warnings().iter().any(|w| w.contains("skipped 4 assets")), "{:?}", source.warnings());
    assert_eq!(asset(&source, ASSET_A).original_size, Some(2_345_678), "the duplicate does not replace the first");
    let h1 = asset(&source, "H1");
    assert_eq!((h1.original_path, h1.created, h1.location, h1.width, h1.height), (None, None, None, None, None));
    assert_eq!((h1.kind, h1.favourite, h1.original_size, h1.keywords.len()), (AssetKind::Other(99), false, None, 0));
    assert_eq!(h1.caption, None);
    for uuid in ["H2", "H3", "H4"] {
        assert_eq!(asset(&source, uuid).original_path, None, "{uuid}");
    }
    assert_eq!(asset(&source, "H2").created, None);
    assert_eq!(source.album_assets(ALBUM).unwrap().unwrap(), [ASSET_A, ASSET_B], "unknown members are dropped");
}

#[test]
fn time_zone_offsets_out_of_range_fall_back_to_utc() {
    assert_eq!(core_data_date(0.0, Some(86_400)).unwrap().to_rfc3339(), "2001-01-01T00:00:00+00:00");
    assert_eq!(core_data_date(0.0, Some(-3600)).unwrap().to_rfc3339(), "2000-12-31T23:00:00-01:00");
    assert_eq!(core_data_date(-0.25, None).unwrap().to_rfc3339(), "2000-12-31T23:59:59.750+00:00");
    assert_eq!(core_data_date(f64::NAN, None), None);
    assert_eq!(core_data_date(f64::INFINITY, None), None);
}

#[test]
fn looping_folders_do_not_hang() {
    let mut tables = fixture::photos_tables(Layout::Photos6);
    table(&mut tables, "ZGENERICALBUM")
        .row(&[("Z_PK", V::Int(60)), ("ZUUID", t("L1")), ("ZTITLE", t("Loop 1")), ("ZKIND", V::Int(4000)), ("ZPARENTFOLDER", V::Int(61))])
        .row(&[("Z_PK", V::Int(61)), ("ZUUID", t("L2")), ("ZTITLE", t("Loop 2")), ("ZKIND", V::Int(4000)), ("ZPARENTFOLDER", V::Int(60))])
        .row(&[("Z_PK", V::Int(62)), ("ZUUID", t("L3")), ("ZTITLE", t("Self")), ("ZKIND", V::Int(4000)), ("ZPARENTFOLDER", V::Int(62))])
        .row(&[("Z_PK", V::Int(63)), ("ZUUID", t("L4")), ("ZTITLE", t("In loop")), ("ZKIND", V::Int(2)), ("ZPARENTFOLDER", V::Int(60))])
        .row(&[("Z_PK", V::Int(64)), ("ZUUID", t(ALBUM)), ("ZTITLE", t("Duplicate")), ("ZKIND", V::Int(2))]);
    let mut deep = Vec::new();
    for i in 0..200i64 {
        deep.push((70 + i, if i == 0 { None } else { Some(69 + i) }));
    }
    for (pk, parent) in deep {
        let mut row = vec![("Z_PK", V::Int(pk)), ("ZUUID", t(&format!("D{pk}"))), ("ZKIND", V::Int(4000))];
        if let Some(parent) = parent {
            row.push(("ZPARENTFOLDER", V::Int(parent)));
        }
        table(&mut tables, "ZGENERICALBUM").row(&row);
    }
    let source = open_tables(&tables).unwrap();
    let albums = source.albums().unwrap();
    for uuid in ["L1", "L2", "L3", "L4"] {
        assert_eq!(albums.iter().find(|a| a.uuid == uuid).unwrap().parent, None, "{uuid}");
    }
    assert_eq!(albums.iter().filter(|a| a.uuid == ALBUM).count(), 1, "duplicate UUIDs are listed once");
    assert!(source.warnings().iter().any(|w| w.contains("looping")), "{:?}", source.warnings());
    let position = |uuid: &str| albums.iter().position(|a| a.uuid == uuid).unwrap();
    for album in &albums {
        if let Some(parent) = &album.parent {
            assert!(position(parent) < position(&album.uuid), "{} listed before its parent", album.uuid);
        }
    }
}

#[test]
fn unreadable_optional_tables_become_warnings() {
    let tables = fixture::photos_tables(Layout::Photos6);
    let index = tables.iter().position(|t| t.name == "ZINTERNALRESOURCE").unwrap();
    let mut image = fixture::database(&tables);
    // each fixture table is one leaf page, in order after the schema page
    image[(index + 1) * fixture::PAGE] = 0x07;
    let source = open(&Library::from_bytes(&image, None)).unwrap();
    assert_eq!(asset(&source, ASSET_A).availability, Availability::Unknown);
    assert!(source.warnings().iter().any(|w| w.contains("ZINTERNALRESOURCE")), "{:?}", source.warnings());

    let index = tables.iter().position(|t| t.name == "ZASSET").unwrap();
    let mut image = fixture::database(&tables);
    image[(index + 1) * fixture::PAGE] = 0x07;
    assert!(matches!(open(&Library::from_bytes(&image, None)), Err(PhotosError::Unreadable(_))));
}

#[test]
fn truncated_and_garbage_databases_fail_cleanly() {
    let image = fixture::database(&fixture::photos_tables(Layout::Photos6));
    for len in (0..image.len()).step_by(97).chain([image.len() - fixture::PAGE, image.len() - 1]) {
        let library = Library::from_bytes(&image[..len], None);
        assert!(open(&library).is_err(), "truncated to {len} bytes");
    }
    let library = Library::from_bytes(&vec![0x5a; 3 * fixture::PAGE], None);
    assert!(matches!(open(&library), Err(PhotosError::Unreadable(_))));
}

#[test]
fn corrupted_bytes_never_panic() {
    let image = fixture::database(&fixture::photos_tables(Layout::Photos6));
    let mut state = 0x9e37_79b9_7f4a_7c15u64;
    let mut next = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    for _ in 0..400 {
        let mut bad = image.clone();
        for _ in 0..(next() % 8 + 1) {
            let at = (next() % bad.len() as u64) as usize;
            bad[at] = next() as u8;
        }
        let library = Library::from_bytes(&bad, None);
        // any result is fine; a panic fails the test
        let _ = open(&library).map(|s| (s.asset_count(), s.albums().map(|a| a.len())));
    }
}

#[test]
fn cancel_stops_reading() {
    let library = Library::new(&fixture::photos_tables(Layout::Photos6));
    let result = SqliteSource::open(&PhotosLibrary::at(&library.root).unwrap(), &AtomicBool::new(true));
    assert!(matches!(result, Err(PhotosError::Cancelled)));
}

#[test]
fn a_change_while_reading_is_reported() {
    let library = Library::new(&fixture::photos_tables(Layout::Photos6));
    let snapshot = Snapshot::open(&library.database()).unwrap();
    snapshot.ensure_unchanged().unwrap();
    std::fs::write(library.database().with_extension("sqlite-wal"), b"x").unwrap();
    assert_eq!(snapshot.ensure_unchanged(), Err(PhotosError::Changed), "a new WAL");
    let snapshot = Snapshot::open(&library.database()).err();
    assert!(matches!(snapshot, Some(PhotosError::Unreadable(_))), "a WAL too short for a header");
    std::fs::remove_file(library.database().with_extension("sqlite-wal")).unwrap();
    let snapshot = Snapshot::open(&library.database()).unwrap();
    let mut bytes = std::fs::read(library.database()).unwrap();
    bytes.extend(vec![0; fixture::PAGE]);
    std::fs::write(library.database(), bytes).unwrap();
    assert_eq!(snapshot.ensure_unchanged(), Err(PhotosError::Changed), "a longer database");
}

/// Opt-in check against a real library: `LIGHTCRAFT_PHOTOS_LIBRARY=<path to .photoslibrary>`.
/// Prints counts only, never names or places.
#[test]
fn real_library_opens_when_named() {
    let Some(path) = std::env::var_os("LIGHTCRAFT_PHOTOS_LIBRARY") else {
        lightcraft_testkit::skip!(Input, "set LIGHTCRAFT_PHOTOS_LIBRARY=<path to a .photoslibrary>");
        return;
    };
    let library = PhotosLibrary::at(Path::new(&path)).unwrap();
    let started = std::time::Instant::now();
    let source = match SqliteSource::open(&library, &AtomicBool::new(false)) {
        Err(PhotosError::Changed) => SqliteSource::open(&library, &AtomicBool::new(false)).unwrap(),
        other => other.unwrap(),
    };
    let assets = source.assets(0, usize::MAX).unwrap();
    let count = |f: &dyn Fn(&PhotosAsset) -> bool| assets.iter().filter(|a| f(a)).count();
    eprintln!(
        "{:?}: {} assets in {:?}; local {}, cloud-only {}, unknown {}; dated {}, located {}, with path {}, keywords {}, titles {}, captions {}; {} albums; RAW+JPEG pairs local {}, cloud-only {}, unknown {}; Live Photo videos local {}, cloud-only {}, unknown {}; warnings {:?}",
        source.schema(),
        assets.len(),
        started.elapsed(),
        count(&|a| a.availability == Availability::Local),
        count(&|a| a.availability == Availability::CloudOnly),
        count(&|a| a.availability == Availability::Unknown),
        count(&|a| a.created.is_some()),
        count(&|a| a.location.is_some()),
        count(&|a| a.original_path.is_some()),
        count(&|a| !a.keywords.is_empty()),
        count(&|a| a.title.is_some()),
        count(&|a| a.caption.is_some()),
        source.albums().unwrap().len(),
        count(&|a| matches!(a.paired_original, Companion::Local(_))),
        count(&|a| a.paired_original == Companion::CloudOnly),
        count(&|a| a.paired_original == Companion::Unknown),
        count(&|a| matches!(a.live_video, Companion::Local(_))),
        count(&|a| a.live_video == Companion::CloudOnly),
        count(&|a| a.live_video == Companion::Unknown),
        source.warnings(),
    );
    assert!(!assets.is_empty());
}

mod photokit {
    use super::super::photokit_source::{map_albums, map_asset, uuid_of};
    use super::*;
    use lightcraft_photokit as pk;

    fn known_a() -> PhotosAsset {
        let library = Library::new(&fixture::photos_tables(Layout::Photos6));
        asset(&open(&library).unwrap(), ASSET_A)
    }

    #[test]
    fn local_identifiers_give_uuids() {
        assert_eq!(uuid_of(&format!("{ASSET_A}/L0/001")).as_deref(), Some(ASSET_A));
        assert_eq!(uuid_of(ASSET_A).as_deref(), Some(ASSET_A));
        for bad in ["", "/L0/001", "../x/L0/001", "a b/L0/001", &"A".repeat(100)] {
            assert_eq!(uuid_of(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn photokit_assets_are_completed_from_photos_sqlite() {
        let known = known_a();
        // 2021-06-15T12:00:00.5Z, as PhotoKit gives it (seconds since 1970)
        let photokit = pk::Asset {
            local_id: format!("{ASSET_A}/L0/001"),
            kind: Some(pk::MediaKind::Image),
            live_photo: true,
            width: 4032,
            height: 3024,
            created: Some(1_623_758_400.5),
            location: Some((48.5, -3.25)),
            favourite: true,
            ..pk::Asset::default()
        };
        let mapped = map_asset(photokit.clone(), Some(&known)).unwrap();
        assert_eq!(mapped.created.unwrap().to_rfc3339(), "2021-06-15T14:00:00.500+02:00", "time zone from Photos.sqlite");
        assert_eq!((mapped.title.as_deref(), mapped.caption.as_deref()), (Some("Harbour at noon"), Some("Boats, gulls and a lighthouse.")));
        assert_eq!(mapped.keywords, ["boats", "sea"]);
        assert_eq!(
            (mapped.original_path, mapped.availability, mapped.original_size),
            (known.original_path.clone(), Availability::Local, Some(2_345_678))
        );
        assert_eq!(mapped.original_file_name.as_deref(), Some("IMG_0001.HEIC"));
        assert!(mapped.favourite && mapped.live_photo && !mapped.trashed);

        // macOS 27: PhotoKit's own caption and keywords win, even when empty
        let newer =
            pk::Asset { caption: None, keywords: Some(vec!["harbour".into()]), original_file_name: Some("IMG_1.HEIC".into()), ..photokit.clone() };
        let mapped = map_asset(newer, Some(&known)).unwrap();
        assert_eq!((mapped.caption, mapped.keywords, mapped.original_file_name.as_deref()), (None, vec!["harbour".to_string()], Some("IMG_1.HEIC")));

        let alone = map_asset(photokit, None).unwrap();
        assert_eq!(alone.created.unwrap().to_rfc3339(), "2021-06-15T12:00:00.500+00:00");
        assert_eq!((alone.title, alone.original_path, alone.availability), (None, None, Availability::Unknown));
    }

    #[test]
    fn hostile_photokit_values_are_dropped() {
        let bad = pk::Asset {
            local_id: format!("{ASSET_B}/L0/001"),
            kind: None,
            width: u64::MAX,
            height: 0,
            created: Some(f64::NAN),
            location: Some((120.0, 0.0)),
            ..pk::Asset::default()
        };
        let mapped = map_asset(bad, None).unwrap();
        assert_eq!((mapped.width, mapped.height, mapped.created, mapped.location, mapped.kind), (None, None, None, None, AssetKind::Other(0)));
        assert_eq!(map_asset(pk::Asset { local_id: "../../etc".into(), ..pk::Asset::default() }, None), None);
        assert_eq!(map_asset(pk::Asset { local_id: String::new(), created: Some(1e300), ..pk::Asset::default() }, None), None);
    }

    #[test]
    fn photokit_albums_map_to_uuids_parents_first() {
        let album = |id: &str, folder: bool, parent: Option<&str>| pk::Album {
            local_id: format!("{id}/L0/040"),
            title: Some(id.into()),
            folder,
            parent: parent.map(|p| format!("{p}/L0/020")),
        };
        let input = [
            album("F1", true, None),
            album("A1", false, Some("F1")),
            album("A1", false, None),
            album("A2", false, Some("missing")),
            album("bad id", false, None),
            pk::Album { local_id: "A3/L0/040".into(), title: None, folder: false, parent: None },
        ];
        let (albums, ids) = map_albums(&input);
        let got: Vec<_> = albums.iter().map(|a| (a.uuid.as_str(), a.kind, a.parent.as_deref(), a.title.as_str())).collect();
        assert_eq!(
            got,
            [
                ("F1", AlbumKind::Folder, None, "F1"),
                ("A1", AlbumKind::Album, Some("F1"), "A1"),
                ("A2", AlbumKind::Album, None, "A2"),
                ("A3", AlbumKind::Album, None, "")
            ]
        );
        assert_eq!(ids.get("A1").map(String::as_str), Some("A1/L0/040"));
    }

    #[test]
    fn photokit_source_without_access_is_a_clear_error() {
        // a test binary has no NSPhotoLibraryUsageDescription, so PhotoKit is never called
        let err = PhotoKitSource::open(None).unwrap_err();
        assert!(matches!(err, PhotosError::Unavailable(_)), "{err}");
    }
}

#[test]
fn blank_captions_titles_and_keywords_are_no_text() {
    let mut tables = fixture::photos_tables(Layout::Photos6);
    let blank = " ".repeat(31);
    table(&mut tables, "ZASSETDESCRIPTION").rows[0].1[2] = t(&blank);
    table(&mut tables, "ZADDITIONALASSETATTRIBUTES").rows[0].1[4] = t("\t \n");
    table(&mut tables, "ZKEYWORD").row(&[("Z_PK", V::Int(33)), ("ZTITLE", t("   "))]);
    table(&mut tables, "Z_1KEYWORDS").row(&[("Z_1ASSETATTRIBUTES", V::Int(11)), ("Z_54KEYWORDS", V::Int(33))]);
    let a = asset(&open_tables(&tables).unwrap(), ASSET_A);
    assert_eq!((a.caption, a.title), (None, None));
    assert_eq!(a.keywords, ["boats", "sea"]);
}

mod companions {
    use super::*;

    const ASSET_D: &str = "0A1B2C3D-0000-4000-8000-000000000004";

    fn resource(pk: i64, asset: i64, kind: i64, local: bool) -> [(&'static str, V); 6] {
        [
            ("Z_PK", V::Int(pk)),
            ("ZASSET", V::Int(asset)),
            ("ZRESOURCETYPE", V::Int(kind)),
            ("ZDATASTORESUBTYPE", V::Int(if kind == 3 { 18 } else { 17 })),
            ("ZVERSION", V::Int(0)),
            ("ZLOCALAVAILABILITY", V::Int(if local { 1 } else { -1 })),
        ]
    }

    /// The small library plus asset D (a JPEG with a local RAW), a local Live Photo video for A, a
    /// cloud-only RAW for B, and a RAW for C that the library says is local but is not on disk.
    fn tables(layout: Layout) -> Vec<Table> {
        let mut tables = fixture::photos_tables(layout);
        let asset_table = if layout == Layout::Photos5 { "ZGENERICASSET" } else { "ZASSET" };
        table(&mut tables, asset_table).row(&[
            ("Z_PK", V::Int(4)),
            ("ZUUID", t(ASSET_D)),
            ("ZFILENAME", t(&format!("{ASSET_D}.jpeg"))),
            ("ZDIRECTORY", t("0")),
            ("ZUNIFORMTYPEIDENTIFIER", t("public.jpeg")),
        ]);
        let resources = table(&mut tables, "ZINTERNALRESOURCE");
        resources.row(&resource(60, 1, 3, true)).row(&resource(61, 2, 4, false)).row(&resource(62, 3, 4, true)).row(&resource(63, 4, 4, true));
        // an edit's resource (it has a recipe) is not a companion of the original
        resources.row(&[
            ("Z_PK", V::Int(64)),
            ("ZASSET", V::Int(2)),
            ("ZRESOURCETYPE", V::Int(3)),
            ("ZVERSION", V::Int(0)),
            ("ZRECIPEID", V::Int(131_275)),
            ("ZLOCALAVAILABILITY", V::Int(1)),
        ]);
        tables
    }

    fn write(library: &Library, name: &str) -> PathBuf {
        let path = library.root.join("originals/0").join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"x").unwrap();
        path
    }

    fn check(layout: Layout) {
        let library = Library::new(&tables(layout));
        let main_a = write(&library, &format!("{ASSET_A}.heic"));
        let video = write(&library, &format!("{ASSET_A}_3.mov"));
        write(&library, &format!("{ASSET_A}_5.aae"));
        let main_d = write(&library, &format!("{ASSET_D}.jpeg"));
        let raw = write(&library, &format!("{ASSET_D}_4.raf"));
        let source = open(&library).unwrap();

        let a = asset(&source, ASSET_A);
        assert_eq!((a.live_video.clone(), a.paired_original.clone()), (Companion::Local(video), Companion::None));
        assert_eq!(a.originals(), [main_a.as_path()], "the Live Photo video and the .aae edit file are not originals");
        assert_eq!(asset(&source, ASSET_B).paired_original, Companion::CloudOnly);
        assert_eq!(asset(&source, ASSET_B).live_video, Companion::None, "an edit's video is not the Live Photo's");
        assert_eq!(asset(&source, ASSET_C).paired_original, Companion::Unknown, "said to be local, but not on disk");
        let d = asset(&source, ASSET_D);
        assert_eq!(d.paired_original, Companion::Local(raw.clone()));
        assert_eq!(d.originals(), [main_d.as_path(), raw.as_path()], "a RAW+JPEG pair stays together");
    }

    #[test]
    fn raw_jpeg_pairs_and_live_videos_are_found_next_to_the_original() {
        check(Layout::Photos6);
    }

    #[test]
    fn photos5_companions_read_the_same_way() {
        check(Layout::Photos5);
    }

    #[test]
    fn only_plain_companion_files_are_taken() {
        let library = Library::new(&tables(Layout::Photos6));
        // a folder, names that are not `<UUID>_<3|4>.<ext>`, and a file of another asset's UUID
        std::fs::create_dir_all(library.root.join(format!("originals/0/{ASSET_D}_4.raf"))).unwrap();
        for name in [
            format!("{ASSET_A}_3"),
            format!("{ASSET_A}_3."),
            format!("{ASSET_A}_3.m/v"),
            format!("{ASSET_A}_x.mov"),
            format!("{ASSET_A}_33.mov"),
            "_3.mov".into(),
        ] {
            if !name.contains('/') {
                write(&library, &name);
            }
        }
        #[cfg(unix)]
        {
            let target = write(&library, "target.mov");
            std::os::unix::fs::symlink(&target, library.root.join(format!("originals/0/{ASSET_A}_3.mov"))).unwrap();
        }
        let source = open(&library).unwrap();
        assert_eq!(asset(&source, ASSET_D).paired_original, Companion::Unknown, "a folder is not a file");
        assert_eq!(asset(&source, ASSET_A).live_video, Companion::Unknown, "a link or an odd name is not taken");
        for (name, stem) in [
            (format!("{ASSET_A}_3.mov"), Some(format!("{ASSET_A}_3"))),
            (format!("{ASSET_A}_4.raf"), Some(format!("{ASSET_A}_4"))),
            (format!("{ASSET_A}_5.aae"), None),
            (format!("{ASSET_A}.heic"), None),
            (format!("{ASSET_A}_4.tar.gz"), None),
            ("../x_4.raf".into(), None),
            ("_4.raf".into(), None),
        ] {
            assert_eq!(companion_stem(&name).map(str::to_owned), stem, "{name}");
        }
    }

    #[test]
    fn companions_are_unknown_without_the_resource_table_or_folder() {
        let mut tables = tables(Layout::Photos6);
        tables.retain(|t| t.name != "ZINTERNALRESOURCE");
        let source = open_tables(&tables).unwrap();
        let d = asset(&source, ASSET_D);
        assert_eq!((d.paired_original, d.live_video), (Companion::Unknown, Companion::Unknown));

        // the library says the RAW is here, but originals/0 does not exist
        let source = open_tables(&tables_with_resources()).unwrap();
        assert_eq!(asset(&source, ASSET_D).paired_original, Companion::Unknown);
        assert!(source.warnings().iter().any(|w| w.contains("originals/0")), "{:?}", source.warnings());
    }

    fn tables_with_resources() -> Vec<Table> {
        tables(Layout::Photos6)
    }
}
