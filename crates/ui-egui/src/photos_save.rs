//! Saving edits back to Apple Photos in the app (issue #1622): `applePhotos.saveEdit` renders
//! each edited photo copied from Photos and hands it to Photos on a worker, one at a time, under
//! a row in the activity stack that shows an iCloud download Photos needs first and can stop it
//! (see [`lightcraft_engine::photos_save`]). PhotoKit answers on the main thread, so the save
//! never runs there. An edited Photos item not copied yet (its copy is made in the background
//! after its first edit) is copied first and saved once its copy is there: Save never has to
//! wait to be offered.

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
    /// Edited Photos items to save once their copy is made.
    after_copy: Vec<lightcraft_catalog::PhotoId>,
}

/// A batch of saves handed to Photos as one change (macOS asks once).
struct Running {
    /// The batch's photos, for a worker that ends without an answer.
    photos: Vec<(lightcraft_catalog::PhotoId, String)>,
    /// Each photo's outcome: the asset's modification time in Photos after the save.
    rx: Receiver<Vec<lightcraft_engine::photos_save::Saved>>,
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
        self.running.is_some() || !self.queue.is_empty() || !self.after_copy.is_empty()
    }
}

/// `applePhotos.saveEdit` from the UI: set up the renders of the target photos and queue them.
pub fn start(app: &mut LightcraftApp, p: &Value) -> Result<Value, String> {
    let ids = app.session.targets(p).map_err(|e| e.to_string())?;
    // edited Photos items not copied yet: copied first, saved after
    let uncopied: Vec<lightcraft_catalog::PhotoId> = ids
        .iter()
        .copied()
        .filter(|id| {
            app.session.catalog.photo(*id).is_some_and(|p| matches!(p.source, lightcraft_catalog::Source::ApplePhotos { .. }) && p.is_edited())
        })
        .collect();
    let rest: Vec<lightcraft_catalog::PhotoId> = ids.iter().copied().filter(|id| !uncopied.contains(id)).collect();
    let (saves, skipped) = app.session.photos_saves(&rest).map_err(|e| e.to_string())?;
    if saves.is_empty() && uncopied.is_empty() {
        let why = skipped.first().map_or("no photo selected", |k| k.why);
        return Err(format!("nothing to save to Apple Photos: {why}"));
    }
    let queued: Vec<u64> = saves.iter().map(|s| s.id.0).collect();
    if uncopied.is_empty() {
        app.photos_save.queue.extend(saves);
    } else {
        // all wait for the copies, so they go to Photos as one batch (macOS asks once)
        crate::photos_edit::start(app, &json!({"ids": uncopied.iter().map(|id| id.0).collect::<Vec<_>>()}))?;
        for id in uncopied.iter().copied().chain(saves.iter().map(|s| s.id)) {
            if !app.photos_save.after_copy.contains(&id) {
                app.photos_save.after_copy.push(id);
            }
        }
    }
    start_next(app);
    Ok(json!({
        "queued": queued,
        "afterCopy": uncopied.iter().map(|id| id.0).collect::<Vec<_>>(),
        "skipped": skipped.iter().map(|k| json!({"id": k.id.0, "why": k.why})).collect::<Vec<_>>(),
    }))
}

/// Queue the saves of photos whose copy is now made; say why for those whose copy failed.
fn after_copies(app: &mut LightcraftApp, ctx: &egui::Context) {
    if app.photos_save.after_copy.is_empty() {
        return;
    }
    // saved together once every copy is made, so macOS asks once
    if app.photos_save.after_copy.iter().any(|id| app.photos_edit.copying(*id)) {
        ctx.request_repaint_after(std::time::Duration::from_millis(100));
        return;
    }
    let waiting = std::mem::take(&mut app.photos_save.after_copy);
    let mut ready = Vec::new();
    for id in waiting {
        let copied = app.session.catalog.photo(id).is_some_and(|p| p.from_photos.is_some());
        if copied && !app.photos_edit.copying(id) {
            ready.push(id);
        } else if app.photos_edit.copying(id) {
            app.photos_save.after_copy.push(id);
        } else if app.photos_edit.failed(id) || app.session.catalog.photo(id).is_none() {
            // the copy's own message says why
            log::info!("not saved to Apple Photos: photo {} wasn't copied", id.0);
        } else {
            // not queued for a copy any more (a cancelled read, a photo removed): nothing to save
            log::info!("not saved to Apple Photos: photo {} is no longer waiting for its copy", id.0);
        }
    }
    if ready.is_empty() {
        ctx.request_repaint_after(std::time::Duration::from_millis(100));
        return;
    }
    match app.session.photos_saves(&ready) {
        Ok((saves, _)) => app.photos_save.queue.extend(saves),
        Err(e) => app.toast(ctx, e.to_string()),
    }
    start_next(app);
}

