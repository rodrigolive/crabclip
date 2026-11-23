//! The snippet editor in Preferences › Snippets, like Clipy's: folders and
//! snippets in an outline, a pane to edit the one selected (a snippet's
//! title and text, a folder's name and shortcut), + Folder, + Snippet, −,
//! and drag and drop to reorder or move them.

use super::app;
use crate::db::{AppInfo, Entry, Rep};
use crate::ingest::Capture;
use crate::{hotkeys, log};
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{DefinedClass, Message, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{
    NSAutoresizingMaskOptions, NSBezelStyle, NSControlTextEditingDelegate, NSBorderType, NSButton, NSColor, NSDragOperation, NSDraggingInfo, NSFont,
    NSImageView, NSOutlineView, NSOutlineViewDataSource, NSOutlineViewDelegate, NSPasteboardItem, NSPasteboardWriting,
    NSScrollView, NSTableCellView, NSTableColumn, NSTableViewDelegate, NSTableViewDataSource, NSTextField, NSTextView,
    NSView,
};
use objc2_foundation::{NSArray, NSIndexSet, NSInteger, NSNotification, NSPoint, NSRect, NSSize, NSString};
use std::cell::{Cell, OnceCell, RefCell};
use std::collections::HashMap;

/// Pasteboard type of an outline row being dragged (its key).
const DRAG_TYPE: &str = "com.rod.crabclip.snippet-item";

// Tags of the editor's controls.
const ADD_FOLDER: isize = 1;
const ADD_SNIPPET: isize = 2;
const REMOVE: isize = 3;
const RENAME: isize = 4;
const SET_SHORTCUT: isize = 5;
const SAVE: isize = 6;
const DROP_GONE: isize = 7;

/// What the right pane shows.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Shown {
    Nothing,
    Folder(i64),
    Snippet { folder: i64, clip: i64 },
    /// A snippet being written, not saved yet.
    New { folder: i64 },
}

/// An outline row: a folder or a snippet in a folder.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Item {
    Folder(i64),
    Snippet { folder: i64, clip: i64 },
}

impl Item {
    fn key(self) -> String {
        match self {
            Item::Folder(id) => format!("f{id}"),
            Item::Snippet { folder, clip } => format!("s{folder}:{clip}"),
        }
    }

    fn parse(key: &str) -> Option<Item> {
        if let Some(id) = key.strip_prefix('f') {
            return id.parse().ok().map(Item::Folder);
        }
        let (f, c) = key.strip_prefix('s')?.split_once(':')?;
        Some(Item::Snippet { folder: f.parse().ok()?, clip: c.parse().ok()? })
    }
}

struct Ui {
    outline: Retained<NSOutlineView>,
    folder_pane: Retained<NSView>,
    folder_name: Retained<NSTextField>,
    folder_hotkey: Retained<NSTextField>,
    folder_info: Retained<NSTextField>,
    snippet_pane: Retained<NSView>,
    snippet_head: Retained<NSTextField>,
    title: Retained<NSTextField>,
    text: Retained<NSTextView>,
    hint: Retained<NSTextField>,
    gone: Retained<NSTextField>,
    gone_btn: Retained<NSButton>,
}

pub struct EditorIvars {
    ui: OnceCell<Ui>,
    /// One object per row key, kept for good: the outline knows rows by identity.
    items: RefCell<HashMap<String, Retained<NSString>>>,
    /// What each folder (0: the top level) holds, in order: folders, then snippets.
    children: RefCell<HashMap<i64, Vec<Item>>>,
    labels: RefCell<HashMap<String, String>>,
    shown: Cell<Shown>,
    /// Reloading the outline: its selection changes aren't the user's.
    reloading: Cell<bool>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "CrabclipSnippetEditor"]
    #[ivars = EditorIvars]
    pub struct Editor;

    unsafe impl NSObjectProtocol for Editor {}

    unsafe impl NSTableViewDataSource for Editor {}

    unsafe impl NSOutlineViewDataSource for Editor {
        #[unsafe(method(outlineView:numberOfChildrenOfItem:))]
        fn number_of_children(&self, _ov: &NSOutlineView, item: Option<&AnyObject>) -> NSInteger {
            self.kids(item).len() as NSInteger
        }

        #[unsafe(method_id(outlineView:child:ofItem:))]
        fn child(&self, _ov: &NSOutlineView, index: NSInteger, item: Option<&AnyObject>) -> Retained<AnyObject> {
            let key = self.kids(item).get(index as usize).map(|i| i.key()).unwrap_or_default();
            Retained::into_super(Retained::into_super(self.object(&key)))
        }

        #[unsafe(method(outlineView:isItemExpandable:))]
        fn expandable(&self, _ov: &NSOutlineView, item: &AnyObject) -> bool {
            matches!(item_of(Some(item)), Some(Item::Folder(id)) if self.ivars().children.borrow().get(&id).is_some_and(|c| !c.is_empty()))
        }

        #[unsafe(method_id(outlineView:pasteboardWriterForItem:))]
        fn writer(&self, _ov: &NSOutlineView, item: &AnyObject) -> Option<Retained<ProtocolObject<dyn NSPasteboardWriting>>> {
            writer_for(item)
        }

        #[unsafe(method(outlineView:validateDrop:proposedItem:proposedChildIndex:))]
        fn validate_drop(
            &self,
            ov: &NSOutlineView,
            info: &ProtocolObject<dyn NSDraggingInfo>,
            item: Option<&AnyObject>,
            index: NSInteger,
        ) -> NSDragOperation {
            match self.drop_target(info, item, index) {
                Some((folder, at)) => {
                    let target = (folder != 0).then(|| self.object(&Item::Folder(folder).key()));
                    unsafe { ov.setDropItem_dropChildIndex(target.as_deref().map(|t| t as &AnyObject), at) };
                    NSDragOperation::Move
                }
                None => NSDragOperation::None,
            }
        }

        #[unsafe(method(outlineView:acceptDrop:item:childIndex:))]
        fn accept_drop(
            &self,
            ov: &NSOutlineView,
            info: &ProtocolObject<dyn NSDraggingInfo>,
            item: Option<&AnyObject>,
            index: NSInteger,
        ) -> bool {
            let _ = ov;
            self.accept(info, item, index)
        }
    }

    unsafe impl NSControlTextEditingDelegate for Editor {}

    unsafe impl NSTableViewDelegate for Editor {}

    unsafe impl NSOutlineViewDelegate for Editor {
        #[unsafe(method_id(outlineView:viewForTableColumn:item:))]
        fn view_for(&self, _ov: &NSOutlineView, _col: Option<&NSTableColumn>, item: &AnyObject) -> Option<Retained<NSView>> {
            self.view_for_item(item)
        }

        #[unsafe(method(outlineViewSelectionDidChange:))]
        fn selection_changed(&self, _n: &NSNotification) {
            if !self.ivars().reloading.get() {
                self.selected();
            }
        }
    }

    impl Editor {
        #[unsafe(method(snippetAction:))]
        fn snippet_action(&self, sender: Option<&AnyObject>) {
            let tag: isize = sender.map(|s| unsafe { msg_send![s, tag] }).unwrap_or(0);
            // Preferences may be laid out again meanwhile (an alert lets the
            // main queue run): this editor lives till the action is done.
            let _keep = self.retain();
            self.action(tag);
        }
    }
);

