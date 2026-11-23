//! Global shortcuts: what each one opens, lists of them ("cmd+alt+v,
//! ctrl+shift+cmd+v") and how they read in menus ("⌘⌥V").

use crate::settings::Settings;
use global_hotkey::hotkey::HotKey;

/// What a global shortcut opens.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    History,
    /// The snippet folders, at the top level.
    Folders,
    /// Straight into this folder ("Clients/Formosa").
    Folder(String),
    Prefs,
}

impl Action {
    /// "the history", "the folder Clients/Formosa": for messages.
    pub fn describe(&self) -> String {
        match self {
            Action::History => "the history".into(),
            Action::Folders => "the folders".into(),
            Action::Folder(path) => format!("the folder {path}"),
            Action::Prefs => "the preferences".into(),
        }
    }
}

/// The shortcuts in a list ("cmd+alt+v, ctrl+shift+cmd+v"), trimmed. A comma
/// right after a "+" is the comma key ("cmd+,"), not a separator; stray
/// commas are dropped.
pub fn split(list: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for ch in list.chars() {
        let t = cur.trim();
        if ch == ',' && t.is_empty() {
            continue;
        } else if ch == ',' && !t.ends_with('+') {
            out.push(t.to_string());
            cur.clear();
        } else {
            cur.push(ch);
        }
    }
    if !cur.trim().is_empty() {
        out.push(cur.trim().to_string());
    }
    out
}

pub fn join(specs: &[String]) -> String {
    specs.join(", ")
}

/// Every shortcut the settings ask for, with what it opens, in order.
pub fn bindings(s: &Settings) -> Vec<(String, Action)> {
    let mut out: Vec<(String, Action)> = split(&s.hotkey).into_iter().map(|k| (k, Action::History)).collect();
    out.extend(split(&s.folder_hotkey).into_iter().map(|k| (k, Action::Folders)));
    out.extend(split(&s.prefs_hotkey).into_iter().map(|k| (k, Action::Prefs)));
    for f in &s.folder_shortcuts {
        out.extend(split(&f.hotkey).into_iter().map(|k| (k, Action::Folder(f.folder.clone()))));
    }
    out
}

pub fn parse(spec: &str) -> Result<HotKey, String> {
    spec.parse::<HotKey>().map_err(|e| format!("bad shortcut {spec:?}: {e}"))
}

/// Every shortcut parses, the history has one, and none is used twice.
pub fn check(s: &Settings) -> Result<(), String> {
    if split(&s.hotkey).is_empty() {
        return Err("The history needs a shortcut.".into());
    }
    let mut seen: Vec<(u32, Action)> = Vec::new();
    for (spec, action) in bindings(s) {
        let id = parse(&spec)?.id();
        if let Some((_, other)) = seen.iter().find(|(i, _)| *i == id) {
            let what = if *other == action {
                format!("twice for {}", action.describe())
            } else {
                format!("for {} and for {}", other.describe(), action.describe())
            };
            return Err(format!("{} is set {what}: each shortcut can open only one thing.", label(&spec)));
        }
        seen.push((id, action));
    }
    Ok(())
}

/// "cmd+ctrl+v" → "⌃⌘V", the way menus show shortcuts.
pub fn label(spec: &str) -> String {
    let mut mods = String::new();
    let mut key = String::new();
    for part in spec.split('+').map(|p| p.trim().to_lowercase()) {
        match part.as_str() {
            "ctrl" | "control" => mods.push('⌃'),
            "alt" | "option" | "opt" => mods.push('⌥'),
            "shift" => mods.push('⇧'),
            "cmd" | "command" | "super" | "meta" | "cmdorctrl" | "cmdorcontrol" => mods.push('⌘'),
            k => key = key_label(k),
        }
    }
    // Menus order modifiers ⌃⌥⇧⌘.
    let mut m: Vec<char> = mods.chars().collect();
    m.sort_by_key(|c| "⌃⌥⇧⌘".chars().position(|x| x == *c));
    m.into_iter().collect::<String>() + &key
}

/// "⌘⌥V, ⌃⇧⌘V" for a list.
pub fn labels(list: &str) -> String {
    split(list).iter().map(|s| label(s)).collect::<Vec<_>>().join(", ")
}

fn key_label(k: &str) -> String {
    let k = k.strip_prefix("key").or_else(|| k.strip_prefix("digit")).unwrap_or(k);
    match k {
        "space" => "Space".into(),
        "enter" | "return" => "↩".into(),
        "tab" => "⇥".into(),
        "backspace" => "⌫".into(),
        "escape" | "esc" => "⎋".into(),
        "arrowup" | "up" => "↑".into(),
        "arrowdown" | "down" => "↓".into(),
        "arrowleft" | "left" => "←".into(),
        "arrowright" | "right" => "→".into(),
        "comma" => ",".into(),
        "period" => ".".into(),
        "slash" => "/".into(),
        "backslash" => "\\".into(),
        "semicolon" => ";".into(),
        "quote" => "'".into(),
        "backquote" => "`".into(),
        "minus" => "-".into(),
        "equal" => "=".into(),
        "bracketleft" => "[".into(),
        "bracketright" => "]".into(),
        _ if k.chars().count() == 1 => k.to_uppercase(),
        _ => {
            let mut c = k.chars();
            c.next().map(|f| f.to_uppercase().chain(c).collect()).unwrap_or_default()
        }
    }
}
