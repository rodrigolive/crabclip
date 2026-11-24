//! The menu bar app: status item, global hotkey, pasteboard watcher, the
//! history panel (list + preview) and the paste flow.

mod build;
pub mod classes;
mod draw;
mod files;
pub mod folders;
pub mod prefs;
mod snippets;

use crate::db::{Db, Detail, Filter, Kind, RowInfo};
use crate::hotkeys::{self, Action};
use crate::ingest::{Capture, SKIP_MARKERS};
use crate::settings::Settings;
use crate::worker::{Event, Job};
use crate::{log, timefmt};
use classes::{ClipPanel, ClipTable, Handler, RowView};
use dispatch2::DispatchQueue;
use global_hotkey::hotkey::HotKey;
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject, Sel};
use objc2::{AnyThread, DefinedClass, MainThreadMarker, Message, msg_send, sel};
use objc2_app_kit::{
    NSAlert, NSAlertFirstButtonReturn, NSApplication, NSApplicationActivationOptions,
    NSApplicationActivationPolicy, NSAttributedStringAppKitDocumentFormats, NSButton, NSColor,
    NSControl, NSEvent, NSEventModifierFlags, NSEventType, NSFont, NSImage, NSImageView,
    NSMenu, NSPasteboard, NSPopUpButton, NSRunningApplication, NSScreen, NSScrollView,
    NSBackgroundColorAttributeName, NSForegroundColorAttributeName, NSParagraphStyleAttributeName, NSSearchField, NSSplitView, NSStatusItem, NSTableView, NSTextField, NSTextView, NSUserInterfaceItemIdentification, NSView, NSWorkspace,
};
use objc2_foundation::{
    NSAttributedString, NSBundle, NSData, NSDistributedNotificationCenter, NSIndexSet, NSMutableAttributedString,
    NSNotification, NSPoint, NSRange, NSRect, NSRunLoop, NSSize, NSRunLoopCommonModes, NSString,
    NSTimer, NSUserDefaults,
};
use std::cell::{Cell, OnceCell, RefCell};
use std::collections::{HashMap, HashSet, VecDeque};
use std::rc::Rc;
use std::sync::mpsc::Sender;
use std::time::Instant;

pub const SHOW_NOTIFICATION: &str = "com.rod.crabclip.show";
pub const QUIT_NOTIFICATION: &str = "com.rod.crabclip.quit";
/// Shortcut settings were changed by another process (`--import-clipy`).
pub const SHORTCUTS_NOTIFICATION: &str = "com.rod.crabclip.shortcuts";
pub const AGENT_LABEL: &str = "com.rod.crabclip.agent";
pub const PANEL_AUTOSAVE: &str = "crabclip.panel";
const CACHE_ROWS: usize = 160;
/// How long a delete can be undone.
const UNDO_SECONDS: f64 = 4.0;
const SEARCH_LIMIT: usize = 200_000;

thread_local! {
    static APP: OnceCell<&'static App> = const { OnceCell::new() };
}

pub fn app() -> &'static App {
    APP.with(|a| *a.get().expect("app not started"))
}

pub struct RowData {
    pub info: RowInfo,
    pub thumb: Option<Retained<NSImage>>,
}

pub struct Ui {
    pub panel: Retained<ClipPanel>,
    pub table: Retained<ClipTable>,
    pub search: Retained<NSSearchField>,
    pub app_popup: Retained<NSPopUpButton>,
    pub star_btn: Retained<NSButton>,
    pub pin_btn: Retained<NSButton>,
    pub preview_btn: Retained<NSButton>,
    pub split: Retained<NSSplitView>,
    pub list: Retained<NSScrollView>,
    pub preview: Retained<NSView>,
    pub zoom: Retained<NSView>,
    pub tags_field: Retained<objc2_app_kit::NSTokenField>,
    pub folders_field: Retained<objc2_app_kit::NSTokenField>,
    pub tag_strip: Retained<classes::TagStrip>,
    pub tag_menu: Retained<objc2_app_kit::NSPanel>,
    pub tag_menu_view: Retained<classes::PillMenu>,
    pub top_bar: Retained<NSView>,
    pub bottom_bar: Retained<NSView>,
    pub date_label: Retained<NSTextField>,
    pub undo_btn: Retained<NSButton>,
    pub count_label: Retained<NSTextField>,
    pub text_scroll: Retained<NSScrollView>,
    pub text_view: Retained<NSTextView>,
    pub image_view: Retained<NSImageView>,
    pub meta: Retained<NSTextField>,
    pub status_item: Retained<NSStatusItem>,
    pub status_menu: Retained<NSMenu>,
}

pub struct App {
    pub mtm: MainThreadMarker,
    pub handler: Retained<Handler>,
    pub db: Db,
    pub settings: RefCell<Settings>,
    pub jobs: Sender<Job>,
    pub ids: RefCell<Vec<i64>>,
    cache: RefCell<HashMap<i64, Rc<RowData>>>,
    cache_order: RefCell<VecDeque<i64>>,
    icons: RefCell<HashMap<String, Retained<NSImage>>>,
    pub ui: OnceCell<Ui>,
    last_change: Cell<isize>,
    prev_app: RefCell<Option<Retained<NSRunningApplication>>>,
    app_filter: RefCell<Option<String>>,
    hotkey: RefCell<Option<(GlobalHotKeyManager, Vec<HotKey>)>>,
    /// What each registered shortcut opens, by hotkey id.
    hotkey_actions: RefCell<HashMap<u32, Action>>,
    /// Folder mode (the search starts with "/"): the folder listed (-1: none
    /// at that path) and what each row is.
    folder: Cell<Option<i64>>,
    entries: RefCell<HashMap<i64, crate::db::Entry>>,
    /// Row id to select after the next search (going back up a folder).
    select_after: Cell<Option<i64>>,
    /// Clip whose folders the folder field shows.
    folders_clip: Cell<i64>,
    /// The Quick Look view of the file pane (made the first time) and the
    /// file it shows.
    ql: RefCell<Option<Retained<objc2_app_kit::NSView>>>,
    ql_path: RefCell<Option<String>>,
    pasting: Cell<bool>,
    modal: Cell<bool>,
    /// Clip whose tags the tag field shows (saved there even if the
    /// selection moved before editing ended).
    tags_clip: Cell<i64>,
    /// A deleted clip that can still be brought back.
    pending_delete: Cell<Option<i64>>,
    delete_token: Cell<u64>,
    last_hide: Cell<Option<Instant>>,
    last_prune: Cell<Option<Instant>>,
    seen_apps: RefCell<HashSet<String>>,
    dirty: Cell<bool>,
    own_bundle: Option<String>,
}

/// Entry point: runs the Cocoa app (never returns).
pub fn run() {
    let mtm = MainThreadMarker::new().expect("must run on the main thread");
    let nsapp = NSApplication::sharedApplication(mtm);
    nsapp.setActivationPolicy(NSApplicationActivationPolicy::Accessory);

    // Single instance. The login agent's copy (restarted by launchd if it ever
    // crashes) takes over from a hand-launched one; any other second launch just
    // asks the running copy to show its window.
    if let Some(bid) = NSBundle::mainBundle().bundleIdentifier() {
        let me = std::process::id() as i32;
        let others = || {
            NSRunningApplication::runningApplicationsWithBundleIdentifier(&bid)
                .iter()
                .filter(|r| r.processIdentifier() != me)
                .count()
        };
        if others() > 0 {
            let is_agent = running_as_agent() || std::env::args().any(|a| a == "--takeover");
            let name = if is_agent { QUIT_NOTIFICATION } else { SHOW_NOTIFICATION };
            post_distributed(name);
            if !is_agent {
                std::process::exit(0);
            }
            for _ in 0..50 {
                std::thread::sleep(std::time::Duration::from_millis(100));
                if others() == 0 {
                    break;
                }
            }
        }
    }

    let handler = Handler::new(mtm);
    nsapp.setDelegate(Some(ProtocolObject::from_ref(&*handler)));
    nsapp.run();
    drop(handler);
}

fn start(handler: &Handler) {
    let mtm = MainThreadMarker::new().unwrap();
    log!("crabclip {} starting (pid {})", env!("CARGO_PKG_VERSION"), std::process::id());
    let db_path = Db::default_path();
    let db = match Db::open(&db_path) {
        Ok(db) => db,
        Err(e) => {
            log!("fatal: cannot open {}: {e}", db_path.display());
            std::process::exit(2);
        }
    };
    let mut settings = Settings::load(&db);
    // Every launch shows the whole history: a starred-only filter left on
    // looked like a lost database.
    settings.star_filter = false;
    let notify: crate::worker::Notify = std::sync::Arc::new(|e: Event| {
        DispatchQueue::main().exec_async(move || app().on_event(e));
    });
    let (jobs, _ocr_wake) = crate::worker::spawn(db_path, notify);
    let pb = NSPasteboard::generalPasteboard();
    let app_state = App {
        mtm,
        handler: handler.retain(),
        db,
        settings: RefCell::new(settings),
        jobs,
        ids: RefCell::new(Vec::new()),
        cache: RefCell::new(HashMap::new()),
        cache_order: RefCell::new(VecDeque::new()),
        icons: RefCell::new(HashMap::new()),
        ui: OnceCell::new(),
        last_change: Cell::new(pb.changeCount()),
        prev_app: RefCell::new(None),
        app_filter: RefCell::new(None),
        hotkey: RefCell::new(None),
        hotkey_actions: RefCell::new(HashMap::new()),
        folder: Cell::new(None),
        entries: RefCell::new(HashMap::new()),
        select_after: Cell::new(None),
        folders_clip: Cell::new(0),
        ql: RefCell::new(None),
        ql_path: RefCell::new(None),
        pasting: Cell::new(false),
        modal: Cell::new(false),
        tags_clip: Cell::new(0),
        pending_delete: Cell::new(None),
        delete_token: Cell::new(0),
        last_hide: Cell::new(None),
        last_prune: Cell::new(None),
        seen_apps: RefCell::new(HashSet::new()),
        dirty: Cell::new(true),
        own_bundle: NSBundle::mainBundle().bundleIdentifier().map(|s| s.to_string()),
    };
    let a: &'static App = Box::leak(Box::new(app_state));
    APP.with(|cell| {
        let _ = cell.set(a);
    });

    build::main_menu(a);
    let ui = build::build(a);
    let _ = a.ui.set(ui);
    a.apply_buttons();
    a.apply_scale();
    a.apply_preview(false);

    if let Ok(out) = std::env::var("CRABCLIP_SNAPSHOT") {
        a.snapshot(&out);
        std::process::exit(0);
    }

    a.register_hotkeys();

    unsafe {
        let timer = NSTimer::timerWithTimeInterval_target_selector_userInfo_repeats(
            0.25,
            &a.handler,
            sel!(tick:),
            None,
            true,
        );
        NSRunLoop::mainRunLoop().addTimer_forMode(&timer, NSRunLoopCommonModes);
        let center = NSDistributedNotificationCenter::defaultCenter();
        center.addObserver_selector_name_object(
            &a.handler,
            sel!(showFromNotification:),
            Some(&NSString::from_str(SHOW_NOTIFICATION)),
            None,
        );
        center.addObserver_selector_name_object(
            &a.handler,
            sel!(quitFromNotification:),
            Some(&NSString::from_str(QUIT_NOTIFICATION)),
            None,
        );
        center.addObserver_selector_name_object(
            &a.handler,
            sel!(shortcutsFromNotification:),
            Some(&NSString::from_str(SHORTCUTS_NOTIFICATION)),
            None,
        );
    }

    let installed = NSBundle::mainBundle().bundlePath().to_string().starts_with("/Applications/");
    let want_login = a.settings.borrow().launch_at_login;
    if installed && want_login != crate::login::is_enabled() && !(running_as_agent() && !want_login) {
        if let Err(e) = crate::login::set_enabled(want_login) {
            log!("login item: {e}");
        }
    }
    if !crate::ax::trusted() && a.db.get_setting("ax_prompted").is_none() {
        let _ = a.db.set_setting("ax_prompted", "1");
        crate::ax::prompt();
    }
    if a.db.get_setting("clipy_asked").is_none() && crate::clipy::default_db(false).is_some() {
        DispatchQueue::main().exec_async(prefs::offer_clipy);
    }
    let _ = a.jobs.send(Job::Prune);
    a.last_prune.set(Some(Instant::now()));
    log!("ready: {} clips", a.db.count().unwrap_or(0));
}

