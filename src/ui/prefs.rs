//! Preferences window, built in code. Every control applies immediately.

use super::app;
use crate::hotkeys;
use crate::settings::{Settings, UI_SCALES, ui_scale_stop};
use crate::{log, timefmt};
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{MainThreadMarker, MainThreadOnly, Message, msg_send, sel};
use objc2_app_kit::{
    NSApplication, NSBackingStoreType, NSButton, NSColor, NSControlStateValueOff,
    NSControlStateValueOn, NSFont, NSPopUpButton, NSRunningApplication, NSScreen, NSScrollView, NSSlider, NSTextAlignment, NSTextField,
    NSToolbar, NSToolbarItem, NSView, NSWindow, NSWindowStyleMask, NSWindowToolbarStyle, NSWorkspace,
};
use objc2_foundation::{NSArray, NSNotification, NSPoint, NSRect, NSSize, NSString, NSURL};
use std::cell::RefCell;

const OPEN_AT: &[(&str, &str)] = &[
    ("last", "Last window position"),
    ("menubar", "Under the menu bar icon"),
    ("mouse", "At the mouse pointer"),
];
const ROWS: &[(f64, &str)] = &[(52.0, "Compact"), (72.0, "Normal"), (96.0, "Large"), (128.0, "Huge")];
const FONTS: &[f64] = &[11.0, 12.0, 13.0, 14.0, 15.0, 16.0, 18.0, 20.0, 24.0];
const ZSTD: &[(i32, &str)] = &[(3, "Fast (zstd 3)"), (9, "Balanced (zstd 9)"), (19, "Smallest (zstd 19)")];
const KEEP: &[(u32, &str)] = &[(0, "Forever"), (365, "1 year"), (180, "6 months"), (90, "90 days"), (30, "30 days")];

struct Prefs {
    window: Retained<NSWindow>,
    /// The page of each tab.
    tabs: Vec<Retained<NSView>>,
    hotkey: Retained<NSTextField>,
    folder_hotkey: Retained<NSTextField>,
    prefs_hotkey: Retained<NSTextField>,
    /// Preferences › Snippets: folders and snippets.
    editor: Retained<super::snippets::Editor>,
    open_at: Retained<NSPopUpButton>,
    login: Retained<NSButton>,
    move_top: Retained<NSButton>,
    enter_pastes: Retained<NSButton>,
    ocr: Retained<NSButton>,
    rows: Retained<NSPopUpButton>,
    font: Retained<NSPopUpButton>,
    scale: Retained<NSSlider>,
    scale_label: Retained<NSTextField>,
    img_pct: Retained<NSTextField>,
    img_mp: Retained<NSTextField>,
    zstd: Retained<NSPopUpButton>,
    keep: Retained<NSPopUpButton>,
    ignore: Retained<NSTextField>,
    stats: Retained<NSTextField>,
    ax: Retained<NSTextField>,
}

thread_local! {
    static PREFS: RefCell<Option<Prefs>> = const { RefCell::new(None) };
}

fn ns(s: &str) -> Retained<NSString> {
    NSString::from_str(s)
}

fn rect(x: f64, y: f64, w: f64, h: f64) -> NSRect {
    NSRect::new(NSPoint::new(x, y), NSSize::new(w, h))
}

struct Form<'a> {
    mtm: MainThreadMarker,
    view: &'a NSView,
    y: f64,
    target: &'a AnyObject,
}

