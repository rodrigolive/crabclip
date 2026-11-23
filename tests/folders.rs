use crabclip::db::{AppInfo, Db, Entry, Filter, Kind, Rep};
use crabclip::ingest::{Capture, build};
use crabclip::settings::Settings;
use rusqlite::{Connection, params};

fn insert(db: &Db, ts: i64, text: &str) -> i64 {
    let cap = Capture {
        ts,
        reps: vec![Rep { item: 0, uti: "public.utf8-plain-text".into(), data: text.as_bytes().to_vec() }],
        app: AppInfo { bundle: Some("com.test.app".into()), name: Some("App".into()), path: None },
        ..Default::default()
    };
    db.insert(&build(cap, &Settings::default()).unwrap(), 3).unwrap().id()
}

fn search(db: &Db, q: &str) -> Vec<i64> {
    db.search(&Filter { query: q, starred: false, app: None }, 100).unwrap()
}

fn labels(entries: &[Entry]) -> Vec<String> {
    entries
        .iter()
        .map(|e| match e {
            Entry::Folder { name, items, .. } => format!("{name}/ ({items})"),
            Entry::Snippet { title, .. } => title.clone(),
        })
        .collect()
}

#[test]
fn folders_nest_list_and_prune() {
    let db = Db::open_memory().unwrap();
    let a = insert(&db, 1, "ssh deploy@prod.example.com");
    let b = insert(&db, 2, "VPN secret thing");
    let c = insert(&db, 3, "unfiled clip");

    let got = db.set_folders(a, &[" Clients / Formosa ".into(), "clients/formosa".into(), "Servers".into()]).unwrap();
    assert_eq!(got, vec!["Clients/Formosa", "Servers"], "paths are trimmed, case-insensitive, deduplicated");
    db.set_folders(b, &["Clients/Formosa".into()]).unwrap();
    db.file_snippet(db.folder_id("Clients/Formosa").unwrap().unwrap(), b, "2. Tunnel", None).unwrap();

    assert_eq!(labels(&db.folder_entries(0, "").unwrap()), vec!["Clients/ (1)", "Servers/ (1)"]);
    let clients = db.folder_id("clients").unwrap().unwrap();
    assert_eq!(labels(&db.folder_entries(clients, "").unwrap()), vec!["Formosa/ (2)"]);
    let formosa = db.folder_id("Clients/Formosa").unwrap().unwrap();
    assert_eq!(labels(&db.folder_entries(formosa, "").unwrap()), vec!["ssh deploy@prod.example.com", "2. Tunnel"]);
    assert_eq!(labels(&db.folder_entries(formosa, "tun").unwrap()), vec!["2. Tunnel"], "filter by name, any case");
    assert_eq!(db.folder_path(formosa).unwrap(), "Clients/Formosa");
    assert_eq!(
        db.all_folder_paths().unwrap(),
        vec![("Clients".to_string(), 0), ("Clients/Formosa".to_string(), 2), ("Servers".to_string(), 1)]
    );

    // Folder names and snippet titles are searchable.
    assert_eq!(search(&db, "formosa"), vec![b, a]);
    assert_eq!(search(&db, "tunnel"), vec![b]);

    let d = db.detail(b).unwrap().unwrap();
    assert_eq!(d.folders, vec!["Clients/Formosa"]);
    assert_eq!(d.title.as_deref(), Some("2. Tunnel"));

    // Moving a clip up into the parent of its only folder.
    let x = insert(&db, 4, "deep one");
    db.set_folders(x, &["X/Y/Z".into()]).unwrap();
    assert_eq!(db.set_folders(x, &["X/Y".into()]).unwrap(), vec!["X/Y"]);
    assert_eq!(db.folder_id("X/Y/Z").unwrap(), None);
    db.delete(x).unwrap();
    assert_eq!(db.folder_id("X").unwrap(), None, "deleting a folder's last clip removes the folder");

    // Taking the last clip out of a folder removes it, and empty parents too.
    db.set_folders(a, &["Clients/Formosa".into()]).unwrap();
    assert_eq!(db.folder_id("Servers").unwrap(), None);
    db.set_folders(a, &[]).unwrap();
    db.set_folders(b, &[]).unwrap();
    assert!(db.folder_entries(0, "").unwrap().is_empty());
    assert!(db.all_folder_paths().unwrap().is_empty());

    // Filed clips survive pruning like starred ones; deleting a clip unfiles it.
    db.set_folders(c, &["Keep".into()]).unwrap();
    assert_eq!(db.clear(Some(i64::MAX), false).unwrap(), 2);
    assert_eq!(db.count().unwrap(), 1);
    db.make_folder("Empty/Tree").unwrap();
    db.clear(None, true).unwrap();
    assert!(db.all_folder_paths().unwrap().is_empty(), "clearing everything leaves no folders");
}