impl App {
    pub fn ui(&self) -> &Ui {
        self.ui.get().expect("ui")
    }

    /// Debug aid: render the panel offscreen to a PNG (never shown, never key).
    /// CRABCLIP_SNAPSHOT_QUERY / _ROW pick what is displayed.
    fn snapshot(&self, out: &str) {
        crate::settings::read_only();
        let ui = self.ui();
        // CRABCLIP_SNAPSHOT_APPEARANCE=dark|light instead of the system's.
        if let Ok(look) = std::env::var("CRABCLIP_SNAPSHOT_APPEARANCE") {
            let name = unsafe { if look == "dark" { objc2_app_kit::NSAppearanceNameDarkAqua } else { objc2_app_kit::NSAppearanceNameAqua } };
            NSApplication::sharedApplication(self.mtm).setAppearance(objc2_app_kit::NSAppearance::appearanceNamed(name).as_deref());
        }
        self.rebuild_app_popup();
        let q = std::env::var("CRABCLIP_SNAPSHOT_QUERY").unwrap_or_default();
        ui.search.setStringValue(&NSString::from_str(&q));
        self.run_search(true);
        self.update_tag_menu();
        let row: isize = std::env::var("CRABCLIP_SNAPSHOT_ROW").ok().and_then(|r| r.parse().ok()).unwrap_or(0);
        self.select_row(row);
        self.update_preview();
        // Arrow keys as the list gets them: r → l ← u ↑ d ↓.
        for k in std::env::var("CRABCLIP_SNAPSHOT_KEYS").unwrap_or_default().chars() {
            match k {
                'r' | 'l' => self.arrow(k == 'r'),
                'u' | 'd' => self.move_selection(if k == 'd' { 1 } else { -1 }),
                _ => {}
            }
        }
        if std::env::var("CRABCLIP_SNAPSHOT_DELETE").is_ok() {
            // Shows the undo pill; the process exits before the delete is final.
            // Never for a starred clip: its confirmation alert would take focus.
            let starred = self.selected_id().and_then(|id| self.db.row(id).ok().flatten()).is_some_and(|r| r.starred);
            let filed = self.selected_id().is_some_and(|id| !self.db.folders_of(id).unwrap_or_default().is_empty());
            if starred || filed {
                log!("snapshot: selected clip is starred or filed, not deleting");
            } else {
                self.delete_selected();
            }
        }
        if std::env::var("CRABCLIP_SNAPSHOT_TOGGLE").is_ok() {
            // As if → / ← was pressed: fold or open the preview.
            let w = ui.panel.frame().size.width;
            self.toggle_preview();
            log!("snapshot: preview toggled, window {w} -> {}, list {} split {}", ui.panel.frame().size.width, ui.list.frame().size.width, ui.split.frame().size.width);
        }
        if std::env::var("CRABCLIP_SNAPSHOT_PREFS").is_ok() {
            prefs::snapshot(out);
            return;
        }
        if std::env::var("CRABCLIP_SNAPSHOT_LIVE").is_ok() {
            // Real compositing, far off every screen and never key: nothing
            // shows and no keystroke can land in it.
            let mut f = ui.panel.frame();
            // Don't let the far-away position become the remembered one.
            ui.panel.setFrameAutosaveName(&NSString::from_str(""));
            f.origin = NSPoint::new(-30000.0, -30000.0);
            ui.panel.setFrame_display(f, true);
            ui.panel.orderFrontRegardless();
            NSRunLoop::currentRunLoop().runUntilDate(&objc2_foundation::NSDate::dateWithTimeIntervalSinceNow(0.8));
            let n = ui.panel.windowNumber().to_string();
            let _ = std::process::Command::new("/usr/sbin/screencapture").args(["-x", "-o", "-l", &n, out]).status();
            if ui.tag_menu.isVisible() {
                let m = ui.tag_menu.windowNumber().to_string();
                let mout = out.replace(".png", "-menu.png");
                let _ = std::process::Command::new("/usr/sbin/screencapture").args(["-x", "-o", "-l", &m, &mout]).status();
            }
            ui.panel.orderOut(None);
            log!("live snapshot of window {n} written to {out}");
            return;
        }
        let Some(view) = ui.panel.contentView() else { return };
        view.layoutSubtreeIfNeeded();
        ui.table.tile();
        let b = view.bounds();
        let Some(rep) = view.bitmapImageRepForCachingDisplayInRect(b) else { return };
        view.cacheDisplayInRect_toBitmapImageRep(b, &rep);
        let props = objc2_foundation::NSDictionary::new();
        if let Some(png) = unsafe { rep.representationUsingType_properties(objc2_app_kit::NSBitmapImageFileType::PNG, &props) } {
            let _ = std::fs::write(out, png.to_vec());
            log!("snapshot written to {out}");
        }
    }

    // ---------------------------------------------------------------- hotkey

    /// (Re)register every global shortcut. Returns the ones that couldn't be
    /// had (bad syntax, another app holds them) and why; the others work.
    pub fn register_hotkeys(&self) -> Vec<(String, String)> {
        let bindings = hotkeys::bindings(&self.settings.borrow());
        let mut slot = self.hotkey.borrow_mut();
        if slot.is_none() {
            let m = match GlobalHotKeyManager::new() {
                Ok(m) => m,
                Err(e) => {
                    log!("hotkey: {e}");
                    return bindings.into_iter().map(|(k, _)| (k, e.to_string())).collect();
                }
            };
            GlobalHotKeyEvent::set_event_handler(Some(|e: GlobalHotKeyEvent| {
                if e.state == HotKeyState::Pressed {
                    let id = e.id;
                    DispatchQueue::main().exec_async(move || app().hotkey_pressed(id));
                }
            }));
            *slot = Some((m, Vec::new()));
        }
        let (m, current) = slot.as_mut().unwrap();
        for old in current.drain(..) {
            let _ = m.unregister(old);
        }
        let mut actions = self.hotkey_actions.borrow_mut();
        actions.clear();
        let mut failed = Vec::new();
        let mut history = Vec::new();
        for (spec, action) in bindings {
            let hk = match hotkeys::parse(&spec) {
                Ok(h) => h,
                Err(e) => {
                    failed.push((spec, e));
                    continue;
                }
            };
            let res = match actions.get(&hk.id()) {
                Some(other) => Err(format!("{} already opens {}", hotkeys::label(&spec), other.describe())),
                // Another app holding it doesn't take the other shortcuts down.
                None => m.register(hk).map_err(|e| format!("cannot register {}: {e}", hotkeys::label(&spec))),
            };
            match res {
                Ok(()) => {
                    log!("hotkey: {spec} opens {}", action.describe());
                    if action == Action::History {
                        history.push(hotkeys::label(&spec));
                    }
                    current.push(hk);
                    actions.insert(hk.id(), action);
                }
                Err(e) => {
                    log!("hotkey: {e}");
                    failed.push((spec, e));
                }
            }
        }
        if let Some(btn) = self.ui.get().and_then(|ui| ui.status_item.button(self.mtm)) {
            let tip = if history.is_empty() {
                format!("crabclip — {} unavailable", hotkeys::labels(&self.settings.borrow().hotkey))
            } else {
                format!("crabclip — {}", history.join(", "))
            };
            btn.setToolTip(Some(&NSString::from_str(&tip)));
        }
        failed
    }

    fn hotkey_pressed(&self, id: u32) {
        let Some(action) = self.hotkey_actions.borrow().get(&id).cloned() else { return };
        let p = &self.ui().panel;
        let up = p.isVisible() && p.isKeyWindow();
        match action {
            Action::History if up && !self.in_folders() => self.hide(),
            Action::History => self.show(),
            Action::Folders if up && self.in_folders() => self.hide(),
            Action::Folders => self.show_folders(),
            Action::Folder(path) if up && self.folder.get() == self.db.nearest_folder(&path).ok() => self.hide(),
            Action::Folder(path) => self.show_folder(&path),
            Action::Prefs => prefs::toggle(),
        }
    }

    pub fn status_clicked(&self) {
        let nsapp = NSApplication::sharedApplication(self.mtm);
        let ev = nsapp.currentEvent();
        let right = ev.as_ref().is_some_and(|e| {
            e.r#type() == NSEventType::RightMouseUp
                || e.modifierFlags().contains(NSEventModifierFlags::Control)
        });
        let ui = self.ui();
        if right {
            ui.status_item.setMenu(Some(&ui.status_menu));
            if let Some(b) = ui.status_item.button(self.mtm) {
                unsafe { b.performClick(None) };
            }
            ui.status_item.setMenu(None);
            return;
        }
        let just_hidden = self.last_hide.get().is_some_and(|t| t.elapsed().as_millis() < 300);
        if ui.panel.isVisible() || just_hidden {
            self.hide();
        } else {
            self.show();
        }
    }

