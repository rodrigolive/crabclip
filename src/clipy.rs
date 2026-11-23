//! Importer for Clipy (github.com/Clipy/Clipy). Current Clipy keeps everything
//! in SQLite: `pasteboardHistories` + `pasteboardHistoryAssets` (each pasteboard
//! type byte-for-byte, in order) and `snippetFolders` + `snippets`. Older
//! Clipy (Realm) converts its data to that file when the current version
//! starts. Both versions export snippets as XML:
//! <folders><folder><title/><snippets><snippet><title/><content/>…

use crate::db::{AppInfo, Db, Inserted, Rep};
use crate::ingest::{self, Capture};
use crate::settings::Settings;
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub const BUNDLE: &str = "com.clipy-app.Clipy";

fn support_dirs() -> Vec<PathBuf> {
    let home = PathBuf::from(std::env::var("HOME").unwrap_or_default());
    vec![
        home.join("Library/Application Support").join(BUNDLE),
        home.join("Library/Containers").join(BUNDLE).join("Data/Library/Application Support").join(BUNDLE),
    ]
}

/// Clipy's database, if it has one on this Mac. `container` also looks in
/// its sandbox container, which macOS may guard with an "access data from
/// other apps" prompt: only when the user asked for an import.
pub fn default_db(container: bool) -> Option<PathBuf> {
    let dirs = support_dirs();
    let n = if container { dirs.len() } else { 1 };
    dirs.into_iter().take(n).map(|d| d.join("sqlite.db")).find(|p| p.exists())
}

/// Data of an old Clipy (Realm) that was never opened by a current version.
pub fn legacy_realm() -> Option<PathBuf> {
    support_dirs().into_iter().map(|d| d.join("default.realm")).find(|p| p.exists())
}

#[derive(Debug, Default, Clone, Copy)]
pub struct Stats {
    pub history: usize,
    pub added: usize,
    pub merged: usize,
    pub skipped: usize,
    pub folders: usize,
    pub snippets: usize,
    /// Snippets with the same text as another in the same folder (one clip,
    /// filed once: the later title wins).
    pub duplicates: usize,
    /// Clipy shortcuts brought in, and the ones left out (logged).
    pub shortcuts: usize,
    pub shortcuts_skipped: usize,
}

impl Stats {
    pub fn summary(&self) -> String {
        let mut s = format!(
            "{} history clips ({} added, {} already there, {} skipped), {} snippets in {} folders",
            self.history, self.added, self.merged, self.skipped, self.snippets, self.folders
        );
        if self.duplicates > 0 {
            s += &format!(" ({} with the same text as another in their folder were merged)", self.duplicates);
        }
        if self.shortcuts > 0 {
            s += &format!(", {} shortcuts", self.shortcuts);
        }
        if self.shortcuts_skipped > 0 {
            s += &format!(" ({} Clipy shortcuts left out, see the log)", self.shortcuts_skipped);
        }
        s
    }
}

/// Import a Clipy database or snippets XML export into `db`, and the
/// shortcuts in Clipy's preferences `prefs` (`default_prefs()`), if any.
pub fn import(
    path: &Path,
    prefs: Option<&plist::Dictionary>,
    db: &Db,
    settings: &Settings,
    progress: impl FnMut(&Stats),
) -> Result<Stats, String> {
    crate::log!("clipy: importing {}", path.display());
    let mut head = [0u8; 16];
    let n = std::io::Read::read(&mut std::fs::File::open(path).map_err(|e| format!("open {}: {e}", path.display()))?, &mut head)
        .map_err(|e| format!("read {}: {e}", path.display()))?;
    // Clipy folder ids → crabclip folder paths (an XML export has no ids).
    let mut ids = HashMap::new();
    let mut st = if head[..n] == *b"SQLite format 3\0" {
        import_sqlite(path, db, settings, &mut ids, progress)?
    } else {
        let xml = std::fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
        import_xml(&String::from_utf8_lossy(&xml), db, settings)?
    };
    if let Some(prefs) = prefs {
        import_shortcuts(&read_shortcuts(prefs), &ids, db, &mut st);
    }
    crate::log!("clipy: {} -> {:?}", path.display(), st);
    Ok(st)
}

struct Folder {
    title: String,
    index: i64,
    snippets: Vec<(String, String, i64)>, // title, content, index
}

