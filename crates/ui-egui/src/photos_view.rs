//! Apple Photos in the app (issue #1622): `applePhotos.show` from the UI reads the library on a
//! worker thread, under a row in the activity stack that can cancel it, and the view switches
//! to its photos when it has been read. Reading never writes anything in the library.

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

/// Start reading the view `p` names (`applePhotos.show` params). A read already running is
/// cancelled: the last view asked for wins.
#[cfg(not(target_arch = "wasm32"))]
pub fn start(app: &mut LightcraftApp, p: &Value) -> Result<Value, String> {
    let view = lightcraft_engine::photos_browse::PhotosView::from_params(p)?;
    let library = lightcraft_engine::photos_browse::library_param(&app.session, p)?;
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
}