    // --------------------------------------------------------------- capture

    pub fn tick(&self) {
        let pb = NSPasteboard::generalPasteboard();
        let cc = pb.changeCount();
        if cc != self.last_change.get() {
            self.last_change.set(cc);
            self.capture(&pb);
        }
        if self.last_prune.get().is_none_or(|t| t.elapsed().as_secs() > 3600) {
            self.last_prune.set(Some(Instant::now()));
            let _ = self.jobs.send(Job::Prune);
        }
    }

    fn capture(&self, pb: &NSPasteboard) {
        let Some(items) = pb.pasteboardItems() else { return };
        let mut reps = Vec::new();
        for (i, item) in items.iter().enumerate() {
            for t in item.types().iter() {
                let uti = t.to_string();
                if SKIP_MARKERS.contains(&uti.as_str()) {
                    log!("capture: skipped concealed/transient clip ({uti})");
                    return;
                }
                if let Some(d) = item.dataForType(&t) {
                    if d.len() <= 512 * 1024 * 1024 {
                        reps.push(crate::db::Rep { item: i as i64, uti, data: d.to_vec() });
                    }
                }
            }
        }
        if reps.is_empty() {
            return;
        }
        let ws = NSWorkspace::sharedWorkspace();
        let front = ws.frontmostApplication();
        let mut info = crate::db::AppInfo::default();
        let mut pid = 0;
        if let Some(f) = &front {
            info.bundle = f.bundleIdentifier().map(|s| s.to_string());
            info.name = f.localizedName().map(|s| s.to_string());
            info.path = f.bundleURL().and_then(|u| u.path()).map(|s| s.to_string());
            pid = f.processIdentifier();
        }
        if let Some(b) = &info.bundle {
            if self.settings.borrow().ignore_apps.iter().any(|x| x == b) {
                log!("capture: ignored app {b}");
                return;
            }
        }
        let (window_title, source_path) = crate::ax::focused_window_info(pid);
        if let (Some(key), Some(path)) = (info.bundle.clone().or(info.name.clone()), info.path.clone()) {
            if self.seen_apps.borrow_mut().insert(key.clone()) {
                let _ = self.jobs.send(Job::SaveAppIcon { key, name: info.name.clone(), path });
            }
        }
        let cap = Capture { ts: timefmt::now_ms(), reps, app: info, window_title, source_path, starred: false };
        let _ = self.jobs.send(Job::Capture(cap));
    }

    fn on_event(&self, e: Event) {
        match e {
            Event::Ingested(id) | Event::Ocr(id) => {
                if matches!(e, Event::Ingested(_)) && self.pending_delete.get() == Some(id) {
                    // Copied again while its delete could still be undone: keep it.
                    self.pending_delete.set(None);
                    self.show_undo(false);
                }
                self.invalidate(id);
                let is_ocr = matches!(e, Event::Ocr(_));
                if is_ocr {
                    if self.selected_id() == Some(id) {
                        self.update_preview();
                    }
                    return;
                }
            }
            Event::Pruned(_) => self.invalidate_all(),
        }
        if self.ui().panel.isVisible() {
            self.run_search(false);
        } else {
            self.dirty.set(true);
        }
    }

    // ----------------------------------------------------------------- cache

    pub fn invalidate(&self, id: i64) {
        self.cache.borrow_mut().remove(&id);
    }

    pub fn invalidate_all(&self) {
        self.cache.borrow_mut().clear();
        self.cache_order.borrow_mut().clear();
    }

    pub fn row_data(&self, id: i64) -> Option<Rc<RowData>> {
        if let Some(d) = self.cache.borrow().get(&id) {
            return Some(d.clone());
        }
        let mut info = self.db.row(id).ok()??;
        let thumb = info
            .thumb
            .take()
            .and_then(|t| NSImage::initWithData(NSImage::alloc(), &NSData::from_vec(t)));
        let d = Rc::new(RowData { info, thumb });
        let mut cache = self.cache.borrow_mut();
        let mut order = self.cache_order.borrow_mut();
        cache.insert(id, d.clone());
        order.push_back(id);
        while order.len() > CACHE_ROWS {
            if let Some(old) = order.pop_front() {
                if old != id {
                    cache.remove(&old);
                }
            }
        }
        Some(d)
    }

    pub fn app_icon(&self, info: &RowInfo) -> Retained<NSImage> {
        let key = format!("app:{}", info.app_key);
        if let Some(i) = self.icons.borrow().get(&key) {
            return i.clone();
        }
        let img = info
            .app_path
            .as_deref()
            .filter(|p| std::path::Path::new(p).exists())
            .map(|p| NSWorkspace::sharedWorkspace().iconForFile(&NSString::from_str(p)))
            .or_else(|| {
                let png = self.db.app_icon(&info.app_key).ok()??;
                NSImage::initWithData(NSImage::alloc(), &NSData::from_vec(png))
            })
            .or_else(|| symbol("doc.on.clipboard"))
            .unwrap_or_else(NSImage::new);
        self.icons.borrow_mut().insert(key, img.clone());
        img
    }

    pub fn file_icon(&self, path: &str) -> Retained<NSImage> {
        let key = format!("file:{path}");
        if let Some(i) = self.icons.borrow().get(&key) {
            return i.clone();
        }
        let img = NSWorkspace::sharedWorkspace().iconForFile(&NSString::from_str(path));
        self.icons.borrow_mut().insert(key, img.clone());
        img
    }

    pub fn small_app_icon(&self, key: &str, path: Option<&str>) -> Option<Retained<NSImage>> {
        let ck = format!("small:{key}");
        if let Some(i) = self.icons.borrow().get(&ck) {
            return Some(i.clone());
        }
        let info = RowInfo {
            id: 0,
            kind: Kind::Other,
            preview: String::new(),
            starred: false,
            app_key: key.to_string(),
            app_name: None,
            app_path: path.map(String::from),
            copied_at: 0,
            img_w: 0,
            img_h: 0,
            img_scale: 1.0,
            thumb: None,
            tags: Vec::new(),
        };
        let big = self.app_icon(&info);
        let small: Retained<NSImage> = unsafe { msg_send![&*big, copy] };
        small.setSize(objc2_foundation::NSSize::new(16.0, 16.0));
        self.icons.borrow_mut().insert(ck, small.clone());
        Some(small)
    }

    // ----------------------------------------------------------------- list

    /// The selected clip (not a folder row).
    pub fn selected_id(&self) -> Option<i64> {
        self.selected_row_id().filter(|id| *id > 0)
    }

    /// What the selected row shows: a clip id, or minus a folder id.
    pub fn selected_row_id(&self) -> Option<i64> {
        let row = self.ui().table.selectedRow();
        if row < 0 {
            return None;
        }
        self.ids.borrow().get(row as usize).copied()
    }

    pub fn clicked_row(&self) -> isize {
        self.ui().table.clickedRow()
    }

    pub fn run_search(&self, reset_selection: bool) {
        let ui = self.ui();
        let q = ui.search.stringValue().to_string();
        let star = self.settings.borrow().star_filter;
        let app_f = self.app_filter.borrow().clone();
        let prev = self.select_after.take().or(if reset_selection { None } else { self.selected_row_id() });
        let t0 = Instant::now();
        let ids = if let Some(fq) = crate::query::parse_folder_query(&q) {
            self.folder_ids(&fq)
        } else {
            self.folder.set(None);
            self.entries.borrow_mut().clear();
            self.db
                .search(&Filter { query: &q, starred: star, app: app_f.as_deref() }, SEARCH_LIMIT)
                .unwrap_or_else(|e| {
                    log!("search {q:?}: {e}");
                    Vec::new()
                })
        };
        let pending = self.pending_delete.get();
        let ids: Vec<i64> = ids.into_iter().filter(|id| Some(*id) != pending).collect();
        let row = prev.and_then(|id| ids.iter().position(|x| *x == id)).unwrap_or(0);
        let n = ids.len();
        *self.ids.borrow_mut() = ids;
        self.dirty.set(false);
        self.apply_row_height();
        ui.table.reloadData();
        if n > 0 {
            self.select_row(row as isize);
        }
        self.update_preview();
        self.update_count();
        if t0.elapsed().as_millis() > 50 {
            log!("search {q:?}: {n} rows in {:?}", t0.elapsed());
        }
    }

    /// Compact rows while browsing folders, the chosen row size otherwise.
    pub fn apply_row_height(&self) {
        let h = if self.in_folders() { self.folder_row_height() } else { self.settings.borrow().row_height };
        let t = &self.ui().table;
        if t.rowHeight() != h {
            t.setRowHeight(h);
        }
    }

    fn update_count(&self) {
        let ui = self.ui();
        let n = self.ids.borrow().len();
        let filtered = ui.search.stringValue().length() > 0
            || self.settings.borrow().star_filter
            || self.app_filter.borrow().is_some();
        let label = if self.in_folders() {
            format!("{n} item{}", if n == 1 { "" } else { "s" })
        } else if self.settings.borrow().star_filter {
            format!("{n} starred only")
        } else if filtered {
            format!("{n} found")
        } else {
            format!("{n}")
        };
        ui.count_label.setStringValue(&NSString::from_str(&label));
    }

    fn select_row(&self, row: isize) {
        let t = &self.ui().table;
        let n = self.ids.borrow().len() as isize;
        if n == 0 {
            return;
        }
        let row = row.clamp(0, n - 1);
        t.selectRowIndexes_byExtendingSelection(&NSIndexSet::indexSetWithIndex(row as usize), false);
        t.scrollRowToVisible(row);
    }

    fn move_selection(&self, delta: isize) {
        let cur = self.ui().table.selectedRow().max(0);
        self.select_row(cur + delta);
    }

    pub fn selection_changed(&self) {
        // Rows draw their selected look themselves; don't count on
        // setBackgroundStyle: to make them redraw.
        let t = &self.ui().table;
        let vis = t.rowsInRect(t.visibleRect());
        for r in vis.location..vis.location + vis.length {
            if let Some(v) = t.viewAtColumn_row_makeIfNecessary(0, r as isize, false) {
                v.setNeedsDisplay(true);
            }
        }
        self.update_preview();
    }