/// The outline item an object stands for.
fn item_of(obj: Option<&AnyObject>) -> Option<Item> {
    Item::parse(&obj?.downcast_ref::<NSString>()?.to_string())
}

fn writer_for(item: &AnyObject) -> Option<Retained<ProtocolObject<dyn NSPasteboardWriting>>> {
    let key = item.downcast_ref::<NSString>()?.to_string();
    let pb = NSPasteboardItem::new();
    pb.setString_forType(&NSString::from_str(&key), &NSString::from_str(DRAG_TYPE));
    Some(ProtocolObject::from_retained(pb))
}

/// The row being dragged.
fn dragged(info: &ProtocolObject<dyn NSDraggingInfo>) -> Option<Item> {
    let pb = info.draggingPasteboard();
    Item::parse(&pb.stringForType(&NSString::from_str(DRAG_TYPE))?.to_string())
}

fn ns(s: &str) -> Retained<NSString> {
    NSString::from_str(s)
}

fn rect(x: f64, y: f64, w: f64, h: f64) -> NSRect {
    NSRect::new(NSPoint::new(x, y), NSSize::new(w, h))
}

/// A row: folder or document icon and its name.
fn cell(mtm: MainThreadMarker, it: Item, label: &str) -> Retained<NSTableCellView> {
    let v = NSTableCellView::initWithFrame(NSTableCellView::alloc(mtm), rect(0.0, 0.0, 200.0, 20.0));
    let sym = if matches!(it, Item::Folder(_)) { "folder" } else { "doc.text" };
    let img = NSImageView::initWithFrame(NSImageView::alloc(mtm), rect(2.0, 2.0, 16.0, 16.0));
    img.setImage(super::symbol(sym).as_deref());
    img.setContentTintColor(Some(&NSColor::secondaryLabelColor()));
    let tf = NSTextField::labelWithString(&ns(label), mtm);
    tf.setFrame(rect(22.0, 2.0, 176.0, 17.0));
    tf.setAutoresizingMask(NSAutoresizingMaskOptions::ViewWidthSizable);
    tf.setLineBreakMode(objc2_app_kit::NSLineBreakMode::ByTruncatingTail);
    v.addSubview(&img);
    v.addSubview(&tf);
    unsafe {
        v.setImageView(Some(&img));
        v.setTextField(Some(&tf));
    }
    v
}

