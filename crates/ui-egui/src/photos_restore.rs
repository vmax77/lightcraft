//! Reading edits back from Apple Photos in the app (issue #1622): `applePhotos.restoreEdit` asks
//! Photos on a worker, under a row in the activity stack that shows an iCloud download Photos
//! needs first and can stop it, for the settings LightCraft stored with each photo's edit (see
//! [`lightcraft_engine::photos_restore`]); the settings found are applied as one undo step when
//! the worker is done. PhotoKit answers on the main thread, so it is never asked from there.

use std::sync::mpsc::{Receiver, TryRecvError};

use lightcraft_catalog::PhotoId;
use lightcraft_engine::activity::TaskGuard;
use lightcraft_engine::photos_restore::{NOT_LIGHTCRAFT, NOT_STORED, PhotosRestore, Restored};
use lightcraft_engine::photos_save::Skipped;
use serde_json::{Value, json};

use crate::LightcraftApp;

/// What the worker found for each photo.
type Found = Result<Vec<(PhotoId, Restored)>, String>;

/// A read of edits from Photos under way.
pub struct Running {
    rx: Receiver<Found>,
    /// Photos skipped before Photos was asked.
    skipped: Vec<Skipped>,
    _guard: TaskGuard,
}

impl std::fmt::Debug for Running {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Running").finish_non_exhaustive()
    }
}

/// `applePhotos.restoreEdit` from the UI: ask Photos about the target photos on a worker.
pub fn start(app: &mut LightcraftApp, p: &Value) -> Result<Value, String> {
    if cfg!(target_arch = "wasm32") {
        return Err("Apple Photos is only available on macOS".into());
    }
    if app.photos_restore.is_some() {
        return Err("edits are already being read from Apple Photos".into());
    }
    let ids = app.session.targets(p).map_err(|e| e.to_string())?;
    let (restores, skipped) = app.session.photos_restores(&ids).map_err(|e| e.to_string())?;
    if restores.is_empty() {
        let why = skipped.first().map_or("no photo selected", |k| k.why);
        return Err(format!("nothing to read from Apple Photos: {why}"));
    }
    let asked: Vec<u64> = restores.iter().map(|r| r.id.0).collect();
    run(app, restores, skipped);
    Ok(json!({"asked": asked}))
}

#[cfg(not(target_arch = "wasm32"))]
fn run(app: &mut LightcraftApp, restores: Vec<PhotosRestore>, skipped: Vec<Skipped>) {
    let (tx, rx) = std::sync::mpsc::channel();
    let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let label = crate::i18n::tr("Reading edits from Apple Photos").to_string();
    let guard = app.session.activity.start("applePhotosRestore", &label, lightcraft_engine::activity::Cancel::Flag(cancel.clone()));
    let handle = guard.handle();
    std::thread::spawn(move || {
        let n = restores.len() as u64;
        let mut found = Vec::with_capacity(restores.len());
        for (i, one) in restores.iter().enumerate() {
            let done = i as u64;
            let mut progress = |f: f64| handle.progress(done * 1000 + (f * 1000.0).round() as u64, n * 1000);
            let mut read = lightcraft_engine::photos_restore::photokit_read(&cancel, &mut progress);
            match one.read(&mut read) {
                Ok(r) => found.push((one.id, r)),
                Err(e) => {
                    let _ = tx.send(Err(e));
                    return;
                }
            }
        }
        let _ = tx.send(Ok(found));
    });
    app.photos_restore = Some(Running { rx, skipped, _guard: guard });
}

#[cfg(target_arch = "wasm32")]
fn run(_: &mut LightcraftApp, _: Vec<PhotosRestore>, _: Vec<Skipped>) {}

/// Every frame: apply what a finished read found, and say how it went.
pub fn poll(app: &mut LightcraftApp, ctx: &egui::Context) {
    let Some(running) = app.photos_restore.as_ref() else { return };
    let found = match running.rx.try_recv() {
        Ok(found) => found,
        Err(TryRecvError::Empty) => {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
            return;
        }
        Err(TryRecvError::Disconnected) => Err(crate::i18n::tr("Reading edits from Apple Photos stopped").to_string()),
    };
    let Some(Running { skipped, .. }) = app.photos_restore.take() else { return };
    let text = match found {
        Err(e) => e,
        Ok(found) => {
            let mut not_lightcraft = skipped.len();
            let mut settings = Vec::new();
            for (id, r) in found {
                match r {
                    Restored::Settings(s) => settings.push((id, *s)),
                    Restored::NotLightCraft | Restored::NotStored | Restored::OtherBase => not_lightcraft += 1,
                }
            }
            match app.session.adopt_restored(settings) {
                Ok(done) if done.is_empty() => {
                    format!("{} {}", crate::i18n::tr("No edit restored:"), if not_lightcraft > 0 { NOT_LIGHTCRAFT } else { NOT_STORED })
                }
                Ok(done) => format!("{} {}", crate::i18n::tr("Edits restored from Apple Photos:"), done.len()),
                Err(e) => e.to_string(),
            }
        }
    };
    app.toast(ctx, text);
    ctx.request_repaint();
}