#[cfg(not(target_arch = "wasm32"))]
fn start_next(app: &mut LightcraftApp) {
    if app.photos_save.running.is_some() {
        return;
    }
    // everything waiting goes as one batch: one change in Photos, one question from macOS
    let batch: Vec<lightcraft_engine::photos_save::PhotosSave> = app.photos_save.queue.drain(..).collect();
    if batch.is_empty() {
        return;
    }
    let photos: Vec<(lightcraft_catalog::PhotoId, String)> = batch.iter().map(|s| (s.id, s.file_name.clone())).collect();
    let (tx, rx) = std::sync::mpsc::channel();
    let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let label = match photos.as_slice() {
        [(_, name)] => format!("{} {name}", crate::i18n::tr("Saving to Apple Photos:")),
        many => crate::i18n::tr_format!("Saving {n} photos to Apple Photos", n = many.len()),
    };
    let guard = app.session.activity.start("applePhotosSave", &label, lightcraft_engine::activity::Cancel::Flag(cancel.clone()));
    let handle = guard.handle();
    std::thread::spawn(move || {
        // rendering takes the first half of the row, Photos preparing the assets the second
        let mut rendered = |f: f64| handle.progress((f * 500.0).round() as u64, 1000);
        let mut prepared = |f: f64| handle.progress(500 + (f * 500.0).round() as u64, 1000);
        let mut checked = |_: f64| {};
        let mut access = lightcraft_engine::photos_sync::PhotoKitAccess { cancel: &cancel, progress: &mut checked };
        let mut to_photos = lightcraft_engine::photos_save::photokit_save_many(&cancel, &mut prepared);
        let _ = tx.send(lightcraft_engine::photos_save::run_many(batch, &mut to_photos, &mut access, &mut rendered));
    });
    app.photos_save.running = Some(Running { photos, rx, _guard: guard });
}

#[cfg(target_arch = "wasm32")]
fn start_next(app: &mut LightcraftApp) {
    app.photos_save.queue.clear();
}

/// Every frame: take a finished save, say how it went, and start the next.
pub fn poll(app: &mut LightcraftApp, ctx: &egui::Context) {
    after_copies(app, ctx);
    let Some(running) = app.photos_save.running.as_ref() else { return };
    let done = match running.rx.try_recv() {
        Ok(done) => done,
        Err(TryRecvError::Empty) => {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
            return;
        }
        Err(TryRecvError::Disconnected) => {
            let why = crate::i18n::tr("Saving to Apple Photos stopped").to_string();
            running.photos.iter().map(|(id, name)| (*id, name.clone(), Err(why.clone()))).collect()
        }
    };
    app.photos_save.running = None;
    let mut saved = Vec::new();
    let mut failed = Vec::new();
    for (id, name, r) in done {
        match r.and_then(|modified| app.session.saved_to_photos(id, modified).map_err(|e| e.to_string())) {
            Ok(()) => saved.push(name),
            Err(e) => failed.push(e),
        }
    }
    let text = match (saved.as_slice(), failed.as_slice()) {
        ([name], []) => format!("{} {name}", crate::i18n::tr("Saved to Apple Photos:")),
        ([], [why]) => why.clone(),
        ([], [first, ..]) => crate::i18n::tr_format!("{m} edits not saved to Apple Photos: {why}", m = failed.len(), why = first),
        (_, []) => crate::i18n::tr_format!("Saved {n} edits to Apple Photos", n = saved.len()),
        (_, [first, ..]) => {
            crate::i18n::tr_format!("Saved {n} edits to Apple Photos; {m} not saved: {why}", n = saved.len(), m = failed.len(), why = first)
        }
    };
    app.toast(ctx, text);
    start_next(app);
    ctx.request_repaint();
}

/// `applePhotos.saveAllEdits` from the UI: every photo from Photos with LightCraft edits not
/// saved there, library-wide, as one batch (items not copied yet are copied first).
pub fn start_all(app: &mut LightcraftApp) -> Result<Value, String> {
    let ids = app.session.unsaved_photos_edits();
    if ids.is_empty() {
        return Err(crate::i18n::tr("Every edit is already saved to Apple Photos").to_string());
    }
    start(app, &json!({"ids": ids.iter().map(|id| id.0).collect::<Vec<_>>()}))
}