/// The editor laid out in `view` from `top` down, `w` wide. Returns it and
/// the y below it.
pub fn build(mtm: MainThreadMarker, view: &NSView, top: f64, w: f64) -> (Retained<Editor>, f64) {
    let ed: Retained<Editor> = {
        let this = Editor::alloc(mtm).set_ivars(EditorIvars {
            ui: OnceCell::new(),
            items: RefCell::new(HashMap::new()),
            children: RefCell::new(HashMap::new()),
            labels: RefCell::new(HashMap::new()),
            shown: Cell::new(Shown::Nothing),
            reloading: Cell::new(false),
        });
        unsafe { msg_send![super(this), init] }
    };
    let target: &AnyObject = &ed;
    let button = |x: f64, y: f64, w: f64, title: &str, tag: isize| {
        let b = unsafe { NSButton::buttonWithTitle_target_action(&ns(title), Some(target), Some(sel!(snippetAction:)), mtm) };
        b.setFrame(rect(x, y, w, 30.0));
        b.setTag(tag);
        b
    };
    let label = |text: &str, frame: NSRect, small: bool| {
        let l = if small { NSTextField::wrappingLabelWithString(&ns(text), mtm) } else { NSTextField::labelWithString(&ns(text), mtm) };
        l.setFrame(frame);
        if small {
            l.setFont(Some(&NSFont::systemFontOfSize(11.0)));
            l.setTextColor(Some(&NSColor::secondaryLabelColor()));
        }
        l
    };
    let field = |frame: NSRect, tag: isize| {
        let f = NSTextField::textFieldWithString(&ns(""), mtm);
        f.setFrame(frame);
        f.setTag(tag);
        unsafe {
            f.setTarget(Some(target));
            f.setAction(Some(sel!(snippetAction:)));
        }
        f
    };

    // Buttons over the outline.
    let mut y = top - 4.0;
    for (x, bw, title, tag) in [(12.0, 90.0, "+ Folder", ADD_FOLDER), (104.0, 96.0, "+ Snippet", ADD_SNIPPET), (202.0, 40.0, "−", REMOVE)] {
        view.addSubview(&button(x, y, bw, title, tag));
    }
    y -= 6.0;

    // The outline, on the left.
    let h = 360.0;
    let (lw, rx) = (232.0, 264.0);
    y -= h;
    let scroll = NSScrollView::initWithFrame(NSScrollView::alloc(mtm), rect(16.0, y, lw, h));
    scroll.setHasVerticalScroller(true);
    scroll.setBorderType(NSBorderType::BezelBorder);
    let outline = NSOutlineView::initWithFrame(NSOutlineView::alloc(mtm), rect(0.0, 0.0, lw, h));
    let col = NSTableColumn::initWithIdentifier(NSTableColumn::alloc(mtm), &ns("name"));
    col.setWidth(lw - 24.0);
    outline.addTableColumn(&col);
    unsafe { outline.setOutlineTableColumn(Some(&col)) };
    outline.setHeaderView(None);
    outline.setRowHeight(22.0);
    outline.setAutoresizesOutlineColumn(false);
    unsafe {
        outline.setDataSource(Some(ProtocolObject::from_ref(&*ed)));
        outline.setDelegate(Some(ProtocolObject::from_ref(&*ed)));
    }
    outline.registerForDraggedTypes(&NSArray::from_retained_slice(&[ns(DRAG_TYPE)]));
    outline.setDraggingSourceOperationMask_forLocal(NSDragOperation::Move, true);
    scroll.setDocumentView(Some(&outline));
    view.addSubview(&scroll);

    // The folder pane, on the right.
    let pw = w - rx - 16.0;
    let folder_pane = NSView::initWithFrame(NSView::alloc(mtm), rect(rx, y, pw, h));
    folder_pane.addSubview(&label("Name:", rect(0.0, h - 30.0, 70.0, 18.0), false));
    let folder_name = field(rect(74.0, h - 33.0, pw - 154.0, 24.0), RENAME);
    folder_pane.addSubview(&folder_name);
    folder_pane.addSubview(&button(pw - 78.0, h - 36.0, 78.0, "Rename", RENAME));
    folder_pane.addSubview(&label("Shortcut:", rect(0.0, h - 64.0, 70.0, 18.0), false));
    let folder_hotkey = field(rect(74.0, h - 67.0, pw - 154.0, 24.0), SET_SHORTCUT);
    folder_hotkey.setPlaceholderString(Some(&ns("none, e.g. ctrl+alt+cmd+1")));
    folder_pane.addSubview(&folder_hotkey);
    folder_pane.addSubview(&button(pw - 78.0, h - 70.0, 78.0, "Set", SET_SHORTCUT));
    let folder_info = label("", rect(74.0, h - 150.0, pw - 74.0, 70.0), true);
    folder_pane.addSubview(&folder_info);
    view.addSubview(&folder_pane);

    // The snippet pane.
    let snippet_pane = NSView::initWithFrame(NSView::alloc(mtm), rect(rx, y, pw, h));
    let snippet_head = label("", rect(0.0, h - 18.0, pw, 16.0), true);
    snippet_pane.addSubview(&snippet_head);
    snippet_pane.addSubview(&label("Title:", rect(0.0, h - 46.0, 44.0, 18.0), false));
    let title = field(rect(48.0, h - 49.0, pw - 48.0, 24.0), SAVE);
    title.setPlaceholderString(Some(&ns("none: its first line shows")));
    snippet_pane.addSubview(&title);
    let text_scroll = NSTextView::scrollableTextView(mtm);
    text_scroll.setFrame(rect(0.0, 40.0, pw, h - 98.0));
    text_scroll.setBorderType(NSBorderType::BezelBorder);
    let text: Retained<NSTextView> =
        text_scroll.documentView().and_then(|v| v.downcast::<NSTextView>().ok()).expect("scrollableTextView has a text view");
    text.setRichText(false);
    text.setFont(Some(&NSFont::monospacedSystemFontOfSize_weight(12.0, 0.0)));
    text.setAutomaticQuoteSubstitutionEnabled(false);
    text.setAutomaticDashSubstitutionEnabled(false);
    text.setAutomaticTextReplacementEnabled(false);
    text.setAutomaticSpellingCorrectionEnabled(false);
    snippet_pane.addSubview(&text_scroll);
    snippet_pane.addSubview(&label(
        "− takes it out of this folder: the clip stays in the history.",
        rect(0.0, 2.0, pw - 90.0, 30.0),
        true,
    ));
    let save = button(pw - 80.0, 4.0, 80.0, "Save", SAVE);
    save.setBezelStyle(NSBezelStyle::Push);
    save.setKeyEquivalent(&ns("s"));
    save.setKeyEquivalentModifierMask(objc2_app_kit::NSEventModifierFlags::Command);
    snippet_pane.addSubview(&save);
    view.addSubview(&snippet_pane);

    let hint = label(
        "Select a folder or a snippet to edit it. Drag them to reorder them or to move them to another folder.\n\n\
         In the history, ⌘L files the selected clip in folders (type / there for the list of them), and / in the \
         search browses them.",
        rect(rx, y + h - 110.0, pw, 100.0),
        true,
    );
    view.addSubview(&hint);

    // Shortcuts left behind by folders that are gone.
    y -= 32.0;
    let gone = label("", rect(16.0, y - 2.0, w - 140.0, 30.0), true);
    view.addSubview(&gone);
    let gone_btn = button(w - 116.0, y, 100.0, "Remove", DROP_GONE);
    view.addSubview(&gone_btn);

    let _ = ed.ivars().ui.set(Ui {
        outline,
        folder_pane,
        folder_name,
        folder_hotkey,
        folder_info,
        snippet_pane,
        snippet_head,
        title,
        text,
        hint,
        gone,
        gone_btn,
    });
    ed.reload(None);
    (ed, y - 24.0)
}

