use crabclip::db::Db;
use crabclip::hotkeys::{self, Action};
use crabclip::settings::{FolderShortcut, Settings};

#[test]
fn lists_and_labels() {
    assert_eq!(hotkeys::split("cmd+alt+v"), vec!["cmd+alt+v"]);
    assert_eq!(hotkeys::split(" alt+shift+cmd+v ,ctrl+shift+cmd+v, "), vec!["alt+shift+cmd+v", "ctrl+shift+cmd+v"]);
    assert_eq!(hotkeys::split("cmd+,, ctrl+,"), vec!["cmd+,", "ctrl+,"], "a comma after + is the comma key");
    assert!(hotkeys::split("  ").is_empty());
    assert_eq!(hotkeys::split(",cmd+v,, ctrl+v,"), vec!["cmd+v", "ctrl+v"], "stray commas are dropped");
    assert_eq!(hotkeys::label("cmd+ctrl+v"), "⌃⌘V");
    assert_eq!(hotkeys::label("ctrl+alt+shift+cmd+f"), "⌃⌥⇧⌘F");
    assert_eq!(hotkeys::labels("alt+shift+cmd+v, cmd+shift+Space"), "⌥⇧⌘V, ⇧⌘Space");
    assert_eq!(hotkeys::labels("cmd+Comma, cmd+,, alt+BracketLeft, F5"), "⌘,, ⌘,, ⌥[, F5");
}

#[test]
fn bindings_and_duplicates() {
    let s = Settings {
        hotkey: "alt+shift+cmd+v, ctrl+shift+cmd+v".into(),
        folder_hotkey: "cmd+shift+b".into(),
        prefs_hotkey: "ctrl+alt+shift+cmd+v".into(),
        folder_shortcuts: vec![
            FolderShortcut { folder: "Formosa".into(), hotkey: "ctrl+alt+shift+cmd+f".into() },
            FolderShortcut { folder: "Clients/Corrientes".into(), hotkey: String::new() },
        ],
        ..Settings::default()
    };
    assert_eq!(
        hotkeys::bindings(&s),
        vec![
            ("alt+shift+cmd+v".to_string(), Action::History),
            ("ctrl+shift+cmd+v".to_string(), Action::History),
            ("cmd+shift+b".to_string(), Action::Folders),
            ("ctrl+alt+shift+cmd+v".to_string(), Action::Prefs),
            ("ctrl+alt+shift+cmd+f".to_string(), Action::Folder("Formosa".into())),
        ],
        "a row without a shortcut yet binds nothing"
    );
    assert_eq!(hotkeys::check(&s), Ok(()));

    // The same keys in another order or case are the same shortcut.
    let twice = Settings { folder_hotkey: "Shift+Cmd+Alt+V".into(), ..s.clone() };
    let e = hotkeys::check(&twice).unwrap_err();
    assert!(e.contains("⌥⇧⌘V") && e.contains("the history") && e.contains("the folders"), "{e}");
    let folder = Settings {
        folder_shortcuts: vec![
            FolderShortcut { folder: "A".into(), hotkey: "cmd+alt+1".into() },
            FolderShortcut { folder: "B".into(), hotkey: "alt+cmd+1".into() },
        ],
        ..s.clone()
    };
    let e = hotkeys::check(&folder).unwrap_err();
    assert!(e.contains("the folder A") && e.contains("the folder B"), "{e}");
    let e = hotkeys::check(&Settings { hotkey: "cmd+alt+v, cmd+alt+v".into(), ..Settings::default() }).unwrap_err();
    assert!(e.contains("twice for the history"), "{e}");

    assert!(hotkeys::check(&Settings { prefs_hotkey: "cmd+banana".into(), ..s.clone() }).unwrap_err().contains("bad shortcut"));
    assert!(hotkeys::check(&Settings { hotkey: " ".into(), ..s }).is_err(), "the history needs one");
}

#[test]
fn shortcut_settings_roundtrip_and_old_settings() {
    let db = Db::open_memory().unwrap();
    // Settings saved before these existed: one history and one folders shortcut.
    db.set_setting("hotkey", "cmd+alt+v").unwrap();
    db.set_setting("folder_hotkey", "cmd+alt+b").unwrap();
    let old = Settings::load(&db);
    assert_eq!((old.hotkey.as_str(), old.folder_hotkey.as_str(), old.prefs_hotkey.as_str()), ("cmd+alt+v", "cmd+alt+b", ""));
    assert!(old.folder_shortcuts.is_empty());
    assert_eq!(hotkeys::bindings(&old).len(), 2);

    let s = Settings {
        hotkey: "alt+shift+cmd+v, ctrl+shift+cmd+v".into(),
        prefs_hotkey: "ctrl+alt+shift+cmd+v".into(),
        folder_shortcuts: vec![
            FolderShortcut { folder: "Clients/Formosa, S.A.".into(), hotkey: "ctrl+alt+shift+cmd+f".into() },
            FolderShortcut { folder: "General".into(), hotkey: String::new() },
        ],
        ..old
    };
    s.save(&db);
    assert_eq!(Settings::load(&db), s);

    // Hand-edited: a line without a folder is dropped, spaces trimmed.
    db.set_setting("folder_shortcuts", "cmd+1\t  Notes \nbroken line\ncmd+2\t\n").unwrap();
    assert_eq!(Settings::load(&db).folder_shortcuts, vec![FolderShortcut { folder: "Notes".into(), hotkey: "cmd+1".into() }]);
}
