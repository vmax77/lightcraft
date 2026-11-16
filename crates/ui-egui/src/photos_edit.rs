//! Editing an Apple Photos item in the app (issue #1622): when a Photos item open in the Edit
//! tools is first edited, its original is copied into the library on a worker (downloaded from
//! iCloud first when it isn't on this Mac, under a row that shows progress and can stop it),
//! under a row in the activity stack, and the photo becomes that copy (see
//! [`lightcraft_engine::photos_edit`]). Only looking at it in the Edit tools copies nothing: the
//! proxy shows the original, or Photos' preview while that is only in iCloud. Edits made
//! meanwhile apply to the same photo and stay with it. `applePhotos.edit` from a menu or the
//! control channel queues its photos the same way, edited or not.

use std::collections::{HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, TryRecvError};

use lightcraft_catalog::{PhotoId, Source};
use lightcraft_engine::activity::TaskGuard;
use lightcraft_engine::photos_edit::PhotosCopy;
use serde_json::{Value, json};

use crate::LightcraftApp;

/// Copies waiting and the one being made.
#[derive(Default)]
pub struct PhotosEdit {
    running: Option<Running>,
    queue: VecDeque<PhotosCopy>,
    /// Photos whose copy failed, or whose download was stopped: the Edit tools don't try again
    /// for them until asked (`applePhotos.edit`), so a failure is told once, not every frame.
    failed: HashSet<PhotoId>,
}

struct Running {
    copy: PhotosCopy,
    rx: Receiver<Result<PathBuf, String>>,
    _guard: TaskGuard,
}

impl std::fmt::Debug for PhotosEdit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PhotosEdit").field("running", &self.running.is_some()).field("queued", &self.queue.len()).finish_non_exhaustive()
    }
}

impl PhotosEdit {
    /// A copy is being made or waits.
    pub fn busy(&self) -> bool {
        self.running.is_some() || !self.queue.is_empty()
    }

    fn has(&self, id: PhotoId) -> bool {
        self.running.as_ref().is_some_and(|r| r.copy.id == id) || self.queue.iter().any(|c| c.id == id)
    }

    /// What the loupe says in place of "This photo can't be opened" while `id`'s copy is made or
    /// waits: a proxy whose original is only in iCloud (or not readable yet) fails to render until
    /// its copy is there, and then renders by itself. `None` when no copy of it is under way.
    pub fn waiting_text(&self, id: PhotoId) -> Option<&'static str> {
        let copy = self.running.as_ref().map(|r| &r.copy).filter(|c| c.id == id).or_else(|| self.queue.iter().find(|c| c.id == id))?;
        Some(if copy.in_icloud() { "Downloading from iCloud…" } else { "Copying from Apple Photos…" })
    }
}

/// `applePhotos.edit` from the UI: queue the copies of the target photos (made one at a time on a
/// worker). Photos that aren't Photos items are left as they are.
pub fn start(app: &mut LightcraftApp, p: &Value) -> Result<Value, String> {
    let ids = app.session.targets(p).map_err(|e| e.to_string())?;
    let copies = app.session.photos_copies(&ids).map_err(|e| e.to_string())?;
    let mut queued = Vec::new();
    for copy in copies {
        app.photos_edit.failed.remove(&copy.id);
        if !app.photos_edit.has(copy.id) {
            queued.push(copy.id.0);
            app.photos_edit.queue.push_back(copy);
        }
    }
    start_next(app);
    Ok(json!({"queued": queued}))
}

#[cfg(not(target_arch = "wasm32"))]
fn start_next(app: &mut LightcraftApp) {
    if app.photos_edit.running.is_some() {
        return;
    }
    let Some(copy) = app.photos_edit.queue.pop_front() else { return };
    let (tx, rx) = std::sync::mpsc::channel();
    let work = copy.clone();
    let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (label, how) = if copy.in_icloud() {
        // a download can take a while and be stopped; its row shows how far it got
        ("Downloading from iCloud:", lightcraft_engine::activity::Cancel::Flag(cancel.clone()))
    } else {
        ("Copying from Apple Photos:", lightcraft_engine::activity::Cancel::No)
    };
    let label = format!("{} {}", crate::i18n::tr(label), copy.file_name);
    let guard = app.session.activity.start("applePhotosCopy", &label, how);
    let handle = guard.handle();
    std::thread::spawn(move || {
        let mut progress = |f: f64| handle.progress((f * 1000.0).round() as u64, 1000);
        let mut fetch = lightcraft_engine::photos_edit::photokit_fetch(&cancel, &mut progress);
        let _ = tx.send(work.make(&mut fetch));
    });
    app.photos_edit.running = Some(Running { copy, rx, _guard: guard });
}

