# crabclip 🦀📋

A forever clipboard history for macOS: a native AppKit menu bar app written in
Rust. Every copy goes straight to a local SQLite database, so nothing is lost
on a crash or restart, and years of history stay searchable without eating
memory.

- **⌘⌥V** opens the history (at its last position; first time under the menu bar crab claw).
  Global shortcuts are yours to set: one or more for the history, the folders and
  the preferences, and one for any folder you open often.
- Every copy is written to SQLite immediately: all pasteboard representations
  byte-for-byte (zstd-compressed when it helps), source app, window title,
  document path, source URL, timestamps. Recall puts back exactly what was copied.
- Text found in images (Apple Vision OCR) is searchable.
- Tags, stars, an app filter, and an encrypted `password` tag for secrets.
- Snippet folders: file clips in nested folders and browse them from the
  keyboard (type `/`), with their own global shortcut, or jump straight into
  one with its shortcut.
- Imports the history of Clipy (history, snippet folders and shortcuts) or Collective.app
  if you are coming from one of them.
- Runs as a login agent that launchd restarts if it ever crashes.
- Everything stays on your Mac: no network access, no telemetry.

## Keys

| Key | Action |
| --- | --- |
| ↑ ↓ / PgUp PgDn | move (also while typing in search) |
| ⏎ / double-click | copy to the clipboard and close · ⇧⏎ as plain text (Preferences: "⏎ pastes" pastes into the app in front instead) |
| ⌘F, or just start typing | search |
| `/` | browse the snippet folders (below) |
| ⇧⌘S or click the ☆ next to the app filter | starred-only filter (off again at every launch) |
| space, ⌘S, or click ☆ | star / unstar |
| ⌘1…⌘9 | the nth clip, like ⏎ |
| ⌘⏎ | the other one: paste when ⏎ copies, copy when ⏎ pastes |
| ⌘C | copy to the clipboard |
| ⌫ | delete clip (undo for 4 s: ⌘Z or the pill in the bottom bar) |
| → ← | open / fold the preview pane (or click its button, top right) |
| ⌘T | edit the clip's tags (⏎ or Esc: back to the list) |
| ⌘L | file the clip in folders, e.g. `Clients/ssh` (⏎ or Esc: back to the list) |
| ⌘P | pin (window stays open) |
| Esc | clear search, then close |
| ⌘+ / ⌘− / ⌘0 | interface size bigger / smaller / 100% |
| ⌘, | preferences (again, or ⌘W: close them and go back to the history) |

Tags: shown as pills on the clip and under the preview. `url` is added to every
link automatically. Tagging a clip `password` encrypts it (XChaCha20-Poly1305,
key in the login Keychain): the list shows dots, the preview hides it, its text
leaves the search index, ⏎ still copies it (marked concealed so other clipboard
tools skip it); removing the tag stores it in the clear again. Sealing wipes the old copy from
the database files (secure_delete, index rewrite, WAL checkpoint), but backups
made before (Time Machine, APFS snapshots) still hold it. While typing in
search, matching tags drop down: Tab or a click filters by that tag.

Folders: a clip can be filed in one or more folders, nested as deep as you
like (⌘L, paths like `Clients/Formosa/ssh`, completed as you type). A search
starting with `/` browses them instead of the history: `/` lists the top
level, ↑ ↓ move, → opens the selected folder (the search becomes
`/Clients/`), ← goes back up, and text after the last `/` filters the
folder. While a folder is selected the right pane shows what it holds; on a
snippet it shows the value and ⏎ copies it like any clip. Filed clips
are never pruned, like starred ones; a folder goes away when its last clip
leaves it through ⌘L or a delete.

