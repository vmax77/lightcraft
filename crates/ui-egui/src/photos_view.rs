//! Apple Photos in the app (issue #1622): `applePhotos.show` from the UI reads the library on a
//! worker thread, under a row in the activity stack that can cancel it, and the view switches
//! to its photos when it has been read. Reading never writes anything in the library.
//!
//! While the view is shown, a watcher thread looks at when the library's database last changed
//! ([`PhotosWatch`]); when Photos changes it (an edit, a revert, a new photo), the view is read
//! again, so what LightCraft shows follows Photos. A change seen while another source is shown
//! is read when the view is shown again.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, TryRecvError};

use lightcraft_engine::activity::TaskGuard;
use lightcraft_engine::photos_browse::PhotosRead;
use serde_json::{Value, json};

use crate::LightcraftApp;

/// A Photos library being read for the sidebar.
pub struct PhotosTask {
    rx: Receiver<Result<PhotosRead, String>>,
    cancel: Arc<AtomicBool>,
    guard: TaskGuard,
}

impl std::fmt::Debug for PhotosTask {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PhotosTask").finish_non_exhaustive()
    }
}

/// How often the watcher looks at the shown library's database.
pub const WATCH_EVERY: std::time::Duration = std::time::Duration::from_secs(2);

/// Watches the shown Photos library's database (`database/Photos.sqlite` and its `-wal`) for
/// changes made in Photos; dropping it stops its thread.
pub struct PhotosWatch {
    library: std::path::PathBuf,
    rx: Receiver<()>,
    stop: Arc<AtomicBool>,
    /// A change not read yet (seen while another source was shown).
    pending: bool,
}

impl std::fmt::Debug for PhotosWatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PhotosWatch").field("library", &self.library).finish_non_exhaustive()
    }
}

impl Drop for PhotosWatch {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// When the library's database last changed: its main file's and its write-ahead log's times.
#[cfg(not(target_arch = "wasm32"))]
fn changed_at(library: &std::path::Path) -> [Option<std::time::SystemTime>; 2] {
    let at = |name: &str| std::fs::metadata(library.join("database").join(name)).and_then(|m| m.modified()).ok();
    [at("Photos.sqlite"), at("Photos.sqlite-wal")]
}

/// Watch `library` on a thread of its own, looking every `every`.
#[cfg(not(target_arch = "wasm32"))]
fn watch(library: std::path::PathBuf, every: std::time::Duration) -> PhotosWatch {
    let (tx, rx) = std::sync::mpsc::channel();
    let stop = Arc::new(AtomicBool::new(false));
    let (flag, at) = (stop.clone(), library.clone());
    std::thread::spawn(move || {
        let mut last = changed_at(&at);
        while !flag.load(Ordering::Relaxed) {
            std::thread::sleep(every);
            let now = changed_at(&at);
            if now != last {
                last = now;
                if tx.send(()).is_err() {
                    return;
                }
            }
        }
    });
    PhotosWatch { library, rx, stop, pending: false }
}

/// Start reading the view `p` names (`applePhotos.show` params). A read already running is
/// cancelled: the last view asked for wins.
#[cfg(not(target_arch = "wasm32"))]
pub fn start(app: &mut LightcraftApp, p: &Value) -> Result<Value, String> {
    let view = lightcraft_engine::photos_browse::PhotosView::from_params(p)?;
    let library = lightcraft_engine::photos_browse::library_param(&app.session, p)?;
    read(app, library, view)
}

/// Read `view` of `library` on a worker (see [`start`]).
#[cfg(not(target_arch = "wasm32"))]
fn read(app: &mut LightcraftApp, library: std::path::PathBuf, view: lightcraft_engine::photos_browse::PhotosView) -> Result<Value, String> {
    stop(app);
    let cancel = Arc::new(AtomicBool::new(false));
    let (tx, rx) = std::sync::mpsc::channel();
    let (flag, at) = (cancel.clone(), library.clone());
    let worker_view = view.clone();
    std::thread::spawn(move || {
        let read = lightcraft_engine::photos_browse::read_view(&at, &worker_view, &flag).map_err(|e| e.to_string());
        let _ = tx.send(read);
    });
    let label = crate::i18n::tr("Reading Apple Photos");
    let guard = app.session.activity.start("applePhotos", label, lightcraft_engine::activity::Cancel::Flag(cancel.clone()));
    app.photos_task = Some(PhotosTask { rx, cancel, guard });
    Ok(json!({"reading": true, "library": library.to_string_lossy(), "view": view}))
}

/// Cancel a read in progress; its worker stops at its next page and its result is dropped.
pub fn stop(app: &mut LightcraftApp) {
    if let Some(task) = app.photos_task.take() {
        task.cancel.store(true, Ordering::Relaxed);
    }
}

/// Take a finished read and show it (called every frame).
pub fn poll(app: &mut LightcraftApp, ctx: &egui::Context) {
    let Some(task) = app.photos_task.as_ref() else { return };
    let end = match task.rx.try_recv() {
        Ok(end) => end,
        Err(TryRecvError::Empty) => {
            if task.guard.is_cancelled() {
                stop(app);
            } else {
                ctx.request_repaint_after(std::time::Duration::from_millis(100));
            }
            return;
        }
        Err(TryRecvError::Disconnected) => Err(crate::i18n::tr("Reading Apple Photos stopped").to_string()),
    };
    let Some(task) = app.photos_task.take() else { return };
    if task.cancel.load(Ordering::Relaxed) || task.guard.is_cancelled() {
        return;
    }
    let shown = end.and_then(|read| app.session.show_photos(read).map_err(|e| e.to_string()));
    if let Err(e) = shown {
        app.toast(ctx, e);
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        let library = app.session.photos_browse.as_ref().map(|b| std::path::PathBuf::from(&b.library));
        if let Some(library) = library
            && app.photos_watch.as_ref().is_none_or(|w| w.library != library)
        {
            app.photos_watch = Some(watch(library, app.photos_watch_every));
        }
    }
}

/// Every frame: read the shown view again when Photos changed the library since it was read.
#[cfg(not(target_arch = "wasm32"))]
pub fn poll_watch(app: &mut LightcraftApp, ctx: &egui::Context) {
    let Some(w) = app.photos_watch.as_mut() else { return };
    while w.rx.try_recv().is_ok() {
        w.pending = true;
    }
    let shown = app.session.source == lightcraft_engine::LibrarySource::ApplePhotos;
    if !shown {
        return;
    }
    // looked at again while the view is shown, so a change shows without other input
    ctx.request_repaint_after(app.photos_watch_every);
    let view = app.session.photos_browse.as_ref().filter(|b| std::path::Path::new(&b.library) == w.library).map(|b| b.view.clone());
    if w.pending
        && app.photos_task.is_none()
        && let Some(view) = view
    {
        w.pending = false;
        let library = w.library.clone();
        if let Err(e) = read(app, library, view) {
            log::info!("reading Apple Photos again: {e}");
        }
    }
}

#[cfg(target_arch = "wasm32")]
pub fn poll_watch(_: &mut LightcraftApp, _: &egui::Context) {}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;
    use lightcraft_engine::photos_browse::{PhotosItem, PhotosView};

