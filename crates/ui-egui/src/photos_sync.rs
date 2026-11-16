//! Keeping photos copied from Apple Photos in step with Photos in the app (issue #1622): when such
//! a photo is open in the Edit tools, Photos is asked once a session, on a worker under a row in
//! the activity stack, whether it shows an edit LightCraft didn't make; if so that version is
//! brought in (see [`lightcraft_engine::photos_sync`]). `applePhotos.sync` asks again for its
//! photos and says how it went. A version found while a slider is being dragged waits for the
//! drag to end. Asked by itself, a photo Photos can't be asked about (another library, a build
//! without access to Photos) says nothing.

use std::collections::{HashSet, VecDeque};
use std::sync::mpsc::{Receiver, TryRecvError};

use lightcraft_catalog::{PhotoId, Source};
use lightcraft_engine::activity::TaskGuard;
use lightcraft_engine::photos_sync::{Adopted, PhotosSyncJob, SyncFound};
use serde_json::{Value, json};

use crate::LightcraftApp;

/// Checks waiting and the one being made.
#[derive(Default)]
pub struct PhotosSyncs {
    running: Option<Running>,
    /// Each with whether it was asked for (`applePhotos.sync`) rather than made by itself.
    queue: VecDeque<(PhotosSyncJob, bool)>,
    /// Photos checked this session by themselves.
    checked: HashSet<PhotoId>,
    /// A finished check waiting for a slider drag to end.
    found: Option<(PhotosSyncJob, bool, Result<SyncFound, String>)>,
}

struct Running {
    job: PhotosSyncJob,
    asked: bool,
    rx: Receiver<Result<SyncFound, String>>,
    _guard: TaskGuard,
}

impl std::fmt::Debug for PhotosSyncs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PhotosSyncs").field("running", &self.running.is_some()).field("queued", &self.queue.len()).finish_non_exhaustive()
    }
}

impl PhotosSyncs {
    /// A check is being made, waits, or waits to be applied.
    pub fn busy(&self) -> bool {
        self.running.is_some() || !self.queue.is_empty() || self.found.is_some()
    }

    fn has(&self, id: PhotoId) -> bool {
        self.running.as_ref().is_some_and(|r| r.job.id == id)
            || self.queue.iter().any(|(j, _)| j.id == id)
            || self.found.as_ref().is_some_and(|f| f.0.id == id)
    }
}

/// `applePhotos.sync` from the UI: queue checks of the target photos.
pub fn start(app: &mut LightcraftApp, p: &Value) -> Result<Value, String> {
    if cfg!(target_arch = "wasm32") {
        return Err("Apple Photos is only available on macOS".into());
    }
    let ids = app.session.targets(p).map_err(|e| e.to_string())?;
    let jobs = app.session.photos_syncs(&ids);
    if jobs.is_empty() {
        return Err("nothing to sync with Apple Photos: no photo copied from Apple Photos".into());
    }
    let mut queued = Vec::new();
    for job in jobs {
        if !app.photos_sync.has(job.id) {
            queued.push(job.id.0);
            app.photos_sync.queue.push_back((job, true));
        }
    }
    start_next(app);
    Ok(json!({"queued": queued}))
}

#[cfg(not(target_arch = "wasm32"))]
fn start_next(app: &mut LightcraftApp) {
    if app.photos_sync.running.is_some() {
        return;
    }
    let Some((job, asked)) = app.photos_sync.queue.pop_front() else { return };
    let (tx, rx) = std::sync::mpsc::channel();
    let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let label = format!("{} {}", crate::i18n::tr("Checking Apple Photos:"), job.file_name);
    let guard = app.session.activity.start("applePhotosSync", &label, lightcraft_engine::activity::Cancel::Flag(cancel.clone()));
    let handle = guard.handle();
    let work = job.clone();
    std::thread::spawn(move || {
        let mut progress = |f: f64| handle.progress((f * 1000.0).round() as u64, 1000);
        let mut photos = lightcraft_engine::photos_sync::PhotoKitAccess { cancel: &cancel, progress: &mut progress };
        let _ = tx.send(work.check(&mut photos));
    });
    app.photos_sync.running = Some(Running { job, asked, rx, _guard: guard });
}

#[cfg(target_arch = "wasm32")]
fn start_next(app: &mut LightcraftApp) {
    app.photos_sync.queue.clear();
}

/// The photo copied from Photos the Edit tools are open on, when it hasn't been checked yet.
fn to_check(app: &LightcraftApp) -> Option<PhotoId> {
    if cfg!(target_arch = "wasm32") || crate::panels::detail::prep_mode(app) != lightcraft_engine::prep::Mode::Develop {
        return None;
    }
    let id = app.session.active()?;
    let p = app.session.catalog.photo(id)?;
    (p.from_photos.is_some() && matches!(p.source, Source::File { .. }) && !app.photos_sync.checked.contains(&id) && !app.photos_sync.has(id))
        .then_some(id)
}

/// Every frame: check the photo open in the Edit tools, and apply a finished check.
pub fn poll(app: &mut LightcraftApp, ctx: &egui::Context) {
    if let Some(id) = to_check(app) {
        app.photos_sync.checked.insert(id);
        app.photos_sync.queue.extend(app.session.photos_syncs(&[id]).into_iter().map(|j| (j, false)));
        start_next(app);
    }
    if app.photos_sync.found.is_none()
        && let Some(running) = app.photos_sync.running.as_ref()
    {
        let end = match running.rx.try_recv() {
            Ok(end) => end,
            Err(TryRecvError::Empty) => {
                ctx.request_repaint_after(std::time::Duration::from_millis(100));
                return;
            }
            Err(TryRecvError::Disconnected) => Err(crate::i18n::tr("Checking Apple Photos stopped").to_string()),
        };
        if let Some(Running { job, asked, .. }) = app.photos_sync.running.take() {
            app.photos_sync.found = Some((job, asked, end));
        }
    }
    // a new version changes the photo's settings: never in the middle of a drag
    if app.session.interaction.is_some() {
        if app.photos_sync.found.is_some() {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }
        return;
    }
    if let Some((job, asked, end)) = app.photos_sync.found.take() {
        if let Some(text) = apply(app, &job, asked, end) {
            app.toast(ctx, text);
        }
        start_next(app);
        ctx.request_repaint();
    }
}

/// Apply a finished check; what to say about it, if anything.
fn apply(app: &mut LightcraftApp, job: &PhotosSyncJob, asked: bool, end: Result<SyncFound, String>) -> Option<String> {
    let made = match &end {
        Ok(SyncFound::NewVersion { path, .. }) => Some(path.clone()),
        _ => None,
    };
    let adopted = end.and_then(|found| {
        app.session.adopt_photos_version(job, found).map_err(|e| {
            if let Some(p) = &made {
                let _ = std::fs::remove_file(p);
            }
            e.to_string()
        })
    });
    let tr = crate::i18n::tr;
    match adopted {
        Ok(Adopted::NewVersion { snapshot: Some(name), .. }) => Some(format!(
            "{} {} ({} “{name}”)",
            tr("Brought in the edit from Apple Photos:"),
            job.file_name,
            tr("your changes since the last save are in the snapshot")
        )),
        Ok(Adopted::NewVersion { .. }) => Some(format!("{} {}", tr("Brought in the edit from Apple Photos:"), job.file_name)),
        Ok(Adopted::InStep) => asked.then(|| format!("{} {}", tr("Already in step with Apple Photos:"), job.file_name)),
        Err(e) => {
            log::info!("checking Apple Photos: {e}");
            asked.then_some(e)
        }
    }
}
