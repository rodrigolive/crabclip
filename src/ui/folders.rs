//! Snippet folders: browsing them in the list (a search starting with "/"),
//! the folder preview, and the folder field under the clip.

use super::{App, beep, draw, is_obj};
use crate::db::Entry;
use crate::log;
use crate::query::FolderQuery;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2_app_kit::{
    NSApplication, NSAttributedStringAttachmentConveniences, NSColor, NSEventType, NSFont, NSTextAttachment,
};
use objc2_foundation::{NSArray, NSAttributedString, NSMutableAttributedString, NSRange, NSString};

impl App {
    pub fn in_folders(&self) -> bool {
        self.folder.get().is_some()
    }

    pub fn folder_row_height(&self) -> f64 {
        (self.settings.borrow().font_size * 2.0 + 6.0).round()
    }

    /// Rows for a folder search: subfolders as minus their id, then snippets.
    pub(super) fn folder_ids(&self, fq: &FolderQuery) -> Vec<i64> {
        let fid = if fq.path.is_empty() { Some(0) } else { self.db.folder_id(&fq.path).ok().flatten() };
        self.folder.set(Some(fid.unwrap_or(-1)));
        let entries = fid.map(|f| self.db.folder_entries(f, &fq.filter).unwrap_or_default()).unwrap_or_default();
        let mut map = self.entries.borrow_mut();
        map.clear();
        let mut ids = Vec::with_capacity(entries.len());
        for e in entries {
            let id = match &e {
                Entry::Folder { id, .. } => -id,
                Entry::Snippet { clip_id, .. } => *clip_id,
            };
            if !map.contains_key(&id) {
                ids.push(id);
                map.insert(id, e);
            }
        }
        ids
    }

    pub fn entry(&self, row_id: i64) -> Option<Entry> {
        self.entries.borrow().get(&row_id).cloned()
    }

    /// "/Clients/ssh" while browsing folders.
    pub(super) fn folder_label(&self) -> Option<String> {
        let f = self.folder.get()?;
        let path = if f > 0 { self.db.folder_path(f).unwrap_or_default() } else { String::new() };
        Some(format!("/{path}"))
    }

    fn set_search(&self, text: &str) {
        let ui = self.ui();
        ui.search.setStringValue(&NSString::from_str(text));
        if let Some(ed) = ui.search.currentEditor() {
            ed.setSelectedRange(NSRange::new(ui.search.stringValue().length(), 0));
        }
    }

    /// The caret sits at the end of the search text with nothing selected.
    pub(super) fn caret_at_end(&self) -> bool {
        let ui = self.ui();
        ui.search.currentEditor().is_some_and(|ed| {
            let r = ed.selectedRange();
            r.length == 0 && r.location == ui.search.stringValue().length()
        })
    }

    /// Show the history browsing the folders (the folders shortcut).
    pub fn show_folders(&self) {
        self.show_folder("");
    }

    /// Show the history in the folder at `path` (a folder's shortcut), or in
    /// its nearest parent still there if it is gone.
    pub fn show_folder(&self, path: &str) {
        let fid = self.db.nearest_folder(path).unwrap_or(0);
        if fid == 0 && !path.is_empty() {
            log!("folders: {path:?} is gone, showing the top level");
        }
        let q = match self.db.folder_path(fid).unwrap_or_default() {
            p if p.is_empty() => "/".to_string(),
            p => format!("/{p}/"),
        };
        self.show();
        self.set_search(&q);
        self.run_search(true);
        let ui = self.ui();
        ui.panel.makeFirstResponder(Some(&ui.search));
        self.set_search(&q);
    }

    pub fn open_folder(&self, fid: i64) {
        let path = self.db.folder_path(fid).unwrap_or_default();
        self.set_search(&format!("/{path}/"));
        self.run_search(true);
    }

    /// → on a folder row: list what it holds. False when it isn't a folder.
    pub fn open_selected_folder(&self) -> bool {
        match self.selected_row_id() {
            Some(id) if id < 0 => {
                self.open_folder(-id);
                true
            }
            _ => false,
        }
    }