    fn reload_row_of(&self, id: i64) {
        let pos = self.ids.borrow().iter().position(|x| *x == id);
        if let Some(row) = pos {
            self.ui().table.reloadDataForRowIndexes_columnIndexes(
                &NSIndexSet::indexSetWithIndex(row),
                &NSIndexSet::indexSetWithIndex(0),
            );
        }
    }

    // --------------------------------------------------------------- preview

    pub fn update_preview(&self) {
        let ui = self.ui();
        if let Some(fid) = self.selected_row_id().filter(|id| *id < 0).map(|id| -id) {
            self.set_quicklook(None);
            self.show_folder_preview(fid);
            return;
        }
        let Some(id) = self.selected_id() else {
            self.set_quicklook(None);
            ui.image_view.setImage(None);
            ui.image_view.setHidden(true);
            ui.text_scroll.setHidden(false);
            set_text(&ui.text_view, "", false, self.settings.borrow().font_size);
            ui.meta.setStringValue(&NSString::from_str(""));
            ui.date_label.setStringValue(&NSString::from_str(""));
            self.set_tags_field(None, &[]);
            self.set_folders_field(None, &[]);
            if let Some(path) = self.folder_label() {
                ui.date_label.setStringValue(&NSString::from_str(&path));
            }
            return;
        };
        let Ok(Some(d)) = self.db.detail(id) else { return };
        self.set_tags_field(Some(id), &d.tags);
        self.set_folders_field(Some(id), &d.folders);
        ui.date_label.setStringValue(&NSString::from_str(&timefmt::human(d.copied_at)));
        if !self.settings.borrow().preview_open {
            self.set_quicklook(None);
            return; // filled in when the pane opens
        }
        ui.meta.setStringValue(&NSString::from_str(&meta_text(&d)));
        if d.kind == Kind::File && !d.sealed() && self.show_file_preview(&d) {
            return;
        }
        self.set_quicklook(None);
        let mut shown_image = false;
        if d.kind == Kind::Image && !d.sealed() {
            let types = self.db.rep_types(id).unwrap_or_default();
            let uti = crate::ingest::IMAGE_TYPES.iter().find(|t| types.iter().any(|x| x == *t));
            if let Some(data) = uti.and_then(|u| self.db.rep(id, u).ok().flatten()) {
                if let Some(img) = NSImage::initWithData(NSImage::alloc(), &NSData::from_vec(data)) {
                    ui.image_view.setImage(Some(&img));
                    shown_image = true;
                }
            }
        }
        ui.image_view.setHidden(!shown_image);
        ui.text_scroll.setHidden(shown_image);
        if !shown_image {
            ui.image_view.setImage(None);
            self.show_text_preview(id, &d);
        }
    }

    fn show_text_preview(&self, id: i64, d: &Detail) {
        let tv = &self.ui().text_view;
        if d.sealed() {
            let msg = "🔒  Password, stored encrypted.\n\n⏎ copies it. Remove the “password” tag (⌘T) to store it in the clear again.";
            set_text(tv, msg, false, self.settings.borrow().font_size);
            return;
        }
        if matches!(d.kind, Kind::Text | Kind::Url) {
            if let Ok(Some(rtf)) = self.db.rep(id, "public.rtf") {
                if rtf.len() < 2 * 1024 * 1024 {
                    let data = NSData::from_vec(rtf);
                    let s = unsafe {
                        NSAttributedString::initWithRTF_documentAttributes(NSAttributedString::alloc(), &data, None)
                    };
                    if let Some(s) = s {
                        let m = NSMutableAttributedString::initWithAttributedString(NSMutableAttributedString::alloc(), &s);
                        neutral_colors(&m);
                        if let Some(storage) = unsafe { tv.textStorage() } {
                            storage.setAttributedString(&m);
                        }
                        tv.setDrawsBackground(false);
                        tv.scrollRangeToVisible(NSRange::new(0, 0));
                        return;
                    }
                }
            }
        }
        let mono = d.app_bundle.as_deref().is_some_and(is_code_app);
        let text = match d.kind {
            Kind::Other => format!("Data types:\n{}", d.types.replace(',', "\n")),
            _ => d.text.clone().unwrap_or_default(),
        };
        let shown: String = text.chars().take(1_000_000).collect();
        set_text(tv, &shown, mono, self.settings.borrow().font_size);
    }

    // ----------------------------------------------------------------- show

    /// The app focus goes back to when the history closes (unless it is us).
    pub fn remember_front_app(&self) {
        if let Some(front) = NSWorkspace::sharedWorkspace().frontmostApplication() {
            let bid = front.bundleIdentifier().map(|s| s.to_string());
            if bid.is_none() || bid != self.own_bundle {
                *self.prev_app.borrow_mut() = Some(front);
            }
        }
    }

    pub fn show(&self) {
        let ui = self.ui();
        self.commit_tag_edit();
        self.remember_front_app();
        *self.app_filter.borrow_mut() = None;
        self.rebuild_app_popup();
        ui.search.setStringValue(&NSString::from_str(""));
        self.run_search(true);
        self.position_panel();
        ui.panel.makeKeyAndOrderFront(None);
        ui.panel.makeFirstResponder(Some(&ui.table));
    }

    fn position_panel(&self) {
        let ui = self.ui();
        let panel = &ui.panel;
        let mut frame = panel.frame();
        let open_at = self.settings.borrow().open_at.clone();
        let remembered = NSUserDefaults::standardUserDefaults()
            .objectForKey(&NSString::from_str(&format!("NSWindow Frame {PANEL_AUTOSAVE}")))
            .is_some();
        let top_left: Option<NSPoint> = match open_at.as_str() {
            "mouse" => Some(NSEvent::mouseLocation()),
            "menubar" => self.status_anchor(),
            _ if !remembered => self.status_anchor(),
            _ => None,
        };
        if let Some(p) = top_left {
            frame.origin.x = p.x;
            frame.origin.y = p.y - frame.size.height;
        }
        // Keep it on a screen (monitors come and go).
        let screens = NSScreen::screens(self.mtm);
        let on_screen = screens.iter().find(|s| {
            let v = s.visibleFrame();
            frame.origin.x < v.origin.x + v.size.width - 40.0
                && frame.origin.x + frame.size.width > v.origin.x + 40.0
                && frame.origin.y < v.origin.y + v.size.height
                && frame.origin.y + frame.size.height > v.origin.y + 40.0
        });
        let screen = on_screen.or_else(|| NSScreen::mainScreen(self.mtm));
        if let Some(s) = screen {
            let v = s.visibleFrame();
            frame.size.width = frame.size.width.min(v.size.width);
            frame.size.height = frame.size.height.min(v.size.height);
            frame.origin.x = frame.origin.x.clamp(v.origin.x, v.origin.x + v.size.width - frame.size.width);
            frame.origin.y = frame.origin.y.clamp(v.origin.y, v.origin.y + v.size.height - frame.size.height);
        }
        panel.setFrame_display(frame, true);
    }

    fn status_anchor(&self) -> Option<NSPoint> {
        let b = self.ui().status_item.button(self.mtm)?;
        let w = b.window()?;
        let f = w.frame();
        Some(NSPoint::new(f.origin.x, f.origin.y - 4.0))
    }

    pub fn hide(&self) {
        self.hide_tag_menu();
        self.commit_tag_edit();
        let p = &self.ui().panel;
        if p.isVisible() {
            p.orderOut(None);
            self.set_quicklook(None);
            self.last_hide.set(Some(Instant::now()));
            // We may have activated ourselves (alerts): hand focus back unless
            // another of our windows (Preferences) is in use.
            let nsapp = NSApplication::sharedApplication(self.mtm);
            if nsapp.isActive() && !self.pasting.get() && nsapp.keyWindow().is_none() {
                if let Some(prev) = self.prev_app.borrow().as_ref() {
                    prev.activateWithOptions(NSApplicationActivationOptions::empty());
                }
            }
        }
    }

    /// Hide for another of our windows (Preferences) without handing focus
    /// back to the previous app: that activation would race with ours and
    /// could leave Preferences behind the other app.
    pub fn hide_for_window(&self) {
        self.hide_tag_menu();
        self.commit_tag_edit();
        let p = &self.ui().panel;
        if p.isVisible() {
            self.modal.set(true); // no resign-key hide meanwhile
            p.orderOut(None);
            self.set_quicklook(None);
            self.modal.set(false);
            self.last_hide.set(Some(Instant::now()));
        }
    }

    pub fn window_resigned_key(&self, n: &NSNotification) {
        let ui = self.ui();
        let is_panel = n
            .object()
            .is_some_and(|o| std::ptr::eq(&*o as *const AnyObject, &*ui.panel as *const ClipPanel as *const AnyObject));
        if is_panel && !self.settings.borrow().pinned && !self.pasting.get() && !self.modal.get() {
            // Focus went elsewhere on purpose: don't pull the previous app forward.
            self.hide_for_window();
        }
    }

    pub fn escape(&self) {
        let s = &self.ui().search;
        if s.stringValue().length() > 0 {
            s.setStringValue(&NSString::from_str(""));
            self.run_search(true);
            self.ui().panel.makeFirstResponder(Some(&self.ui().table));
        } else {
            self.hide();
        }
    }

    // ---------------------------------------------------------------- paste

    /// ⏎, double-click, ⌘1…9: paste or only copy, per the "⏎ pastes"
    /// preference; `other` (⌘⏎) does the opposite.
    pub fn enter(&self, plain: bool, other: bool) {
        let Some(id) = self.selected_row_id() else { return };
        self.enter_id(id, plain, other);
    }

    fn enter_id(&self, id: i64, plain: bool, other: bool) {
        if id < 0 {
            self.open_folder(-id);
            return;
        }
        let paste = self.settings.borrow().enter_pastes != other;
        self.paste_id(id, plain, paste);
    }