    #[allow(clippy::unwrap_used)]
    fn library(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("lc-photos-watch-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("database")).unwrap();
        std::fs::write(dir.join("database").join("Photos.sqlite"), b"v1").unwrap();
        dir
    }

    /// A change to the database (its main file or its write-ahead log) is seen; none is not.
    #[test]
    fn the_watcher_sees_photos_change_its_database() {
        let dir = library("sees");
        let w = watch(dir.clone(), std::time::Duration::from_millis(20));
        std::thread::sleep(std::time::Duration::from_millis(100));
        assert!(w.rx.try_recv().is_err(), "nothing changed");
        // a later modification time (set, so a coarse file system clock can't hide it)
        let wal = std::fs::File::create(dir.join("database").join("Photos.sqlite-wal")).unwrap();
        wal.set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(5)).unwrap();
        drop(wal);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while w.rx.try_recv().is_err() {
            assert!(std::time::Instant::now() < deadline, "the change was never seen");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        drop(w);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A change seen while the view is shown reads the view again; one seen while another source
    /// is shown waits until the view is shown.
    #[test]
    fn a_change_reads_the_shown_view_again() {
        let dir = library("reads");
        let services = crate::Services { png: None, local_places: Some(Vec::new()), ..Default::default() };
        let mut app = LightcraftApp::new(lightcraft_engine::Session::new(), services);
        let ctx = egui::Context::default();
        let item = PhotosItem { uuid: "U1".into(), file_name: "U1.JPG".into(), format: "JPG".into(), width: 4, height: 3, ..Default::default() };
        let read =
            PhotosRead { library: dir.to_string_lossy().into(), view: PhotosView::All, items: vec![item], albums: Vec::new(), warnings: Vec::new() };
        app.session.show_photos(read).unwrap();
        let mut w = watch(dir.clone(), std::time::Duration::from_secs(3600));
        w.pending = true;
        app.photos_watch = Some(w);
        // another source shown: it waits
        app.session.execute("library.source", &json!({"kind": "all"})).unwrap();
        poll_watch(&mut app, &ctx);
        assert!(app.photos_task.is_none());
        assert!(app.photos_watch.as_ref().is_some_and(|w| w.pending));
        // the view shown again: read again
        app.session.source = lightcraft_engine::LibrarySource::ApplePhotos;
        poll_watch(&mut app, &ctx);
        assert!(app.photos_task.is_some(), "the view is read again");
        assert!(app.photos_watch.as_ref().is_some_and(|w| !w.pending));
        stop(&mut app);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