fn import_sqlite(
    path: &Path,
    db: &Db,
    settings: &Settings,
    ids: &mut HashMap<String, String>,
    mut progress: impl FnMut(&Stats),
) -> Result<Stats, String> {
    let e = |e: rusqlite::Error| e.to_string();
    let src = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)
        .map_err(|e| format!("open {}: {e}", path.display()))?;
    src.busy_timeout(std::time::Duration::from_secs(10)).map_err(e)?;
    let has = |table: &str| -> bool {
        src.query_row("SELECT 1 FROM sqlite_master WHERE type='table' AND name=?", [table], |_| Ok(()))
            .optional()
            .ok()
            .flatten()
            .is_some()
    };
    if !has("pasteboardHistories") && !has("snippetFolders") {
        return Err("not a Clipy database".into());
    }
    let mut st = Stats::default();

    if has("pasteboardHistories") && has("pasteboardHistoryAssets") {
        let created = src
            .prepare("SELECT 1 FROM pragma_table_info('pasteboardHistories') WHERE name='createdAt'")
            .and_then(|mut s| s.exists([]))
            .unwrap_or(false);
        let sql = format!(
            "SELECT id, updateAt FROM pasteboardHistories ORDER BY {} ",
            if created { "createdAt, updateAt" } else { "updateAt" }
        );
        let rows: Vec<(String, i64)> = {
            let mut q = src.prepare(&sql).map_err(e)?;
            q.query_map([], |r| Ok((r.get(0)?, r.get(1)?))).map_err(e)?.collect::<rusqlite::Result<_>>().map_err(e)?
        };
        st.history = rows.len();
        let mut assets = src
            .prepare(r#"SELECT pasteboardType, data FROM pasteboardHistoryAssets WHERE pasteboardHistoryID=? ORDER BY "index""#)
            .map_err(e)?;
        for (id, when) in rows {
            // Clipy doesn't keep item boundaries: the nth asset of a type
            // belongs to the nth item.
            let mut seen: HashMap<String, i64> = HashMap::new();
            let reps: Vec<Rep> = assets
                .query_map([&id], |r| Ok((r.get::<_, String>(0)?, r.get::<_, Vec<u8>>(1)?)))
                .map_err(e)?
                .filter_map(|r| r.ok())
                .map(|(uti, data)| {
                    let n = seen.entry(uti.clone()).or_insert(0);
                    let rep = Rep { item: *n, uti, data };
                    *n += 1;
                    rep
                })
                .collect();
            let cap = Capture { ts: to_ms(when), reps: file_urls(reps), ..Default::default() };
            match ingest::build(cap, settings).map(|c| db.insert_imported(&c, settings.zstd_level, true)) {
                Some(Ok(Inserted::New(_))) => st.added += 1,
                Some(Ok(Inserted::Existing(_))) => st.merged += 1,
                Some(Err(err)) => {
                    crate::log!("clipy: insert failed: {err}");
                    st.skipped += 1;
                }
                None => st.skipped += 1,
            }
            progress(&st);
        }
    }

    if has("snippetFolders") && has("snippets") {
        let folders: Vec<(String, String, i64)> = {
            let mut q = src.prepare(r#"SELECT id, title, "index" FROM snippetFolders ORDER BY "index""#).map_err(e)?;
            q.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).map_err(e)?.collect::<rusqlite::Result<_>>().map_err(e)?
        };
        let mut q = src.prepare(r#"SELECT title, content, "index" FROM snippets WHERE folderID=? ORDER BY "index""#).map_err(e)?;
        let mut all = Vec::new();
        for (id, title, index) in folders {
            // As the folder will be stored ("A  B" is "A B").
            ids.insert(id.to_lowercase(), crate::db::folder_parts(&folder_name(&title)).join("/"));
            let snippets = q
                .query_map([&id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
                .map_err(e)?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(e)?;
            all.push(Folder { title, index, snippets });
        }
        add_snippets(all, db, settings, &mut st)?;
    }
    Ok(st)
}

fn import_xml(xml: &str, db: &Db, settings: &Settings) -> Result<Stats, String> {
    let doc = roxmltree::Document::parse(xml).map_err(|e| format!("not a Clipy snippets export: {e}"))?;
    let root = doc.root_element();
    if root.tag_name().name() != "folders" {
        return Err("not a Clipy snippets export (no <folders>)".into());
    }
    let child = |n: roxmltree::Node, name: &str| -> String {
        n.children().find(|c| c.has_tag_name(name)).and_then(|c| c.text()).unwrap_or("").to_string()
    };
    let mut all = Vec::new();
    for (i, f) in root.children().filter(|n| n.has_tag_name("folder")).enumerate() {
        let snippets = f
            .children()
            .filter(|n| n.has_tag_name("snippets"))
            .flat_map(|s| s.children().filter(|n| n.has_tag_name("snippet")))
            .enumerate()
            .map(|(j, s)| (child(s, "title"), child(s, "content"), j as i64))
            .collect();
        all.push(Folder { title: child(f, "title"), index: i as i64, snippets });
    }
    let mut st = Stats::default();
    add_snippets(all, db, settings, &mut st)?;
    Ok(st)
}

/// Snippets become text clips filed in top-level folders named like Clipy's,
/// in Clipy's order. They are dated just before the oldest clip so they don't
/// crowd the top of the history (a snippet already in it keeps its date).
fn add_snippets(folders: Vec<Folder>, db: &Db, settings: &Settings, st: &mut Stats) -> Result<(), String> {
    let e = |e: rusqlite::Error| e.to_string();
    let ts = db.snippet_ts().map_err(e)?;
    let app = AppInfo {
        bundle: Some(BUNDLE.into()),
        name: Some("Clipy".into()),
        path: Some("/Applications/Clipy.app".into()).filter(|p: &String| Path::new(p).exists()),
    };
    if !db.has_app(BUNDLE).unwrap_or(false) {
        let icon = app.path.as_deref().and_then(crate::appicon::png_for_path);
        let _ = db.save_app(BUNDLE, app.name.as_deref(), app.path.as_deref(), icon.as_deref());
    }
    let mut filed = std::collections::HashSet::new();
    for f in folders {
        let name = folder_name(&f.title);
        let fid = db.make_folder(&name).map_err(e)?;
        db.set_folder_pos(fid, f.index).map_err(e)?;
        st.folders += 1;
        for (title, content, index) in f.snippets {
            let reps = vec![Rep { item: 0, uti: "public.utf8-plain-text".into(), data: content.into_bytes() }];
            let cap = Capture { ts, reps, app: app.clone(), ..Default::default() };
            let Some(clip) = ingest::build(cap, settings) else {
                st.skipped += 1;
                continue;
            };
            // A snippet already in the history keeps its dates.
            let id = db.insert_imported(&clip, settings.zstd_level, false).map_err(e)?.id();
            if !filed.insert((fid, id)) {
                crate::log!("clipy: {name:?}: {:?} has the same text as an earlier snippet; merged", title.trim());
                st.duplicates += 1;
            }
            db.file_snippet(fid, id, title.trim(), Some(index)).map_err(e)?;
            st.snippets += 1;
        }
    }
    Ok(())
}

/// The crabclip folder a Clipy folder becomes (top level, its title).
fn folder_name(title: &str) -> String {
    let name = title.replace('/', "∕");
    if name.trim().is_empty() { "Untitled".to_string() } else { name }
}

/// Clipy's shortcuts replace crabclip's for the same things: the history
/// (Clipy's main and history menus both open it), the folders (its snippets
/// menu) and the preferences; a folder's shortcut joins the others. A
/// crabclip shortcut that one of them takes over is dropped.
fn import_shortcuts(sc: &Shortcuts, ids: &HashMap<String, String>, db: &Db, st: &mut Stats) {
    for why in &sc.skipped {
        crate::log!("clipy: shortcut left out: {why}");
    }
    st.shortcuts_skipped += sc.skipped.len();
    if sc.is_empty() {
        return;
    }
    let id = |k: &String| crate::hotkeys::parse(k).ok().map(|h| h.id());
    let mut s = Settings::load(db);
    // Without a history shortcut from Clipy, crabclip's own stays: a Clipy
    // one on the same keys is left out.
    let reserved: Vec<u32> =
        if sc.history.is_empty() { crate::hotkeys::split(&s.hotkey).iter().filter_map(id).collect() } else { Vec::new() };
    let mut imported: Vec<u32> = Vec::new();
    let mut keep = |list: &[String], st: &mut Stats| -> Vec<String> {
        let mut out = Vec::new();
        for spec in list {
            match crate::hotkeys::parse(spec) {
                Ok(h) if !imported.contains(&h.id()) && !reserved.contains(&h.id()) => {
                    imported.push(h.id());
                    out.push(spec.clone());
                    st.shortcuts += 1;
                }
                Ok(_) => {
                    crate::log!("clipy: {spec} is taken by another shortcut; left out");
                    st.shortcuts_skipped += 1;
                }
                Err(e) => {
                    crate::log!("clipy: {e}");
                    st.shortcuts_skipped += 1;
                }
            }
        }
        out
    };
    let history = keep(&sc.history, st);
    let snippets = keep(&sc.snippets, st);
    let settings = keep(&sc.settings, st);
    let mut folders = Vec::new();
    for (folder_id, spec) in &sc.folders {
        match ids.get(folder_id) {
            Some(path) => {
                for spec in keep(std::slice::from_ref(spec), st) {
                    folders.push(crate::settings::FolderShortcut { folder: path.clone(), hotkey: spec });
                }
            }
            None => {
                crate::log!("clipy: shortcut {spec} is for a folder not in this import ({folder_id})");
                st.shortcuts_skipped += 1;
            }
        }
    }
    // crabclip's shortcuts on keys Clipy's took are dropped.
    let free = |list: &str| -> String {
        let specs = crate::hotkeys::split(list);
        crate::hotkeys::join(&specs.into_iter().filter(|k| id(k).is_none_or(|i| !imported.contains(&i))).collect::<Vec<_>>())
    };
    let replace = |mine: &str, clipy: &[String]| if clipy.is_empty() { free(mine) } else { crate::hotkeys::join(clipy) };
    s.hotkey = replace(&s.hotkey, &history);
    s.folder_hotkey = replace(&s.folder_hotkey, &snippets);
    s.prefs_hotkey = replace(&s.prefs_hotkey, &settings);
    // A folder with a shortcut from Clipy takes it instead of the one it
    // had; a row left with no shortcut by the import goes.
    s.folder_shortcuts = std::mem::take(&mut s.folder_shortcuts)
        .into_iter()
        .filter(|f| !folders.iter().any(|n| n.folder.to_lowercase() == f.folder.to_lowercase()))
        .filter_map(|f| {
            let hotkey = free(&f.hotkey);
            (hotkey.is_empty() == f.hotkey.trim().is_empty()).then_some(crate::settings::FolderShortcut { hotkey, ..f })
        })
        .collect();
    s.folder_shortcuts.extend(folders);
    s.save(db);
}

/// Old Clipy kept files only as NSFilenamesPboardType (a plist of paths):
/// turn that into file URLs, one per item, like a copy from Finder today.
fn file_urls(reps: Vec<Rep>) -> Vec<Rep> {
    if reps.iter().any(|r| r.uti == "public.file-url") {
        return reps;
    }
    let Some(names) = reps.iter().find(|r| r.uti == "NSFilenamesPboardType") else { return reps };
    let paths: Vec<String> = plist::Value::from_reader(std::io::Cursor::new(&names.data))
        .ok()
        .and_then(|v| v.into_array())
        .map(|a| a.into_iter().filter_map(|p| p.into_string()).collect())
        .unwrap_or_default();
    if paths.is_empty() {
        return reps;
    }
    let mut out: Vec<Rep> = reps.into_iter().filter(|r| r.uti != "NSFilenamesPboardType").collect();
    for (i, p) in paths.iter().enumerate() {
        let url = format!("file://{}", percent_encode(p));
        out.push(Rep { item: i as i64, uti: "public.file-url".into(), data: url.into_bytes() });
    }
    out
}

fn percent_encode(path: &str) -> String {
    let mut s = String::new();
    for b in path.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'/' | b'-' | b'_' | b'.' | b'~' => s.push(b as char),
            _ => s += &format!("%{b:02X}"),
        }
    }
    s
}

/// Clipy stores unix seconds; take milliseconds too, should a version use them.
fn to_ms(t: i64) -> i64 {
    if t > 100_000_000_000 { t } else { t * 1000 }
}

// ------------------------------------------------------------- shortcuts

/// Clipy's preferences, where it keeps its shortcuts, if this Mac has them.
/// `defaults` reads them through cfprefsd (what Clipy set last, even if not
/// on disk yet); a sandboxed Clipy keeps them in its container.
pub fn default_prefs() -> Option<plist::Dictionary> {
    let exported = std::process::Command::new("/usr/bin/defaults")
        .args(["export", BUNDLE, "-"])
        .stderr(std::process::Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| plist::Value::from_reader(std::io::Cursor::new(o.stdout)).ok())
        .and_then(|v| v.into_dictionary())
        .filter(|d| !d.is_empty());
    exported.or_else(|| {
        let home = PathBuf::from(std::env::var("HOME").unwrap_or_default());
        let file = home.join("Library/Containers").join(BUNDLE).join("Data/Library/Preferences").join(format!("{BUNDLE}.plist"));
        plist::Value::from_file(file).ok()?.into_dictionary()
    })
}

/// Clipy's shortcuts in global-hotkey syntax.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Shortcuts {
    /// The main menu (history and snippets) and the history menu.
    pub history: Vec<String>,
    /// The snippets menu.
    pub snippets: Vec<String>,
    pub settings: Vec<String>,
    /// Clipy folder id (lowercase) and its shortcut.
    pub folders: Vec<(String, String)>,
    /// Set in Clipy but with no crabclip equivalent: what and why.
    pub skipped: Vec<String>,
}