    fn write_clip(&self, id: i64, plain: bool) -> bool {
        let pb = NSPasteboard::generalPasteboard();
        let sealed = self.db.is_sealed(id);
        if sealed && self.db.vault(false).is_none() {
            // Don't wipe the clipboard with nothing.
            self.confirm(
                "Can't decrypt this clip",
                "The encryption key is not available: the Keychain is locked, access was denied, or it is not the key this clip was sealed with.",
                "OK",
            );
            return false;
        }
        // Other clipboard tools (and history apps) skip concealed data.
        let concealed = |item: i64| crate::db::Rep { item, uti: "org.nspasteboard.ConcealedType".into(), data: Vec::new() };
        let cc = if plain {
            match self.db.detail(id).ok().flatten().and_then(|d| d.text) {
                Some(t) if sealed => {
                    let text = crate::db::Rep { item: 0, uti: "public.utf8-plain-text".into(), data: t.into_bytes() };
                    crate::paste::write(&pb, &[text, concealed(0)])
                }
                Some(t) => crate::paste::plain_text(&pb, &t),
                None => return false,
            }
        } else {
            match self.db.reps(id) {
                Ok(mut reps) if !reps.is_empty() => {
                    if sealed {
                        if reps.iter().all(|r| r.data.is_empty()) {
                            beep();
                            return false;
                        }
                        let first = reps[0].item;
                        reps.push(concealed(first));
                    }
                    crate::paste::write(&pb, &reps)
                }
                _ => return false,
            }
        };
        self.last_change.set(cc);
        true
    }

    /// Puts the clip on the clipboard, closes the window (unless pinned) and,
    /// with `paste`, sends ⌘V to the app that was in front.
    fn paste_id(&self, id: i64, plain: bool, paste: bool) {
        if !self.write_clip(id, plain) {
            beep();
            return;
        }
        let move_top = self.settings.borrow().move_to_top;
        let _ = self.db.touch_pasted(id, timefmt::now_ms(), move_top);
        self.invalidate(id);
        self.dirty.set(true);
        if !paste {
            if self.settings.borrow().pinned {
                self.run_search(true);
            } else {
                self.hide();
            }
            return;
        }
        self.pasting.set(true);
        self.hide_tag_menu();
        self.ui().panel.orderOut(None);
        self.set_quicklook(None);
        let nsapp = NSApplication::sharedApplication(self.mtm);
        if nsapp.isActive() {
            if let Some(prev) = self.prev_app.borrow().as_ref() {
                prev.activateWithOptions(NSApplicationActivationOptions::empty());
            }
        }
        unsafe {
            let _: () = msg_send![&*self.handler, performSelector: sel!(postPaste:), withObject: Option::<&AnyObject>::None, afterDelay: 0.12f64];
        }
    }

    pub fn post_paste(&self) {
        self.pasting.set(false);
        if crate::ax::trusted() {
            crate::paste::post_cmd_v();
        } else {
            log!("paste: no accessibility permission; clip left on the clipboard");
            crate::ax::prompt();
        }
        if self.settings.borrow().pinned {
            self.run_search(true);
            self.ui().panel.orderFrontRegardless();
        }
    }

    pub fn copy_selected(&self) {
        if let Some(id) = self.selected_id() {
            if self.write_clip(id, false) {
                self.hide();
            }
        }
    }

    // ------------------------------------------------------------- stars etc

    pub fn toggle_star(&self, id: i64) {
        let starred = self.db.row(id).ok().flatten().is_some_and(|r| r.starred);
        let _ = self.db.set_star(id, !starred);
        self.invalidate(id);
        if self.settings.borrow().star_filter {
            self.run_search(false);
        } else {
            self.reload_row_of(id);
            if self.selected_id() == Some(id) {
                self.update_preview();
            }
        }
    }

    pub fn toggle_star_filter(&self) {
        {
            let mut s = self.settings.borrow_mut();
            s.star_filter = !s.star_filter;
            s.save(&self.db);
        }
        self.apply_buttons();
        self.run_search(true);
    }

    pub fn toggle_pin(&self) {
        {
            let mut s = self.settings.borrow_mut();
            s.pinned = !s.pinned;
            s.save(&self.db);
        }
        self.apply_buttons();
    }

    /// Zooms the whole panel: `zoom`'s bounds are its frame divided by the
    /// interface size, so every bar, row, icon and text draws bigger or smaller.
    /// Its three bars are placed here (AppKit's autoresizing ignores the zoom);
    /// what is inside each bar autoresizes as usual.
    pub fn apply_scale(&self) {
        let Some(ui) = self.ui.get() else { return };
        let Some(content) = ui.panel.contentView() else { return };
        let k = self.settings.borrow().ui_scale / 100.0;
        let frame = content.bounds();
        let (w, h) = (frame.size.width / k, frame.size.height / k);
        ui.zoom.setFrame(frame);
        ui.zoom.setBoundsOrigin(NSPoint::new(0.0, 0.0));
        ui.zoom.setBoundsSize(NSSize::new(w, h));
        let r = |y: f64, hh: f64| NSRect::new(NSPoint::new(0.0, y), NSSize::new(w, hh.max(0.0)));
        ui.top_bar.setFrame(r(h - build::TOP, build::TOP));
        self.layout_top(w);
        ui.split.setFrame(r(build::BOTTOM, h - build::TOP - build::BOTTOM));
        ui.bottom_bar.setFrame(r(0.0, build::BOTTOM));
        // Folded, the window is only the list.
        let min_w = if ui.preview.isHidden() { 300.0 } else { 460.0 };
        ui.panel.setMinSize(NSSize::new(min_w * k, 280.0 * k));
    }

    /// Top bar: close, app filter, star, search, preview, pin. The search
    /// field takes the slack; under ~420 pt the app filter narrows instead of
    /// letting the search field vanish.
    fn layout_top(&self, w: f64) {
        let ui = self.ui();
        let cy = (build::TOP - 24.0) / 2.0;
        let at = |x: f64, width: f64| NSRect::new(NSPoint::new(x, cy), NSSize::new(width.max(0.0), 24.0));
        let right = w - 72.0; // preview and pin buttons
        let popup_w = (right - 38.0 - 4.0 - 28.0 - 6.0 - 100.0).clamp(56.0, 176.0);
        let star_x = 38.0 + popup_w + 4.0;
        let search_x = star_x + 34.0;
        ui.app_popup.setFrame(at(38.0, popup_w));
        ui.star_btn.setFrame(at(star_x, 28.0));
        ui.search.setFrame(at(search_x, right - search_x));
        ui.preview_btn.setFrame(at(w - 66.0, 28.0));
        ui.pin_btn.setFrame(at(w - 36.0, 28.0));
    }

    /// The interface size changed from `old` %: grow or shrink the window by the
    /// same ratio, keeping its top-left corner, then re-zoom its content.
    pub fn rescale_window(&self, old: f64) {
        let ui = self.ui();
        let k = self.settings.borrow().ui_scale / old;
        let mut f = ui.panel.frame();
        let top = f.origin.y + f.size.height;
        f.size.width = (f.size.width * k).round();
        f.size.height = (f.size.height * k).round();
        f.origin.y = top - f.size.height;
        if let Some(vis) = ui.panel.screen().map(|s| s.visibleFrame()) {
            f.size.width = f.size.width.min(vis.size.width);
            f.size.height = f.size.height.min(vis.size.height);
            f.origin.x = f.origin.x.min(vis.origin.x + vis.size.width - f.size.width).max(vis.origin.x);
            f.origin.y = f.origin.y.max(vis.origin.y).min(vis.origin.y + vis.size.height - f.size.height);
        }
        ui.panel.setFrame_display(f, ui.panel.isVisible());
        self.apply_scale();
    }

    /// ⌘+ / ⌘- step the interface size through its stops; ⌘0 goes to 100%.
    pub fn zoom(&self, step: i32) {
        use crate::settings::{UI_SCALES, ui_scale_stop};
        let old = self.settings.borrow().ui_scale;
        let new = match step {
            0 => 100.0,
            _ => {
                let i = ui_scale_stop(old) as i32 + step;
                UI_SCALES[i.clamp(0, UI_SCALES.len() as i32 - 1) as usize]
            }
        };
        if new == old {
            beep();
            return;
        }
        {
            let mut s = self.settings.borrow_mut();
            s.ui_scale = new;
            s.save(&self.db);
        }
        self.rescale_window(old);
        prefs::sync_scale();
    }

    pub fn toggle_preview(&self) {
        let open = self.settings.borrow().preview_open;
        self.set_preview_open(!open);
    }

    pub fn set_preview_open(&self, open: bool) {
        if self.settings.borrow().preview_open == open {
            return; // already so: key repeats do nothing
        }
        if !open {
            self.set_quicklook(None); // stops what it was playing
        }
        {
            let mut s = self.settings.borrow_mut();
            s.preview_open = open;
            s.save(&self.db);
        }
        self.apply_preview(true);
        if open {
            self.update_preview();
        }
    }

    /// Folds or unfolds the preview pane to match the setting. The list keeps
    /// its width: the window shrinks by the pane's width when it folds (the
    /// pane slides under the list) and grows to the right when it opens,
    /// moving left only if it would run off the screen. With `resize` false
    /// (at launch, to repair an autosaved split that disagrees with the
    /// setting) the pane folds or opens inside the window as it is.
    pub fn apply_preview(&self, resize: bool) {
        let ui = self.ui();
        let open = self.settings.borrow().preview_open;
        let defaults = NSUserDefaults::standardUserDefaults();
        let key = NSString::from_str("crabclip.previewWidth");
        let k = self.settings.borrow().ui_scale / 100.0;
        let divider = ui.split.dividerThickness();
        let list_w = ui.list.frame().size.width;
        let mut f = ui.panel.frame();
        if open && ui.preview.isHidden() {
            let saved = defaults.doubleForKey(&key);
            let pane = if saved >= 200.0 { saved } else { list_w.max(320.0) };
            let mut left = list_w;
            if resize {
                f.size.width = ((list_w + divider + pane) * k).round();
                if let Some(vis) = ui.panel.screen().map(|s| s.visibleFrame()) {
                    let over = f.origin.x + f.size.width - (vis.origin.x + vis.size.width);
                    if over > 0.0 {
                        f.origin.x = (f.origin.x - over).max(vis.origin.x);
                        f.size.width = f.size.width.min(vis.size.width);
                    }
                }
                ui.panel.setFrame_display(f, ui.panel.isVisible());
            } else {
                let total = ui.split.frame().size.width;
                left = (total - pane - divider).max(total * 0.48).round();
            }
            ui.preview.setHidden(false);
            self.apply_scale();
            // The window may not have grown (already as wide as the screen):
            // the pane still gets a usable share.
            let total = ui.split.frame().size.width;
            left = left.min(total - divider - pane.min(total * 0.52));
            ui.split.adjustSubviews();
            ui.split.setPosition_ofDividerAtIndex(left, 0);
        } else if !open && !ui.preview.isHidden() {
            let pane = ui.preview.frame().size.width;
            defaults.setDouble_forKey(pane, &key);
            let in_pane = ui.panel.firstResponder().is_some_and(|r| {
                r.downcast_ref::<NSView>().is_some_and(|v| v.isDescendantOf(&ui.preview))
            });
            if in_pane {
                ui.panel.makeFirstResponder(Some(&ui.table));
            }
            ui.preview.setHidden(true);
            if resize {
                // Borderless sideways: the window is the list plus the divider
                // a collapsed split view still keeps at its edge.
                f.size.width = ((list_w + divider) * k).round();
                ui.panel.setFrame_display(f, ui.panel.isVisible());
            }
            self.apply_scale();
            ui.split.adjustSubviews();
        }
        let (tip, color) = if open {
            ("Hide preview (←)", NSColor::secondaryLabelColor())
        } else {
            ("Show preview (→)", NSColor::tertiaryLabelColor())
        };
        ui.preview_btn.setImage(draw::tinted_symbol("sidebar.right", &color, 14.0).as_deref());
        ui.preview_btn.setToolTip(Some(&NSString::from_str(tip)));
    }