impl Editor {
    fn ui(&self) -> &Ui {
        self.ivars().ui.get().expect("snippet editor ui")
    }

    /// The object standing for a row key (always the same one).
    fn object(&self, key: &str) -> Retained<NSString> {
        self.ivars().items.borrow_mut().entry(key.to_string()).or_insert_with(|| ns(key)).clone()
    }

    fn kids(&self, item: Option<&AnyObject>) -> Vec<Item> {
        let folder = match item_of(item) {
            None => 0,
            Some(Item::Folder(id)) => id,
            Some(Item::Snippet { .. }) => return Vec::new(),
        };
        self.ivars().children.borrow().get(&folder).cloned().unwrap_or_default()
    }

    /// Read the folders and snippets again and show them, keeping what was
    /// open, and select `select` (else what was selected, if still there).
    fn reload(&self, select: Option<Item>) {
        let a = app();
        let ui = self.ui();
        let was = select.or_else(|| self.selected_item());
        let open: Vec<String> = self
            .ivars()
            .children
            .borrow()
            .values()
            .flatten()
            .filter(|i| matches!(i, Item::Folder(_)))
            .map(|i| i.key())
            .filter(|k| unsafe { ui.outline.isItemExpanded(Some(&self.object(k))) })
            .collect();
        let mut children: HashMap<i64, Vec<Item>> = HashMap::new();
        let mut labels = HashMap::new();
        for (id, parent, name) in a.db.folder_tree().unwrap_or_default() {
            children.entry(parent).or_default().push(Item::Folder(id));
            labels.insert(Item::Folder(id).key(), name);
        }
        let folders: Vec<i64> = labels.keys().filter_map(|k| match Item::parse(k) {
            Some(Item::Folder(id)) => Some(id),
            _ => None,
        }).collect();
        for folder in folders {
            for e in a.db.folder_entries(folder, "").unwrap_or_default() {
                if let Entry::Snippet { clip_id, title } = e {
                    let it = Item::Snippet { folder, clip: clip_id };
                    children.entry(folder).or_default().push(it);
                    labels.insert(it.key(), title);
                }
            }
        }
        *self.ivars().children.borrow_mut() = children;
        *self.ivars().labels.borrow_mut() = labels;
        self.ivars().reloading.set(true);
        ui.outline.reloadData();
        for k in open {
            unsafe { ui.outline.expandItem(Some(&self.object(&k))) };
        }
        if let Some(it) = was {
            self.select(it);
        }
        self.ivars().reloading.set(false);
        self.selected();
        self.show_gone();
    }

    /// Select a row, opening the folders above it.
    fn select(&self, it: Item) {
        let ui = self.ui();
        let mut chain = Vec::new();
        let mut folder = match it {
            Item::Folder(id) => self.parent_of(id),
            Item::Snippet { folder, .. } => Some(folder),
        };
        while let Some(f) = folder.filter(|f| *f != 0) {
            chain.push(f);
            folder = self.parent_of(f);
        }
        for f in chain.iter().rev() {
            unsafe { ui.outline.expandItem(Some(&self.object(&Item::Folder(*f).key()))) };
        }
        let row = unsafe { ui.outline.rowForItem(Some(&self.object(&it.key()))) };
        if row >= 0 {
            ui.outline.selectRowIndexes_byExtendingSelection(&NSIndexSet::indexSetWithIndex(row as usize), false);
            ui.outline.scrollRowToVisible(row);
        }
    }

    fn parent_of(&self, folder: i64) -> Option<i64> {
        self.ivars().children.borrow().iter().find(|(_, kids)| kids.contains(&Item::Folder(folder))).map(|(p, _)| *p)
    }

    fn selected_item(&self) -> Option<Item> {
        let ui = self.ui();
        let row = ui.outline.selectedRow();
        if row < 0 {
            return None;
        }
        item_of(ui.outline.itemAtRow(row).as_deref())
    }