impl Form<'_> {
    fn label(&mut self, text: &str) {
        let l = NSTextField::labelWithString(&ns(text), self.mtm);
        l.setFrame(rect(16.0, self.y + 3.0, 196.0, 18.0));
        l.setAlignment(NSTextAlignment::Right);
        self.view.addSubview(&l);
    }

    fn note(&mut self, text: &str) -> Retained<NSTextField> {
        let l = NSTextField::wrappingLabelWithString(&ns(text), self.mtm);
        l.setFrame(rect(222.0, self.y - 2.0, 330.0, 30.0));
        l.setFont(Some(&NSFont::systemFontOfSize(11.0)));
        l.setTextColor(Some(&NSColor::secondaryLabelColor()));
        self.view.addSubview(&l);
        self.y -= 30.0;
        l
    }

    fn popup(&mut self, text: &str, items: &[&str], selected: usize) -> Retained<NSPopUpButton> {
        self.label(text);
        let p = NSPopUpButton::initWithFrame_pullsDown(NSPopUpButton::alloc(self.mtm), rect(220.0, self.y, 230.0, 26.0), false);
        for i in items {
            p.addItemWithTitle(&ns(i));
        }
        p.selectItemAtIndex(selected as isize);
        unsafe {
            p.setTarget(Some(self.target));
            p.setAction(Some(sel!(prefChanged:)));
        }
        self.view.addSubview(&p);
        self.y -= 34.0;
        p
    }

    fn check(&mut self, text: &str, on: bool) -> Retained<NSButton> {
        let b = unsafe {
            NSButton::checkboxWithTitle_target_action(&ns(text), Some(self.target), Some(sel!(prefChanged:)), self.mtm)
        };
        b.setFrame(rect(220.0, self.y, 330.0, 22.0));
        b.setState(if on { NSControlStateValueOn } else { NSControlStateValueOff });
        self.view.addSubview(&b);
        self.y -= 28.0;
        b
    }

    fn field(&mut self, x: f64, w: f64, value: &str) -> Retained<NSTextField> {
        let f = NSTextField::textFieldWithString(&ns(value), self.mtm);
        f.setFrame(rect(x, self.y, w, 24.0));
        unsafe {
            f.setTarget(Some(self.target));
            f.setAction(Some(sel!(prefChanged:)));
        }
        self.view.addSubview(&f);
        f
    }

    fn button(&mut self, x: f64, w: f64, title: &str, tag: isize) -> Retained<NSButton> {
        let b = unsafe { NSButton::buttonWithTitle_target_action(&ns(title), Some(self.target), Some(sel!(prefAction:)), self.mtm) };
        b.setFrame(rect(x, self.y - 3.0, w, 30.0));
        b.setTag(tag);
        self.view.addSubview(&b);
        b
    }

    /// A shortcut field with its Set button (both send `tag`).
    fn hotkey(&mut self, text: &str, value: &str, placeholder: &str, tag: isize) -> Retained<NSTextField> {
        self.label(text);
        let f = self.field(220.0, 316.0, value);
        if !placeholder.is_empty() {
            f.setPlaceholderString(Some(&ns(placeholder)));
        }
        unsafe {
            f.setAction(Some(sel!(prefAction:)));
        }
        f.setTag(tag);
        self.button(540.0, 70.0, "Set", tag);
        self.y -= 30.0;
        f
    }

    fn inline_label(&mut self, x: f64, w: f64, text: &str) {
        let l = NSTextField::labelWithString(&ns(text), self.mtm);
        l.setFrame(rect(x, self.y + 3.0, w, 18.0));
        self.view.addSubview(&l);
    }
}

pub fn show() {
    let a = app();
    let mtm = a.mtm;
    let existing = PREFS.with(|p| p.borrow().as_ref().map(|p| p.window.clone()));
    if let Some(w) = existing {
        // ⌘, again closes them; the gear / menu bar item only brings them up.
        let by_key = NSApplication::sharedApplication(mtm)
            .currentEvent()
            .is_some_and(|e| e.r#type() == objc2_app_kit::NSEventType::KeyDown);
        if by_key && w.isVisible() && w.isKeyWindow() {
            w.close(); // ⌘, again closes them (and brings the history back)
            return;
        }
        fill(&w); // folders may have come and gone
        refresh_status();
        a.remember_front_app();
        a.hide_for_window();
        bring_forward(&w);
        return;
    }
    let window = new_window(mtm);
    window.center();
    window.setDelegate(Some(objc2::runtime::ProtocolObject::from_ref(&*a.handler)));
    refresh_status();
    a.remember_front_app();
    a.hide_for_window();
    bring_forward(&window);
}

fn new_window(mtm: MainThreadMarker) -> Retained<NSWindow> {
    let window = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(
            NSWindow::alloc(mtm),
            rect(0.0, 0.0, 664.0, 600.0),
            NSWindowStyleMask::Titled | NSWindowStyleMask::Closable,
            NSBackingStoreType::Buffered,
            false,
        )
    };
    unsafe { window.setReleasedWhenClosed(false) };
    let tab = TABS[current_tab()];
    let toolbar = NSToolbar::initWithIdentifier(NSToolbar::alloc(mtm), &ns("crabclip.prefs"));
    toolbar.setDelegate(Some(objc2::runtime::ProtocolObject::from_ref(&*app().handler)));
    toolbar.setSelectedItemIdentifier(Some(&ns(tab.0)));
    window.setToolbar(Some(&toolbar));
    window.setToolbarStyle(NSWindowToolbarStyle::Preference);
    window.setTitle(&ns(tab.1));
    fill(&window);
    window
}

/// Debug aid (CRABCLIP_SNAPSHOT_PREFS): the Preferences window drawn far off
/// every screen, never key, captured to `out`.
/// CRABCLIP_SNAPSHOT_TAB=snippets picks the tab.
pub fn snapshot(out: &str) {
    let window = new_window(app().mtm);
    if let Some(i) = std::env::var("CRABCLIP_SNAPSHOT_TAB").ok().and_then(|t| TABS.iter().position(|(id, ..)| *id == t)) {
        show_tab(i);
    }
    // CRABCLIP_SNAPSHOT_ROW: the snippet editor's row selected, folders open.
    if let Some(row) = std::env::var("CRABCLIP_SNAPSHOT_ROW").ok().and_then(|r| r.parse().ok()) {
        if let Some(ed) = PREFS.with(|p| p.borrow().as_ref().map(|p| p.editor.clone())) {
            ed.snapshot_select(row);
        }
    }
    window.setFrameTopLeftPoint(NSPoint::new(-30000.0, -30000.0));
    window.orderFrontRegardless();
    objc2_foundation::NSRunLoop::currentRunLoop().runUntilDate(&objc2_foundation::NSDate::dateWithTimeIntervalSinceNow(0.8));
    let n = window.windowNumber().to_string();
    let _ = std::process::Command::new("/usr/sbin/screencapture").args(["-x", "-o", "-l", &n, out]).status();
    window.orderOut(None);
    log!("preferences snapshot of window {n} written to {out}");
}