    pub fn apply_buttons(&self) {
        let ui = self.ui();
        let (star, pin) = {
            let s = self.settings.borrow();
            (s.star_filter, s.pinned)
        };
        let star_img = if star {
            draw::tinted_symbol("star.fill", &NSColor::systemYellowColor(), 14.0)
        } else {
            draw::tinted_symbol("star", &NSColor::secondaryLabelColor(), 14.0)
        };
        ui.star_btn.setImage(star_img.as_deref());
        ui.star_btn.setToolTip(Some(&NSString::from_str(if star {
            "Showing starred only (⇧⌘S)"
        } else {
            "Show starred only (⇧⌘S)"
        })));
        let pin_img = if pin {
            draw::tinted_symbol("pin.fill", &NSColor::controlAccentColor(), 14.0)
        } else {
            draw::tinted_symbol("pin", &NSColor::secondaryLabelColor(), 14.0)
        };
        ui.pin_btn.setImage(pin_img.as_deref());
        ui.pin_btn.setToolTip(Some(&NSString::from_str(if pin {
            "Pinned: stays open (⌘P)"
        } else {
            "Pin window open (⌘P)"
        })));
        self.apply_row_height();
    }

    /// Deleting hides the clip at once; it is really deleted after a few
    /// seconds unless ⌘Z / the "Undo" pill in the bottom bar brings it back.
    pub fn delete_selected(&self) {
        let Some(id) = self.selected_id() else { return };
        let row = self.ui().table.selectedRow();
        let starred = self.db.row(id).ok().flatten().is_some_and(|r| r.starred);
        let folders = self.db.folders_of(id).unwrap_or_default();
        if starred && !self.confirm("Delete this starred clip?", "It will be removed from the history for good.", "Delete") {
            return;
        }
        if !starred
            && !folders.is_empty()
            && !self.confirm(
                "Delete this snippet?",
                &format!("It will be removed from the history and from {} for good.", folders.join(", ")),
                "Delete",
            )
        {
            return;
        }
        self.finish_delete(); // only one undoable delete at a time
        self.pending_delete.set(Some(id));
        let token = self.delete_token.get() + 1;
        self.delete_token.set(token);
        self.ids.borrow_mut().retain(|x| *x != id);
        self.ui().table.reloadData();
        self.select_row(row);
        self.update_preview();
        self.update_count();
        self.show_undo(true);
        let t = objc2_foundation::NSNumber::new_u64(token);
        unsafe {
            let _: () = msg_send![&*self.handler, performSelector: sel!(deleteTimeout:), withObject: &*t, afterDelay: UNDO_SECONDS];
        }
    }

    pub fn delete_timeout(&self, token: u64) {
        if token == self.delete_token.get() {
            self.finish_delete();
        }
    }

    /// Really delete the clip whose delete can still be undone, if any.
    pub fn finish_delete(&self) {
        if let Some(id) = self.pending_delete.take() {
            if let Err(e) = self.db.delete(id) {
                log!("delete {id}: {e}");
            }
            self.invalidate(id);
            self.show_undo(false);
        }
    }

    pub fn undo_delete(&self) {
        let Some(id) = self.pending_delete.take() else { return };
        self.show_undo(false);
        let sel = self.selected_id();
        self.run_search(false);
        // Back where it was, selected.
        let pos = self.ids.borrow().iter().position(|x| *x == id);
        match pos {
            Some(p) => self.select_row(p as isize),
            None => {
                if let Some(p) = sel.and_then(|s| self.ids.borrow().iter().position(|x| *x == s)) {
                    self.select_row(p as isize);
                }
            }
        }
        self.update_preview();
    }

    fn show_undo(&self, on: bool) {
        let ui = self.ui();
        ui.undo_btn.setHidden(!on);
        ui.date_label.setHidden(on);
    }

    /// Typing in a text field (⌘Z there undoes typing).
    fn editing_text(&self) -> bool {
        let ui = self.ui();
        ui.search.currentEditor().is_some()
            || ui.tags_field.currentEditor().is_some()
            || ui.folders_field.currentEditor().is_some()
    }

    pub fn app_filter_changed(&self) {
        let item = self.ui().app_popup.selectedItem();
        let key = item.and_then(|i| i.representedObject()).and_then(|o| o.downcast::<NSString>().ok());
        *self.app_filter.borrow_mut() = key.map(|k| k.to_string());
        self.run_search(true);
    }

    fn rebuild_app_popup(&self) {
        let pop = &self.ui().app_popup;
        pop.removeAllItems();
        pop.addItemWithTitle(&NSString::from_str("All Applications"));
        if let Some(menu) = pop.menu() {
            menu.addItem(&objc2_app_kit::NSMenuItem::separatorItem(self.mtm));
        }
        for a in self.db.apps().unwrap_or_default() {
            let title = NSString::from_str(&format!("{}  ({})", a.name.trim_start_matches('\u{200e}'), a.count));
            pop.addItemWithTitle(&title);
            if let Some(item) = pop.lastItem() {
                unsafe { item.setRepresentedObject(Some(&NSString::from_str(&a.key))) };
                item.setImage(self.small_app_icon(&a.key, a.path.as_deref()).as_deref());
            }
        }
        pop.selectItemAtIndex(0);
    }

    // ------------------------------------------------------------- keyboard

    /// Cmd-shortcuts anywhere in the panel.
    pub fn key_equivalent(&self, ev: &NSEvent) -> bool {
        let flags = ev.modifierFlags();
        if !flags.contains(NSEventModifierFlags::Command) {
            return false;
        }
        let shift = flags.contains(NSEventModifierFlags::Shift);
        let chars = ev.charactersIgnoringModifiers().map(|s| s.to_string()).unwrap_or_default().to_lowercase();
        let ui = self.ui();
        let table_focused = ui
            .panel
            .firstResponder()
            .is_some_and(|r| std::ptr::eq(&*r as *const _ as *const AnyObject, &*ui.table as *const ClipTable as *const AnyObject));
        let nsapp = NSApplication::sharedApplication(self.mtm);
        let send = |s: Sel| unsafe { nsapp.sendAction_to_from(s, None, Some(&ui.panel)) };
        match chars.as_str() {
            "f" => {
                ui.panel.makeFirstResponder(Some(&ui.search));
                true
            }
            "," => {
                if !ev.isARepeat() {
                    prefs::show();
                }
                true
            }
            "=" | "+" => {
                self.zoom(1);
                true
            }
            "-" | "_" => {
                self.zoom(-1);
                true
            }
            "0" => {
                self.zoom(0);
                true
            }
            "t" => {
                self.focus_tags();
                true
            }
            "l" => {
                self.focus_folders();
                true
            }
            "p" => {
                self.toggle_pin();
                true
            }
            "w" => {
                self.hide();
                true
            }
            "q" => {
                self.quit();
                true
            }
            "s" if shift => {
                self.toggle_star_filter();
                true
            }
            "s" => {
                if let Some(id) = self.selected_id() {
                    self.toggle_star(id);
                }
                true
            }
            "\r" => {
                self.enter(shift, true);
                true
            }
            "\u{7f}" | "\u{8}" if table_focused => {
                self.delete_selected();
                true
            }
            "c" if table_focused => {
                self.copy_selected();
                true
            }
            "c" => send(sel!(copy:)),
            "x" => send(sel!(cut:)),
            "v" => send(sel!(paste:)),
            "a" => send(sel!(selectAll:)),
            "z" if shift => send(sel!(redo:)),
            "z" if self.pending_delete.get().is_some() && !self.editing_text() => {
                self.undo_delete();
                true
            }
            "z" => send(sel!(undo:)),
            d if d.len() == 1 && ("1"..="9").contains(&d) => {
                let n: usize = d.parse().unwrap_or(1);
                let id = self.ids.borrow().get(n - 1).copied();
                if let Some(id) = id {
                    self.enter_id(id, false, false);
                }
                true
            }
            _ => false,
        }
    }

    /// Plain keys while the list has focus.
    pub fn table_key(&self, ev: &NSEvent) -> bool {
        let flags = ev.modifierFlags();
        if flags.intersects(NSEventModifierFlags::Command | NSEventModifierFlags::Control) {
            return false;
        }
        let code = ev.keyCode();
        let ui = self.ui();
        match code {
            36 | 76 => {
                self.enter(flags.contains(NSEventModifierFlags::Shift), false);
                return true;
            }
            53 => {
                self.escape();
                return true;
            }
            51 | 117 => {
                self.delete_selected();
                return true;
            }
            48 => {
                ui.panel.makeFirstResponder(Some(&ui.search));
                return true;
            }
            116 | 121 => {
                self.move_selection(if code == 116 { -10 } else { 10 });
                return true;
            }
            115 | 119 => {
                self.select_row(if code == 115 { 0 } else { isize::MAX / 2 });
                return true;
            }
            123 | 124 => {
                self.arrow(code == 124);
                return true;
            }
            _ => {}
        }
        let chars = ev.characters().map(|s| s.to_string()).unwrap_or_default();
        // Plain letters (s included) start a search: a bare `s` used to flip
        // the starred-only filter, so typing "s…" hid nearly everything.
        match chars.as_str() {
            " " => {
                if let Some(id) = self.selected_id() {
                    self.toggle_star(id);
                }
                true
            }
            c if c.chars().count() == 1 && c.chars().all(|ch| !ch.is_control() && !('\u{f700}'..='\u{f8ff}').contains(&ch)) => {
                // Start typing a search right away.
                ui.panel.makeFirstResponder(Some(&ui.search));
                if let Some(ed) = ui.search.currentEditor() {
                    let len = ui.search.stringValue().length();
                    ed.setSelectedRange(NSRange::new(len, 0));
                    unsafe {
                        let _: () = msg_send![&*ed, insertText: &*NSString::from_str(c)];
                    }
                }
                true
            }
            _ => false,
        }
    }