impl Shortcuts {
    pub fn is_empty(&self) -> bool {
        self.history.is_empty() && self.snippets.is_empty() && self.settings.is_empty() && self.folders.is_empty()
    }
}

/// Read Clipy's shortcuts from its preferences. Current Clipy stores each as
/// JSON in a data value: {"key":"v","modifiers":2816,"doubledModifiers":false}
/// (Carbon modifiers), `folderKeyCombos` as {"<folder id>": {…}}.
pub fn read_shortcuts(prefs: &plist::Dictionary) -> Shortcuts {
    let json = |name: &str| -> Option<Json> {
        let bytes = match prefs.get(name)? {
            plist::Value::Data(d) => d.clone(),
            plist::Value::String(s) => s.clone().into_bytes(),
            _ => return None,
        };
        Json::parse(&String::from_utf8_lossy(&bytes)).filter(|j| *j != Json::Null)
    };
    let mut sc = Shortcuts::default();
    let take = |name: &str, what: &str, sc: &mut Shortcuts| -> Option<String> {
        match key_combo(&json(name)?) {
            Ok(spec) => Some(spec),
            Err(e) => {
                sc.skipped.push(format!("{what}: {e}"));
                None
            }
        }
    };
    for (name, what) in [("mainKeyCombo", "main menu"), ("historyKeyCombo", "history menu")] {
        if let Some(spec) = take(name, what, &mut sc) {
            sc.history.push(spec);
        }
    }
    if let Some(spec) = take("snippetKeyCombo", "snippets menu", &mut sc) {
        sc.snippets.push(spec);
    }
    if let Some(spec) = take("settingsKeyCombo", "settings", &mut sc) {
        sc.settings.push(spec);
    }
    if json("clearHistoryKeyCombo").is_some() {
        sc.skipped.push("clear history: not imported (crabclip keeps its history)".into());
    }
    if json("editSnippetsKeyCombo").is_some() {
        sc.skipped.push("edit snippets: crabclip has no snippet editor (⌘L files a clip)".into());
    }
    if let Some(Json::Obj(folders)) = json("folderKeyCombos") {
        for (id, combo) in folders {
            match key_combo(&combo) {
                Ok(spec) => sc.folders.push((id.to_lowercase(), spec)),
                Err(e) => sc.skipped.push(format!("folder {id}: {e}")),
            }
        }
    }
    sc
}

