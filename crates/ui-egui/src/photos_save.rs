//! Saving edits back to Apple Photos in the app (issue #1622): `applePhotos.saveEdit` renders
//! each edited photo copied from Photos and hands it to Photos on a worker, one at a time, under
//! a row in the activity stack that shows an iCloud download Photos needs first and can stop it
//! (see [`lightcraft_engine::photos_save`]). PhotoKit answers on the main thread, so the save
//! never runs there.

use std::collections::VecDeque;
use std::sync::mpsc::{Receiver, TryRecvError};

use lightcraft_engine::activity::TaskGuard;
use lightcraft_engine::photos_save::PhotosSave;
use serde_json::{Value, json};

use crate::LightcraftApp;

/// Saves waiting and the one being made.
#[derive(Default)]
pub struct PhotosSaves {
    running: Option<Running>,
    queue: VecDeque<PhotosSave>,
}

struct Running {
    file_name: String,
    rx: Receiver<Result<(), String>>,
    _guard: TaskGuard,
}

impl std::fmt::Debug for PhotosSaves {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PhotosSaves").field("running", &self.running.is_some()).field("queued", &self.queue.len()).finish_non_exhaustive()
    }
}

impl PhotosSaves {
    /// A save is being made or waits.
    pub fn busy(&self) -> bool {
        self.running.is_some() || !self.queue.is_empty()
    }
}

/// `applePhotos.saveEdit` from the UI: set up the renders of the target photos and queue them.
pub fn start(app: &mut LightcraftApp, p: &Value) -> Result<Value, String> {
    let ids = app.session.targets(p).map_err(|e| e.to_string())?;
    let (saves, skipped) = app.session.photos_saves(&ids).map_err(|e| e.to_string())?;
    if saves.is_empty() {
        let why = skipped.first().map_or("no photo selected", |k| k.why);
        return Err(format!("nothing to save to Apple Photos: {why}"));
    }
    let queued: Vec<u64> = saves.iter().map(|s| s.id.0).collect();
    app.photos_save.queue.extend(saves);
    start_next(app);
    Ok(json!({"queued": queued, "skipped": skipped.iter().map(|k| json!({"id": k.id.0, "why": k.why})).collect::<Vec<_>>()}))
}

#[cfg(not(target_arch = "wasm32"))]
fn start_next(app: &mut LightcraftApp) {
    if app.photos_save.running.is_some() {
        return;
    }
    let Some(save) = app.photos_save.queue.pop_front() else { return };
    let file_name = save.file_name.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let label = format!("{} {file_name}", crate::i18n::tr("Saving to Apple Photos:"));
    let guard = app.session.activity.start("applePhotosSave", &label, lightcraft_engine::activity::Cancel::Flag(cancel.clone()));
    let handle = guard.handle();
    std::thread::spawn(move || {
        let mut progress = |f: f64| handle.progress((f * 1000.0).round() as u64, 1000);
        let mut to_photos = lightcraft_engine::photos_save::photokit_save(&cancel, &mut progress);
        let _ = tx.send(save.run(&mut to_photos));
    });
    app.photos_save.running = Some(Running { file_name, rx, _guard: guard });
}

#[cfg(target_arch = "wasm32")]
fn start_next(app: &mut LightcraftApp) {
    app.photos_save.queue.clear();
}

/// Every frame: take a finished save, say how it went, and start the next.
pub fn poll(app: &mut LightcraftApp, ctx: &egui::Context) {
    let Some(running) = app.photos_save.running.as_ref() else { return };
    let end = match running.rx.try_recv() {
        Ok(end) => end,
        Err(TryRecvError::Empty) => {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
            return;
        }
        Err(TryRecvError::Disconnected) => Err(crate::i18n::tr("Saving to Apple Photos stopped").to_string()),
    };
    let Some(Running { file_name, .. }) = app.photos_save.running.take() else { return };
    match end {
        Ok(()) => app.toast(ctx, format!("{} {file_name}", crate::i18n::tr("Saved to Apple Photos:"))),
        Err(e) => app.toast(ctx, e),
    }
    start_next(app);
    ctx.request_repaint();
}