/// Build the form into `window` (again when folder shortcut rows come or
/// go: the window grows with them, its top left staying put).
fn fill(window: &NSWindow) {
    // A field being typed in sends its action now, before the form is read.
    window.makeFirstResponder(None);
    save_snippet_edits();
    let a = app();
    let mtm = a.mtm;
    let s = a.settings.borrow().clone();
    let target: &AnyObject = &a.handler;
    let hotkey_note = "e.g. cmd+alt+v, ctrl+shift+space, cmd+shift+KeyC. Several: separate them with commas.";

    // General
    let general = page(mtm);
    let mut f = Form { mtm, view: &general, y: PAGE_TOP, target };
    let hotkey = f.hotkey("Show history with:", &s.hotkey, "", 101);
    let prefs_hotkey = f.hotkey("Show preferences with:", &s.prefs_hotkey, "none: ⌘, in the history", 108);
    f.note(hotkey_note);
    f.label("Behavior:");
    let login = f.check("Launch at login (restarts if it ever crashes)", s.launch_at_login);
    let move_top = f.check("Move pasted clips to the top", s.move_to_top);
    let enter_pastes = f.check("⏎ pastes into the app in front", s.enter_pastes);
    f.note("Off: ⏎ only copies the clip. ⌘⏎ always does the other.");
    f.label("Accessibility:");
    let ax = NSTextField::labelWithString(&ns(""), mtm);
    ax.setFrame(rect(220.0, f.y + 3.0, 200.0, 18.0));
    general.addSubview(&ax);
    f.button(400.0, 160.0, "Open Settings…", 104);
    f.y -= 30.0;
    f.note("Needed to paste into the frontmost app (⏎) and to record window titles and documents.");
    trim(&general, f.y);

    // Appearance
    let appearance = page(mtm);
    let mut f = Form { mtm, view: &appearance, y: PAGE_TOP, target };
    let open_idx = OPEN_AT.iter().position(|(k, _)| *k == s.open_at).unwrap_or(0);
    let open_at = f.popup("Open window at:", &OPEN_AT.iter().map(|(_, t)| *t).collect::<Vec<_>>(), open_idx);
    let row_idx = ROWS.iter().position(|(v, _)| (*v - s.row_height).abs() < 1.0).unwrap_or(1);
    let rows = f.popup("Row size:", &ROWS.iter().map(|(_, t)| *t).collect::<Vec<_>>(), row_idx);
    let font_idx = FONTS.iter().position(|v| (*v - s.font_size).abs() < 0.5).unwrap_or(2);
    let font_titles: Vec<String> = FONTS.iter().map(|v| if *v == 13.0 { format!("{v} pt (default)") } else { format!("{v} pt") }).collect();
    let font = f.popup("Text size:", &font_titles.iter().map(|t| t.as_str()).collect::<Vec<_>>(), font_idx);
    f.label("Interface size:");
    let scale = unsafe {
        NSSlider::sliderWithValue_minValue_maxValue_target_action(
            ui_scale_stop(s.ui_scale) as f64,
            0.0,
            (UI_SCALES.len() - 1) as f64,
            Some(target),
            Some(sel!(prefChanged:)),
            mtm,
        )
    };
    scale.setFrame(rect(220.0, f.y, 230.0, 24.0));
    scale.setNumberOfTickMarks(UI_SCALES.len() as isize);
    scale.setAllowsTickMarkValuesOnly(true);
    scale.setContinuous(false);
    appearance.addSubview(&scale);
    let scale_label = NSTextField::labelWithString(&ns(&format!("{}%", s.ui_scale)), mtm);
    scale_label.setFrame(rect(458.0, f.y + 3.0, 60.0, 18.0));
    appearance.addSubview(&scale_label);
    f.y -= 30.0;
    f.note("Zooms the whole window: text, rows, icons and its size.");
    trim(&appearance, f.y);

    // History
    let history = page(mtm);
    let mut f = Form { mtm, view: &history, y: PAGE_TOP, target };
    let keep_idx = KEEP.iter().position(|(v, _)| *v == s.keep_days).unwrap_or(0);
    let keep = f.popup("Keep history:", &KEEP.iter().map(|(_, t)| *t).collect::<Vec<_>>(), keep_idx);
    f.note("Starred clips and clips in folders are always kept.");
    f.label("Ignore apps (bundle ids):");
    let ignore = f.field(220.0, 330.0, &s.ignore_apps.join(", "));
    f.y -= 30.0;
    f.note("Password-manager copies marked concealed are never recorded.");
    f.label("Search:");
    let ocr = f.check("Recognize text in images (searchable)", s.ocr);
    f.y -= 6.0;
    f.label("Store images larger than:");
    let img_mp = f.field(220.0, 50.0, &format!("{}", s.img_scale_min_mp));
    f.inline_label(276.0, 60.0, "MP at");
    let img_pct = f.field(322.0, 50.0, &format!("{}", s.img_scale_pct));
    f.inline_label(378.0, 140.0, "% of their size");
    f.y -= 30.0;
    f.note("100% keeps every image byte-for-byte (default). Smaller images are always kept as-is; pasting uses what is stored.");
    let zstd_idx = ZSTD.iter().position(|(v, _)| *v == s.zstd_level).unwrap_or(1);
    let zstd = f.popup("Compression:", &ZSTD.iter().map(|(_, t)| *t).collect::<Vec<_>>(), zstd_idx);
    f.label("Database:");
    let stats = NSTextField::labelWithString(&ns(""), mtm);
    stats.setFrame(rect(220.0, f.y + 3.0, 340.0, 18.0));
    history.addSubview(&stats);
    f.y -= 30.0;
    f.button(216.0, 150.0, "Reveal in Finder", 102);
    f.y -= 30.0;
    trim(&history, f.y);

    // Snippets
    let snippets = page(mtm);
    let mut f = Form { mtm, view: &snippets, y: PAGE_TOP, target };
    let folder_hotkey = f.hotkey("Show folders with:", &s.folder_hotkey, "none: type / in the history", 106);
    f.note(hotkey_note);
    let (editor, y) = super::snippets::build(mtm, &snippets, f.y, PAGE_W);
    f.y = y;
    trim(&snippets, f.y);

    // Import
    let import = page(mtm);
    let mut f = Form { mtm, view: &import, y: PAGE_TOP, target };
    f.label("Clipy:");
    f.button(216.0, 190.0, "Import from Clipy…", 105);
    f.y -= 30.0;
    f.note("Its history, its snippet folders and its shortcuts (they replace crabclip's).");
    f.label("Collective:");
    f.button(216.0, 190.0, "Import Collective history", 103);
    f.y -= 30.0;
    f.note("Quit Collective first so it saves its latest clips.");
    f.note("Clips already in crabclip are merged, not duplicated: importing again is safe.");
    trim(&import, f.y);

    let tabs = vec![general, appearance, history, snippets, import];
    set_content(window, &tabs[current_tab()]);
    PREFS.with(|p| {
        *p.borrow_mut() = Some(Prefs {
            window: window.retain(),
            tabs,
            hotkey,
            folder_hotkey,
            prefs_hotkey,
            editor,
            open_at,
            login,
            move_top,
            enter_pastes,
            ocr,
            rows,
            font,
            scale,
            scale_label,
            img_pct,
            img_mp,
            zstd,
            keep,
            ignore,
            stats,
            ax,
        })
    });
    refresh_status();
}

