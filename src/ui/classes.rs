//! Objective-C classes: the delegate/target object, the panel, the clip table,
//! its custom-drawn row view and the always-emphasized row background.

use super::app;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject, NSObjectProtocol, Sel};
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_app_kit::{
    NSApplication, NSApplicationDelegate, NSBackgroundStyle, NSControl, NSControlTextEditingDelegate,
    NSEvent, NSMenuDelegate, NSPanel, NSSearchFieldDelegate, NSTableRowView, NSTableView,
    NSTableViewDataSource, NSTableViewDelegate, NSTextFieldDelegate, NSTextView, NSView,
    NSWindowDelegate, NSTableColumn, NSTokenField, NSTokenFieldDelegate, NSToolbar, NSToolbarDelegate,
    NSToolbarItem,
};
use objc2_foundation::{NSArray, NSInteger, NSNotification, NSPoint, NSRect, NSSize, NSString};
use std::cell::{Cell, RefCell};

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "CrabclipHandler"]
    pub struct Handler;

    unsafe impl NSObjectProtocol for Handler {}

    unsafe impl NSApplicationDelegate for Handler {
        #[unsafe(method(applicationDidFinishLaunching:))]
        fn did_finish_launching(&self, _n: &NSNotification) {
            super::start(self);
        }

        #[unsafe(method(applicationWillTerminate:))]
        fn will_terminate(&self, _n: &NSNotification) {
            super::prefs::save_snippet_edits();
            app().finish_delete();
        }

        #[unsafe(method(applicationShouldHandleReopen:hasVisibleWindows:))]
        fn should_reopen(&self, _app: &NSApplication, _visible: bool) -> bool {
            app().show();
            false
        }
    }

    unsafe impl NSTableViewDataSource for Handler {
        #[unsafe(method(numberOfRowsInTableView:))]
        fn number_of_rows(&self, _tv: &NSTableView) -> NSInteger {
            app().ids.borrow().len() as NSInteger
        }
    }

    unsafe impl NSTableViewDelegate for Handler {
        #[unsafe(method_id(tableView:viewForTableColumn:row:))]
        fn view_for_row(
            &self,
            tv: &NSTableView,
            _col: Option<&NSTableColumn>,
            row: NSInteger,
        ) -> Option<Retained<NSView>> {
            Some(super::row_view_for(tv, row))
        }

        #[unsafe(method_id(tableView:rowViewForRow:))]
        fn row_bg_for_row(&self, _tv: &NSTableView, _row: NSInteger) -> Option<Retained<NSTableRowView>> {
            let mtm = MainThreadMarker::new().unwrap();
            let v: Retained<RowBg> = unsafe { msg_send![RowBg::alloc(mtm), init] };
            Some(Retained::into_super(v))
        }

        #[unsafe(method(tableViewSelectionDidChange:))]
        fn selection_changed(&self, _n: &NSNotification) {
            app().selection_changed();
        }
    }

    unsafe impl NSControlTextEditingDelegate for Handler {
        #[unsafe(method(controlTextDidChange:))]
        fn control_text_did_change(&self, n: &NSNotification) {
            app().text_changed(n);
        }

        #[unsafe(method(control:textView:doCommandBySelector:))]
        fn do_command(&self, control: &NSControl, _tv: &NSTextView, sel: Sel) -> bool {
            app().search_command(control, sel)
        }

        #[unsafe(method(controlTextDidEndEditing:))]
        fn control_text_did_end_editing(&self, n: &NSNotification) {
            app().text_ended(n);
        }
    }

    unsafe impl NSTextFieldDelegate for Handler {}

    unsafe impl NSTokenFieldDelegate for Handler {
        #[unsafe(method_id(tokenField:displayStringForRepresentedObject:))]
        fn token_display(&self, tf: &NSTokenField, obj: &AnyObject) -> Option<Retained<NSString>> {
            let folders = app().is_folders_field(tf);
            obj.downcast_ref::<NSString>().map(|s| {
                let s = s.to_string();
                NSString::from_str(&if folders { super::folders::folder_label(&s) } else { super::tag_label(&s) })
            })
        }

        #[unsafe(method_id(tokenField:editingStringForRepresentedObject:))]
        fn token_editing(&self, _tf: &NSTokenField, obj: &AnyObject) -> Option<Retained<NSString>> {
            obj.downcast_ref::<NSString>().map(|s| NSString::from_str(&s.to_string()))
        }

        #[unsafe(method_id(tokenField:completionsForSubstring:indexOfToken:indexOfSelectedItem:))]
        fn token_completions(
            &self,
            tf: &NSTokenField,
            sub: &NSString,
            _idx: NSInteger,
            _selected: *mut NSInteger,
        ) -> Option<Retained<NSArray<NSString>>> {
            let a = app();
            let names: Vec<Retained<NSString>> = if a.is_folders_field(tf) {
                a.folder_matches(&sub.to_string(), 12).into_iter().map(|p| NSString::from_str(&p)).collect()
            } else {
                a.tag_matches(&sub.to_string(), 12).into_iter().map(|(t, _)| NSString::from_str(&t)).collect()
            };
            Some(NSArray::from_retained_slice(&names))
        }
    }
    unsafe impl NSSearchFieldDelegate for Handler {}

    // The Preferences window's tabs.
    unsafe impl NSToolbarDelegate for Handler {
        #[unsafe(method_id(toolbar:itemForItemIdentifier:willBeInsertedIntoToolbar:))]
        fn toolbar_item(&self, _tb: &NSToolbar, id: &NSString, _flag: bool) -> Option<Retained<NSToolbarItem>> {
            super::prefs::tab_item(&id.to_string())
        }

        #[unsafe(method_id(toolbarDefaultItemIdentifiers:))]
        fn toolbar_default(&self, _tb: &NSToolbar) -> Retained<NSArray<NSString>> {
            super::prefs::tab_ids()
        }

        #[unsafe(method_id(toolbarAllowedItemIdentifiers:))]
        fn toolbar_allowed(&self, _tb: &NSToolbar) -> Retained<NSArray<NSString>> {
            super::prefs::tab_ids()
        }

        #[unsafe(method_id(toolbarSelectableItemIdentifiers:))]
        fn toolbar_selectable(&self, _tb: &NSToolbar) -> Retained<NSArray<NSString>> {
            super::prefs::tab_ids()
        }
    }

    unsafe impl NSWindowDelegate for Handler {
        #[unsafe(method(windowDidResignKey:))]
        fn window_did_resign_key(&self, n: &NSNotification) {
            app().window_resigned_key(n);
        }

        #[unsafe(method(windowWillClose:))]
        fn window_will_close(&self, n: &NSNotification) {
            super::prefs::closing(n);
        }

        #[unsafe(method(windowDidResize:))]
        fn window_did_resize(&self, _n: &NSNotification) {
            app().apply_scale();
        }
    }

    unsafe impl NSMenuDelegate for Handler {}

    impl Handler {
        #[unsafe(method(tick:))]
        fn tick(&self, _t: Option<&AnyObject>) {
            app().tick();
        }

        #[unsafe(method(statusClicked:))]
        fn status_clicked(&self, _s: Option<&AnyObject>) {
            app().status_clicked();
        }

        #[unsafe(method(closePanel:))]
        fn close_panel(&self, _s: Option<&AnyObject>) {
            app().hide();
        }

        #[unsafe(method(toggleStarFilter:))]
        fn toggle_star_filter(&self, _s: Option<&AnyObject>) {
            app().toggle_star_filter();
        }

        #[unsafe(method(togglePin:))]
        fn toggle_pin(&self, _s: Option<&AnyObject>) {
            app().toggle_pin();
        }

        #[unsafe(method(togglePreview:))]
        fn toggle_preview(&self, _s: Option<&AnyObject>) {
            app().toggle_preview();
        }

        #[unsafe(method(appFilterChanged:))]
        fn app_filter_changed(&self, _s: Option<&AnyObject>) {
            app().app_filter_changed();
        }

        #[unsafe(method(searchChanged:))]
        fn search_changed(&self, _s: Option<&AnyObject>) {
            let a = app();
            a.run_search(true);
            a.update_tag_menu();
        }

        #[unsafe(method(undoDelete:))]
        fn undo_delete(&self, _s: Option<&AnyObject>) {
            app().undo_delete();
        }

        #[unsafe(method(deleteTimeout:))]
        fn delete_timeout(&self, token: Option<&AnyObject>) {
            let t: u64 = token.map(|t| unsafe { msg_send![t, unsignedLongLongValue] }).unwrap_or(0);
            app().delete_timeout(t);
        }

        #[unsafe(method(tagsChanged:))]
        fn tags_changed(&self, _s: Option<&AnyObject>) {
            app().save_tags();
        }

        #[unsafe(method(foldersChanged:))]
        fn folders_changed(&self, _s: Option<&AnyObject>) {
            app().save_folders();
        }

        #[unsafe(method(rowDoubleClicked:))]
        fn row_double_clicked(&self, _s: Option<&AnyObject>) {
            if app().clicked_row() >= 0 {
                app().enter(false, false);
            }
        }

        #[unsafe(method(showPreferences:))]
        fn show_preferences(&self, _s: Option<&AnyObject>) {
            super::prefs::show();
        }

        #[unsafe(method(showAbout:))]
        fn show_about(&self, _s: Option<&AnyObject>) {
            app().show_about();
        }

        #[unsafe(method(clearHistory:))]
        fn clear_history(&self, sender: Option<&AnyObject>) {
            let tag: NSInteger = sender.map(|s| unsafe { msg_send![s, tag] }).unwrap_or(0);
            app().clear_history(tag);
        }

        #[unsafe(method(quit:))]
        fn quit(&self, _s: Option<&AnyObject>) {
            app().quit();
        }

        #[unsafe(method(prefChanged:))]
        fn pref_changed(&self, sender: Option<&AnyObject>) {
            super::prefs::changed(sender);
        }

        #[unsafe(method(prefAction:))]
        fn pref_action(&self, sender: Option<&AnyObject>) {
            super::prefs::action(sender);
        }

        #[unsafe(method(prefTab:))]
        fn pref_tab(&self, sender: Option<&NSToolbarItem>) {
            if let Some(item) = sender {
                super::prefs::select_tab(&item.itemIdentifier().to_string());
            }
        }

        #[unsafe(method(postPaste:))]
        fn post_paste(&self, _s: Option<&AnyObject>) {
            app().post_paste();
        }

        #[unsafe(method(quitFromNotification:))]
        fn quit_from_notification(&self, _n: Option<&AnyObject>) {
            crate::log!("another instance (login agent) takes over");
            app().quit();
        }

        #[unsafe(method(shortcutsFromNotification:))]
        fn shortcuts_from_notification(&self, _n: Option<&AnyObject>) {
            super::prefs::reload_hotkeys();
        }

        #[unsafe(method(showFromNotification:))]
        fn show_from_notification(&self, _n: Option<&AnyObject>) {
            app().show();
        }
    }
);