    /// The folder new things go in: the one selected, or the selected
    /// snippet's (0: the top level).
    fn target_folder(&self) -> i64 {
        match self.ivars().shown.get() {
            Shown::Folder(f) | Shown::Snippet { folder: f, .. } | Shown::New { folder: f } => f,
            Shown::Nothing => 0,
        }
    }

    /// The selection changed: keep what was being edited, show the new one.
    fn selected(&self) {
        if self.save_edits() {
            // A snippet came or went: show the rows anew once this
            // selection change is done.
            dispatch2::DispatchQueue::main().exec_async(super::prefs::reload_snippet_editor);
        }
        let shown = match self.selected_item() {
            Some(Item::Folder(id)) => Shown::Folder(id),
            Some(Item::Snippet { folder, clip }) => Shown::Snippet { folder, clip },
            None => Shown::Nothing,
        };
        self.show(shown);
    }

    fn show(&self, shown: Shown) {
        let a = app();
        let ui = self.ui();
        self.ivars().shown.set(shown);
        ui.folder_pane.setHidden(!matches!(shown, Shown::Folder(_)));
        ui.snippet_pane.setHidden(!matches!(shown, Shown::Snippet { .. } | Shown::New { .. }));
        ui.hint.setHidden(shown != Shown::Nothing);
        match shown {
            Shown::Folder(id) => {
                let path = a.db.folder_path(id).unwrap_or_default();
                let name = path.rsplit('/').next().unwrap_or("").to_string();
                ui.folder_name.setStringValue(&ns(&name));
                ui.folder_hotkey.setStringValue(&ns(&folder_hotkey(&a.settings.borrow(), &path)));
                let kids = self.ivars().children.borrow().get(&id).cloned().unwrap_or_default();
                let nf = kids.iter().filter(|k| matches!(k, Item::Folder(_))).count();
                let count = |n: usize, one: &str, many: &str| format!("{n} {}", if n == 1 { one } else { many });
                ui.folder_info.setStringValue(&ns(&format!(
                    "{} · {} in /{path}.\nThe shortcut opens the history right in it.\n− deletes the folder and the folders in it; their clips stay in the history.",
                    count(nf, "folder", "folders"),
                    count(kids.len() - nf, "snippet", "snippets"),
                )));
            }
            Shown::Snippet { folder, clip } if !a.db.is_filed(folder, clip) => self.show(Shown::Nothing),
            Shown::Snippet { folder, clip } => {
                let d = a.db.detail(clip).ok().flatten();
                let sealed = a.db.is_sealed(clip);
                let path = a.db.folder_path(folder).unwrap_or_default();
                let others = d.as_ref().map(|d| d.folders.len().saturating_sub(1)).unwrap_or(0);
                let head = match others {
                    0 => format!("In /{path}"),
                    n => format!("In /{path} and {n} other folder{}", if n == 1 { "" } else { "s" }),
                };
                let why = d.as_ref().and_then(|d| not_editable(d, sealed));
                let rich = d.as_ref().is_some_and(|d| why.is_none() && is_rich(d));
                let head = match (&why, rich) {
                    (Some(w), _) => format!("{head} · {w}"),
                    (None, true) => format!("{head} · editing its text drops its formatting"),
                    _ => head,
                };
                ui.snippet_head.setStringValue(&ns(&head));
                ui.title.setStringValue(&ns(&a.db.snippet_title(folder, clip).unwrap_or_default()));
                let text = if sealed { String::new() } else { d.and_then(|d| d.text).unwrap_or_default() };
                ui.text.setString(&ns(&text));
                ui.text.setEditable(why.is_none());
            }
            Shown::New { folder } => {
                let path = a.db.folder_path(folder).unwrap_or_default();
                ui.snippet_head.setStringValue(&ns(&format!("New snippet in /{path}")));
                ui.title.setStringValue(&ns(""));
                ui.text.setString(&ns(""));
                ui.text.setEditable(true);
            }
            Shown::Nothing => {}
        }
    }

    /// Shortcuts to folders that aren't there anymore, with a button to drop them.
    fn show_gone(&self) {
        let ui = self.ui();
        let gone = gone_shortcuts();
        ui.gone.setHidden(gone.is_empty());
        ui.gone_btn.setHidden(gone.is_empty());
        let list = gone.iter().map(|f| format!("{} ({})", f.folder, hotkeys::labels(&f.hotkey))).collect::<Vec<_>>().join(", ");
        ui.gone.setStringValue(&ns(&format!("Shortcuts for folders no longer here: {list}")));
    }