    /// → / ← in the list: into the selected folder / back up; past the ends
    /// (or outside folders) they open / fold the preview.
    pub fn arrow(&self, right: bool) {
        let moved = self.in_folders() && if right { self.open_selected_folder() } else { self.folder_up() };
        if !moved {
            self.set_preview_open(right);
        }
    }

    /// ← : back to the parent folder, with the one we left selected. False
    /// at the top level.
    pub fn folder_up(&self) -> bool {
        let q = self.ui().search.stringValue().to_string();
        let Some(fq) = crate::query::parse_folder_query(&q) else { return false };
        if fq.path.is_empty() {
            return false;
        }
        let here = self.db.folder_id(&fq.path).ok().flatten();
        let parent = fq.path.rsplit_once('/').map(|(p, _)| p).unwrap_or("");
        self.select_after.set(here.map(|f| -f));
        self.set_search(&if parent.is_empty() { "/".to_string() } else { format!("/{parent}/") });
        self.run_search(true);
        true
    }

    /// The right pane for a folder row: what the folder holds.
    pub(super) fn show_folder_preview(&self, fid: i64) {
        let ui = self.ui();
        self.set_tags_field(None, &[]);
        self.set_folders_field(None, &[]);
        let path = self.db.folder_path(fid).unwrap_or_default();
        ui.date_label.setStringValue(&NSString::from_str(&format!("/{path}")));
        if !self.settings.borrow().preview_open {
            return;
        }
        let entries = self.db.folder_entries(fid, "").unwrap_or_default();
        let fs = self.settings.borrow().font_size;
        let font = NSFont::systemFontOfSize(fs);
        let plain = draw::attrs(&font, &NSColor::labelColor(), false);
        let dim = draw::attrs(&NSFont::systemFontOfSize(fs - 1.0), &NSColor::tertiaryLabelColor(), false);
        let out = NSMutableAttributedString::new();
        // Names start at one tab stop, whatever the icon's width.
        let para = objc2_app_kit::NSMutableParagraphStyle::new();
        para.setTabStops(Some(&NSArray::new()));
        para.setDefaultTabInterval((fs * 1.9).round());
        para.setLineSpacing(3.0);
        para.setLineBreakMode(objc2_app_kit::NSLineBreakMode::ByTruncatingTail);
        let add = |text: &str, a: &objc2_foundation::NSDictionary<NSString, AnyObject>| {
            let s = unsafe { NSAttributedString::new_with_attributes(&NSString::from_str(text), a) };
            out.appendAttributedString(&s);
        };
        let (mut nf, mut ns) = (0, 0);
        for e in &entries {
            let (sym, label) = match e {
                Entry::Folder { name, .. } => {
                    nf += 1;
                    ("folder", name.as_str())
                }
                Entry::Snippet { title, .. } => {
                    ns += 1;
                    ("doc.text", title.as_str())
                }
            };
            if let Some(img) = draw::tinted_symbol(sym, &NSColor::secondaryLabelColor(), fs) {
                let att = NSTextAttachment::new();
                let sz = img.size();
                att.setImage(Some(&img));
                att.setBounds(objc2_foundation::NSRect::new(
                    objc2_foundation::NSPoint::new(0.0, -(sz.height - fs) / 2.0 - 2.0),
                    sz,
                ));
                out.appendAttributedString(&NSAttributedString::attributedStringWithAttachment(&att));
            }
            add(&format!("\t{label}"), &plain);
            if let Entry::Folder { items, .. } = e {
                add(&format!("   {items} ›"), &dim);
            }
            add("\n", &plain);
        }
        if entries.is_empty() {
            add("Empty folder", &dim);
        }
        unsafe {
            out.addAttribute_value_range(
                objc2_app_kit::NSParagraphStyleAttributeName,
                &para,
                NSRange::new(0, out.length()),
            );
        }
        ui.image_view.setImage(None);
        ui.image_view.setHidden(true);
        ui.text_scroll.setHidden(false);
        let tv = &ui.text_view;
        tv.setDrawsBackground(false);
        if let Some(storage) = unsafe { tv.textStorage() } {
            storage.setAttributedString(&out);
        }
        tv.scrollRangeToVisible(NSRange::new(0, 0));
        let count = |n: usize, one: &str, many: &str| format!("{n} {}", if n == 1 { one } else { many });
        let meta = format!(
            "Folder /{path}\n{} · {}\n→ opens it, ← goes back up",
            count(nf, "folder", "folders"),
            count(ns, "snippet", "snippets")
        );
        ui.meta.setStringValue(&NSString::from_str(&meta));
    }