impl Handler {
    pub fn new(mtm: MainThreadMarker) -> Retained<Self> {
        unsafe { msg_send![Self::alloc(mtm), init] }
    }
}

define_class!(
    #[unsafe(super(NSPanel))]
    #[thread_kind = MainThreadOnly]
    #[name = "CrabclipPanel"]
    pub struct ClipPanel;

    impl ClipPanel {
        #[unsafe(method(canBecomeKeyWindow))]
        fn can_become_key(&self) -> bool {
            true
        }

        #[unsafe(method(canBecomeMainWindow))]
        fn can_become_main(&self) -> bool {
            false
        }

        #[unsafe(method(performKeyEquivalent:))]
        fn perform_key_equivalent(&self, ev: &NSEvent) -> bool {
            app().key_equivalent(ev) || unsafe { msg_send![super(self), performKeyEquivalent: ev] }
        }

        #[unsafe(method(cancelOperation:))]
        fn cancel_operation(&self, _s: Option<&AnyObject>) {
            app().escape();
        }
    }
);

define_class!(
    #[unsafe(super(NSTableView))]
    #[thread_kind = MainThreadOnly]
    #[name = "CrabclipTable"]
    pub struct ClipTable;

    impl ClipTable {
        #[unsafe(method(keyDown:))]
        fn key_down(&self, ev: &NSEvent) {
            if !app().table_key(ev) {
                unsafe {
                    let _: () = msg_send![super(self), keyDown: ev];
                }
            }
        }

        // Separators only between real rows, not ruled down the empty space
        // below the last one.
        #[unsafe(method(drawGridInClipRect:))]
        fn draw_grid(&self, clip: NSRect) {
            let n = self.numberOfRows();
            if n > 0 {
                let last = self.rectOfRow(n - 1);
                let bottom = last.origin.y + last.size.height;
                let mut r = clip;
                r.size.height = (bottom - r.origin.y).min(r.size.height);
                if r.size.height > 0.0 {
                    unsafe {
                        let _: () = msg_send![super(self), drawGridInClipRect: r];
                    }
                }
            }
        }

        #[unsafe(method(copy:))]
        fn copy(&self, _s: Option<&AnyObject>) {
            app().copy_selected();
        }

        #[unsafe(method(validateUserInterfaceItem:))]
        fn validate(&self, _item: &AnyObject) -> bool {
            true
        }
    }
);