/// Carbon modifier bits (Clipy's `modifiers`).
const CMD: i64 = 256;
const SHIFT: i64 = 512;
const OPTION: i64 = 2048;
const CONTROL: i64 = 4096;

/// A Clipy (Magnet) key combo in global-hotkey syntax: "ctrl+alt+shift+cmd+f".
pub fn key_combo(j: &Json) -> Result<String, String> {
    if j.get("doubledModifiers") == Some(&Json::Bool(true)) {
        return Err("a double-tapped modifier can't be a global shortcut here".into());
    }
    let key = match (j.get("key"), j.get("QWERTYKeyCode").or_else(|| j.get("keyCode"))) {
        (Some(Json::Str(k)), _) => KEYS.iter().find(|(name, ..)| name == k).map(|(.., g)| *g).ok_or(format!("unknown key {k:?}"))?,
        (_, Some(Json::Num(code))) => {
            KEYS.iter().find(|(_, c, _)| f64::from(*c) == *code).map(|(.., g)| *g).ok_or(format!("unknown key code {code}"))?
        }
        _ => return Err("no key".into()),
    };
    let mods = match j.get("modifiers") {
        Some(Json::Num(m)) => *m as i64,
        _ => 0,
    };
    let mut spec = String::new();
    for (bit, name) in [(CONTROL, "ctrl"), (OPTION, "alt"), (SHIFT, "shift"), (CMD, "cmd")] {
        if mods & bit != 0 {
            spec += name;
            spec.push('+');
        }
    }
    Ok(spec + key)
}

