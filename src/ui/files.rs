//! The right pane for file clips: a Quick Look preview of a single file (what
//! the space bar shows in Finder), the files with their icons for several.

use super::{App, draw};
use crate::db::Detail;
use objc2::rc::{Allocated, Retained};
use objc2::runtime::{AnyClass, AnyObject};
use objc2::msg_send;
use objc2_app_kit::{NSAttributedStringAttachmentConveniences, NSAutoresizingMaskOptions, NSColor, NSFont, NSTextAttachment, NSView};
use objc2_foundation::{NSAttributedString, NSMutableAttributedString, NSPoint, NSRange, NSRect, NSSize, NSString, NSURL};

#[link(name = "Quartz", kind = "framework")]
unsafe extern "C" {}

impl App {
    /// Fill the pane for a file clip. False when there is nothing to show.
    pub(super) fn show_file_preview(&self, d: &Detail) -> bool {
        let paths: Vec<&str> = d.text.as_deref().unwrap_or("").lines().filter(|p| !p.is_empty()).collect();
        let ui = self.ui();
        if paths.is_empty() {
            return false;
        }
        ui.image_view.setImage(None);
        ui.image_view.setHidden(true);
        if paths.len() == 1 && std::fs::exists(paths[0]).unwrap_or(false) && self.set_quicklook(Some(paths[0])) {
            ui.text_scroll.setHidden(true);
            return true;
        }
        self.set_quicklook(None);
        ui.text_scroll.setHidden(false);
        self.show_file_list(&paths);
        true
    }

    /// Show `path` in the Quick Look view, or hide it. True when it shows.
    pub(super) fn set_quicklook(&self, path: Option<&str>) -> bool {
        let Some(path) = path else {
            if self.ql_path.replace(None).is_some() {
                if let Some(v) = self.ql.borrow().as_ref() {
                    v.setHidden(true);
                    // Stops a video or sound it was playing.
                    let _: () = unsafe { msg_send![&**v, setPreviewItem: None::<&AnyObject>] };
                }
            }
            return false;
        };
        if self.ql.borrow().is_none() {
            let Some(v) = self.make_quicklook() else { return false };
            self.ql.replace(Some(v));
        }
        let ql = self.ql.borrow();
        let Some(v) = ql.as_ref() else { return false };
        if self.ql_path.borrow().as_deref() != Some(path) {
            let url = NSURL::fileURLWithPath(&NSString::from_str(path));
            let _: () = unsafe { msg_send![&**v, setPreviewItem: &*url] };
            self.ql_path.replace(Some(path.to_string()));
        }
        v.setHidden(false);
        true
    }

    fn make_quicklook(&self) -> Option<Retained<NSView>> {
        let ui = self.ui();
        let cls = AnyClass::get(c"QLPreviewView")?;
        let frame = ui.text_scroll.frame();
        let v: Option<Retained<NSView>> = unsafe {
            let a: Allocated<NSView> = msg_send![cls, alloc];
            // QLPreviewViewStyleNormal = 0: with the controls for video and pages.
            msg_send![a, initWithFrame: frame, style: 0isize]
        };
        let v = v?;
        unsafe {
            // The panel only hides: keep the view usable when it comes back.
            let _: () = msg_send![&*v, setShouldCloseWithWindow: false];
        }
        v.setAutoresizingMask(NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewHeightSizable);
        v.setHidden(true);
        ui.preview.addSubview(&v);
        Some(v)
    }

    /// Several files (or one that is gone): each with its icon and folder.
    fn show_file_list(&self, paths: &[&str]) {
        let ui = self.ui();
        let fs = self.settings.borrow().font_size;
        let plain = draw::attrs(&NSFont::systemFontOfSize(fs), &NSColor::labelColor(), false);
        let dim = draw::attrs(&NSFont::systemFontOfSize(fs - 1.0), &NSColor::secondaryLabelColor(), false);
        let out = NSMutableAttributedString::new();
        let add = |text: &str, a: &objc2_foundation::NSDictionary<NSString, AnyObject>| {
            let s = unsafe { NSAttributedString::new_with_attributes(&NSString::from_str(text), a) };
            out.appendAttributedString(&s);
        };
        let home = std::env::var("HOME").unwrap_or_default();
        let isz = (fs * 2.4).round();
        // Names and folders start at one tab stop, after the icon.
        let para = objc2_app_kit::NSMutableParagraphStyle::new();
        para.setTabStops(Some(&objc2_foundation::NSArray::new()));
        para.setDefaultTabInterval(isz + 8.0);
        para.setLineBreakMode(objc2_app_kit::NSLineBreakMode::ByTruncatingMiddle);
        for p in paths {
            let exists = std::fs::exists(p).unwrap_or(false);
            let icon = self.file_icon(p);
            let att = NSTextAttachment::new();
            att.setImage(Some(&icon));
            att.setBounds(NSRect::new(NSPoint::new(0.0, -isz / 4.0), NSSize::new(isz, isz)));
            out.appendAttributedString(&NSAttributedString::attributedStringWithAttachment(&att));
            let (dir, name) = p.rsplit_once('/').unwrap_or(("", p));
            let dir = match dir.strip_prefix(&home) {
                Some(rest) if !home.is_empty() => format!("~{rest}"),
                _ => dir.to_string(),
            };
            add(&format!("\t{name}\n"), &plain);
            let note = if exists { dir } else { format!("{dir} · not found") };
            add(&format!("\t{note}\n\n"), &dim);
        }
        unsafe {
            out.addAttribute_value_range(objc2_app_kit::NSParagraphStyleAttributeName, &para, NSRange::new(0, out.length()));
        }
        let tv = &ui.text_view;
        tv.setDrawsBackground(false);
        if let Some(storage) = unsafe { tv.textStorage() } {
            storage.setAttributedString(&out);
        }
        tv.scrollRangeToVisible(NSRange::new(0, 0));
    }
}

/// The file line of a file clip's details: size and date, or what is gone.
pub fn file_summary(text: &str) -> String {
    let paths: Vec<&str> = text.lines().filter(|p| !p.is_empty()).collect();
    let metas: Vec<Option<std::fs::Metadata>> = paths.iter().map(|p| std::fs::metadata(p).ok()).collect();
    let modified = |m: &std::fs::Metadata| {
        m.modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| format!(" · modified {}", crate::timefmt::full(d.as_millis() as i64)))
            .unwrap_or_default()
    };
    match metas.as_slice() {
        [] => "No files".into(),
        [None] => "File not found".into(),
        [Some(m)] if m.is_dir() => format!("Folder{}", modified(m)),
        [Some(m)] => format!("File {}{}", crate::timefmt::bytes(m.len() as i64), modified(m)),
        _ => {
            let gone = metas.iter().filter(|m| m.is_none()).count();
            let total: u64 = metas.iter().flatten().filter(|m| m.is_file()).map(|m| m.len()).sum();
            match gone {
                0 => format!("{} files · {}", paths.len(), crate::timefmt::bytes(total as i64)),
                g if g == paths.len() => format!("{g} files, not found"),
                g => format!("{} files · {} · {g} not found", paths.len(), crate::timefmt::bytes(total as i64)),
            }
        }
    }
}
