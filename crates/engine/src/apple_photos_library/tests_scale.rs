//! What reading a Photos library costs at 200k assets (ignored:
//! `cargo test --release -p lightcraft-engine --lib photos_scale_200k -- --ignored --nocapture`).
//!
//! The synthetic library is shaped like a real one, scaled up: about seven `ZINTERNALRESOURCE`
//! rows per asset (1.45 million rows) and attribute rows of about 1.4 KB (280 MB). Both are past
//! what `Database::read_table` keeps in memory (a million rows, 256 MB), so only streamed reads
//! open it. Plus keywords, captions and 200 albums of 1,000 assets.

use std::time::Instant;

use super::fixture::{self, JUNE_2021, Layout, Library, Table, V, t};
use super::*;

const ASSETS: i64 = 200_000;
const ALBUMS: i64 = 200;

fn uuid(pk: i64) -> String {
    format!("C0000000-0000-4000-8000-{pk:012}")
}

fn table<'a>(tables: &'a mut [Table], name: &str) -> &'a mut Table {
    tables.iter_mut().find(|t| t.name == name).unwrap()
}

fn tables() -> Vec<Table> {
    let mut tables = fixture::photos_tables(Layout::Photos6);
    let first = 1_000;
    let pks = first..first + ASSETS;
    let assets = table(&mut tables, "ZASSET");
    for pk in pks.clone() {
        let uuid = uuid(pk);
        assets.row(&[
            ("Z_PK", V::Int(pk)),
            ("ZUUID", t(&uuid)),
            ("ZFILENAME", t(&format!("{uuid}.heic"))),
            ("ZDIRECTORY", t(&format!("{:X}", pk % 16))),
            ("ZDATECREATED", V::Real(JUNE_2021 + pk as f64 * 60.0)),
            ("ZKIND", V::Int(0)),
            ("ZKINDSUBTYPE", V::Int(if pk % 4 == 0 { 2 } else { 0 })),
            ("ZUNIFORMTYPEIDENTIFIER", t("public.heic")),
            ("ZWIDTH", V::Int(4032)),
            ("ZHEIGHT", V::Int(3024)),
            ("ZLATITUDE", V::Real(48.0 + (pk % 100) as f64 / 100.0)),
            ("ZLONGITUDE", V::Real(-3.0)),
            ("ZADDITIONALATTRIBUTES", V::Int(10_000_000 + pk)),
        ]);
    }
    let attributes = table(&mut tables, "ZADDITIONALASSETATTRIBUTES");
    // stands in for the large values Photos keeps there (reverse-geocoding data and the like)
    attributes.columns.push("ZREVERSELOCATIONDATA".into());
    let filler = "x".repeat(1_400);
    for pk in pks.clone() {
        let mut row = vec![
            ("Z_PK", V::Int(10_000_000 + pk)),
            ("ZASSET", V::Int(pk)),
            ("ZORIGINALFILENAME", t(&format!("IMG_{pk:06}.HEIC"))),
            ("ZORIGINALFILESIZE", V::Int(2_000_000 + pk)),
            ("ZTIMEZONEOFFSET", V::Int(3600)),
            ("ZREVERSELOCATIONDATA", t(&filler)),
        ];
        if pk % 20 == 0 {
            row.push(("ZASSETDESCRIPTION", V::Int(20_000_000 + pk)));
        }
        attributes.row(&row);
    }
    let descriptions = table(&mut tables, "ZASSETDESCRIPTION");
    for pk in pks.clone().filter(|pk| pk % 20 == 0) {
        descriptions.row(&[("Z_PK", V::Int(20_000_000 + pk)), ("ZLONGDESCRIPTION", t(&format!("Caption {pk}")))]);
    }
    let keywords = table(&mut tables, "ZKEYWORD");
    for k in 0..50 {
        keywords.row(&[("Z_PK", V::Int(100 + k)), ("ZTITLE", t(&format!("keyword {k}")))]);
    }
    let links = table(&mut tables, "Z_1KEYWORDS");
    for pk in pks.clone().filter(|pk| pk % 5 == 0) {
        links.row(&[("Z_1ASSETATTRIBUTES", V::Int(10_000_000 + pk)), ("Z_54KEYWORDS", V::Int(100 + pk % 50))]);
        links.row(&[("Z_1ASSETATTRIBUTES", V::Int(10_000_000 + pk)), ("Z_54KEYWORDS", V::Int(100 + (pk + 7) % 50))]);
    }
    let albums = table(&mut tables, "ZGENERICALBUM");
    for a in 0..ALBUMS {
        albums.row(&[
            ("Z_PK", V::Int(1_000 + a)),
            ("ZUUID", t(&format!("E0000000-0000-4000-8000-{a:012}"))),
            ("ZTITLE", t(&format!("Album {a}"))),
            ("ZKIND", V::Int(2)),
        ]);
    }
    let members = table(&mut tables, "Z_35ASSETS");
    for a in 0..ALBUMS {
        for i in 0..1_000 {
            let pk = first + (a * 997 + i * 131) % ASSETS;
            members.row(&[("Z_35ALBUMS", V::Int(1_000 + a)), ("Z_3ASSETS", V::Int(pk)), ("Z_FOK_3ASSETS", V::Int(i))]);
        }
    }
    let resources = table(&mut tables, "ZINTERNALRESOURCE");
    let mut next = 100_000_000i64;
    for pk in pks {
        let mut add = |kind: i64, subtype: i64, local: bool| {
            next += 1;
            resources.row(&[
                ("Z_PK", V::Int(next)),
                ("ZASSET", V::Int(pk)),
                ("ZRESOURCETYPE", V::Int(kind)),
                ("ZDATASTORESUBTYPE", V::Int(subtype)),
                ("ZVERSION", V::Int(0)),
                ("ZLOCALAVAILABILITY", V::Int(i64::from(local))),
            ]);
        };
        add(0, 1, pk % 20 != 0);
        for subtype in 2..8 {
            add(0, subtype, true);
        }
        if pk % 4 == 0 {
            // cloud-only, so no folder of companion files is needed
            add(3, 18, false);
        }
    }
    tables
}