    pub fn text_changed(&self, _n: &NSNotification) {
        // The search field's own action (sent on every keystroke) runs the query.
    }

    pub fn text_ended(&self, n: &NSNotification) {
        let ui = self.ui();
        if is_obj(n.object().as_deref(), &*ui.search) {
            self.hide_tag_menu();
        }
        if is_obj(n.object().as_deref(), &*ui.tags_field) {
            // Back to the pills once the action (sent first) has saved.
            DispatchQueue::main().exec_async(|| {
                let ui = app().ui();
                if ui.tags_field.currentEditor().is_none() {
                    ui.tags_field.setHidden(true);
                    ui.tag_strip.setHidden(false);
                }
            });
        }
        // The tag field saves through its action (sent on end editing too).
    }

    // ------------------------------------------------------------------ tags

    /// Tags starting with `word`, then containing it; the two standard tags
    /// are always offered.
    pub fn tag_matches(&self, word: &str, max: usize) -> Vec<(String, i64)> {
        let w = crate::db::normalize_tag(word);
        let all = self.db.all_tags().unwrap_or_default();
        let mut out: Vec<(String, i64)> = all.iter().filter(|(t, _)| t.starts_with(&w)).cloned().collect();
        out.extend(all.iter().filter(|(t, _)| !t.starts_with(&w) && t.contains(&w)).cloned());
        for std in [crate::db::PASSWORD, crate::db::URL_TAG] {
            if std.starts_with(&w) && !out.iter().any(|(t, _)| t == std) {
                out.push((std.to_string(), 0));
            }
        }
        out.truncate(max);
        out
    }

    fn set_tags_field(&self, id: Option<i64>, tags: &[String]) {
        let f = &self.ui().tags_field;
        // Don't clobber what is being typed for this clip.
        if f.currentEditor().is_some() && id == Some(self.tags_clip.get()) {
            return;
        }
        self.tags_clip.set(id.unwrap_or(0));
        f.setEnabled(id.is_some());
        self.fill_tags(tags);
    }

    fn fill_tags(&self, tags: &[String]) {
        let f = &self.ui().tags_field;
        let names: Vec<Retained<NSString>> = tags.iter().map(|t| NSString::from_str(t)).collect();
        let arr = objc2_foundation::NSArray::from_retained_slice(&names);
        unsafe { f.setObjectValue(Some(arr.as_ref())) };
        let strip = &self.ui().tag_strip;
        *strip.ivars().tags.borrow_mut() = tags.to_vec();
        strip.setNeedsDisplay(true);
    }

    /// Ends an edit in the tag field (saving it) before the window goes away
    /// or the selection is reset.
    fn commit_tag_edit(&self) {
        let ui = self.ui();
        if ui.tags_field.currentEditor().is_some() || ui.folders_field.currentEditor().is_some() {
            ui.panel.makeFirstResponder(Some(&ui.table));
        }
    }

    pub fn focus_tags(&self) {
        if self.selected_id().is_none() {
            beep();
            return;
        }
        self.set_preview_open(true);
        let ui = self.ui();
        ui.tag_strip.setHidden(true);
        ui.tags_field.setHidden(false);
        ui.panel.makeFirstResponder(Some(&ui.tags_field));
    }