#[cfg(target_arch = "wasm32")]
fn start_next(app: &mut LightcraftApp) {
    app.photos_edit.queue.clear();
}

/// The Photos item the Edit tools are open on, when it has been edited and its original still
/// has to be copied. Viewing it unedited needs no copy.
fn editing_proxy(app: &LightcraftApp) -> Option<PhotoId> {
    if crate::panels::detail::prep_mode(app) != lightcraft_engine::prep::Mode::Develop {
        return None;
    }
    let id = app.session.active()?;
    let p = app.session.catalog.photo(id)?;
    (matches!(p.source, Source::ApplePhotos { .. }) && p.is_edited()).then_some(id)
}

/// Every frame: start the copy of a Photos item edited in the Edit tools, and take a finished copy.
pub fn poll(app: &mut LightcraftApp, ctx: &egui::Context) {
    if let Some(id) = editing_proxy(app)
        && !app.photos_edit.has(id)
        && !app.photos_edit.failed.contains(&id)
    {
        match app.session.photos_copies(&[id]) {
            Ok(copies) => app.photos_edit.queue.extend(copies),
            Err(e) => {
                app.photos_edit.failed.insert(id);
                app.toast(ctx, e.to_string());
            }
        }
        start_next(app);
    }
    let Some(running) = app.photos_edit.running.as_ref() else { return };
    let end = match running.rx.try_recv() {
        Ok(end) => end,
        Err(TryRecvError::Empty) => {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
            return;
        }
        Err(TryRecvError::Disconnected) => Err(crate::i18n::tr("Copying from Apple Photos stopped").to_string()),
    };
    let Some(Running { copy, .. }) = app.photos_edit.running.take() else { return };
    let adopted = end.and_then(|path| {
        app.session.adopt_photos_copy(&copy, &path).map_err(|e| {
            // the photo changed meanwhile: the copy made for it isn't used
            let _ = std::fs::remove_file(&path);
            e.to_string()
        })
    });
    if let Err(e) = adopted {
        app.photos_edit.failed.insert(copy.id);
        app.toast(ctx, e);
    }
    start_next(app);
    ctx.request_repaint();
}

#[cfg(test)]
mod tests {
    use super::*;
    use lightcraft_catalog::PhotosLink;

    fn copy(id: u64, original: Option<&str>) -> PhotosCopy {
        PhotosCopy {
            id: PhotoId(id),
            link: PhotosLink { library: "Test.photoslibrary".into(), uuid: format!("U{id}"), ..Default::default() },
            original: original.map(PathBuf::from),
            size: 10,
            file_name: format!("U{id}.JPG"),
            dir: PathBuf::from("lib"),
        }
    }

    /// The loupe of a Photos item whose copy waits says so, not "This photo can't be opened": an
    /// iCloud-only original can't render until it has been downloaded.
    #[test]
    fn a_photo_whose_copy_waits_says_so_instead_of_failing() {
        let mut edit = PhotosEdit::default();
        assert_eq!(edit.waiting_text(PhotoId(1)), None);
        edit.queue.push_back(copy(1, None));
        edit.queue.push_back(copy(2, Some("Test.photoslibrary/originals/0/U2.jpg")));
        assert_eq!(edit.waiting_text(PhotoId(1)), Some("Downloading from iCloud…"));
        assert_eq!(edit.waiting_text(PhotoId(2)), Some("Copying from Apple Photos…"));
        assert_eq!(edit.waiting_text(PhotoId(3)), None);
    }
}