pub struct RowIvars {
    pub clip: Cell<i64>,
    pub selected: Cell<bool>,
}

define_class!(
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "CrabclipRowView"]
    #[ivars = RowIvars]
    pub struct RowView;

    impl RowView {
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }

        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _dirty: NSRect) {
            super::draw::row(self);
        }

        // Redraw at the new size instead of stretching the old drawing (the
        // star looked wide after the window was folded or zoomed).
        #[unsafe(method(setFrameSize:))]
        fn set_frame_size(&self, size: NSSize) {
            let old = self.frame().size;
            unsafe {
                let _: () = msg_send![super(self), setFrameSize: size];
            }
            if old != size {
                self.setNeedsDisplay(true);
            }
        }

        #[unsafe(method(setBackgroundStyle:))]
        fn set_background_style(&self, style: NSBackgroundStyle) {
            let sel = style == NSBackgroundStyle::Emphasized;
            if sel != self.ivars().selected.get() {
                self.ivars().selected.set(sel);
                self.setNeedsDisplay(true);
            }
        }

        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, ev: &NSEvent) {
            let p: NSPoint = self.convertPoint_fromView(ev.locationInWindow(), None);
            let r = super::draw::star_rect(self.bounds());
            let inside = p.x >= r.origin.x && p.x <= r.origin.x + r.size.width && p.y >= r.origin.y && p.y <= r.origin.y + r.size.height;
            if inside && !app().in_folders() {
                app().toggle_star(self.ivars().clip.get());
                return;
            }
            unsafe {
                let _: () = msg_send![super(self), mouseDown: ev];
            }
        }
    }
);