/// Width of the Preferences window; a page is laid out top down from
/// `PAGE_TOP` in room to spare, then trimmed to what it used.
const PAGE_W: f64 = 664.0;
const PAGE_H: f64 = 2000.0;
const PAGE_TOP: f64 = PAGE_H - 36.0;

fn page(mtm: MainThreadMarker) -> Retained<NSView> {
    NSView::initWithFrame(NSView::alloc(mtm), rect(0.0, 0.0, PAGE_W, PAGE_H))
}

/// Cut a page down to what its form used (`y`: where its next row would go).
fn trim(view: &NSView, y: f64) {
    let spare = y + 12.0;
    for v in view.subviews().iter() {
        let o = v.frame().origin;
        v.setFrameOrigin(NSPoint::new(o.x, o.y - spare));
    }
    view.setFrameSize(NSSize::new(PAGE_W, PAGE_H - spare));
}

/// Preferences tabs: toolbar item id, title, SF Symbol.
const TABS: &[(&str, &str, &str)] = &[
    ("general", "General", "gearshape"),
    ("appearance", "Appearance", "paintbrush"),
    ("history", "History", "clock.arrow.circlepath"),
    ("snippets", "Snippets", "folder"),
    ("import", "Import", "square.and.arrow.down"),
];

/// The tab shown, as left last time.
fn current_tab() -> usize {
    app().db.get_setting("prefs_tab").and_then(|t| TABS.iter().position(|(id, ..)| *id == t)).unwrap_or(0)
}

pub fn tab_ids() -> Retained<NSArray<NSString>> {
    let ids: Vec<Retained<NSString>> = TABS.iter().map(|(id, ..)| ns(id)).collect();
    NSArray::from_retained_slice(&ids)
}

pub fn tab_item(id: &str) -> Option<Retained<NSToolbarItem>> {
    let (_, title, symbol) = TABS.iter().find(|(i, ..)| *i == id)?;
    let mtm = app().mtm;
    let item = NSToolbarItem::initWithItemIdentifier(NSToolbarItem::alloc(mtm), &ns(id));
    item.setLabel(&ns(title));
    item.setImage(super::symbol(symbol).as_deref());
    unsafe {
        item.setTarget(Some(&app().handler));
        item.setAction(Some(sel!(prefTab:)));
    }
    Some(item)
}