/// This process's resident memory in MB, from `ps`.
fn resident_mb() -> Option<u64> {
    let out = std::process::Command::new("ps").args(["-o", "rss=", "-p", &std::process::id().to_string()]).output().ok()?;
    String::from_utf8(out.stdout).ok()?.trim().parse::<u64>().ok().map(|kb| kb / 1024)
}

fn ms(since: Instant) -> f64 {
    since.elapsed().as_secs_f64() * 1e3
}

#[test]
#[ignore]
fn photos_scale_200k() {
    let started = Instant::now();
    let library = Library::new(&tables());
    let size = std::fs::metadata(library.database()).unwrap().len() >> 20;
    eprintln!("build: {:.0} ms, Photos.sqlite {size} MB", ms(started));

    let before = resident_mb();
    let started = Instant::now();
    let source = SqliteSource::open(&PhotosLibrary::at(&library.root).unwrap(), &AtomicBool::new(false)).unwrap();
    let after = resident_mb();
    eprintln!("open: {:.0} ms, resident memory {before:?} -> {after:?} MB", ms(started));
    assert_eq!(source.asset_count(), ASSETS as usize + 3);
    assert!(source.warnings().is_empty(), "{:?}", source.warnings());

    let started = Instant::now();
    let (mut seen, mut local, mut cloud, mut captions, mut keyworded, mut live_cloud) = (0, 0, 0, 0, 0, 0);
    for start in (0..source.asset_count()).step_by(500) {
        for a in source.assets(start, 500).unwrap() {
            seen += 1;
            local += usize::from(a.availability == Availability::Local);
            cloud += usize::from(a.availability == Availability::CloudOnly);
            captions += usize::from(a.caption.is_some());
            keyworded += usize::from(!a.keywords.is_empty());
            live_cloud += usize::from(a.live_video == Companion::CloudOnly);
        }
    }
    eprintln!("every asset, in pages of 500: {:.0} ms, resident memory {:?} MB", ms(started), resident_mb());
    let n = ASSETS as usize;
    assert_eq!(seen, n + 3);
    // the three small-library assets: A is local, B cloud-only, C unknown
    assert_eq!((local, cloud), (n - n / 20 + 1, n / 20 + 1));
    assert_eq!((captions, keyworded, live_cloud), (n / 20 + 1, n / 5 + 1, n / 4));

    let started = Instant::now();
    for i in 0..10_000 {
        let pk = 1_000 + (i * 7_919) % ASSETS;
        let a = source.asset(&uuid(pk)).unwrap().unwrap();
        assert_eq!(a.original_size, Some(2_000_000 + pk as u64));
    }
    eprintln!("10,000 lookups by UUID: {:.0} ms", ms(started));

    let started = Instant::now();
    let albums = source.albums().unwrap();
    let members: usize = albums.iter().map(|a| source.album_assets(&a.uuid).unwrap().map_or(0, |m| m.len())).sum();
    eprintln!("{} albums, {members} members: {:.0} ms", albums.len(), ms(started));
    assert_eq!(members, ALBUMS as usize * 1_000 + 3);
}