/// A database laid out like Clipy's (its migrations V1, V3 and V4). Clipy
/// registers uuid() itself; the importer opens the file without it.
fn clipy_db(path: &std::path::Path) {
    let c = Connection::open(path).unwrap();
    c.create_scalar_function("uuid", 0, rusqlite::functions::FunctionFlags::SQLITE_UTF8, |_| Ok("x".to_string()))
        .unwrap();
    c.execute_batch(
        r#"
        CREATE TABLE "pasteboardHistories" (
          "id" TEXT PRIMARY KEY NOT NULL ON CONFLICT REPLACE DEFAULT (uuid()),
          "title" TEXT NOT NULL ON CONFLICT REPLACE DEFAULT '',
          "pasteboardTypes" TEXT NOT NULL ON CONFLICT REPLACE DEFAULT '[]',
          "deviceID" TEXT,
          "updateAt" INTEGER NOT NULL ON CONFLICT REPLACE DEFAULT (unixepoch())
        ) STRICT;
        CREATE TABLE "pasteboardHistoryAssets" (
          "id" TEXT PRIMARY KEY NOT NULL ON CONFLICT REPLACE DEFAULT (uuid()),
          "pasteboardHistoryID" TEXT NOT NULL,
          "index" INTEGER NOT NULL ON CONFLICT REPLACE DEFAULT 0,
          "pasteboardType" TEXT NOT NULL ON CONFLICT REPLACE DEFAULT '',
          "data" BLOB NOT NULL,
          FOREIGN KEY ("pasteboardHistoryID") REFERENCES "pasteboardHistories" ("id") ON DELETE CASCADE
        ) STRICT;
        CREATE TABLE "snippetFolders" (
          "id" TEXT PRIMARY KEY NOT NULL ON CONFLICT REPLACE DEFAULT (uuid()),
          "title" TEXT NOT NULL ON CONFLICT REPLACE DEFAULT '',
          "index" INTEGER NOT NULL ON CONFLICT REPLACE DEFAULT 0,
          "isEnabled" INTEGER NOT NULL ON CONFLICT REPLACE DEFAULT 1
        ) STRICT;
        CREATE TABLE "snippets" (
          "id" TEXT PRIMARY KEY NOT NULL ON CONFLICT REPLACE DEFAULT (uuid()),
          "folderID" TEXT NOT NULL,
          "title" TEXT NOT NULL ON CONFLICT REPLACE DEFAULT '',
          "content" TEXT NOT NULL ON CONFLICT REPLACE DEFAULT '',
          "index" INTEGER NOT NULL ON CONFLICT REPLACE DEFAULT 0,
          "isEnabled" INTEGER NOT NULL ON CONFLICT REPLACE DEFAULT 1,
          FOREIGN KEY ("folderID") REFERENCES "snippetFolders" ("id") ON DELETE CASCADE
        ) STRICT;
        ALTER TABLE "pasteboardHistories" ADD COLUMN "createdAt" INTEGER NOT NULL ON CONFLICT REPLACE DEFAULT 0;
        ALTER TABLE "pasteboardHistories" ADD COLUMN "ocrText" TEXT;
        "#,
    )
    .unwrap();
    let hist = |id: &str, t: i64| {
        c.execute(
            r#"INSERT INTO pasteboardHistories(id, title, updateAt, createdAt) VALUES(?, '', ?, ?)"#,
            params![id, t, t - 10],
        )
        .unwrap();
    };
    let asset = |h: &str, i: i64, uti: &str, data: &[u8]| {
        c.execute(
            r#"INSERT INTO pasteboardHistoryAssets(id, pasteboardHistoryID, "index", pasteboardType, data) VALUES(?, ?, ?, ?, ?)"#,
            params![format!("{h}-{i}"), h, i, uti, data],
        )
        .unwrap();
    };
    hist("h1", 1_700_000_000);
    asset("h1", 0, "public.utf8-plain-text", b"hello from clipy");
    asset("h1", 1, "public.rtf", br"{\rtf1 hello from clipy}");
    hist("h2", 1_700_000_100);
    asset("h2", 0, "public.file-url", b"file:///tmp/a.txt");
    asset("h2", 1, "public.file-url", b"file:///tmp/b.txt");
    // Converted from old Clipy: files only as a plist of paths.
    hist("h3", 1_700_000_200);
    let mut names = Vec::new();
    plist::Value::Array(vec!["/tmp/my doc.txt".into()]).to_writer_binary(&mut names).unwrap();
    asset("h3", 0, "NSFilenamesPboardType", &names);
    let folder = |id: &str, title: &str, i: i64| {
        c.execute(r#"INSERT INTO snippetFolders(id, title, "index") VALUES(?, ?, ?)"#, params![id, title, i]).unwrap();
    };
    let snippet = |f: &str, title: &str, content: &str, i: i64| {
        c.execute(
            r#"INSERT INTO snippets(id, folderID, title, content, "index") VALUES(?, ?, ?, ?, ?)"#,
            params![format!("{f}-{i}"), f, title, content, i],
        )
        .unwrap();
    };
    folder("f2", "General", 1);
    folder("f1", "Formosa", 0);
    folder("f3", " My  Work ", 2);
    snippet("f3", "todo", "todo list", 0);
    snippet("f1", "2. VPN Password", "vpn-pass-77", 1);
    snippet("f1", "1. API Key", "api-key-123", 0);
    snippet("f2", "hello", "hello from clipy", 0);
    snippet("f2", "hello again", "hello from clipy", 1);
}

#[test]
fn imports_clipy_database() {
    let dir = std::env::temp_dir().join(format!("crabclip-clipy-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("sqlite.db");
    let _ = std::fs::remove_file(&path);
    clipy_db(&path);

    let db = Db::open_memory().unwrap();
    let s = Settings::default();
    let st = crabclip::clipy::import(&path, None, &db, &s, |_| {}).unwrap();
    assert_eq!((st.history, st.added, st.merged, st.skipped), (3, 3, 0, 0));
    assert_eq!((st.folders, st.snippets, st.duplicates), (3, 5, 1));

    // History: the text clip with its RTF, the two files as two items.
    let text = search(&db, "hello from clipy");
    assert_eq!(text.len(), 1, "the snippet with the same text is the same clip");
    let d = db.detail(text[0]).unwrap().unwrap();
    assert_eq!(d.copied_at, 1_700_000_000_000);
    assert!(d.types.contains("public.rtf"));
    assert_eq!(d.folders, vec!["General"]);
    let old = search(&db, "my doc");
    assert_eq!(old.len(), 1, "old Clipy file clips become files");
    assert_eq!(db.detail(old[0]).unwrap().unwrap().kind, Kind::File);
    let files: Vec<i64> = search(&db, "type:file").into_iter().filter(|id| *id != old[0]).collect();
    assert_eq!(files.len(), 1);
    let reps = db.reps(files[0]).unwrap();
    assert_eq!(reps.iter().map(|r| r.item).collect::<Vec<_>>(), vec![0, 1]);
    assert_eq!(db.detail(files[0]).unwrap().unwrap().kind, Kind::File);

    // Snippets: Clipy's folders and order, its titles, dated before the history.
    assert_eq!(labels(&db.folder_entries(0, "").unwrap()), vec!["Formosa/ (2)", "General/ (1)", "My Work/ (1)"]);
    let general = db.folder_id("General").unwrap().unwrap();
    assert_eq!(labels(&db.folder_entries(general, "").unwrap()), vec!["hello again"], "same text: merged, later title");
    let formosa = db.folder_id("Formosa").unwrap().unwrap();
    assert_eq!(labels(&db.folder_entries(formosa, "").unwrap()), vec!["1. API Key", "2. VPN Password"]);
    let key = search(&db, "api-key-123")[0];
    let kd = db.detail(key).unwrap().unwrap();
    assert!(kd.copied_at < 1_700_000_000_000);
    assert_eq!(kd.app_name.as_deref(), Some("Clipy"));
    assert_eq!(kd.title.as_deref(), Some("1. API Key"));

    // Importing again changes nothing.
    let n = db.count().unwrap();
    let before: Vec<(i64, i64, i64)> = db
        .conn
        .prepare("SELECT created_at, copied_at, copy_count FROM clips ORDER BY id")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    let st = crabclip::clipy::import(&path, None, &db, &s, |_| {}).unwrap();
    assert_eq!((st.added, st.merged), (0, 3));
    assert_eq!(db.count().unwrap(), n);
    let after: Vec<(i64, i64, i64)> = db
        .conn
        .prepare("SELECT created_at, copied_at, copy_count FROM clips ORDER BY id")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(before, after, "dates and copy counts don't move on a re-import");
    assert_eq!(labels(&db.folder_entries(formosa, "").unwrap()), vec!["1. API Key", "2. VPN Password"]);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn imports_clipy_snippets_xml() {
    let dir = std::env::temp_dir().join(format!("crabclip-clipy-xml-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("snippets.xml");
    std::fs::write(
        &path,
        r#"<?xml version="1.0" encoding="UTF-8"?>
<folders>
  <folder><title>Formosa</title><snippets>
    <snippet><title>1. API Key</title><content>key &amp; more</content></snippet>
    <snippet><title>ssh test</title><content>ssh me@host</content></snippet>
  </snippets></folder>
  <folder><title>a/b</title><snippets><snippet><title>x</title><content>slash folder</content></snippet></snippets></folder>
</folders>"#,
    )
    .unwrap();
    let db = Db::open_memory().unwrap();
    let st = crabclip::clipy::import(&path, None, &db, &Settings::default(), |_| {}).unwrap();
    assert_eq!((st.folders, st.snippets), (2, 3));
    let formosa = db.folder_id("Formosa").unwrap().unwrap();
    assert_eq!(labels(&db.folder_entries(formosa, "").unwrap()), vec!["1. API Key", "ssh test"]);
    assert_eq!(search(&db, "key & more").len(), 1);
    assert_eq!(labels(&db.folder_entries(0, "").unwrap()), vec!["Formosa/ (2)", "a∕b/ (1)"], "a / in a name doesn't nest");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn nearest_folder_for_a_gone_folder() {
    let db = Db::open_memory().unwrap();
    let a = insert(&db, 1, "ssh deploy@prod.example.com");
    db.set_folders(a, &["Clients/Formosa".into()]).unwrap();
    let formosa = db.folder_id("Clients/Formosa").unwrap().unwrap();
    let clients = db.folder_id("Clients").unwrap().unwrap();
    assert_eq!(db.nearest_folder("clients/formosa").unwrap(), formosa);
    assert_eq!(db.nearest_folder("Clients/Formosa/Gone/Deeper").unwrap(), formosa);
    assert_eq!(db.nearest_folder("Clients/Renamed").unwrap(), clients);
    assert_eq!(db.nearest_folder("Nowhere").unwrap(), 0);
    assert_eq!(db.nearest_folder("").unwrap(), 0);
}

fn combo(json: &str) -> Result<String, String> {
    crabclip::clipy::key_combo(&crabclip::clipy::Json::parse(json).expect("json"))
}

#[test]
fn clipy_key_combos() {
    // Carbon modifiers: cmd 256, shift 512, option 2048, control 4096.
    assert_eq!(combo(r#"{"doubledModifiers":false,"key":"v","modifiers":2816}"#).unwrap(), "alt+shift+cmd+v");
    assert_eq!(combo(r#"{"doubledModifiers":false,"key":"v","modifiers":4864}"#).unwrap(), "ctrl+shift+cmd+v");
    assert_eq!(combo(r#"{"key":"b","doubledModifiers":false,"modifiers":768}"#).unwrap(), "shift+cmd+b");
    assert_eq!(combo(r#" { "key" : "f", "doubledModifiers" : false, "modifiers" : 6912 } "#).unwrap(), "ctrl+alt+shift+cmd+f");
    assert_eq!(combo(r#"{"key":"comma","modifiers":256,"doubledModifiers":false}"#).unwrap(), "cmd+Comma");
    // Older Clipy stored the key code.
    assert_eq!(combo(r#"{"QWERTYKeyCode":9,"modifiers":2304,"doubledModifiers":false}"#).unwrap(), "alt+cmd+v");
    assert_eq!(combo(r#"{"keyCode":49,"modifiers":4096,"doubledModifiers":false}"#).unwrap(), "ctrl+Space");
    assert!(combo(r#"{"key":"a","modifiers":256,"doubledModifiers":true}"#).is_err(), "double-tapped modifier");
    assert!(combo(r#"{"key":"nope","modifiers":256,"doubledModifiers":false}"#).is_err());

    // Every Clipy key is one global-hotkey knows, in its own place: keys
    // with the same code (Japanese layouts' @ ^ : sit on ANSI's [ = ') are
    // the same key.
    let mut by_code = std::collections::HashMap::new();
    for (name, code, _) in crabclip::clipy::KEYS {
        let spec = combo(&format!(r#"{{"key":"{name}","modifiers":256,"doubledModifiers":false}}"#)).unwrap();
        let hk = crabclip::hotkeys::parse(&spec).unwrap_or_else(|e| panic!("{name}: {e}"));
        let first = *by_code.entry(*code).or_insert(hk.id());
        assert_eq!(first, hk.id(), "{name}: same code, same key");
        assert_eq!(by_code.values().filter(|i| **i == hk.id()).count(), 1, "{name} maps to another code's key");
        let by_number = combo(&format!(r#"{{"keyCode":{code},"modifiers":256,"doubledModifiers":false}}"#)).unwrap();
        assert_eq!(crabclip::hotkeys::parse(&by_number).unwrap().id(), hk.id());
    }
    for name in ["atSign", "caret", "colon"] {
        assert!(combo(&format!(r#"{{"key":"{name}","modifiers":256,"doubledModifiers":false}}"#)).is_ok(), "{name}");
    }
}

/// Clipy's preferences as `defaults export` gives them: JSON in data values.
fn clipy_prefs() -> plist::Dictionary {
    let mut d = plist::Dictionary::new();
    let mut data = |k: &str, v: &str| {
        d.insert(k.into(), plist::Value::Data(v.as_bytes().to_vec()));
    };
    data("mainKeyCombo", r#"{"doubledModifiers":false,"key":"v","modifiers":2816}"#);
    data("historyKeyCombo", r#"{"doubledModifiers":false,"key":"v","modifiers":4864}"#);
    data("snippetKeyCombo", r#"{"key":"b","doubledModifiers":false,"modifiers":768}"#);
    data("settingsKeyCombo", r#"{"key":"v","doubledModifiers":false,"modifiers":6912}"#);
    data("clearHistoryKeyCombo", "null");
    data("editSnippetsKeyCombo", r#"{"key":"e","doubledModifiers":false,"modifiers":768}"#);
    // Folder ids are upper case here and lower case in its database.
    data(
        "folderKeyCombos",
        r#"{"F1":{"key":"f","doubledModifiers":false,"modifiers":6912},"GONE":{"key":"g","doubledModifiers":false,"modifiers":6912},
            "F3":{"key":"w","doubledModifiers":false,"modifiers":6912}}"#,
    );
    d.insert("kCPYPrefShowStatusItemKey".into(), plist::Value::Integer(1.into()));
    d
}

#[test]
fn imports_clipy_shortcuts() {
    use crabclip::settings::FolderShortcut;
    let dir = std::env::temp_dir().join(format!("crabclip-clipy-keys-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("sqlite.db");
    let _ = std::fs::remove_file(&path);
    clipy_db(&path);

    let sc = crabclip::clipy::read_shortcuts(&clipy_prefs());
    assert_eq!(sc.history, vec!["alt+shift+cmd+v", "ctrl+shift+cmd+v"]);
    assert_eq!(sc.snippets, vec!["shift+cmd+b"]);
    assert_eq!(sc.settings, vec!["ctrl+alt+shift+cmd+v"]);
    assert_eq!(sc.folders.len(), 3);
    assert_eq!(sc.skipped.len(), 1, "edit snippets has no equivalent: {:?}", sc.skipped);
    assert!(crabclip::clipy::read_shortcuts(&plist::Dictionary::new()).is_empty());

    let db = Db::open_memory().unwrap();
    // crabclip's own: one taken over by Clipy's Formosa shortcut, one replaced.
    let mine = Settings {
        folder_shortcuts: vec![
            FolderShortcut { folder: "Elsewhere".into(), hotkey: "ctrl+alt+shift+cmd+f, cmd+alt+9".into() },
            FolderShortcut { folder: "formosa".into(), hotkey: "cmd+alt+1".into() },
        ],
        ..Settings::default()
    };
    mine.save(&db);
    let st = crabclip::clipy::import(&path, Some(&clipy_prefs()), &db, &Settings::default(), |_| {}).unwrap();
    assert_eq!((st.shortcuts, st.shortcuts_skipped), (6, 2), "edit snippets and the folder not in Clipy's database");
    assert!(st.summary().contains("6 shortcuts"), "{}", st.summary());
    let s = Settings::load(&db);
    assert_eq!(s.hotkey, "alt+shift+cmd+v, ctrl+shift+cmd+v", "Clipy's replace ⌘⌥V");
    assert_eq!(s.folder_hotkey, "shift+cmd+b");
    assert_eq!(s.prefs_hotkey, "ctrl+alt+shift+cmd+v");
    assert_eq!(
        s.folder_shortcuts,
        vec![
            FolderShortcut { folder: "Elsewhere".into(), hotkey: "cmd+alt+9".into() },
            FolderShortcut { folder: "Formosa".into(), hotkey: "ctrl+alt+shift+cmd+f".into() },
            FolderShortcut { folder: "My Work".into(), hotkey: "ctrl+alt+shift+cmd+w".into() },
        ],
        "folder paths as stored: \" My  Work \" is My Work"
    );
    assert_eq!(crabclip::hotkeys::check(&s), Ok(()));
    assert_eq!(db.nearest_folder("Formosa").unwrap(), db.folder_id("Formosa").unwrap().unwrap());

    // Again: the same.
    crabclip::clipy::import(&path, Some(&clipy_prefs()), &db, &Settings::default(), |_| {}).unwrap();
    assert_eq!(Settings::load(&db), s);

    // A snippets export has no folder ids: the other shortcuts still come in.
    let xml = dir.join("snippets.xml");
    std::fs::write(&xml, "<folders><folder><title>Formosa</title><snippets/></folder></folders>").unwrap();
    let db = Db::open_memory().unwrap();
    let st = crabclip::clipy::import(&xml, Some(&clipy_prefs()), &db, &Settings::default(), |_| {}).unwrap();
    assert_eq!((st.shortcuts, st.shortcuts_skipped), (4, 4));
    let s = Settings::load(&db);
    assert_eq!(s.prefs_hotkey, "ctrl+alt+shift+cmd+v");
    assert!(s.folder_shortcuts.is_empty());

    // Clipy without a history shortcut: crabclip's stays, and a Clipy one on
    // the same keys is left out rather than taking it.
    let db = Db::open_memory().unwrap();
    let mut p = plist::Dictionary::new();
    p.insert("snippetKeyCombo".into(), plist::Value::Data(br#"{"key":"v","modifiers":2304,"doubledModifiers":false}"#.to_vec()));
    p.insert("settingsKeyCombo".into(), plist::Value::Data(br#"{"key":"p","modifiers":2304,"doubledModifiers":false}"#.to_vec()));
    let st = crabclip::clipy::import(&xml, Some(&p), &db, &Settings::default(), |_| {}).unwrap();
    assert_eq!((st.shortcuts, st.shortcuts_skipped), (1, 1));
    let s = Settings::load(&db);
    assert_eq!((s.hotkey.as_str(), s.folder_hotkey.as_str(), s.prefs_hotkey.as_str()), ("cmd+alt+v", "", "alt+cmd+p"));
    assert_eq!(crabclip::hotkeys::check(&s), Ok(()));

    // No preferences, or none of them shortcuts: settings untouched.
    let db = Db::open_memory().unwrap();
    crabclip::clipy::import(&xml, None, &db, &Settings::default(), |_| {}).unwrap();
    crabclip::clipy::import(&xml, Some(&plist::Dictionary::new()), &db, &Settings::default(), |_| {}).unwrap();
    assert_eq!(Settings::load(&db), Settings::default());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn snippet_editor_operations() {
    use crabclip::settings::FolderShortcut;
    let db = Db::open_memory().unwrap();
    let s = Settings::default();
    let a = insert(&db, 1, "ssh deploy@prod.example.com");
    let b = insert(&db, 2, "VPN secret thing");
    db.set_folders(a, &["Clients/Formosa".into()]).unwrap();
    db.set_folders(b, &["Clients/Formosa".into(), "General".into()]).unwrap();
    let clients = db.folder_id("Clients").unwrap().unwrap();
    let formosa = db.folder_id("Clients/Formosa").unwrap().unwrap();
    let general = db.folder_id("General").unwrap().unwrap();

    // New folders: empty ones stay; names are checked.
    let notes = db.new_folder(clients, "  Notes  ").unwrap();
    assert_eq!(db.folder_path(notes).unwrap(), "Clients/Notes");
    assert!(db.new_folder(clients, "notes").is_err(), "same name, any case");
    assert!(db.new_folder(0, "a/b").is_err());
    assert!(db.new_folder(0, "   ").is_err());
    let tree: Vec<String> = db.folder_tree().unwrap().into_iter().map(|(_, _, n)| n).collect();
    assert_eq!(tree, vec!["Clients", "Formosa", "General", "Notes"], "by place among siblings, then name");

    // Rename where it is: what it holds goes along, and is found by the new name.
    assert_eq!(db.rename_folder(clients, "Customers").unwrap(), "Customers");
    assert_eq!(db.folder_path(formosa).unwrap(), "Customers/Formosa");
    assert_eq!(search(&db, "customers"), vec![b, a]);
    assert!(search(&db, "clients").is_empty());
    assert!(db.rename_folder(formosa, "notes").is_err(), "a sibling has that name");
    assert_eq!(db.rename_folder(formosa, "FORMOSA").unwrap(), "Customers/FORMOSA", "same folder, new case");

    // Folder shortcuts follow a rename and go with a delete.
    let mut st = Settings {
        folder_shortcuts: vec![
            FolderShortcut { folder: "clients/formosa".into(), hotkey: "cmd+1".into() },
            FolderShortcut { folder: "Clients".into(), hotkey: "cmd+2".into() },
            FolderShortcut { folder: "ClientsOld".into(), hotkey: "cmd+3".into() },
        ],
        ..Settings::default()
    };
    assert!(st.folder_renamed("Clients", "Customers"));
    let paths: Vec<&str> = st.folder_shortcuts.iter().map(|f| f.folder.as_str()).collect();
    assert_eq!(paths, vec!["Customers/formosa", "Customers", "ClientsOld"], "not a folder that only starts the same");
    assert!(st.folder_deleted("customers"));
    assert_eq!(st.folder_shortcuts.len(), 1);
    assert!(!st.folder_deleted("Nowhere"));

    // Titles: set, cleared (shows the text again).
    db.set_snippet_title(formosa, a, " Deploy ").unwrap();
    assert_eq!(db.snippet_title(formosa, a).unwrap(), "Deploy");
    assert_eq!(labels(&db.folder_entries(formosa, "").unwrap()), vec!["Deploy", "VPN secret thing"]);
    db.set_snippet_title(formosa, a, "").unwrap();
    assert_eq!(labels(&db.folder_entries(formosa, "").unwrap()), vec!["ssh deploy@prod.example.com", "VPN secret thing"]);

    // Edit the text in place: same clip, folders and title kept, found by the new text.
    db.set_snippet_title(formosa, a, "Deploy").unwrap();
    let cap = |text: &str| crabclip::ingest::build(
        Capture { ts: 9, reps: vec![Rep { item: 0, uti: "public.utf8-plain-text".into(), data: text.as_bytes().to_vec() }], ..Default::default() },
        &s,
    )
    .unwrap();
    assert_eq!(db.set_text(a, &cap("ssh deploy@staging.example.com"), 3).unwrap(), a);
    assert_eq!(search(&db, "staging"), vec![a]);
    assert!(search(&db, "prod.example").is_empty());
    assert_eq!(db.snippet_title(formosa, a).unwrap(), "Deploy");
    assert_eq!(String::from_utf8(db.reps(a).unwrap()[0].data.clone()).unwrap(), "ssh deploy@staging.example.com");
    // Edited to a text another clip has: that one takes its place.
    let n = db.count().unwrap();
    assert_eq!(db.set_text(a, &cap("VPN secret thing"), 3).unwrap(), b);
    assert_eq!(db.count().unwrap(), n - 1);
    assert_eq!(labels(&db.folder_entries(formosa, "").unwrap()), vec!["Deploy"], "b was already there; the title typed wins");

    // Move keeps the title; unfile and delete_folder keep the clip in the history.
    db.set_snippet_title(general, b, "VPN").unwrap();
    db.move_snippet(general, notes, b).unwrap();
    assert_eq!(labels(&db.folder_entries(notes, "").unwrap()), vec!["VPN"]);
    assert!(db.folder_id("General").unwrap().is_some(), "left empty in the editor, the folder stays");
    db.unfile(notes, b).unwrap();
    assert!(db.folder_id("Customers/Notes").unwrap().is_some());
    assert_eq!(db.clips_under(clients).unwrap(), vec![b]);
    db.delete_folder(clients).unwrap();
    let left: Vec<String> = db.folder_tree().unwrap().into_iter().map(|(_, _, n)| n).collect();
    assert_eq!(left, vec!["General"], "Customers and all below it went");
    assert_eq!(db.count().unwrap(), n - 1, "clips stay in the history");
    assert!(db.detail(b).unwrap().unwrap().folders.is_empty());
    assert!(db.snippet_ts().unwrap() < 1);
}

#[test]
fn drag_and_drop_order() {
    let db = Db::open_memory().unwrap();
    let ids: Vec<i64> = ["one", "two", "three"].iter().enumerate().map(|(i, t)| insert(&db, i as i64 + 1, t)).collect();
    for id in &ids {
        db.set_folders(*id, &["A".into()]).unwrap();
    }
    let x = insert(&db, 9, "elsewhere");
    db.set_folders(x, &["B/C".into()]).unwrap();
    let a = db.folder_id("A").unwrap().unwrap();
    let b = db.folder_id("B").unwrap().unwrap();
    let c = db.folder_id("B/C").unwrap().unwrap();
    db.set_snippet_title(a, ids[2], "Third").unwrap();

    // Reorder within a folder.
    db.place_snippet(ids[2], a, a, 0).unwrap();
    assert_eq!(labels(&db.folder_entries(a, "").unwrap()), vec!["Third", "one", "two"]);
    db.place_snippet(ids[2], a, a, 99).unwrap();
    assert_eq!(labels(&db.folder_entries(a, "").unwrap()), vec!["one", "two", "Third"]);
    // Into another folder at a place, title kept.
    db.place_snippet(ids[2], a, c, 0).unwrap();
    assert_eq!(labels(&db.folder_entries(c, "").unwrap()), vec!["Third", "elsewhere"]);
    assert_eq!(labels(&db.folder_entries(a, "").unwrap()), vec!["one", "two"]);

    // Folders: reorder at the top level, move into another, never into itself.
    db.place_folder(b, 0, 0).unwrap();
    assert_eq!(labels(&db.folder_entries(0, "").unwrap()), vec!["B/ (1)", "A/ (2)"]);
    assert!(db.place_folder(b, c, 0).is_err(), "not inside itself");
    assert_eq!(db.place_folder(c, a, 0).unwrap(), "A/C");
    assert!(db.folder_id("B").unwrap().is_some(), "left empty in the editor, B stays");
    assert_eq!(search(&db, "A/C").len(), 2, "clips are found by the new path");
    db.new_folder(0, "C").unwrap();
    assert!(db.place_folder(c, 0, 0).is_err(), "a C is already at the top");
}

#[test]
fn editor_review_cases() {
    let db = Db::open_memory().unwrap();
    let s = Settings::default();
    // Ties in position (an import into a folder that had snippets): a drag
    // works on the order shown.
    let ids: Vec<i64> = ["a", "b", "c", "d"].iter().enumerate().map(|(i, t)| insert(&db, i as i64 + 1, t)).collect();
    db.set_folders(ids[0], &["F".into()]).unwrap();
    let f = db.folder_id("F").unwrap().unwrap();
    for (id, t) in ids.iter().zip(["a", "b", "c", "d"]) {
        db.file_snippet(f, *id, t, Some(0)).unwrap();
    }
    assert_eq!(labels(&db.folder_entries(f, "").unwrap()), vec!["a", "b", "c", "d"]);
    db.place_snippet(ids[0], f, f, 1).unwrap();
    assert_eq!(labels(&db.folder_entries(f, "").unwrap()), vec!["b", "a", "c", "d"]);

    // Dragged into a folder that already holds that clip: one entry, its title.
    db.set_folders(ids[1], &["F".into(), "G".into()]).unwrap();
    let g = db.folder_id("G").unwrap().unwrap();
    db.set_snippet_title(g, ids[1], "B in G").unwrap();
    db.place_snippet(ids[1], g, f, 0).unwrap();
    assert_eq!(labels(&db.folder_entries(f, "").unwrap()), vec!["B in G", "a", "c", "d"]);

    // Edited to another clip's text: titles typed win, tags and star come along.
    let x = insert(&db, 10, "x text");
    let y = insert(&db, 11, "y text");
    db.set_folders(x, &["F".into()]).unwrap();
    db.set_folders(y, &["F".into()]).unwrap();
    db.set_snippet_title(f, x, "typed title").unwrap();
    db.set_star(x, true).unwrap();
    db.set_tags(x, &["work".into()]).unwrap();
    let cap = crabclip::ingest::build(
        Capture { ts: 12, reps: vec![Rep { item: 0, uti: "public.utf8-plain-text".into(), data: b"y text".to_vec() }], ..Default::default() },
        &s,
    )
    .unwrap();
    assert_eq!(db.set_text(x, &cap, 3).unwrap(), y);
    assert!(db.detail(x).unwrap().is_none());
    assert_eq!(db.snippet_title(f, y).unwrap(), "typed title");
    assert!(db.tags(y).unwrap().contains(&"work".to_string()));
    assert_eq!(search(&db, "is:star"), vec![y]);
    assert!(db.is_filed(f, y) && !db.is_filed(f, x));
}