    // -------------------------------------------------------- folder field

    /// Folder paths starting with `word`, then containing it.
    pub fn folder_matches(&self, word: &str, max: usize) -> Vec<String> {
        let w = word.trim().trim_start_matches('/').to_lowercase();
        let all = self.db.all_folder_paths().unwrap_or_default();
        let mut out: Vec<String> = all.iter().filter(|(p, _)| p.to_lowercase().starts_with(&w)).map(|p| p.0.clone()).collect();
        out.extend(
            all.iter()
                .filter(|(p, _)| !p.to_lowercase().starts_with(&w) && p.to_lowercase().contains(&w))
                .map(|p| p.0.clone()),
        );
        out.truncate(max);
        out
    }

    pub(super) fn set_folders_field(&self, id: Option<i64>, folders: &[String]) {
        let f = &self.ui().folders_field;
        if f.currentEditor().is_some() && id == Some(self.folders_clip.get()) {
            return;
        }
        self.folders_clip.set(id.unwrap_or(0));
        f.setEnabled(id.is_some());
        let names: Vec<Retained<NSString>> = folders.iter().map(|t| NSString::from_str(t)).collect();
        let arr = NSArray::from_retained_slice(&names);
        unsafe { f.setObjectValue(Some(arr.as_ref())) };
    }

    /// ⌘L: file the selected clip in folders.
    pub fn focus_folders(&self) {
        if self.selected_id().is_none() {
            beep();
            return;
        }
        self.set_preview_open(true);
        let ui = self.ui();
        ui.panel.makeFirstResponder(Some(&ui.folders_field));
    }

    pub fn is_folders_field(&self, obj: &AnyObject) -> bool {
        self.ui.get().is_some_and(|ui| is_obj(Some(obj), &*ui.folders_field))
    }

    /// The folder field's action: file its clip in exactly those folders.
    pub fn save_folders(&self) {
        let id = self.folders_clip.get();
        if id == 0 {
            return;
        }
        let ui = self.ui();
        let vals: Vec<String> = ui
            .folders_field
            .objectValue()
            .and_then(|o| o.downcast::<NSArray>().ok())
            .map(|a| a.iter().filter_map(|o| o.downcast::<NSString>().ok().map(|s| s.to_string())).collect())
            .unwrap_or_default();
        let mut want: Vec<String> =
            vals.iter().map(|p| crate::db::folder_parts(p).join("/")).filter(|p| !p.is_empty()).collect();
        want.sort_by_key(|p| p.to_lowercase());
        want.dedup_by_key(|p| p.to_lowercase());
        let have = self.db.folders_of(id).unwrap_or_default();
        if want.iter().map(|p| p.to_lowercase()).ne(have.iter().map(|p| p.to_lowercase())) {
            match self.db.set_folders(id, &want) {
                Ok(paths) => {
                    log!("folders: clip {id} -> {}", paths.join(", "));
                    self.set_folders_field(Some(id), &paths);
                    self.invalidate(id);
                    if self.in_folders() {
                        self.run_search(false);
                    }
                }
                Err(e) => {
                    log!("folders: clip {id}: {e}");
                    self.confirm("Couldn't file the clip", &e.to_string(), "OK");
                }
            }
        }
        let enter = NSApplication::sharedApplication(self.mtm)
            .currentEvent()
            .is_some_and(|e| e.r#type() == NSEventType::KeyDown && matches!(e.keyCode(), 36 | 76));
        if enter {
            ui.panel.makeFirstResponder(Some(&ui.table));
        }
    }
}

/// How a folder path reads in its token: "Clients › ssh".
pub fn folder_label(path: &str) -> String {
    path.replace('/', " › ")
}