    /// Keep what was typed in the snippet pane (moving away saves, like
    /// Clipy). True when rows came or went (a new snippet, or one merged
    /// into the clip that already had its text): the outline needs a reload.
    pub fn save_edits(&self) -> bool {
        let a = app();
        let ui = self.ui();
        let title = ui.title.stringValue().to_string().trim().to_string();
        let text = ui.text.string().to_string();
        let level = a.settings.borrow().zstd_level;
        match self.ivars().shown.get() {
            Shown::Snippet { folder, clip } => {
                // Gone meanwhile (deleted in the history): nothing to keep.
                if !a.db.is_filed(folder, clip) {
                    self.ivars().shown.set(Shown::Nothing);
                    return true;
                }
                let it = Item::Snippet { folder, clip };
                if a.db.snippet_title(folder, clip).unwrap_or_default() != title {
                    if let Err(e) = a.db.set_snippet_title(folder, clip, &title) {
                        log!("snippets: title: {e}");
                    }
                    self.relabel(it);
                }
                let Some(d) = a.db.detail(clip).ok().flatten() else { return false };
                if not_editable(&d, a.db.is_sealed(clip)).is_some() {
                    return false;
                }
                let old = d.text.unwrap_or_default();
                if old == text {
                    return false;
                }
                if text.trim().is_empty() {
                    ui.text.setString(&ns(&old));
                    a.confirm("A snippet needs text", "To take it out of its folder use −.", "OK");
                    return false;
                }
                let Some(c) = text_clip(&text, crate::timefmt::now_ms()) else { return false };
                match a.db.set_text(clip, &c, level) {
                    Ok(id) => {
                        log!("snippets: clip {clip} edited");
                        history_changed();
                        if id != clip {
                            self.ivars().shown.set(Shown::Snippet { folder, clip: id });
                            return true;
                        }
                        self.relabel(it); // an untitled snippet shows its text
                        false
                    }
                    Err(e) => {
                        a.confirm("Couldn't save the snippet", &e, "OK");
                        false
                    }
                }
            }
            Shown::New { folder } => {
                if text.trim().is_empty() {
                    return false; // nothing written: nothing to keep
                }
                let ts = a.db.snippet_ts().unwrap_or_else(|_| crate::timefmt::now_ms());
                let Some(c) = text_clip(&text, ts) else { return false };
                // Text already in the history: that clip is filed.
                match a.db.insert_imported(&c, level, false).and_then(|ins| {
                    a.db.file_snippet(folder, ins.id(), &title, None)?;
                    Ok(ins.id())
                }) {
                    Ok(id) => {
                        log!("snippets: new snippet {id} in folder {folder}");
                        self.ivars().shown.set(Shown::Snippet { folder, clip: id });
                        history_changed();
                        true
                    }
                    Err(e) => {
                        a.confirm("Couldn't save the snippet", &e.to_string(), "OK");
                        false
                    }
                }
            }
            _ => false,
        }
    }

    /// Preferences close (or the app quits): keep the edits, and show
    /// nothing, so a later reopening doesn't save this pane again.
    pub fn leave(&self) {
        self.save_edits();
        self.show(Shown::Nothing);
    }

    /// The rows anew (snippets came or went), the selection kept.
    pub fn refresh(&self) {
        self.reload(None);
    }

    /// A row's label changed (a title): show it.
    fn relabel(&self, it: Item) {
        let a = app();
        if let Item::Snippet { folder, clip } = it {
            let label = a
                .db
                .folder_entries(folder, "")
                .unwrap_or_default()
                .into_iter()
                .find_map(|e| match e {
                    Entry::Snippet { clip_id, title } if clip_id == clip => Some(title),
                    _ => None,
                })
                .unwrap_or_default();
            self.ivars().labels.borrow_mut().insert(it.key(), label);
            unsafe { self.ui().outline.reloadItem(Some(&self.object(&it.key()))) };
            history_changed();
        }
    }

    fn action(&self, tag: isize) {
        // A name or shortcut being typed is applied first (its field sends
        // its own action as editing ends), while the pane still shows it.
        if let Some(w) = self.ui().outline.window() {
            w.makeFirstResponder(None);
        }
        match tag {
            ADD_FOLDER => self.add_folder(),
            ADD_SNIPPET => self.add_snippet(),
            REMOVE => self.remove(),
            RENAME => self.rename(),
            SET_SHORTCUT => self.set_shortcut(),
            SAVE => {
                self.save_edits();
                let shown = self.ivars().shown.get();
                if let Shown::Snippet { folder, clip } = shown {
                    self.reload(Some(Item::Snippet { folder, clip }));
                }
            }
            DROP_GONE => {
                let gone = gone_shortcuts();
                super::prefs::set_hotkeys("", |s| s.folder_shortcuts.retain(|f| !gone.contains(f)));
                self.show_gone();
            }
            _ => {}
        }
    }

    fn add_folder(&self) {
        let a = app();
        self.save_edits();
        let parent = match self.ivars().shown.get() {
            Shown::Folder(f) => f,
            Shown::Snippet { folder, .. } | Shown::New { folder } => folder,
            Shown::Nothing => 0,
        };
        let taken: Vec<String> = self
            .ivars()
            .children
            .borrow()
            .get(&parent)
            .map(|k| k.iter().filter_map(|i| self.ivars().labels.borrow().get(&i.key()).map(|l| l.to_lowercase())).collect())
            .unwrap_or_default();
        let name = (1..)
            .map(|n| if n == 1 { "New Folder".to_string() } else { format!("New Folder {n}") })
            .find(|n| !taken.contains(&n.to_lowercase()))
            .unwrap_or_default();
        match a.db.new_folder(parent, &name) {
            Ok(id) => {
                self.ivars().shown.set(Shown::Nothing); // nothing left to save
                self.reload(Some(Item::Folder(id)));
                let ui = self.ui();
                if let Some(w) = ui.folder_name.window() {
                    w.makeFirstResponder(Some(&ui.folder_name));
                }
            }
            Err(e) => {
                a.confirm("Couldn't add a folder", &e, "OK");
            }
        }
    }

