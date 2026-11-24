//! Background threads: the ingest writer (one DB connection, processes captures
//! in order) and the OCR backfiller (recognizes text in images, newest first).

use crate::db::Db;
use crate::ingest::Capture;
use crate::settings::Settings;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::time::Duration;

pub enum Job {
    Capture(Capture),
    SaveAppIcon { key: String, name: Option<String>, path: String },
    Prune,
    Vacuum,
    /// A clip was just sealed: purge its old index entries and pages.
    Scrub,
}

pub enum Event {
    Ingested(i64),
    Pruned(usize),
    Ocr(i64),
}

pub type Notify = std::sync::Arc<dyn Fn(Event) + Send + Sync>;

unsafe extern "C" {
    fn pthread_set_qos_class_self_np(qos: u32, relative_priority: i32) -> i32;
}

const QOS_UTILITY: u32 = 0x11;
const QOS_BACKGROUND: u32 = 0x09;

pub fn spawn(db_path: PathBuf, notify: Notify) -> (Sender<Job>, Sender<()>) {
    let (tx, rx) = channel::<Job>();
    let (ocr_tx, ocr_rx) = channel::<()>();
    let ocr_wake = ocr_tx.clone();
    let p = db_path.clone();
    let n = notify.clone();
    std::thread::Builder::new()
        .name("ingest".into())
        .spawn(move || ingest_loop(p, rx, n, ocr_wake))
        .expect("spawn ingest");
    std::thread::Builder::new()
        .name("ocr".into())
        .spawn(move || ocr_loop(db_path, ocr_rx, notify))
        .expect("spawn ocr");
    (tx, ocr_tx)
}

fn ingest_loop(path: PathBuf, rx: Receiver<Job>, notify: Notify, ocr: Sender<()>) {
    unsafe { pthread_set_qos_class_self_np(QOS_UTILITY, 0) };
    let db = match Db::open(&path) {
        Ok(d) => d,
        Err(e) => {
            crate::log!("ingest: open db: {e}");
            return;
        }
    };
    if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| fix_file_refs(&db, &notify))).is_err() {
        crate::log!("ingest: resolving file references panicked, continuing");
    }
    while let Ok(job) = rx.recv() {
        // A bug in one clip must never stop recording: contain panics per job.
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run_job(&db, job, &notify, &ocr)));
        if r.is_err() {
            crate::log!("ingest: job panicked, continuing");
        }
    }
}

/// File clips stored with a file reference URL showed "/.file/id=…": resolve
/// them to their paths, for the files still there.
fn fix_file_refs(db: &Db, notify: &Notify) {
    let s = Settings::load(db);
    let mut fixed = 0;
    for id in db.file_ref_clips().unwrap_or_default() {
        objc2::rc::autoreleasepool(|_| {
            let Ok(mut reps) = db.reps(id) else { return };
            if !crate::ingest::resolve_file_refs(&mut reps) {
                return;
            }
            let cap = Capture { ts: 0, reps, app: Default::default(), window_title: None, source_path: None, starred: false };
            let Some(clip) = crate::ingest::build(cap, &s) else { return };
            match db.set_text(id, &clip, s.zstd_level) {
                Ok(kept) => {
                    let _ = db.fix_file_ref_source(kept, clip.source_path.as_deref());
                    crate::log!("ingest: clip {id}: file reference resolved to {}", clip.preview);
                    fixed += 1;
                }
                Err(e) => crate::log!("ingest: clip {id}: file reference: {e}"),
            }
        });
    }
    if fixed > 0 {
        notify(Event::Pruned(0));
    }
}

fn run_job(db: &Db, job: Job, notify: &Notify, ocr: &Sender<()>) {
    {
        objc2::rc::autoreleasepool(|_| match job {
            Job::Capture(cap) => {
                let s = Settings::load(db);
                let t0 = std::time::Instant::now();
                let Some(clip) = crate::ingest::build(cap, &s) else { return };
                let kind = clip.kind;
                match db.insert(&clip, s.zstd_level) {
                    Ok(r) => {
                        crate::log!(
                            "ingest: {:?} {} {} reps {} in {:?}",
                            r,
                            kind.as_str(),
                            clip.app.name.as_deref().unwrap_or("?"),
                            clip.reps.len(),
                            t0.elapsed()
                        );
                        notify(Event::Ingested(r.id()));
                        if kind == crate::db::Kind::Image {
                            let _ = ocr.send(());
                        }
                    }
                    Err(e) => crate::log!("ingest: insert failed: {e}"),
                }
            }
            Job::SaveAppIcon { key, name, path } => {
                if !db.has_app(&key).unwrap_or(true) {
                    let icon = crate::appicon::png_for_path(&path);
                    let _ = db.save_app(&key, name.as_deref(), Some(&path), icon.as_deref());
                }
            }
            Job::Prune => {
                let s = Settings::load(db);
                if s.keep_days > 0 {
                    let before = crate::timefmt::now_ms() - s.keep_days as i64 * 86_400_000;
                    match db.clear(Some(before), false) {
                        Ok(n) if n > 0 => {
                            crate::log!("prune: removed {n} clips older than {} days", s.keep_days);
                            let _ = db.vacuum_some();
                            notify(Event::Pruned(n));
                        }
                        Ok(_) => {}
                        Err(e) => crate::log!("prune: {e}"),
                    }
                }
            }
            Job::Vacuum => {
                if let Err(e) = db.vacuum_some() {
                    crate::log!("vacuum: {e}");
                }
            }
            Job::Scrub => {
                if let Err(e) = db.scrub() {
                    crate::log!("scrub: {e}");
                }
            }
        });
    }
}

fn ocr_loop(path: PathBuf, wake: Receiver<()>, notify: Notify) {
    unsafe { pthread_set_qos_class_self_np(QOS_BACKGROUND, 0) };
    let db = match Db::open(&path) {
        Ok(d) => d,
        Err(e) => {
            crate::log!("ocr: open db: {e}");
            return;
        }
    };
    loop {
        let s = Settings::load(&db);
        let pending = if s.ocr { db.pending_ocr(16).unwrap_or_default() } else { vec![] };
        if pending.is_empty() {
            match wake.recv_timeout(Duration::from_secs(600)) {
                Ok(()) | Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
        for id in pending {
            if db.is_sealed(id) {
                continue; // sealed after it was queued: never read it
            }
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| objc2::rc::autoreleasepool(|_| {
                let types = db.rep_types(id).unwrap_or_default();
                let uti = crate::ingest::IMAGE_TYPES
                    .iter()
                    .find(|t| types.iter().any(|x| x == *t) && **t != "com.adobe.pdf");
                let data = uti.and_then(|u| db.rep(id, u).ok().flatten());
                let res = match data {
                    Some(d) => crate::ocr::recognize(&d),
                    None => Err("no image".into()),
                };
                let _ = match res {
                    Ok(text) => db.set_ocr(id, Some(&text), 1),
                    Err(e) => {
                        crate::log!("ocr: clip {id}: {e}");
                        db.set_ocr(id, None, 2)
                    }
                };
                notify(Event::Ocr(id));
            })));
            if r.is_err() {
                crate::log!("ocr: clip {id} panicked");
                let _ = db.set_ocr(id, None, 2);
            }
        }
    }
}