/// Clipy's key names (Sauce's `Key`), their Carbon key codes (older Clipy
/// stored those) and global-hotkey's name for the key in that place.
pub const KEYS: &[(&str, u16, &str)] = &[
    ("a", 0x00, "a"), ("s", 0x01, "s"), ("d", 0x02, "d"), ("f", 0x03, "f"), ("h", 0x04, "h"), ("g", 0x05, "g"),
    ("z", 0x06, "z"), ("x", 0x07, "x"), ("c", 0x08, "c"), ("v", 0x09, "v"), ("section", 0x0a, "IntlBackslash"),
    ("b", 0x0b, "b"), ("q", 0x0c, "q"), ("w", 0x0d, "w"), ("e", 0x0e, "e"), ("r", 0x0f, "r"), ("y", 0x10, "y"),
    ("t", 0x11, "t"), ("one", 0x12, "1"), ("two", 0x13, "2"), ("three", 0x14, "3"), ("four", 0x15, "4"),
    ("six", 0x16, "6"), ("five", 0x17, "5"), ("equal", 0x18, "Equal"), ("nine", 0x19, "9"), ("seven", 0x1a, "7"),
    ("minus", 0x1b, "Minus"), ("eight", 0x1c, "8"), ("zero", 0x1d, "0"), ("rightBracket", 0x1e, "BracketRight"),
    ("o", 0x1f, "o"), ("u", 0x20, "u"), ("leftBracket", 0x21, "BracketLeft"), ("i", 0x22, "i"), ("p", 0x23, "p"),
    ("return", 0x24, "Enter"), ("l", 0x25, "l"), ("j", 0x26, "j"), ("quote", 0x27, "Quote"), ("k", 0x28, "k"),
    ("semicolon", 0x29, "Semicolon"), ("backslash", 0x2a, "Backslash"), ("comma", 0x2b, "Comma"),
    ("slash", 0x2c, "Slash"), ("n", 0x2d, "n"), ("m", 0x2e, "m"), ("period", 0x2f, "Period"), ("tab", 0x30, "Tab"),
    ("space", 0x31, "Space"), ("grave", 0x32, "Backquote"), ("delete", 0x33, "Backspace"), ("escape", 0x35, "Escape"),
    ("f17", 0x40, "F17"), ("keypadDecimal", 0x41, "NumpadDecimal"), ("keypadMultiply", 0x43, "NumpadMultiply"),
    ("keypadPlus", 0x45, "NumpadAdd"), ("keypadClear", 0x47, "NumLock"), ("keypadDivide", 0x4b, "NumpadDivide"),
    ("keypadEnter", 0x4c, "NumpadEnter"), ("keypadMinus", 0x4e, "NumpadSubtract"), ("f18", 0x4f, "F18"),
    ("f19", 0x50, "F19"), ("keypadEquals", 0x51, "NumpadEqual"), ("keypadZero", 0x52, "Numpad0"),
    ("keypadOne", 0x53, "Numpad1"), ("keypadTwo", 0x54, "Numpad2"), ("keypadThree", 0x55, "Numpad3"),
    ("keypadFour", 0x56, "Numpad4"), ("keypadFive", 0x57, "Numpad5"), ("keypadSix", 0x58, "Numpad6"),
    ("keypadSeven", 0x59, "Numpad7"), ("f20", 0x5a, "F20"), ("keypadEight", 0x5b, "Numpad8"),
    ("keypadNine", 0x5c, "Numpad9"), ("yen", 0x5d, "IntlYen"), ("underscore", 0x5e, "IntlRo"),
    ("keypadComma", 0x5f, "NumpadComma"), ("f5", 0x60, "F5"), ("f6", 0x61, "F6"), ("f7", 0x62, "F7"),
    ("f3", 0x63, "F3"), ("f8", 0x64, "F8"), ("f9", 0x65, "F9"), ("eisu", 0x66, "Lang2"), ("f11", 0x67, "F11"),
    ("kana", 0x68, "Lang1"), ("f13", 0x69, "F13"), ("f16", 0x6a, "F16"), ("f14", 0x6b, "F14"), ("f10", 0x6d, "F10"),
    ("f12", 0x6f, "F12"), ("f15", 0x71, "F15"), ("help", 0x72, "Help"), ("home", 0x73, "Home"),
    ("pageUp", 0x74, "PageUp"), ("forwardDelete", 0x75, "Delete"), ("f4", 0x76, "F4"), ("end", 0x77, "End"),
    ("f2", 0x78, "F2"), ("pageDown", 0x79, "PageDown"), ("f1", 0x7a, "F1"), ("leftArrow", 0x7b, "ArrowLeft"),
    ("rightArrow", 0x7c, "ArrowRight"), ("downArrow", 0x7d, "ArrowDown"), ("upArrow", 0x7e, "ArrowUp"),
    // Japanese layouts: these sit where ANSI has [ = '.
    ("atSign", 0x21, "BracketLeft"), ("caret", 0x18, "Equal"), ("colon", 0x27, "Quote"),
];