    fn add_snippet(&self) {
        let a = app();
        self.save_edits();
        let folder = self.target_folder();
        if folder == 0 {
            a.confirm("Pick a folder first", "Snippets go in folders: select one, or make one with + Folder.", "OK");
            return;
        }
        self.show(Shown::New { folder });
        let ui = self.ui();
        if let Some(w) = ui.title.window() {
            w.makeFirstResponder(Some(&ui.title));
        }
    }

    fn remove(&self) {
        let a = app();
        match self.ivars().shown.get() {
            Shown::Snippet { folder, clip } => {
                let next = self.neighbour(Item::Snippet { folder, clip });
                self.ivars().shown.set(Shown::Nothing); // not saved on the way out
                if let Err(e) = a.db.unfile(folder, clip) {
                    log!("snippets: unfile: {e}");
                }
                history_changed();
                self.reload(next);
            }
            Shown::New { folder } => {
                self.ivars().shown.set(Shown::Nothing);
                self.reload(Some(Item::Folder(folder)));
            }
            Shown::Folder(id) => {
                let path = a.db.folder_path(id).unwrap_or_default();
                let n = a.db.clips_under(id).map(|c| c.len()).unwrap_or(0);
                let what = if n == 0 { String::new() } else { format!(" Its {n} snippet{} stay in the history.", if n == 1 { "" } else { "s" }) };
                if !a.confirm(&format!("Delete the folder {path}?"), &format!("With the folders in it.{what}"), "Delete") {
                    return;
                }
                let next = self.neighbour(Item::Folder(id));
                self.ivars().shown.set(Shown::Nothing);
                if let Err(e) = a.db.delete_folder(id) {
                    log!("snippets: delete folder: {e}");
                }
                super::prefs::set_hotkeys("", |s| {
                    s.folder_deleted(&path);
                });
                history_changed();
                self.reload(next);
            }
            Shown::Nothing => super::beep(),
        }
    }

    /// The row to select once `it` is gone: the next one, else the one before, else its folder.
    fn neighbour(&self, it: Item) -> Option<Item> {
        let parent = match it {
            Item::Folder(id) => self.parent_of(id).unwrap_or(0),
            Item::Snippet { folder, .. } => folder,
        };
        let kids = self.ivars().children.borrow().get(&parent).cloned().unwrap_or_default();
        let i = kids.iter().position(|k| *k == it)?;
        kids.get(i + 1).or(i.checked_sub(1).and_then(|j| kids.get(j))).copied().or((parent != 0).then_some(Item::Folder(parent)))
    }

    fn rename(&self) {
        let a = app();
        let Shown::Folder(id) = self.ivars().shown.get() else { return };
        let name = self.ui().folder_name.stringValue().to_string();
        let old = a.db.folder_path(id).unwrap_or_default();
        if old.rsplit('/').next() == Some(name.trim()) {
            return;
        }
        match a.db.rename_folder(id, &name) {
            Ok(new) => {
                log!("snippets: folder {old} -> {new}");
                super::prefs::set_hotkeys("", |s| {
                    s.folder_renamed(&old, &new);
                });
                history_changed();
                self.reload(Some(Item::Folder(id)));
            }
            Err(e) => {
                self.ui().folder_name.setStringValue(&ns(old.rsplit('/').next().unwrap_or("")));
                a.confirm("Couldn't rename the folder", &e, "OK");
            }
        }
    }

    fn set_shortcut(&self) {
        let a = app();
        let Shown::Folder(id) = self.ivars().shown.get() else { return };
        let path = a.db.folder_path(id).unwrap_or_default();
        let text = hotkeys::join(&hotkeys::split(&self.ui().folder_hotkey.stringValue().to_string()));
        super::prefs::set_hotkeys(&text, |s| {
            // One entry per folder: what was there is replaced.
            s.folder_shortcuts.retain(|f| f.folder.to_lowercase() != path.to_lowercase());
            if !text.is_empty() {
                s.folder_shortcuts.push(crate::settings::FolderShortcut { folder: path.clone(), hotkey: text.clone() });
            }
        });
        self.ui().folder_hotkey.setStringValue(&ns(&folder_hotkey(&a.settings.borrow(), &path)));
        self.show_gone();
    }

    /// Where a drop lands: (folder, index among its folders or its
    /// snippets, as the outline counts children). None: not allowed there.
    fn drop_target(
        &self,
        info: &ProtocolObject<dyn NSDraggingInfo>,
        item: Option<&AnyObject>,
        index: NSInteger,
    ) -> Option<(i64, NSInteger)> {
        let it = dragged(info)?;
        // Onto a snippet: next to it, in its folder.
        let (folder, index) = match item_of(item) {
            None => (0, index),
            Some(Item::Folder(f)) => (f, index),
            Some(s @ Item::Snippet { folder, .. }) => {
                let at = self.kids_of(folder).iter().position(|k| *k == s).map(|i| i as NSInteger).unwrap_or(-1);
                (folder, at)
            }
        };
        let kids = self.kids_of(folder);
        let nf = kids.iter().filter(|k| matches!(k, Item::Folder(_))).count() as NSInteger;
        let n = kids.len() as NSInteger;
        match it {
            Item::Snippet { .. } => {
                if folder == 0 {
                    return None; // snippets live in folders
                }
                Some((folder, if index < 0 { n } else { index.clamp(nf, n) }))
            }
            Item::Folder(id) => {
                // Not into itself or below it.
                let mut f = folder;
                while f != 0 {
                    if f == id {
                        return None;
                    }
                    f = self.parent_of(f).unwrap_or(0);
                }
                Some((folder, if index < 0 { nf } else { index.min(nf) }))
            }
        }
    }