    /// The tag field's action: write its tokens to the clip it shows.
    pub fn save_tags(&self) {
        let id = self.tags_clip.get();
        if id == 0 {
            return;
        }
        let ui = self.ui();
        let vals: Vec<String> = ui
            .tags_field
            .objectValue()
            .and_then(|o| o.downcast::<objc2_foundation::NSArray>().ok())
            .map(|a| a.iter().filter_map(|o| o.downcast::<NSString>().ok().map(|s| s.to_string())).collect())
            .unwrap_or_default();
        let mut want: Vec<String> = vals.iter().map(|t| crate::db::normalize_tag(t)).filter(|t| !t.is_empty()).collect();
        want.sort();
        want.dedup();
        let mut have = self.db.tags(id).unwrap_or_default();
        have.sort();
        if want != have {
            match self.db.set_tags(id, &vals) {
                Ok(tags) => {
                    log!("tags: clip {id} -> {}", tags.join(","));
                    if tags.iter().any(|t| t == crate::db::PASSWORD) && !have.iter().any(|t| t == crate::db::PASSWORD) {
                        let _ = self.jobs.send(Job::Scrub);
                    }
                    self.fill_tags(&tags);
                    self.invalidate(id);
                    self.reload_row_of(id);
                    if self.selected_id() == Some(id) {
                        self.update_preview();
                    }
                }
                Err(e) => {
                    log!("tags: clip {id}: {e}");
                    self.confirm("Couldn't change the tags", &e, "OK");
                    self.set_tags_field(Some(id), &self.db.tags(id).unwrap_or_default());
                }
            }
        }
        // ⏎ in the tag field: done, back to the list.
        let enter = NSApplication::sharedApplication(self.mtm)
            .currentEvent()
            .is_some_and(|e| e.r#type() == NSEventType::KeyDown && matches!(e.keyCode(), 36 | 76));
        if enter {
            ui.panel.makeFirstResponder(Some(&ui.table));
        }
    }

    /// Drop-down of tags matching the word being typed in the search field.
    pub fn update_tag_menu(&self) {
        let ui = self.ui();
        let text = ui.search.stringValue().to_string();
        if text.is_empty() || text.ends_with(char::is_whitespace) || text.starts_with('/') {
            self.hide_tag_menu();
            return;
        }
        let raw_word = text.rsplit(char::is_whitespace).next().unwrap_or("");
        // Slice by the raw word: lowercasing can change byte lengths (İ, K).
        let before = &text[..text.len() - raw_word.len()];
        let word = raw_word.to_lowercase();
        let (prefixed, w) = match word.strip_prefix("tag:") {
            Some(rest) => (true, rest.to_string()),
            None => (false, word.clone()),
        };
        if (!prefixed && (w.is_empty() || w.contains(':') || w.starts_with('-'))) || w.starts_with('"') {
            self.hide_tag_menu();
            return;
        }
        let used = crate::query::parse(before).tags;
        let mut items = self.tag_matches(&w, 8);
        items.retain(|(t, _)| !used.contains(t) && !(prefixed && *t == w));
        items.truncate(6);
        if items.is_empty() {
            self.hide_tag_menu();
            return;
        }
        let k = self.settings.borrow().ui_scale / 100.0;
        let size = draw::pill_menu_size(&items);
        *ui.tag_menu_view.ivars().items.borrow_mut() = items;
        let sf = ui.panel.convertRectToScreen(ui.search.convertRect_toView(ui.search.bounds(), None));
        let frame = NSRect::new(
            NSPoint::new(sf.origin.x, sf.origin.y - size.height * k - 3.0),
            NSSize::new(size.width * k, size.height * k),
        );
        ui.tag_menu.setFrame_display(frame, false);
        ui.tag_menu_view.setFrame(NSRect::new(NSPoint::new(0.0, 0.0), frame.size));
        ui.tag_menu_view.setBoundsSize(size);
        ui.tag_menu_view.setNeedsDisplay(true);
        if !ui.tag_menu.isVisible() {
            unsafe { ui.panel.addChildWindow_ordered(&ui.tag_menu, objc2_app_kit::NSWindowOrderingMode::Above) };
        }
    }

    pub fn tag_menu_visible(&self) -> bool {
        self.ui.get().is_some_and(|ui| ui.tag_menu.isVisible())
    }

    pub fn hide_tag_menu(&self) {
        if let Some(ui) = self.ui.get() {
            if ui.tag_menu.isVisible() {
                ui.panel.removeChildWindow(&ui.tag_menu);
                ui.tag_menu.orderOut(None);
            }
        }
    }

    /// A tag picked from the drop-down replaces the word being typed.
    pub fn accept_tag(&self, name: &str) {
        let ui = self.ui();
        let text = ui.search.stringValue().to_string();
        let word_len = if text.ends_with(char::is_whitespace) {
            0
        } else {
            text.rsplit(char::is_whitespace).next().map(str::len).unwrap_or(0)
        };
        let new = format!("{}tag:{name} ", &text[..text.len() - word_len]);
        ui.search.setStringValue(&NSString::from_str(&new));
        self.hide_tag_menu();
        ui.panel.makeFirstResponder(Some(&ui.search));
        if let Some(ed) = ui.search.currentEditor() {
            ed.setSelectedRange(NSRange::new(ui.search.stringValue().length(), 0));
        }
        self.run_search(true);
    }

    pub fn search_command(&self, control: &NSControl, sel: Sel) -> bool {
        let ui = self.ui();
        if is_obj(Some(control), &*ui.tags_field) || is_obj(Some(control), &*ui.folders_field) {
            // Esc / Tab leave the tag field (ending the edit saves it).
            if sel == sel!(cancelOperation:) || sel == sel!(insertTab:) || sel == sel!(insertBacktab:) {
                ui.panel.makeFirstResponder(Some(&ui.table));
                return true;
            }
            return false;
        }
        if !std::ptr::eq(control as *const NSControl as *const AnyObject, &*ui.search as *const NSSearchField as *const AnyObject) {
            return false;
        }
        if self.tag_menu_visible() {
            if sel == sel!(insertTab:) {
                let first = ui.tag_menu_view.ivars().items.borrow().first().map(|t| t.0.clone());
                if let Some(t) = first {
                    self.accept_tag(&t);
                }
                return true;
            }
            if sel == sel!(cancelOperation:) {
                self.hide_tag_menu();
                return true;
            }
        }
        if (sel == sel!(moveRight:) || sel == sel!(moveLeft:)) && self.in_folders() && self.caret_at_end() {
            let text = ui.search.stringValue().to_string();
            if sel == sel!(moveRight:) && self.open_selected_folder() {
                return true;
            }
            if sel == sel!(moveLeft:) && text.ends_with('/') && self.folder_up() {
                return true;
            }
            return false;
        }
        let shift = NSApplication::sharedApplication(self.mtm)
            .currentEvent()
            .is_some_and(|e| e.modifierFlags().contains(NSEventModifierFlags::Shift));
        if sel == sel!(moveUp:) {
            self.move_selection(-1);
        } else if sel == sel!(moveDown:) {
            self.move_selection(1);
        } else if sel == sel!(pageUp:) || sel == sel!(scrollPageUp:) {
            self.move_selection(-10);
        } else if sel == sel!(pageDown:) || sel == sel!(scrollPageDown:) {
            self.move_selection(10);
        } else if sel == sel!(insertNewline:) {
            self.enter(shift, false);
        } else if sel == sel!(cancelOperation:) {
            self.escape();
        } else if sel == sel!(insertTab:) {
            ui.panel.makeFirstResponder(Some(&ui.table));
        } else {
            return false;
        }
        true
    }

    // ------------------------------------------------------------ gear menu

    /// Modal confirmation. The panel stays up meanwhile and gets the keyboard
    /// back afterwards (closing it then returns focus to the previous app).
    pub fn confirm(&self, title: &str, text: &str, ok: &str) -> bool {
        let nsapp = NSApplication::sharedApplication(self.mtm);
        let panel_up = self.ui().panel.isVisible();
        self.modal.set(true);
        nsapp.activate();
        let alert = NSAlert::new(self.mtm);
        alert.setMessageText(&NSString::from_str(title));
        alert.setInformativeText(&NSString::from_str(text));
        alert.addButtonWithTitle(&NSString::from_str(ok));
        if ok != "OK" {
            alert.addButtonWithTitle(&NSString::from_str("Cancel"));
        }
        let yes = alert.runModal() == NSAlertFirstButtonReturn;
        self.modal.set(false);
        if panel_up {
            self.ui().panel.makeKeyAndOrderFront(None);
        }
        yes
    }

    pub fn clear_history(&self, tag: isize) {
        let (before, starred, what) = match tag {
            d if d > 0 => (
                Some(timefmt::now_ms() - d as i64 * 86_400_000),
                false,
                format!("unstarred clips older than {d} day{}", if d == 1 { "" } else { "s" }),
            ),
            -1 => (None, false, "all unstarred clips".to_string()),
            -2 => (None, true, "EVERYTHING, starred clips included".to_string()),
            _ => return,
        };
        if !self.confirm(&format!("Delete {what}?"), "This cannot be undone.", "Delete") {
            return;
        }
        match self.db.clear(before, starred) {
            Ok(n) => log!("clear: removed {n} ({what})"),
            Err(e) => log!("clear: {e}"),
        }
        let _ = self.jobs.send(Job::Vacuum);
        self.invalidate_all();
        self.run_search(true);
    }

    pub fn show_about(&self) {
        let nsapp = NSApplication::sharedApplication(self.mtm);
        nsapp.activate();
        let count = self.db.count().unwrap_or(0);
        let credits = format!(
            "A forever clipboard history.\n{count} clips · {} on disk\n{}",
            timefmt::bytes(self.db.file_size()),
            Db::default_path().display()
        );
        let key = NSString::from_str("Credits");
        let val = NSAttributedString::from_nsstring(&NSString::from_str(&credits));
        let dict = objc2_foundation::NSDictionary::<NSString, AnyObject>::from_slices(&[&*key], &[&**val as &AnyObject]);
        unsafe {
            let _: () = msg_send![&*nsapp, orderFrontStandardAboutPanelWithOptions: &*dict];
        }
    }

    pub fn quit(&self) {
        log!("quit requested");
        NSApplication::sharedApplication(self.mtm).terminate(None);
    }
}

// ------------------------------------------------------------------ helpers

#[link(name = "AppKit", kind = "framework")]
unsafe extern "C" {
    #[link_name = "NSBeep"]
    fn ns_beep();
}

fn beep() {
    unsafe { ns_beep() }
}

pub fn running_as_agent() -> bool {
    std::env::var("XPC_SERVICE_NAME").is_ok_and(|v| v == AGENT_LABEL)
}

pub fn post_distributed(name: &str) {
    unsafe {
        NSDistributedNotificationCenter::defaultCenter().postNotificationName_object_userInfo_deliverImmediately(
            &NSString::from_str(name),
            None,
            None,
            true,
        );
    }
}

pub fn symbol(name: &str) -> Option<Retained<NSImage>> {
    NSImage::imageWithSystemSymbolName_accessibilityDescription(&NSString::from_str(name), None)
}

/// How a tag reads in the tag field: the password tag gets a lock.
pub fn tag_label(name: &str) -> String {
    if name == crate::db::PASSWORD { format!("🔒 {name}") } else { name.to_string() }
}

fn is_obj<A: ?Sized + objc2::Message, B: ?Sized + objc2::Message>(a: Option<&A>, b: &B) -> bool {
    a.is_some_and(|a| std::ptr::eq(a as *const A as *const AnyObject, b as *const B as *const AnyObject))
}

fn is_code_app(bundle: &str) -> bool {
    let b = bundle.to_lowercase();
    ["iterm", "terminal", "vscode", "zed", "xcode", "ghostty", "warp", "alacritty", "kitty", "sublime", "jetbrains", "cursor", "nova", "bbedit", "neovim"]
        .iter()
        .any(|k| b.contains(k))
}

/// Rich text copied from a dark page carries its page colors (white text on
/// near-black). Keep the formatting but show it like normal text: drop
/// background colors and turn grey/white/black text into the label colour,
/// keeping real colours (links, syntax highlighting).
fn neutral_colors(s: &NSMutableAttributedString) {
    let len = s.length();
    let all = NSRange::new(0, len);
    unsafe { s.removeAttribute_range(NSBackgroundColorAttributeName, all) };
    // Table cells keep their backgrounds on the paragraphs' text blocks.
    let mut i = 0;
    while i < len {
        let mut r = NSRange::new(0, 0);
        let v = unsafe { s.attribute_atIndex_effectiveRange(NSParagraphStyleAttributeName, i, &mut r) };
        if let Some(style) = v.and_then(|v| v.downcast::<objc2_app_kit::NSParagraphStyle>().ok()) {
            for block in style.textBlocks().iter() {
                block.setBackgroundColor(None);
            }
        }
        i = (r.location + r.length).max(i + 1);
    }
    let srgb = objc2_app_kit::NSColorSpace::sRGBColorSpace();
    let label = NSColor::labelColor();
    let mut grey = Vec::new();
    let mut i = 0;
    while i < len {
        let mut r = NSRange::new(0, 0);
        let v = unsafe { s.attribute_atIndex_effectiveRange(NSForegroundColorAttributeName, i, &mut r) };
        let neutral = match v.and_then(|v| v.downcast::<NSColor>().ok()) {
            Some(c) => c.colorUsingColorSpace(&srgb).is_none_or(|c| {
                let (r, g, b) = (c.redComponent(), c.greenComponent(), c.blueComponent());
                r.max(g).max(b) - r.min(g).min(b) < 0.15
            }),
            None => true,
        };
        if neutral {
            grey.push(r);
        }
        i = (r.location + r.length).max(i + 1);
    }
    for r in grey {
        unsafe { s.addAttribute_value_range(NSForegroundColorAttributeName, &label, r) };
    }
}

fn set_text(tv: &NSTextView, text: &str, mono: bool, size: f64) {
    tv.setDrawsBackground(false);
    tv.setString(&NSString::from_str(text));
    let font = if mono {
        NSFont::monospacedSystemFontOfSize_weight(size - 1.0, 0.0)
    } else {
        NSFont::systemFontOfSize(size)
    };
    tv.setFont(Some(&font));
    tv.setTextColor(Some(&NSColor::labelColor()));
    tv.scrollRangeToVisible(NSRange::new(0, 0));
}

fn meta_text(d: &Detail) -> String {
    let mut lines = Vec::new();
    if let Some(t) = &d.title {
        lines.push(format!("Snippet “{t}”"));
    }
    let app = d.app_name.clone().unwrap_or_else(|| "Unknown app".into());
    match &d.window_title {
        Some(t) => lines.push(format!("{} — {}", app.trim_start_matches('\u{200e}'), t)),
        None => lines.push(app.trim_start_matches('\u{200e}').to_string()),
    }
    if let Some(p) = &d.source_path {
        lines.push(format!("File: {p}"));
    }
    if let Some(u) = &d.source_url {
        lines.push(format!("From: {u}"));
    }
    let mut when = format!("Copied {}", timefmt::full(d.copied_at));
    if d.created_at + 1000 < d.copied_at {
        when += &format!(" · first {}", timefmt::full(d.created_at));
    }
    if d.copy_count > 1 {
        when += &format!(" · copied {}×", d.copy_count);
    }
    if d.paste_count > 0 {
        when += &format!(" · pasted {}×", d.paste_count);
    }
    lines.push(when);
    let mut what = match d.kind {
        _ if d.sealed() => "Encrypted".to_string(),
        Kind::Image => format!("Image {}×{} px", d.img_w, d.img_h),
        Kind::Text | Kind::Url => {
            let t = d.text.as_deref().unwrap_or("");
            format!("{} chars · {} lines", t.chars().count(), t.lines().count().max(1))
        }
        Kind::File => files::file_summary(d.text.as_deref().unwrap_or("")),
        Kind::Other => "Other data".into(),
    };
    what += &format!(
        " · {} → {} stored · {} types",
        timefmt::bytes(d.raw_size),
        timefmt::bytes(d.stored_size),
        d.types.split(',').filter(|s| !s.is_empty()).count()
    );
    if d.starred {
        what += " · ★";
    }
    lines.push(what);
    if let Some(o) = d.ocr_text.as_ref().filter(|_| !d.sealed()) {
        let one: String = o.split_whitespace().collect::<Vec<_>>().join(" ");
        let short: String = one.chars().take(240).collect();
        lines.push(format!("Text in image: {short}{}", if one.chars().count() > 240 { "…" } else { "" }));
    }
    lines.join("\n")
}

/// Table delegate helper: a (reused) row view bound to the clip at `row`.
pub(crate) fn row_view_for(tv: &NSTableView, row: isize) -> Retained<NSView> {
    let mtm = MainThreadMarker::new().unwrap();
    let ident = NSString::from_str("ClipRow");
    let reused = unsafe { tv.makeViewWithIdentifier_owner(&ident, None) }.and_then(|v| v.downcast::<RowView>().ok());
    let v = reused.unwrap_or_else(|| {
        let v = RowView::new(mtm, NSRect::ZERO);
        v.setIdentifier(Some(&ident));
        v
    });
    let id = app().ids.borrow().get(row as usize).copied().unwrap_or(0);
    v.ivars().clip.set(id);
    v.setNeedsDisplay(true);
    Retained::into_super(v)
}