/// Just enough JSON for Clipy's key combos.
#[derive(Debug, Clone, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    Arr(Vec<Json>),
    Obj(Vec<(String, Json)>),
}

impl Json {
    pub fn parse(s: &str) -> Option<Json> {
        let mut p = s.chars().peekable();
        let v = Self::value(&mut p)?;
        Self::ws(&mut p);
        p.peek().is_none().then_some(v)
    }

    pub fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Json::Obj(kv) => kv.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    fn ws(p: &mut std::iter::Peekable<std::str::Chars>) {
        while p.next_if(|c| c.is_whitespace()).is_some() {}
    }

    fn value(p: &mut std::iter::Peekable<std::str::Chars>) -> Option<Json> {
        Self::ws(p);
        match *p.peek()? {
            '{' => {
                p.next();
                let mut kv = Vec::new();
                loop {
                    Self::ws(p);
                    if p.next_if_eq(&'}').is_some() {
                        return Some(Json::Obj(kv));
                    }
                    let Json::Str(k) = Self::value(p)? else { return None };
                    Self::ws(p);
                    p.next_if_eq(&':')?;
                    kv.push((k, Self::value(p)?));
                    Self::ws(p);
                    p.next_if_eq(&',');
                }
            }
            '[' => {
                p.next();
                let mut items = Vec::new();
                loop {
                    Self::ws(p);
                    if p.next_if_eq(&']').is_some() {
                        return Some(Json::Arr(items));
                    }
                    items.push(Self::value(p)?);
                    Self::ws(p);
                    p.next_if_eq(&',');
                }
            }
            '"' => {
                p.next();
                let mut s = String::new();
                loop {
                    match p.next()? {
                        '"' => return Some(Json::Str(s)),
                        '\\' => match p.next()? {
                            'n' => s.push('\n'),
                            't' => s.push('\t'),
                            'r' => s.push('\r'),
                            'b' => s.push('\u{8}'),
                            'f' => s.push('\u{c}'),
                            'u' => {
                                let hex: String = (0..4).filter_map(|_| p.next()).collect();
                                s.push(char::from_u32(u32::from_str_radix(&hex, 16).ok()?).unwrap_or('\u{fffd}'));
                            }
                            c => s.push(c),
                        },
                        c => s.push(c),
                    }
                }
            }
            c if c == '-' || c.is_ascii_digit() => {
                let mut n = String::new();
                while let Some(c) = p.next_if(|c| c.is_ascii_digit() || "+-.eE".contains(*c)) {
                    n.push(c);
                }
                n.parse().ok().map(Json::Num)
            }
            _ => {
                let word: String = std::iter::from_fn(|| p.next_if(|c| c.is_ascii_alphabetic())).collect();
                match word.as_str() {
                    "true" => Some(Json::Bool(true)),
                    "false" => Some(Json::Bool(false)),
                    "null" => Some(Json::Null),
                    _ => None,
                }
            }
        }
    }
}