    fn accept(&self, info: &ProtocolObject<dyn NSDraggingInfo>, item: Option<&AnyObject>, index: NSInteger) -> bool {
        let Some(it) = dragged(info) else { return false };
        let Some((folder, at)) = self.drop_target(info, item, index) else { return false };
        self.dropped(it, folder, at)
    }

    fn view_for_item(&self, item: &AnyObject) -> Option<Retained<NSView>> {
        let it = item_of(Some(item))?;
        let label = self.ivars().labels.borrow().get(&it.key()).cloned().unwrap_or_default();
        Some(Retained::into_super(cell(self.mtm(), it, &label)))
    }

    /// Snapshots: every folder open and row `row` selected.
    pub(super) fn snapshot_select(&self, row: isize) {
        let ui = self.ui();
        unsafe { ui.outline.expandItem_expandChildren(None, true) };
        if row >= 0 {
            ui.outline.selectRowIndexes_byExtendingSelection(&NSIndexSet::indexSetWithIndex(row as usize), false);
        }
    }

    fn kids_of(&self, folder: i64) -> Vec<Item> {
        self.ivars().children.borrow().get(&folder).cloned().unwrap_or_default()
    }

    /// Carry out a drop: `at` counts the outline's children of `folder`.
    fn dropped(&self, it: Item, folder: i64, at: NSInteger) -> bool {
        let a = app();
        if self.save_edits() {
            // Saving changed the rows (the dragged one may be gone): start over.
            self.reload(None);
            return false;
        }
        let kids = self.kids_of(folder);
        let res = match it {
            Item::Snippet { folder: from, clip } => {
                // Counted without the dragged one, as place_snippet does.
                // Counted without the dragged clip, here or already in `folder`.
                let before = kids[..(at.max(0) as usize).min(kids.len())]
                    .iter()
                    .filter(|k| matches!(k, Item::Snippet { clip: c, .. } if *c != clip))
                    .count();
                a.db.place_snippet(clip, from, folder, before).map(|()| Item::Snippet { folder, clip }).map_err(|e| e.to_string())
            }
            Item::Folder(id) => {
                let old = a.db.folder_path(id).unwrap_or_default();
                let before = kids[..(at.max(0) as usize).min(kids.len())]
                    .iter()
                    .filter(|k| matches!(k, Item::Folder(_)) && **k != it)
                    .count();
                a.db.place_folder(id, folder, before).map(|new| {
                    if new != old {
                        super::prefs::set_hotkeys("", |s| {
                            s.folder_renamed(&old, &new);
                        });
                    }
                    Item::Folder(id)
                })
            }
        };
        match res {
            Ok(moved) => {
                self.ivars().shown.set(Shown::Nothing);
                history_changed();
                self.reload(Some(moved));
                true
            }
            Err(e) => {
                a.confirm("Couldn't move it there", &e, "OK");
                false
            }
        }
    }
}

/// Why a snippet's text can't be edited here, if it can't.
fn not_editable(d: &crate::db::Detail, sealed: bool) -> Option<&'static str> {
    use crate::db::Kind;
    if sealed {
        Some("encrypted: remove its password tag to edit it")
    } else if !matches!(d.kind, Kind::Text | Kind::Url) {
        Some("not text: only its title can be changed here")
    } else {
        None
    }
}

/// Text with formatting (RTF, HTML) besides the plain text.
fn is_rich(d: &crate::db::Detail) -> bool {
    d.types.split(',').any(|t| matches!(t.trim(), "public.rtf" | "public.html" | "Apple HTML pasteboard type" | "NeXT Rich Text Format v1.0 pasteboard type"))
}

/// A folder's shortcut as set ("" for none).
fn folder_hotkey(s: &crate::settings::Settings, path: &str) -> String {
    let specs: Vec<String> = s
        .folder_shortcuts
        .iter()
        .filter(|f| f.folder.to_lowercase() == path.to_lowercase())
        .flat_map(|f| hotkeys::split(&f.hotkey))
        .collect();
    hotkeys::join(&specs)
}

/// Folder shortcuts whose folder isn't there anymore.
fn gone_shortcuts() -> Vec<crate::settings::FolderShortcut> {
    let a = app();
    let paths: Vec<String> = a.db.all_folder_paths().unwrap_or_default().into_iter().map(|(p, _)| p.to_lowercase()).collect();
    a.settings.borrow().folder_shortcuts.iter().filter(|f| !paths.contains(&f.folder.to_lowercase())).cloned().collect()
}

/// A text clip for a snippet, as written in the editor.
fn text_clip(text: &str, ts: i64) -> Option<crate::db::NewClip> {
    let a = app();
    let cap = Capture {
        ts,
        reps: vec![Rep { item: 0, uti: "public.utf8-plain-text".into(), data: text.as_bytes().to_vec() }],
        app: AppInfo { bundle: Some("com.rod.crabclip".into()), name: Some("crabclip".into()), path: None },
        ..Default::default()
    };
    crate::ingest::build(cap, &a.settings.borrow())
}

/// Snippets changed: the history shows them anew.
fn history_changed() {
    let a = app();
    a.invalidate_all();
    if a.in_folders() {
        a.run_search(false);
    }
}