/// A toolbar tab was clicked: show its page (the window takes its height).
pub fn select_tab(id: &str) {
    if let Some(i) = TABS.iter().position(|(t, ..)| *t == id) {
        let _ = app().db.set_setting("prefs_tab", id);
        show_tab(i);
    }
}

fn show_tab(i: usize) {
    let w = PREFS.with(|p| p.borrow().as_ref().map(|p| (p.window.clone(), p.tabs[i].clone())));
    let Some((w, page)) = w else { return };
    w.makeFirstResponder(None); // a field being typed in applies first
    save_snippet_edits();
    if let Some(tb) = w.toolbar() {
        tb.setSelectedItemIdentifier(Some(&ns(TABS[i].0)));
    }
    w.setTitle(&ns(TABS[i].1));
    set_content(&w, &page);
}

/// Put the form in the window, keeping its top left; on a screen too short
/// for it the form scrolls.
fn set_content(window: &NSWindow, view: &NSView) {
    let mtm = app().mtm;
    let size = view.frame().size;
    let fits = window.screen().or_else(|| NSScreen::mainScreen(mtm)).map(|s| s.visibleFrame().size.height - 60.0).unwrap_or(size.height);
    let top = window.frame().origin.y + window.frame().size.height;
    let content: Retained<NSView> = if size.height <= fits {
        view.retain()
    } else {
        let scroll = NSScrollView::initWithFrame(NSScrollView::alloc(mtm), rect(0.0, 0.0, size.width, fits));
        scroll.setHasVerticalScroller(true);
        scroll.setDrawsBackground(false);
        scroll.setDocumentView(Some(view));
        view.scrollPoint(NSPoint::new(0.0, size.height));
        Retained::into_super(scroll)
    };
    let was_up = window.contentView().is_some();
    let h = content.frame().size.height;
    window.setContentView(Some(&content));
    window.setContentSize(NSSize::new(size.width, h));
    if was_up {
        window.setFrameTopLeftPoint(NSPoint::new(window.frame().origin.x, top));
    }
}

/// A snippet being edited is kept before the editor goes or hides (or the app quits).
pub(super) fn save_snippet_edits() {
    let ed = PREFS.with(|p| p.borrow().as_ref().map(|p| p.editor.clone()));
    if let Some(ed) = ed {
        ed.save_edits();
    }
}

/// The snippet editor's rows anew (after a save added or merged one).
pub(super) fn reload_snippet_editor() {
    if let Some(ed) = PREFS.with(|p| p.borrow().as_ref().map(|p| p.editor.clone())) {
        ed.refresh();
    }
}

/// Front and key even though crabclip is an accessory app that is usually not
/// active (the history panel never activates it).
fn bring_forward(w: &NSWindow) {
    let nsapp = NSApplication::sharedApplication(app().mtm);
    #[allow(deprecated)]
    nsapp.activateIgnoringOtherApps(true);
    w.makeKeyAndOrderFront(None);
    w.orderFrontRegardless();
}

/// Preferences is closing (button, ⌘W or ⌘, again): back to the history.
pub fn closing(n: &NSNotification) {
    let is_prefs = PREFS.with(|p| {
        p.borrow().as_ref().is_some_and(|p| {
            n.object().is_some_and(|o| std::ptr::eq(&*o as *const AnyObject, &*p.window as *const NSWindow as *const AnyObject))
        })
    });
    if is_prefs {
        if let Some(ed) = PREFS.with(|p| p.borrow().as_ref().map(|p| p.editor.clone())) {
            ed.leave();
        }
        dispatch2::DispatchQueue::main().exec_async(|| {
            let a = app();
            if a.ui().panel.isVisible() {
                // Pinned and still up: keep its search and filter.
                a.ui().panel.makeKeyAndOrderFront(None);
            } else {
                a.show();
            }
        });
    }
}

fn refresh_status() {
    let a = app();
    PREFS.with(|p| {
        let p = p.borrow();
        let Some(p) = p.as_ref() else { return };
        p.stats.setStringValue(&ns(&format!(
            "{} clips · {} on disk",
            a.db.count().unwrap_or(0),
            timefmt::bytes(a.db.file_size())
        )));
        let ok = crate::ax::trusted();
        p.ax.setStringValue(&ns(if ok { "✓ Allowed" } else { "✗ Not allowed yet" }));
    });
}

/// The interface size changed elsewhere (⌘+ / ⌘-): move the slider along.
pub fn sync_scale() {
    let pct = app().settings.borrow().ui_scale;
    PREFS.with(|p| {
        if let Some(p) = p.borrow().as_ref() {
            p.scale.setDoubleValue(ui_scale_stop(pct) as f64);
            p.scale_label.setStringValue(&ns(&format!("{pct}%")));
        }
    });
}