impl RowView {
    pub fn new(mtm: MainThreadMarker, frame: NSRect) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(RowIvars { clip: Cell::new(0), selected: Cell::new(false) });
        unsafe { msg_send![super(this), initWithFrame: frame] }
    }
}

pub struct TagStripIvars {
    pub tags: RefCell<Vec<String>>,
}

define_class!(
    /// The clip's tags as pills, over the tag field when it is not being edited.
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "CrabclipTagStrip"]
    #[ivars = TagStripIvars]
    pub struct TagStrip;

    impl TagStrip {
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }

        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _dirty: NSRect) {
            super::draw::tag_strip(self);
        }

        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, _ev: &NSEvent) {
            app().focus_tags();
        }
    }
);

impl TagStrip {
    pub fn new(mtm: MainThreadMarker, frame: NSRect) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(TagStripIvars { tags: RefCell::new(Vec::new()) });
        unsafe { msg_send![super(this), initWithFrame: frame] }
    }
}

pub struct PillMenuIvars {
    pub items: RefCell<Vec<(String, i64)>>,
}

define_class!(
    /// The search field's drop-down of matching tags.
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "CrabclipPillMenu"]
    #[ivars = PillMenuIvars]
    pub struct PillMenu;

    impl PillMenu {
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }

        #[unsafe(method(acceptsFirstMouse:))]
        fn accepts_first_mouse(&self, _ev: Option<&NSEvent>) -> bool {
            true
        }

        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _dirty: NSRect) {
            super::draw::pill_menu(self);
        }

        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, ev: &NSEvent) {
            let p: NSPoint = self.convertPoint_fromView(ev.locationInWindow(), None);
            let name = super::draw::pill_menu_item_at(p.y).and_then(|i| self.ivars().items.borrow().get(i).map(|t| t.0.clone()));
            if let Some(name) = name {
                app().accept_tag(&name);
            }
        }
    }
);

impl PillMenu {
    pub fn new(mtm: MainThreadMarker, frame: NSRect) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(PillMenuIvars { items: RefCell::new(Vec::new()) });
        unsafe { msg_send![super(this), initWithFrame: frame] }
    }
}

define_class!(
    #[unsafe(super(NSTableRowView))]
    #[thread_kind = MainThreadOnly]
    #[name = "CrabclipRowBg"]
    pub struct RowBg;

    impl RowBg {
        // Keep the blue selection even while the search field has focus.
        #[unsafe(method(isEmphasized))]
        fn is_emphasized(&self) -> bool {
            true
        }
    }
);
