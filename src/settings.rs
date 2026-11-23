//! User preferences, stored as key/value rows in the database.

use crate::db::Db;
use std::sync::atomic::{AtomicBool, Ordering};

static READ_ONLY: AtomicBool = AtomicBool::new(false);

/// From now on `save` does nothing (offscreen snapshots must not change
/// the preferences of the database they look at).
pub fn read_only() {
    READ_ONLY.store(true, Ordering::Relaxed);
}

/// Interface size stops in percent (slider, ⌘+ / ⌘-): fine steps below 100%.
pub const UI_SCALES: &[f64] = &[70.0, 75.0, 80.0, 85.0, 90.0, 95.0, 100.0, 110.0, 125.0];

/// Index of the interface size stop closest to `pct`.
pub fn ui_scale_stop(pct: f64) -> usize {
    (0..UI_SCALES.len())
        .min_by(|a, b| (UI_SCALES[*a] - pct).abs().total_cmp(&(UI_SCALES[*b] - pct).abs()))
        .unwrap_or(6)
}

/// A global shortcut that opens the history straight into a folder.
#[derive(Clone, Debug, PartialEq)]
pub struct FolderShortcut {
    /// Folder path ("Clients/Formosa"). If it is gone, the shortcut opens the
    /// nearest folder still there.
    pub folder: String,
    /// global-hotkey syntax, one or more like `hotkey` ("" = none yet).
    pub hotkey: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Settings {
    /// Global shortcuts for the history, global-hotkey syntax, comma-separated
    /// ("cmd+alt+v" or "cmd+alt+v, ctrl+shift+cmd+v").
    pub hotkey: String,
    /// Global shortcuts that open straight into the snippet folders ("" = none).
    pub folder_hotkey: String,
    /// Global shortcuts that open the preferences ("" = none).
    pub prefs_hotkey: String,
    pub folder_shortcuts: Vec<FolderShortcut>,
    /// Where the window opens: "last" position, under the "menubar" icon, or at the "mouse".
    pub open_at: String,
    pub launch_at_login: bool,
    pub move_to_top: bool,
    /// ⏎ pastes into the frontmost app; off (default): ⏎ only copies.
    pub enter_pastes: bool,
    pub ocr: bool,
    pub row_height: f64,
    /// Zoom of the whole window (text, rows, icons, bars), in percent.
    pub ui_scale: f64,
    /// Text size of the list and the preview, in points.
    pub font_size: f64,
    /// The preview pane on the right is shown (→ opens it, ← folds it).
    pub preview_open: bool,
    /// Images bigger than `img_scale_min_mp` megapixels are stored at this % of
    /// their size. 100 = keep the original bytes (default).
    pub img_scale_pct: u32,
    pub img_scale_min_mp: f64,
    pub zstd_level: i32,
    /// 0 = keep forever. Starred clips are never pruned.
    pub keep_days: u32,
    pub ignore_apps: Vec<String>,
    pub pinned: bool,
    pub star_filter: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            hotkey: "cmd+alt+v".into(),
            folder_hotkey: String::new(),
            prefs_hotkey: String::new(),
            folder_shortcuts: Vec::new(),
            open_at: "last".into(),
            launch_at_login: true,
            move_to_top: true,
            enter_pastes: false,
            ocr: true,
            row_height: 72.0,
            ui_scale: 100.0,
            font_size: 13.0,
            preview_open: true,
            img_scale_pct: 100,
            img_scale_min_mp: 12.0,
            zstd_level: 9,
            keep_days: 0,
            ignore_apps: vec![
                "com.agilebits.onepassword7".into(),
                "com.1password.1password".into(),
                "com.apple.keychainaccess".into(),
                "com.bitwarden.desktop".into(),
            ],
            pinned: false,
            star_filter: false,
        }
    }
}

fn b(v: bool) -> String {
    if v { "1".into() } else { "0".into() }
}

/// `path` is `folder` or a folder below it (any case): the rest of it
/// ("" or "/Sub").
fn below<'a>(path: &'a str, folder: &str) -> Option<&'a str> {
    let n = folder.len();
    let head = path.get(..n)?;
    let rest = &path[n..];
    (head.to_lowercase() == folder.to_lowercase() && (rest.is_empty() || rest.starts_with('/'))).then_some(rest)
}

/// One shortcut a line: "hotkey<TAB>folder path".
fn folder_shortcuts_to_string(list: &[FolderShortcut]) -> String {
    list.iter().map(|f| format!("{}\t{}", f.hotkey.trim(), f.folder.trim())).collect::<Vec<_>>().join("\n")
}

fn folder_shortcuts_from_str(v: &str) -> Vec<FolderShortcut> {
    v.lines()
        .filter_map(|l| l.split_once('\t'))
        .map(|(k, f)| FolderShortcut { folder: f.trim().to_string(), hotkey: k.trim().to_string() })
        .filter(|f| !f.folder.is_empty())
        .collect()
}