fn on(b: &NSButton) -> bool {
    b.state() == NSControlStateValueOn
}

/// Any control changed: read the whole form back into the settings.
pub fn changed(_sender: Option<&AnyObject>) {
    let a = app();
    let (old, new) = PREFS.with(|p| {
        let p = p.borrow();
        let p = p.as_ref()?;
        let old = a.settings.borrow().clone();
        let mut s = old.clone();
        s.open_at = OPEN_AT.get(p.open_at.indexOfSelectedItem().max(0) as usize).map(|x| x.0).unwrap_or("last").into();
        s.row_height = ROWS.get(p.rows.indexOfSelectedItem().max(0) as usize).map(|x| x.0).unwrap_or(72.0);
        s.font_size = FONTS.get(p.font.indexOfSelectedItem().max(0) as usize).copied().unwrap_or(13.0);
        let stop = (p.scale.doubleValue().round().max(0.0) as usize).min(UI_SCALES.len() - 1);
        // Leave a size set outside the stops alone until the slider moves off it.
        if stop != ui_scale_stop(old.ui_scale) {
            s.ui_scale = UI_SCALES[stop];
        }
        p.scale_label.setStringValue(&ns(&format!("{}%", s.ui_scale)));
        s.launch_at_login = on(&p.login);
        s.move_to_top = on(&p.move_top);
        s.enter_pastes = on(&p.enter_pastes);
        s.ocr = on(&p.ocr);
        s.img_scale_pct = p.img_pct.stringValue().to_string().trim().parse::<u32>().unwrap_or(100).clamp(5, 100);
        s.img_scale_min_mp = p.img_mp.stringValue().to_string().trim().parse::<f64>().unwrap_or(12.0).max(0.0);
        p.img_pct.setStringValue(&ns(&s.img_scale_pct.to_string()));
        s.zstd_level = ZSTD.get(p.zstd.indexOfSelectedItem().max(0) as usize).map(|x| x.0).unwrap_or(9);
        s.keep_days = KEEP.get(p.keep.indexOfSelectedItem().max(0) as usize).map(|x| x.0).unwrap_or(0);
        s.ignore_apps = p
            .ignore
            .stringValue()
            .to_string()
            .split(',')
            .map(|x| x.trim().to_string())
            .filter(|x| !x.is_empty())
            .collect();
        Some((old, s))
    })
    .unwrap_or_else(|| {
        let s = a.settings.borrow().clone();
        (s.clone(), s)
    });
    if old == new {
        return;
    }
    if new.keep_days > 0 && new.keep_days != old.keep_days && (old.keep_days == 0 || new.keep_days < old.keep_days) {
        let before = timefmt::now_ms() - new.keep_days as i64 * 86_400_000;
        let n: i64 = a
            .db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM clips WHERE starred=0 AND copied_at < ? AND id NOT IN (SELECT clip_id FROM snippets)",
                [before],
                |r| r.get(0),
            )
            .unwrap_or(0);
        if n > 0
            && !a.confirm(
                &format!("Delete {n} clips older than {} days?", new.keep_days),
                "Clips older than that are removed now and from then on. Starred clips and clips in folders are kept.",
                "Delete",
            )
        {
            PREFS.with(|p| {
                if let Some(p) = p.borrow().as_ref() {
                    let idx = KEEP.iter().position(|(v, _)| *v == old.keep_days).unwrap_or(0);
                    p.keep.selectItemAtIndex(idx as isize);
                }
            });
            return;
        }
    }
    new.save(&a.db);
    *a.settings.borrow_mut() = new.clone();
    log!("prefs: saved");
    if old.launch_at_login != new.launch_at_login {
        if !new.launch_at_login && super::running_as_agent() {
            // Unregistering the agent would kill this very process: start a
            // plain copy that takes over and unregisters it on launch.
            let bundle = objc2_foundation::NSBundle::mainBundle().bundlePath().to_string();
            let _ = std::process::Command::new("/usr/bin/open").args(["-n", &bundle, "--args", "--takeover"]).spawn();
        } else if let Err(e) = crate::login::set_enabled(new.launch_at_login) {
            log!("login item: {e}");
        }
    }
    if old.row_height != new.row_height {
        a.apply_buttons();
        a.invalidate_all();
        a.ui().table.reloadData();
    }
    if old.font_size != new.font_size {
        a.apply_row_height();
        a.ui().table.reloadData();
        a.update_preview();
    }
    if old.ui_scale != new.ui_scale {
        a.rescale_window(old.ui_scale);
    }
    if old.keep_days != new.keep_days {
        let _ = a.jobs.send(crate::worker::Job::Prune);
    }
}