Snippet editor (Preferences › Snippets), like Clipy's: folders and snippets in
a tree. Select a snippet to edit its title and text (moving to another row or
Save, ⌘S, keeps the changes; the text is that clip's, in the history too); a
folder to rename it or set its shortcut. "+ Folder" adds one inside the folder
selected, "+ Snippet" writes a new one in it, − takes a snippet out of its
folder (the clip stays in the history) or deletes a folder with the folders in
it. Drag snippets and folders to reorder them or move them into another
folder. Folders made or emptied here stay until deleted.

Global shortcuts (Preferences › General and › Snippets): "Show history with",
"Show folders with" and "Show preferences with" take one shortcut or several
separated by commas (`alt+shift+cmd+v, ctrl+shift+cmd+v`), in global-hotkey
syntax: modifiers `cmd`, `alt`, `ctrl`, `shift` and a key (`v`, `1`, `Space`,
`F5`, `Comma`, `ArrowUp`…). A folder's shortcut (set on the folder in the
snippet editor) opens the history straight into it. Renamed or moved in the
editor, the folder keeps its shortcut; deleted, the shortcut goes. A folder that
goes away otherwise (its last clip left through ⌘L) keeps its shortcut, listed
under the editor to remove it, which opens its nearest parent still there, or
the top level, until a folder with that path is back. The same shortcut can't
open two things.

Clipy: Preferences › Import › Import from Clipy brings in its history (every format of
each clip) and its snippets, as folders in Clipy's order with their titles
(asked once at launch when Clipy's data is found; `crabclip --import-clipy`
from a terminal). It reads the current Clipy's `sqlite.db`; for an older
Clipy, open the current version once (it converts its data) or import a
snippets export (Clipy › Edit Snippets… › Export, an `.xml` file).
Clipy's shortcuts come along and replace crabclip's: its main menu and history
menu shortcuts both open the history, its snippets menu shortcut the folders,
its settings shortcut the preferences, and each folder shortcut opens that
folder (matched by Clipy's folder id, so not from an `.xml` export). Clipy's
clear history and edit snippets shortcuts have no counterpart and are left out
(the log says which). If Clipy is still running crabclip offers to quit it, so
the two don't both answer the same keys; take it out of your login items too. Shortcut keys are
places on the keyboard (a US layout's, as global-hotkey names them), while Clipy
remembers the character: on a layout that moves letters (AZERTY, QWERTZ,
Dvorak) check the imported ones in Preferences.

Search: words match anywhere (text, OCR text, app, window title, path, URL;
case and accent insensitive). Operators: `"exact phrase"`, `-exclude`,
`app:chrome`, `type:image|text|url|file`, `is:star`, `tag:name`. Folder
names and snippet titles are searchable words too.

Preferences has five tabs: General (history and preferences shortcuts, launch
at login, ⏎ behavior, Accessibility), Appearance (where the window opens, row
and text size, and an interface size slider, 70–125% in 5% steps below 100%,
that zooms the whole window: text, rows, icons and the window itself), History
(how long clips are kept, ignored apps, text in images, image size and
compression, the database), Snippets (the folders shortcut and the snippet
editor) and Import (Clipy, Collective). It opens on the tab used last.

## Install

Requires macOS 13 or later and a Rust toolchain ([rustup](https://rustup.rs)).

```sh
git clone https://github.com/rodrigolive/crabclip
cd crabclip
make install    # release build, signed, copied to /Applications and started
```

The build is signed with the first "Developer ID Application" identity in your
keychain, or ad-hoc if there is none (`SIGN_ID=- make install` forces ad-hoc).
With an ad-hoc signature macOS may ask again for permissions after a rebuild.

Permissions: **Accessibility** is only needed for "⏎ pastes" (it sends ⌘V to
the app in front); without it crabclip copies and you paste. Importing from
Collective reads its sandbox folder, so macOS asks once for access to other
apps' data.

## Build

`make` lists the targets (`build`, `run`, `test`, `install`, `uninstall`,
`import-collective`, `import-clipy`, `unload-collective`, `release`, `logs`, `stats`).

Data: `~/Library/Application Support/crabclip/crabclip.db` · Log: `~/Library/Logs/crabclip.log`

## License

[MIT](LICENSE)