impl Settings {
    pub fn load(db: &Db) -> Settings {
        let d = Settings::default();
        let get = |k: &str| db.get_setting(k);
        let flag = |k: &str, def: bool| get(k).map(|v| v == "1").unwrap_or(def);
        Settings {
            hotkey: get("hotkey").unwrap_or(d.hotkey),
            folder_hotkey: get("folder_hotkey").unwrap_or(d.folder_hotkey),
            prefs_hotkey: get("prefs_hotkey").unwrap_or(d.prefs_hotkey),
            folder_shortcuts: get("folder_shortcuts").map(|v| folder_shortcuts_from_str(&v)).unwrap_or(d.folder_shortcuts),
            open_at: get("open_at").unwrap_or(d.open_at),
            launch_at_login: flag("launch_at_login", d.launch_at_login),
            move_to_top: flag("move_to_top", d.move_to_top),
            enter_pastes: flag("enter_pastes", d.enter_pastes),
            ocr: flag("ocr", d.ocr),
            row_height: get("row_height").and_then(|v| v.parse().ok()).unwrap_or(d.row_height),
            ui_scale: get("ui_scale").and_then(|v| v.parse::<f64>().ok()).filter(|v| v.is_finite()).unwrap_or(d.ui_scale).clamp(50.0, 200.0),
            font_size: get("font_size").and_then(|v| v.parse::<f64>().ok()).filter(|v| v.is_finite()).unwrap_or(d.font_size).clamp(9.0, 28.0),
            preview_open: flag("preview_open", d.preview_open),
            img_scale_pct: get("img_scale_pct")
                .and_then(|v| v.parse().ok())
                .unwrap_or(d.img_scale_pct)
                .clamp(5, 100),
            img_scale_min_mp: get("img_scale_min_mp")
                .and_then(|v| v.parse().ok())
                .unwrap_or(d.img_scale_min_mp),
            zstd_level: get("zstd_level").and_then(|v| v.parse().ok()).unwrap_or(d.zstd_level).clamp(1, 22),
            keep_days: get("keep_days").and_then(|v| v.parse().ok()).unwrap_or(d.keep_days),
            ignore_apps: get("ignore_apps")
                .map(|v| v.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect())
                .unwrap_or(d.ignore_apps),
            pinned: flag("pinned", d.pinned),
            star_filter: flag("star_filter", d.star_filter),
        }
    }

    /// A folder was renamed (or moved) from path `old` to `new`: shortcuts to
    /// it and to folders below it follow. True if any did.
    pub fn folder_renamed(&mut self, old: &str, new: &str) -> bool {
        let mut changed = false;
        for f in &mut self.folder_shortcuts {
            if let Some(rest) = below(&f.folder, old) {
                f.folder = format!("{new}{rest}");
                changed = true;
            }
        }
        changed
    }

    /// A folder was deleted: shortcuts to it and to folders below it go.
    /// True if any did.
    pub fn folder_deleted(&mut self, path: &str) -> bool {
        let n = self.folder_shortcuts.len();
        self.folder_shortcuts.retain(|f| below(&f.folder, path).is_none());
        n != self.folder_shortcuts.len()
    }

    pub fn save(&self, db: &Db) {
        if READ_ONLY.load(Ordering::Relaxed) {
            return;
        }
        let pairs = [
            ("hotkey", self.hotkey.clone()),
            ("folder_hotkey", self.folder_hotkey.clone()),
            ("prefs_hotkey", self.prefs_hotkey.clone()),
            ("folder_shortcuts", folder_shortcuts_to_string(&self.folder_shortcuts)),
            ("open_at", self.open_at.clone()),
            ("launch_at_login", b(self.launch_at_login)),
            ("move_to_top", b(self.move_to_top)),
            ("enter_pastes", b(self.enter_pastes)),
            ("ocr", b(self.ocr)),
            ("row_height", self.row_height.to_string()),
            ("ui_scale", self.ui_scale.to_string()),
            ("font_size", self.font_size.to_string()),
            ("preview_open", b(self.preview_open)),
            ("img_scale_pct", self.img_scale_pct.to_string()),
            ("img_scale_min_mp", self.img_scale_min_mp.to_string()),
            ("zstd_level", self.zstd_level.to_string()),
            ("keep_days", self.keep_days.to_string()),
            ("ignore_apps", self.ignore_apps.join(", ")),
            ("pinned", b(self.pinned)),
            ("star_filter", b(self.star_filter)),
        ];
        for (k, v) in pairs {
            if let Err(e) = db.set_setting(k, &v) {
                crate::log!("settings: save {k}: {e}");
            }
        }
    }
}