pub fn action(sender: Option<&AnyObject>) {
    let tag: isize = sender.map(|s| unsafe { msg_send![s, tag] }).unwrap_or(0);
    match tag {
        101 | 106 | 108 => {
            let text = PREFS.with(|p| {
                p.borrow().as_ref().map(|p| match tag {
                    101 => p.hotkey.stringValue(),
                    106 => p.folder_hotkey.stringValue(),
                    _ => p.prefs_hotkey.stringValue(),
                })
            });
            let Some(text) = text.map(|t| hotkeys::join(&hotkeys::split(&t.to_string()))) else { return };
            set_hotkeys(&text, |s| match tag {
                101 => s.hotkey = text.clone(),
                106 => s.folder_hotkey = text.clone(),
                _ => s.prefs_hotkey = text.clone(),
            });
        }
        105 => import_clipy(),
        102 => {
            let url = NSURL::fileURLWithPath(&ns(&crate::db::Db::default_path().display().to_string()));
            NSWorkspace::sharedWorkspace().activateFileViewerSelectingURLs(&NSArray::from_retained_slice(&[url]));
        }
        103 => import_collective(),
        104 => {
            crate::ax::prompt();
            if let Some(u) = NSURL::URLWithString(&ns(
                "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility",
            )) {
                NSWorkspace::sharedWorkspace().openURL(&u);
            }
        }
        _ => {}
    }
}

/// Change shortcut settings: checked (syntax, used twice), registered and
/// saved. If one of `typed` can't be had the change is undone and the alert
/// says why. The fields show the settings afterwards either way.
pub(super) fn set_hotkeys(typed: &str, edit: impl FnOnce(&mut Settings)) {
    let a = app();
    let old = a.settings.borrow().clone();
    let mut new = old.clone();
    edit(&mut new);
    let typed = hotkeys::split(typed);
    let res = match (hotkeys::check(&new), hotkeys::check(&old)) {
        // A problem already there (settings from an older version) doesn't
        // block other changes; a new one does.
        (Err(e), Err(was)) if e == was => Ok(()),
        (res, _) => res,
    };
    let res = res.and_then(|()| {
        *a.settings.borrow_mut() = new.clone();
        // One that was already out of reach (another app holds it) stays as it
        // was; only the ones just typed must work.
        match a.register_hotkeys().into_iter().find(|(k, _)| typed.contains(k)) {
            Some((_, e)) => Err(e),
            None => Ok(()),
        }
    });
    match res {
        Ok(()) => {
            if new != old {
                new.save(&a.db);
                log!("prefs: shortcuts saved");
            }
        }
        Err(e) => {
            *a.settings.borrow_mut() = old;
            a.register_hotkeys();
            sync_hotkeys();
            a.confirm("Cannot use that shortcut", &e, "OK");
            return;
        }
    }
    sync_hotkeys();
}

/// The shortcut fields show the settings (as saved, or put back).
fn sync_hotkeys() {
    let s = app().settings.borrow().clone();
    PREFS.with(|p| {
        let p = p.borrow();
        let Some(p) = p.as_ref() else { return };
        p.hotkey.setStringValue(&ns(&s.hotkey));
        p.folder_hotkey.setStringValue(&ns(&s.folder_hotkey));
        p.prefs_hotkey.setStringValue(&ns(&s.prefs_hotkey));
    });
}

/// A shortcut field being typed in applies what it holds now.
fn end_editing() {
    let w = PREFS.with(|p| p.borrow().as_ref().map(|p| p.window.clone()));
    if let Some(w) = w {
        w.makeFirstResponder(None);
    }
}

/// Lay the form out again (rows came or went), once the control that asked
/// for it is done with its action.
fn refill() {
    dispatch2::DispatchQueue::main().exec_async(|| {
        let w = PREFS.with(|p| p.borrow().as_ref().map(|p| p.window.clone()));
        if let Some(w) = w {
            fill(&w);
        }
    });
}

/// The preferences shortcut: brings them up, or closes them if they are in front.
pub fn toggle() {
    let w = PREFS.with(|p| p.borrow().as_ref().map(|p| p.window.clone()));
    match w {
        Some(w) if w.isVisible() && w.isKeyWindow() => w.close(),
        _ => show(),
    }
}

/// Shortcut settings saved by another process (an import) take effect.
/// Typing in progress in their fields is dropped: ending it later would
/// write the old values back.
pub fn reload_hotkeys() {
    let a = app();
    PREFS.with(|p| {
        if let Some(p) = p.borrow().as_ref() {
            for f in [&p.hotkey, &p.folder_hotkey, &p.prefs_hotkey] {
                f.abortEditing();
            }
        }
    });
    let saved = Settings::load(&a.db);
    let changed = {
        let mut s = a.settings.borrow_mut();
        let before = (s.hotkey.clone(), s.folder_hotkey.clone(), s.prefs_hotkey.clone(), s.folder_shortcuts.clone());
        s.hotkey = saved.hotkey;
        s.folder_hotkey = saved.folder_hotkey;
        s.prefs_hotkey = saved.prefs_hotkey;
        s.folder_shortcuts = saved.folder_shortcuts;
        before != (s.hotkey.clone(), s.folder_hotkey.clone(), s.prefs_hotkey.clone(), s.folder_shortcuts.clone())
    };
    if changed {
        log!("prefs: shortcuts reloaded");
        a.register_hotkeys();
        sync_hotkeys();
        refill();
    }
}

/// After importing Clipy's shortcuts: Clipy still running answers the same
/// keys, so offer to quit it.
fn offer_quit_clipy() {
    let a = app();
    let clipy = NSRunningApplication::runningApplicationsWithBundleIdentifier(&ns(crate::clipy::BUNDLE));
    if clipy.is_empty()
        || !a.confirm(
            "Quit Clipy?",
            "crabclip now answers Clipy's shortcuts, and Clipy, still running, answers them too. \
             Quit it (and take it out of your login items) so only crabclip does.",
            "Quit Clipy",
        )
    {
        return;
    }
    for c in clipy.iter() {
        c.terminate();
    }
    // Once it has let go of them, take them again and say if any is missing.
    let Ok(when) = dispatch2::DispatchTime::try_from(std::time::Duration::from_secs(2)) else { return };
    let _ = dispatch2::DispatchQueue::main().after(when, || {
        let failed = app().register_hotkeys();
        if !failed.is_empty() {
            let msg = failed.iter().map(|(_, e)| e.clone()).collect::<Vec<_>>().join("\n");
            app().confirm("Some shortcuts are unavailable", &msg, "OK");
        }
    });
}

/// After a Clipy import: its shortcuts (the summary counts them) are in.
fn clipy_imported(summary: &str) {
    reload_hotkeys();
    if summary.contains(" shortcuts") {
        offer_quit_clipy();
    }
}

/// Runs an importer as a child process (they can load a whole archive in
/// memory, which goes back to the system when it exits), then reports.
fn run_import(args: Vec<String>, title: &'static str, after: fn(&str)) {
    let exe = std::env::current_exe().unwrap_or_default();
    std::thread::spawn(move || {
        let out = std::process::Command::new(exe).args(&args).output();
        let msg = match out {
            Ok(o) => String::from_utf8_lossy(&o.stdout).trim().to_string(),
            Err(e) => format!("failed: {e}"),
        };
        dispatch2::DispatchQueue::main().exec_async(move || {
            let a = app();
            a.invalidate_all();
            a.run_search(true);
            refresh_status();
            a.confirm(title, &msg, "OK");
            after(&msg);
        });
    });
}

fn import_collective() {
    end_editing();
    let paths = crate::collective::default_paths();
    if paths.is_empty() {
        app().confirm("No Collective history found", "Collective's archive was not found in its container.", "OK");
        return;
    }
    if !app().confirm(
        "Import Collective history?",
        "Collective saves its history when it quits: quit Collective first to include its latest clips. Clips already in crabclip are merged, not duplicated.",
        "Import",
    ) {
        return;
    }
    run_import(vec!["--import-collective".into()], "Collective import finished", |_| {});
}

const CLIPY_TEXT: &str = "Its history (every format of each clip), its snippet folders in Clipy's order, \
     and its shortcuts (they replace crabclip's). Clips already in crabclip are merged, not duplicated; \
     you can import again later.";

/// Preferences › Import from Clipy: its database, or a file picked by hand.
fn import_clipy() {
    end_editing(); // what is typed in a shortcut field applies before the import
    let a = app();
    let path = match crate::clipy::default_db(true) {
        Some(p) => p,
        None => {
            let msg = if crate::clipy::legacy_realm().is_some() {
                "This Mac has data from an older Clipy. Open the current Clipy once (it converts its data), \
                 or choose a snippets export (Clipy › Edit Snippets… › Export)."
            } else {
                "Clipy's database wasn't found. Choose its sqlite.db or a snippets export \
                 (Clipy › Edit Snippets… › Export)."
            };
            if !a.confirm("Import from Clipy", msg, "Choose File…") {
                return;
            }
            let panel = objc2_app_kit::NSOpenPanel::openPanel(a.mtm);
            panel.setCanChooseFiles(true);
            panel.setCanChooseDirectories(false);
            panel.setAllowsMultipleSelection(false);
            panel.setMessage(Some(&ns("Clipy's sqlite.db or a snippets .xml export")));
            if panel.runModal() != objc2_app_kit::NSModalResponseOK {
                return;
            }
            let Some(p) = panel.URL().and_then(|u| u.path()) else { return };
            std::path::PathBuf::from(p.to_string())
        }
    };
    if !a.confirm("Import from Clipy?", CLIPY_TEXT, "Import") {
        return;
    }
    run_import(vec!["--import-clipy".into(), path.display().to_string()], "Clipy import finished", clipy_imported);
}

/// Asked once, at a launch that finds Clipy's database.
pub fn offer_clipy() {
    let a = app();
    let Some(path) = crate::clipy::default_db(false) else { return };
    let _ = a.db.set_setting("clipy_asked", "1");
    if a.confirm("Bring in your Clipy history and snippets?", CLIPY_TEXT, "Import") {
        run_import(vec!["--import-clipy".into(), path.display().to_string()], "Clipy import finished", clipy_imported);
    }
}
