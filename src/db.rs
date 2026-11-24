//! SQLite storage: one row per clip, every pasteboard representation stored
//! byte-for-byte (zstd-compressed when that helps), and a contentless FTS5
//! trigram index for fast substring search over text, OCR text and metadata.

use rusqlite::{Connection, OptionalExtension, params};
use std::path::{Path, PathBuf};

pub type Result<T> = rusqlite::Result<T>;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS clips(
  id          INTEGER PRIMARY KEY,
  created_at  INTEGER NOT NULL,          -- unix ms, first time seen
  copied_at   INTEGER NOT NULL,          -- unix ms, last copied/pasted (sort key)
  kind        TEXT NOT NULL,             -- text|url|image|file|other
  hash        BLOB NOT NULL UNIQUE,
  preview     TEXT NOT NULL DEFAULT '',  -- first chars of the text, for the list
  text_z      BLOB,                      -- zstd full text
  text_len    INTEGER NOT NULL DEFAULT 0,
  starred     INTEGER NOT NULL DEFAULT 0,
  copy_count  INTEGER NOT NULL DEFAULT 1,
  paste_count INTEGER NOT NULL DEFAULT 0,
  app_bundle  TEXT,
  app_name    TEXT,
  app_path    TEXT,
  window_title TEXT,
  source_path TEXT,
  source_url  TEXT,
  img_w       INTEGER,
  img_h       INTEGER,
  img_scale   REAL,
  thumb       BLOB,
  ocr_text    TEXT,
  ocr_state   INTEGER NOT NULL DEFAULT 0, -- 0 pending (images) / n.a., 1 done, 2 failed
  types       TEXT NOT NULL DEFAULT '',
  raw_size    INTEGER NOT NULL DEFAULT 0,
  stored_size INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS clips_copied ON clips(copied_at DESC);
CREATE INDEX IF NOT EXISTS clips_star ON clips(starred, copied_at DESC);
CREATE INDEX IF NOT EXISTS clips_app ON clips(app_bundle, copied_at DESC);
CREATE INDEX IF NOT EXISTS clips_ocr ON clips(ocr_state, kind);

CREATE TABLE IF NOT EXISTS reps(
  clip_id  INTEGER NOT NULL REFERENCES clips(id) ON DELETE CASCADE,
  item     INTEGER NOT NULL,             -- pasteboard item index
  uti      TEXT NOT NULL,
  codec    INTEGER NOT NULL,             -- 0 raw, 1 zstd, 2 sealed (vault: inner codec byte + data)
  raw_size INTEGER NOT NULL,
  data     BLOB NOT NULL,
  PRIMARY KEY(clip_id, item, uti)
);

CREATE TABLE IF NOT EXISTS apps(
  key  TEXT PRIMARY KEY,                 -- bundle id, or name when unknown
  name TEXT,
  path TEXT,
  icon BLOB                              -- small PNG, survives app uninstall
);

CREATE TABLE IF NOT EXISTS tags(
  clip_id INTEGER NOT NULL REFERENCES clips(id) ON DELETE CASCADE,
  tag     TEXT NOT NULL,                 -- lowercase; "url" automatic, "password" seals the clip
  PRIMARY KEY(clip_id, tag)
);
CREATE INDEX IF NOT EXISTS tags_tag ON tags(tag, clip_id);

-- Folders of snippets, nested; a clip can be filed in several of them.
CREATE TABLE IF NOT EXISTS folders(
  id     INTEGER PRIMARY KEY,
  parent INTEGER NOT NULL DEFAULT 0,     -- 0: top level
  name   TEXT NOT NULL COLLATE NOCASE,
  pos    INTEGER NOT NULL DEFAULT 0,
  UNIQUE(parent, name)
);
CREATE TABLE IF NOT EXISTS snippets(
  folder_id INTEGER NOT NULL REFERENCES folders(id) ON DELETE CASCADE,
  clip_id   INTEGER NOT NULL REFERENCES clips(id) ON DELETE CASCADE,
  title     TEXT NOT NULL DEFAULT '',  -- name in the folder ('' shows the clip's text)
  pos       INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY(folder_id, clip_id)
);
CREATE INDEX IF NOT EXISTS snippets_clip ON snippets(clip_id);

CREATE TABLE IF NOT EXISTS settings(key TEXT PRIMARY KEY, value TEXT NOT NULL);

CREATE VIRTUAL TABLE IF NOT EXISTS fts USING fts5(
  body, meta, content='', contentless_delete=1, tokenize='trigram remove_diacritics 1'
);
"#;

/// Max characters of a clip's text that go into the search index.
const INDEX_CHARS: usize = 256 * 1024;
pub const PREVIEW_CHARS: usize = 600;
/// Tag that seals a clip (encrypted, hidden, not indexed).
pub const PASSWORD: &str = "password";
/// Tag every URL clip gets.
pub const URL_TAG: &str = "url";
/// What the list shows for a sealed clip.
pub const SEALED_PREVIEW: &str = "••••••••";

/// "  #My Tag " -> "my-tag"
pub fn normalize_tag(t: &str) -> String {
    t.trim().trim_start_matches('#').split_whitespace().collect::<Vec<_>>().join("-").to_lowercase()
}

/// "/ Clients /ssh/ " -> ["Clients", "ssh"]
pub fn folder_parts(path: &str) -> Vec<String> {
    path.split('/').map(|p| p.split_whitespace().collect::<Vec<_>>().join(" ")).filter(|p| !p.is_empty()).collect()
}

/// A folder name as stored: spaces collapsed, no "/" (it separates folders).
pub fn folder_name(name: &str) -> std::result::Result<String, String> {
    let name = name.split_whitespace().collect::<Vec<_>>().join(" ");
    if name.is_empty() {
        Err("A folder needs a name.".into())
    } else if name.contains('/') {
        Err("A folder name can't have a / (it separates folders).".into())
    } else {
        Ok(name)
    }
}

/// What a folder holds: subfolders first, then its snippets.
#[derive(Clone, Debug, PartialEq)]
pub enum Entry {
    Folder { id: i64, name: String, items: i64 },
    Snippet { clip_id: i64, title: String },
}

impl Entry {
    pub fn label(&self) -> &str {
        match self {
            Entry::Folder { name, .. } => name,
            Entry::Snippet { title, .. } => title,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Text,
    Url,
    Image,
    File,
    Other,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Text => "text",
            Kind::Url => "url",
            Kind::Image => "image",
            Kind::File => "file",
            Kind::Other => "other",
        }
    }
    pub fn parse(s: &str) -> Kind {
        match s {
            "text" => Kind::Text,
            "url" => Kind::Url,
            "image" => Kind::Image,
            "file" => Kind::File,
            _ => Kind::Other,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rep {
    pub item: i64,
    pub uti: String,
    pub data: Vec<u8>,
}

#[derive(Clone, Debug, Default)]
pub struct AppInfo {
    pub bundle: Option<String>,
    pub name: Option<String>,
    pub path: Option<String>,
}

#[derive(Clone, Debug)]
pub struct NewClip {
    pub ts: i64,
    pub kind: Kind,
    pub hash: Vec<u8>,
    pub text: Option<String>,
    pub preview: String,
    pub app: AppInfo,
    pub window_title: Option<String>,
    pub source_path: Option<String>,
    pub source_url: Option<String>,
    pub img: Option<(i64, i64, f64)>,
    pub thumb: Option<Vec<u8>>,
    pub types: String,
    pub reps: Vec<Rep>,
    pub starred: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Inserted {
    New(i64),
    Existing(i64),
}

impl Inserted {
    pub fn id(self) -> i64 {
        match self {
            Inserted::New(i) | Inserted::Existing(i) => i,
        }
    }
}

#[derive(Clone, Debug)]
pub struct RowInfo {
    pub id: i64,
    pub kind: Kind,
    pub preview: String,
    pub starred: bool,
    pub app_key: String,
    pub app_name: Option<String>,
    pub app_path: Option<String>,
    pub copied_at: i64,
    pub img_w: i64,
    pub img_h: i64,
    pub img_scale: f64,
    pub thumb: Option<Vec<u8>>,
    pub tags: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct Detail {
    pub id: i64,
    pub kind: Kind,
    pub text: Option<String>,
    pub created_at: i64,
    pub copied_at: i64,
    pub app_name: Option<String>,
    pub app_bundle: Option<String>,
    pub window_title: Option<String>,
    pub source_path: Option<String>,
    pub source_url: Option<String>,
    pub ocr_text: Option<String>,
    pub types: String,
    pub raw_size: i64,
    pub stored_size: i64,
    pub copy_count: i64,
    pub paste_count: i64,
    pub img_w: i64,
    pub img_h: i64,
    pub starred: bool,
    pub tags: Vec<String>,
    /// Folder paths the clip is filed in ("Clients/ssh").
    pub folders: Vec<String>,
    /// Its name in a folder, if it was given one.
    pub title: Option<String>,
}

impl Detail {
    pub fn sealed(&self) -> bool {
        self.tags.iter().any(|t| t == PASSWORD)
    }
}

#[derive(Clone, Debug)]
pub struct AppRow {
    pub key: String,
    pub name: String,
    pub path: Option<String>,
    pub count: i64,
}

pub struct Filter<'a> {
    pub query: &'a str,
    pub starred: bool,
    pub app: Option<&'a str>,
}

pub fn compress(data: &[u8], level: i32) -> (i64, Vec<u8>) {
    if data.len() >= 64 {
        if let Ok(z) = zstd::bulk::compress(data, level) {
            if z.len() < data.len() {
                return (1, z);
            }
        }
    }
    (0, data.to_vec())
}

pub fn decompress(codec: i64, data: Vec<u8>, raw_size: i64) -> Vec<u8> {
    if codec == 2 {
        let inner = crate::vault::get().and_then(|v| v.open(&data)).unwrap_or_default();
        return match inner.split_first() {
            Some((c, rest)) if *c != 2 => decompress(*c as i64, rest.to_vec(), raw_size),
            _ => Vec::new(),
        };
    }
    if codec == 1 {
        zstd::bulk::decompress(&data, raw_size.max(0) as usize).unwrap_or_default()
    } else {
        data
    }
}

fn unzip_text(z: Option<Vec<u8>>, len: i64) -> Option<String> {
    let mut z = z?;
    if crate::vault::is_sealed(&z) {
        z = crate::vault::get()?.open(&z)?;
    }
    let raw = zstd::bulk::decompress(&z, len.max(0) as usize).ok()?;
    Some(String::from_utf8_lossy(&raw).into_owned())
}

fn split_tags(s: Option<String>) -> Vec<String> {
    let mut v: Vec<String> = s.unwrap_or_default().split(',').filter(|t| !t.is_empty()).map(String::from).collect();
    // The password pill first, then alphabetical.
    v.sort_by_key(|t| (t != PASSWORD, t.clone()));
    v
}

fn truncate_chars(s: &str, max: usize) -> &str {
    match s.char_indices().nth(max) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}

pub struct Db {
    pub conn: Connection,
    pub path: Option<PathBuf>,
}

impl Db {
    pub fn default_path() -> PathBuf {
        if let Ok(p) = std::env::var("CRABCLIP_DB") {
            return PathBuf::from(p);
        }
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
        Path::new(&home).join("Library/Application Support/crabclip/crabclip.db")
    }

    pub fn open(path: &Path) -> Result<Db> {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let conn = Connection::open(path)?;
        Self::init(conn, Some(path.to_path_buf()))
    }

    pub fn open_memory() -> Result<Db> {
        Self::init(Connection::open_in_memory()?, None)
    }

    fn init(conn: Connection, path: Option<PathBuf>) -> Result<Db> {
        conn.busy_timeout(std::time::Duration::from_secs(15))?;
        // auto_vacuum only takes effect before the first table exists.
        conn.execute_batch(
            "PRAGMA auto_vacuum=INCREMENTAL; PRAGMA journal_mode=WAL; \
             PRAGMA synchronous=NORMAL; PRAGMA foreign_keys=ON; PRAGMA secure_delete=ON;",
        )?;
        conn.execute_batch(SCHEMA)?;
        let db = Db { conn, path };
        if db.get_setting("tags_v1").is_none() {
            // URL clips from before tags existed get the automatic tag.
            let ids: Vec<i64> = {
                let mut st = db.conn.prepare("SELECT id FROM clips WHERE kind='url'")?;
                st.query_map([], |r| r.get(0))?.collect::<Result<_>>()?
            };
            let tx = db.conn.unchecked_transaction()?;
            for id in ids {
                tx.execute("INSERT OR IGNORE INTO tags(clip_id, tag) VALUES(?, ?)", params![id, URL_TAG])?;
                Self::reindex(&tx, id)?;
            }
            tx.execute("INSERT OR REPLACE INTO settings(key, value) VALUES('tags_v1', '1')", [])?;
            tx.commit()?;
        }
        Ok(db)
    }

    /// Insert a clip, or when the same content already exists bump it to the top.
    /// A repeat copy only replaces the stored representations when it carries at
    /// least as many types (a plain-text re-copy never strips RTF/HTML/app data).
    /// Compression happens before the write lock is taken; the transaction is
    /// IMMEDIATE so a concurrent writer makes us wait (busy_timeout) instead of
    /// failing, and SQLITE_BUSY is retried.
    pub fn insert(&self, c: &NewClip, level: i32) -> Result<Inserted> {
        let reps: Vec<(i64, &str, i64, i64, Vec<u8>)> = c
            .reps
            .iter()
            .map(|r| {
                let (codec, data) = compress(&r.data, level);
                (r.item, r.uti.as_str(), codec, r.data.len() as i64, data)
            })
            .collect();
        let text_z = c.text.as_ref().map(|t| zstd::bulk::compress(t.as_bytes(), level.max(3)).unwrap_or_default());
        let mut tries = 0;
        loop {
            match self.insert_tx(c, &reps, text_z.as_deref()) {
                Err(rusqlite::Error::SqliteFailure(e, _))
                    if matches!(e.code, rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked)
                        && tries < 8 =>
                {
                    tries += 1;
                    std::thread::sleep(std::time::Duration::from_millis(250));
                }
                r => return r,
            }
        }
    }

    /// Insert for importers: content already here only widens its first/last
    /// seen dates (when `dates`), keeps its metadata where it has some, and
    /// doesn't count as another copy, so importing again changes nothing.
    pub fn insert_imported(&self, c: &NewClip, level: i32, dates: bool) -> Result<Inserted> {
        let existing: Option<i64> =
            self.conn.query_row("SELECT id FROM clips WHERE hash=?", [&c.hash], |r| r.get(0)).optional()?;
        let Some(id) = existing else { return self.insert(c, level) };
        if dates {
            self.conn.execute(
                "UPDATE clips SET created_at=MIN(created_at, ?2), copied_at=MAX(copied_at, ?2), \
                 starred=MAX(starred, ?3), app_bundle=COALESCE(app_bundle, ?4), app_name=COALESCE(app_name, ?5), \
                 app_path=COALESCE(app_path, ?6) WHERE id=?1",
                params![id, c.ts, c.starred, c.app.bundle, c.app.name, c.app.path],
            )?;
        }
        Ok(Inserted::Existing(id))
    }

    fn insert_tx(
        &self,
        c: &NewClip,
        reps: &[(i64, &str, i64, i64, Vec<u8>)],
        text_z: Option<&[u8]>,
    ) -> Result<Inserted> {
        let tx = rusqlite::Transaction::new_unchecked(&self.conn, rusqlite::TransactionBehavior::Immediate)?;
        let raw: i64 = reps.iter().map(|r| r.3).sum();
        let stored: i64 = reps.iter().map(|r| r.4.len() as i64).sum();
        let text_len = c.text.as_ref().map(|t| t.len() as i64).unwrap_or(0);
        let existing: Option<(i64, i64, String)> = tx
            .query_row("SELECT id, copied_at, types FROM clips WHERE hash=?", [&c.hash], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })
            .optional()?;
        // A copy of a sealed secret matches it by its keyed hash (checked inside
        // the write lock, so a seal committing meanwhile is seen).
        let has_secrets = existing.is_none()
            && tx.query_row("SELECT EXISTS(SELECT 1 FROM tags WHERE tag=?)", [PASSWORD], |r| r.get::<_, bool>(0))?;
        let secret_hash = if has_secrets {
            let v = self.vault(false);
            if v.is_none() {
                crate::log!("vault: key unavailable; a copy of a sealed secret would not be recognized");
            }
            v.map(|v| v.secret_hash(&c.hash))
        } else {
            None
        };
        if existing.is_none() {
            if let Some(sh) = &secret_hash {
                let sealed: Option<i64> =
                    tx.query_row("SELECT id FROM clips WHERE hash=?", [sh], |r| r.get(0)).optional()?;
                if let Some(id) = sealed {
                    // Keep it sealed: only the copy metadata changes.
                    tx.execute(
                        "UPDATE clips SET copied_at=MAX(copied_at, ?2), copy_count=copy_count+1, \
                         app_bundle=?3, app_name=?4, app_path=?5, window_title=?6 WHERE id=?1",
                        params![id, c.ts, c.app.bundle, c.app.name, c.app.path, c.window_title],
                    )?;
                    Self::reindex(&tx, id)?;
                    tx.commit()?;
                    return Ok(Inserted::Existing(id));
                }
            }
        }

        if let Some((id, copied_at, old_types)) = existing {
            if c.ts >= copied_at {
                tx.execute(
                    "UPDATE clips SET copied_at=?2, copy_count=copy_count+1, \
                     starred=MAX(starred, ?3), app_bundle=?4, app_name=?5, app_path=?6, \
                     window_title=?7, source_path=?8, source_url=COALESCE(?9, source_url) WHERE id=?1",
                    params![
                        id,
                        c.ts,
                        c.starred,
                        c.app.bundle,
                        c.app.name,
                        c.app.path,
                        c.window_title,
                        c.source_path,
                        c.source_url
                    ],
                )?;
                let old_n = old_types.split(',').filter(|t| !t.is_empty()).count();
                let new_n = c.types.split(',').filter(|t| !t.is_empty()).count();
                if new_n >= old_n {
                    Self::write_reps(&tx, id, reps, true)?;
                    tx.execute(
                        "UPDATE clips SET types=?2, raw_size=?3, stored_size=?4, preview=?5, \
                         text_z=?6, text_len=?7 WHERE id=?1",
                        params![id, c.types, raw, stored, c.preview, text_z, text_len],
                    )?;
                }
            } else {
                tx.execute(
                    "UPDATE clips SET copy_count=copy_count+1, starred=MAX(starred, ?2), \
                     created_at=MIN(created_at, ?3) WHERE id=?1",
                    params![id, c.starred, c.ts],
                )?;
            }
            Self::reindex(&tx, id)?;
            tx.commit()?;
            return Ok(Inserted::Existing(id));
        }

        let (w, h, scale) = c.img.unwrap_or((0, 0, 1.0));
        let ocr_state = if c.kind == Kind::Image { 0 } else { 1 };
        tx.execute(
            "INSERT INTO clips(created_at, copied_at, kind, hash, preview, text_z, text_len, \
             starred, app_bundle, app_name, app_path, window_title, source_path, source_url, \
             img_w, img_h, img_scale, thumb, ocr_state, types, raw_size, stored_size) \
             VALUES(?1,?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21)",
            params![
                c.ts,
                c.kind.as_str(),
                c.hash,
                c.preview,
                text_z,
                text_len,
                c.starred,
                c.app.bundle,
                c.app.name,
                c.app.path,
                c.window_title,
                c.source_path,
                c.source_url,
                if c.img.is_some() { Some(w) } else { None },
                if c.img.is_some() { Some(h) } else { None },
                if c.img.is_some() { Some(scale) } else { None },
                c.thumb,
                ocr_state,
                c.types,
                raw,
                stored
            ],
        )?;
        let id = tx.last_insert_rowid();
        Self::write_reps(&tx, id, reps, false)?;
        if c.kind == Kind::Url {
            tx.execute("INSERT OR IGNORE INTO tags(clip_id, tag) VALUES(?, ?)", params![id, URL_TAG])?;
        }
        Self::reindex(&tx, id)?;
        tx.commit()?;
        Ok(Inserted::New(id))
    }

    fn write_reps(tx: &Connection, id: i64, reps: &[(i64, &str, i64, i64, Vec<u8>)], replace: bool) -> Result<()> {
        if replace {
            tx.execute("DELETE FROM reps WHERE clip_id=?", [id])?;
        }
        let mut st = tx.prepare_cached(
            "INSERT OR REPLACE INTO reps(clip_id, item, uti, codec, raw_size, data) VALUES(?,?,?,?,?,?)",
        )?;
        for (item, uti, codec, raw_size, data) in reps {
            st.execute(params![id, item, uti, codec, raw_size, data])?;
        }
        Ok(())
    }

    /// Rebuild the FTS row of a clip from its stored text, OCR text and metadata.
    pub fn reindex(conn: &Connection, id: i64) -> Result<()> {
        let row = conn
            .query_row(
                "SELECT text_z, text_len, ocr_text, app_name, window_title, source_path, \
                 source_url, app_bundle FROM clips WHERE id=?",
                [id],
                |r| {
                    Ok((
                        r.get::<_, Option<Vec<u8>>>(0)?,
                        r.get::<_, i64>(1)?,
                        r.get::<_, Option<String>>(2)?,
                        [
                            r.get::<_, Option<String>>(3)?,
                            r.get::<_, Option<String>>(4)?,
                            r.get::<_, Option<String>>(5)?,
                            r.get::<_, Option<String>>(6)?,
                            r.get::<_, Option<String>>(7)?,
                        ],
                    ))
                },
            )
            .optional()?;
        conn.execute("DELETE FROM fts WHERE rowid=?", [id])?;
        let Some((text_z, len, ocr, meta)) = row else {
            return Ok(());
        };
        let tags = Self::tags_of(conn, id)?;
        // A sealed clip is found by its metadata and tags only.
        let mut body = String::new();
        if !tags.iter().any(|t| t == PASSWORD) {
            let text = unzip_text(text_z, len).unwrap_or_default();
            body = truncate_chars(&text, INDEX_CHARS).to_string();
            if let Some(o) = ocr {
                body.push('\n');
                body.push_str(&o);
            }
        }
        let mut meta = meta.into_iter().flatten().collect::<Vec<_>>();
        meta.extend(tags);
        meta.extend(Self::folder_paths_of(conn, id)?);
        let mut st = conn.prepare_cached("SELECT title FROM snippets WHERE clip_id=? AND title<>''")?;
        meta.extend(st.query_map([id], |r| r.get::<_, String>(0))?.collect::<Result<Vec<_>>>()?);
        let meta = meta.join("\n");
        conn.execute("INSERT INTO fts(rowid, body, meta) VALUES(?,?,?)", params![id, body, meta])?;
        Ok(())
    }

    pub fn search(&self, f: &Filter, limit: usize) -> Result<Vec<i64>> {
        let p = crate::query::parse(f.query);
        let mut sql = String::from("SELECT id FROM clips WHERE 1");
        let mut args: Vec<String> = Vec::new();
        let arg = |args: &mut Vec<String>, v: String| {
            args.push(v);
            format!("?{}", args.len())
        };
        if !p.terms.is_empty() {
            let a = arg(&mut args, crate::query::fts_expr(&p.terms));
            sql += &format!(" AND id IN (SELECT rowid FROM fts WHERE fts MATCH {a})");
        }
        for t in &p.not_terms {
            let a = arg(&mut args, crate::query::fts_expr(std::slice::from_ref(t)));
            sql += &format!(" AND id NOT IN (SELECT rowid FROM fts WHERE fts MATCH {a})");
        }
        for s in &p.short {
            let a = arg(&mut args, crate::query::like_pattern(s));
            sql += &format!(" AND (preview LIKE {a} ESCAPE '\\' OR app_name LIKE {a} ESCAPE '\\')");
        }
        if f.starred || p.starred {
            sql += " AND starred=1";
        }
        if let Some(app) = f.app {
            let a = arg(&mut args, app.to_string());
            sql += &format!(" AND COALESCE(app_bundle, app_name, '')={a}");
        }
        if let Some(app) = &p.app {
            let a = arg(&mut args, crate::query::like_pattern(app));
            sql += &format!(" AND (app_name LIKE {a} ESCAPE '\\' OR app_bundle LIKE {a} ESCAPE '\\')");
        }
        for t in &p.tags {
            let a = arg(&mut args, normalize_tag(t));
            sql += &format!(" AND id IN (SELECT clip_id FROM tags WHERE tag={a})");
        }
        if let Some(k) = &p.kind {
            let a = arg(&mut args, k.clone());
            sql += &format!(" AND kind={a}");
        }
        sql += &format!(" ORDER BY copied_at DESC, id DESC LIMIT {limit}");
        let mut st = self.conn.prepare(&sql)?;
        let ids = st
            .query_map(rusqlite::params_from_iter(args.iter()), |r| r.get(0))?
            .collect::<Result<Vec<i64>>>();
        ids
    }

    pub fn row(&self, id: i64) -> Result<Option<RowInfo>> {
        self.conn
            .query_row(
                "SELECT id, kind, preview, starred, COALESCE(app_bundle, app_name, ''), app_name, \
                 app_path, copied_at, COALESCE(img_w,0), COALESCE(img_h,0), COALESCE(img_scale,1), \
                 thumb, (SELECT group_concat(tag, ',') FROM tags WHERE clip_id=clips.id) FROM clips WHERE id=?",
                [id],
                |r| {
                    Ok(RowInfo {
                        id: r.get(0)?,
                        kind: Kind::parse(&r.get::<_, String>(1)?),
                        preview: r.get(2)?,
                        starred: r.get(3)?,
                        app_key: r.get(4)?,
                        app_name: r.get(5)?,
                        app_path: r.get(6)?,
                        copied_at: r.get(7)?,
                        img_w: r.get(8)?,
                        img_h: r.get(9)?,
                        img_scale: r.get(10)?,
                        thumb: r.get(11)?,
                        tags: split_tags(r.get(12)?),
                    })
                },
            )
            .optional()
    }

    pub fn detail(&self, id: i64) -> Result<Option<Detail>> {
        self.conn
            .query_row(
                "SELECT id, kind, text_z, text_len, created_at, copied_at, app_name, app_bundle, \
                 window_title, source_path, source_url, ocr_text, types, raw_size, stored_size, \
                 copy_count, paste_count, COALESCE(img_w,0), COALESCE(img_h,0), starred, \
                 (SELECT group_concat(tag, ',') FROM tags WHERE clip_id=clips.id) FROM clips WHERE id=?",
                [id],
                |r| {
                    Ok(Detail {
                        id: r.get(0)?,
                        kind: Kind::parse(&r.get::<_, String>(1)?),
                        text: unzip_text(r.get(2)?, r.get(3)?),
                        created_at: r.get(4)?,
                        copied_at: r.get(5)?,
                        app_name: r.get(6)?,
                        app_bundle: r.get(7)?,
                        window_title: r.get(8)?,
                        source_path: r.get(9)?,
                        source_url: r.get(10)?,
                        ocr_text: r.get(11)?,
                        types: r.get(12)?,
                        raw_size: r.get(13)?,
                        stored_size: r.get(14)?,
                        copy_count: r.get(15)?,
                        paste_count: r.get(16)?,
                        img_w: r.get(17)?,
                        img_h: r.get(18)?,
                        starred: r.get(19)?,
                        tags: split_tags(r.get(20)?),
                        folders: Vec::new(),
                        title: None,
                    })
                },
            )
            .optional()
            .and_then(|d| match d {
                Some(mut d) => {
                    d.folders = Self::folder_paths_of(&self.conn, d.id)?;
                    d.title = self
                        .conn
                        .query_row(
                            "SELECT title FROM snippets WHERE clip_id=? AND title<>'' ORDER BY folder_id LIMIT 1",
                            [d.id],
                            |r| r.get(0),
                        )
                        .optional()?;
                    Ok(Some(d))
                }
                None => Ok(None),
            })
    }

    /// All representations, decompressed, in pasteboard order.
    pub fn reps(&self, id: i64) -> Result<Vec<Rep>> {
        let mut st = self.conn.prepare_cached(
            "SELECT item, uti, codec, raw_size, data FROM reps WHERE clip_id=? ORDER BY item, rowid",
        )?;
        let rows = st.query_map([id], |r| {
            Ok(Rep {
                item: r.get(0)?,
                uti: r.get(1)?,
                data: decompress(r.get(2)?, r.get(4)?, r.get(3)?),
            })
        })?;
        rows.collect()
    }

    pub fn rep(&self, id: i64, uti: &str) -> Result<Option<Vec<u8>>> {
        self.conn
            .query_row(
                "SELECT codec, raw_size, data FROM reps WHERE clip_id=? AND uti=? ORDER BY item LIMIT 1",
                params![id, uti],
                |r| Ok(decompress(r.get(0)?, r.get(2)?, r.get(1)?)),
            )
            .optional()
    }

    pub fn rep_types(&self, id: i64) -> Result<Vec<String>> {
        let mut st = self.conn.prepare_cached("SELECT DISTINCT uti FROM reps WHERE clip_id=?")?;
        let rows = st.query_map([id], |r| r.get(0))?;
        rows.collect()
    }

    pub fn set_star(&self, id: i64, on: bool) -> Result<()> {
        self.conn.execute("UPDATE clips SET starred=? WHERE id=?", params![on, id])?;
        Ok(())
    }

    pub fn touch_pasted(&self, id: i64, ts: i64, move_to_top: bool) -> Result<()> {
        if move_to_top {
            self.conn.execute(
                "UPDATE clips SET paste_count=paste_count+1, copied_at=? WHERE id=?",
                params![ts, id],
            )?;
        } else {
            self.conn
                .execute("UPDATE clips SET paste_count=paste_count+1 WHERE id=?", [id])?;
        }
        Ok(())
    }

    pub fn delete(&self, id: i64) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        Self::delete_in(&tx, id)?;
        tx.commit()
    }

    fn delete_in(tx: &Connection, id: i64) -> Result<()> {
        tx.execute("DELETE FROM fts WHERE rowid=?", [id])?;
        tx.execute("DELETE FROM reps WHERE clip_id=?", [id])?;
        tx.execute("DELETE FROM tags WHERE clip_id=?", [id])?;
        let folders: Vec<i64> = {
            let mut st = tx.prepare("SELECT folder_id FROM snippets WHERE clip_id=?")?;
            st.query_map([id], |r| r.get(0))?.collect::<Result<_>>()?
        };
        tx.execute("DELETE FROM snippets WHERE clip_id=?", [id])?;
        for f in folders {
            Self::prune_folder(&tx, f)?;
        }
        tx.execute("DELETE FROM clips WHERE id=?", [id])?;
        Ok(())
    }

    /// Delete clips last copied before `before_ms` (None: all). Starred clips and
    /// clips filed in folders are kept unless `include_starred`. Returns the
    /// number deleted.
    pub fn clear(&self, before_ms: Option<i64>, include_starred: bool) -> Result<usize> {
        let mut cond = String::from("1");
        if let Some(b) = before_ms {
            cond += &format!(" AND copied_at < {b}");
        }
        if !include_starred {
            cond += " AND starred=0 AND id NOT IN (SELECT clip_id FROM snippets)";
        }
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(&format!("DELETE FROM fts WHERE rowid IN (SELECT id FROM clips WHERE {cond})"), [])?;
        tx.execute(
            &format!("DELETE FROM reps WHERE clip_id IN (SELECT id FROM clips WHERE {cond})"),
            [],
        )?;
        tx.execute(
            &format!("DELETE FROM tags WHERE clip_id IN (SELECT id FROM clips WHERE {cond})"),
            [],
        )?;
        tx.execute(
            &format!("DELETE FROM snippets WHERE clip_id IN (SELECT id FROM clips WHERE {cond})"),
            [],
        )?;
        let n = tx.execute(&format!("DELETE FROM clips WHERE {cond}"), [])?;
        if include_starred {
            // Folders left with nothing in them, bottom up.
            while tx.execute(
                "DELETE FROM folders WHERE id NOT IN (SELECT folder_id FROM snippets) \
                 AND id NOT IN (SELECT parent FROM folders)",
                [],
            )? > 0
            {}
        }
        tx.commit()?;
        Ok(n)
    }

    pub fn vacuum_some(&self) -> Result<()> {
        self.conn.execute_batch("PRAGMA incremental_vacuum; PRAGMA wal_checkpoint(TRUNCATE);")
    }

    pub fn apps(&self) -> Result<Vec<AppRow>> {
        let mut st = self.conn.prepare(
            "SELECT COALESCE(app_bundle, app_name, '') k, MAX(app_name), MAX(app_path), COUNT(*), \
             MAX(copied_at) m FROM clips GROUP BY k ORDER BY m DESC",
        )?;
        let rows = st.query_map([], |r| {
            let key: String = r.get(0)?;
            let name: Option<String> = r.get(1)?;
            Ok(AppRow {
                name: name.unwrap_or_else(|| if key.is_empty() { "Unknown".into() } else { key.clone() }),
                key,
                path: r.get(2)?,
                count: r.get(3)?,
            })
        })?;
        rows.collect()
    }

    pub fn has_app(&self, key: &str) -> Result<bool> {
        self.conn
            .query_row("SELECT 1 FROM apps WHERE key=? AND icon IS NOT NULL", [key], |_| Ok(()))
            .optional()
            .map(|o| o.is_some())
    }

    pub fn save_app(&self, key: &str, name: Option<&str>, path: Option<&str>, icon: Option<&[u8]>) -> Result<()> {
        self.conn.execute(
            "INSERT INTO apps(key, name, path, icon) VALUES(?,?,?,?) ON CONFLICT(key) DO UPDATE SET \
             name=COALESCE(excluded.name, name), path=COALESCE(excluded.path, path), \
             icon=COALESCE(excluded.icon, icon)",
            params![key, name, path, icon],
        )?;
        Ok(())
    }

    pub fn app_icon(&self, key: &str) -> Result<Option<Vec<u8>>> {
        self.conn
            .query_row("SELECT icon FROM apps WHERE key=?", [key], |r| r.get(0))
            .optional()
            .map(Option::flatten)
    }

    pub fn pending_ocr(&self, limit: usize) -> Result<Vec<i64>> {
        let mut st = self.conn.prepare(&format!(
            "SELECT id FROM clips WHERE ocr_state=0 AND kind='image' ORDER BY copied_at DESC LIMIT {limit}"
        ))?;
        let rows = st.query_map([], |r| r.get(0))?;
        rows.collect()
    }

    pub fn set_ocr(&self, id: i64, text: Option<&str>, state: i64) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        // Never on a sealed clip (OCR may have read the image before the seal).
        tx.execute(
            "UPDATE clips SET ocr_text=?1, ocr_state=?2 WHERE id=?3 \
             AND NOT EXISTS(SELECT 1 FROM tags WHERE clip_id=?3 AND tag=?4)",
            params![text.filter(|t| !t.trim().is_empty()), state, id, PASSWORD],
        )?;
        Self::reindex(&tx, id)?;
        tx.commit()
    }

    /// The vault, if its key is there and is the one this database's clips were
    /// sealed with (fingerprint in settings, written on first seal). `create`
    /// makes a key when there is none yet (sealing only).
    pub fn vault(&self, create: bool) -> Option<&'static crate::vault::Vault> {
        let v = if create { crate::vault::get_or_create() } else { crate::vault::get() }?;
        match self.get_setting("vault_check") {
            Some(c) if c != v.check() => {
                crate::log!("vault: the Keychain key is not the one these clips were sealed with");
                None
            }
            Some(_) => Some(v),
            None => {
                if create {
                    let _ = self.set_setting("vault_check", v.check());
                }
                Some(v)
            }
        }
    }

    /// After sealing: rewrite the search index without the secret's old
    /// entries and push the old pages out of the WAL (secure_delete zeroes
    /// what is freed).
    pub fn scrub(&self) -> Result<()> {
        self.conn.execute("INSERT INTO fts(fts) VALUES('optimize')", [])?;
        self.conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE); PRAGMA incremental_vacuum;")
    }

    pub fn is_sealed(&self, id: i64) -> bool {
        self.conn
            .query_row("SELECT 1 FROM tags WHERE clip_id=? AND tag=?", params![id, PASSWORD], |_| Ok(()))
            .optional()
            .ok()
            .flatten()
            .is_some()
    }

    fn tags_of(conn: &Connection, id: i64) -> Result<Vec<String>> {
        let mut st = conn.prepare_cached("SELECT group_concat(tag, ',') FROM tags WHERE clip_id=?")?;
        let s: Option<String> = st.query_row([id], |r| r.get(0))?;
        Ok(split_tags(s))
    }

    pub fn tags(&self, id: i64) -> Result<Vec<String>> {
        Self::tags_of(&self.conn, id)
    }

    /// Every tag in use with its clip count, most used first.
    pub fn all_tags(&self) -> Result<Vec<(String, i64)>> {
        let mut st = self.conn.prepare_cached("SELECT tag, COUNT(*) n FROM tags GROUP BY tag ORDER BY n DESC, tag")?;
        let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        rows.collect()
    }

    /// Replace a clip's tags. Adding "password" seals the clip (contents and text
    /// encrypted, preview/thumbnail/OCR dropped, not indexed); removing it
    /// restores everything. Fails, changing nothing, without the vault key.
    pub fn set_tags(&self, id: i64, tags: &[String]) -> std::result::Result<Vec<String>, String> {
        let mut new: Vec<String> = Vec::new();
        for t in tags.iter().map(|t| normalize_tag(t)).filter(|t| !t.is_empty()) {
            if !new.contains(&t) {
                new.push(t);
            }
        }
        let old = self.tags(id).map_err(|e| e.to_string())?;
        let want = new.iter().any(|t| t == PASSWORD);
        let had = old.iter().any(|t| t == PASSWORD);
        let run = || -> std::result::Result<(), String> {
            let tx = rusqlite::Transaction::new_unchecked(&self.conn, rusqlite::TransactionBehavior::Immediate)
                .map_err(|e| e.to_string())?;
            if want != had {
                let v = self.vault(true).ok_or(
                    "The encryption key is not available: the Keychain is locked, access was denied, \
                     or it is not the key these clips were sealed with.",
                )?;
                if want {
                    Self::seal_clip(&tx, id, v).map_err(|e| e.to_string())?;
                } else {
                    Self::unseal_clip(&tx, id, v)?;
                }
            }
            tx.execute("DELETE FROM tags WHERE clip_id=?", [id]).map_err(|e| e.to_string())?;
            for t in &new {
                tx.execute("INSERT INTO tags(clip_id, tag) VALUES(?, ?)", params![id, t]).map_err(|e| e.to_string())?;
            }
            Self::reindex(&tx, id).map_err(|e| e.to_string())?;
            tx.commit().map_err(|e| e.to_string())
        };
        run()?;
        self.tags(id).map_err(|e| e.to_string())
    }

    fn seal_clip(tx: &Connection, id: i64, v: &crate::vault::Vault) -> Result<()> {
        let reps: Vec<(i64, i64, Vec<u8>)> = {
            let mut st = tx.prepare("SELECT rowid, codec, data FROM reps WHERE clip_id=?")?;
            st.query_map([id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<Result<_>>()?
        };
        for (rowid, codec, data) in reps {
            if codec == 2 {
                continue;
            }
            let mut inner = Vec::with_capacity(data.len() + 1);
            inner.push(codec as u8);
            inner.extend_from_slice(&data);
            tx.execute("UPDATE reps SET codec=2, data=? WHERE rowid=?", params![v.seal(&inner), rowid])?;
        }
        let (text_z, hash): (Option<Vec<u8>>, Vec<u8>) =
            tx.query_row("SELECT text_z, hash FROM clips WHERE id=?", [id], |r| Ok((r.get(0)?, r.get(1)?)))?;
        let text_z = text_z.map(|z| if crate::vault::is_sealed(&z) { z } else { v.seal(&z) });
        let hash = if hash.starts_with(b"pw:") { hash } else { v.secret_hash(&hash) };
        tx.execute(
            "UPDATE clips SET text_z=?2, hash=?3, preview=?4, thumb=NULL, ocr_text=NULL, ocr_state=1, source_path=NULL, \
             stored_size=(SELECT COALESCE(SUM(length(data)),0) FROM reps WHERE clip_id=?1) WHERE id=?1",
            params![id, text_z, hash, SEALED_PREVIEW],
        )?;
        Ok(())
    }

    fn unseal_clip(tx: &Connection, id: i64, v: &crate::vault::Vault) -> std::result::Result<(), String> {
        let e = |e: rusqlite::Error| e.to_string();
        let rows: Vec<(i64, i64, String, i64, i64, Vec<u8>)> = {
            let mut st = tx
                .prepare("SELECT rowid, item, uti, codec, raw_size, data FROM reps WHERE clip_id=? ORDER BY item, rowid")
                .map_err(e)?;
            st.query_map([id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)))
                .map_err(e)?
                .collect::<Result<_>>()
                .map_err(e)?
        };
        let mut reps = Vec::new();
        for (rowid, item, uti, codec, raw_size, data) in rows {
            let (codec, data) = if codec == 2 {
                let inner = v.open(&data).ok_or("cannot decrypt this clip")?;
                let (c, rest) = inner.split_first().ok_or("empty sealed data")?;
                (*c as i64, rest.to_vec())
            } else {
                (codec, data)
            };
            tx.execute("UPDATE reps SET codec=?, data=? WHERE rowid=?", params![codec, data, rowid]).map_err(e)?;
            reps.push(Rep { item, uti, data: decompress(codec, data, raw_size) });
        }
        let (kind, text_z, text_len, types): (String, Option<Vec<u8>>, i64, String) = tx
            .query_row("SELECT kind, text_z, text_len, types FROM clips WHERE id=?", [id], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
            })
            .map_err(e)?;
        let kind = Kind::parse(&kind);
        let text_z = match text_z {
            Some(z) if crate::vault::is_sealed(&z) => Some(v.open(&z).ok_or("cannot decrypt this clip's text")?),
            z => z,
        };
        let text = unzip_text(text_z.clone(), text_len);
        let preview = match kind {
            Kind::Other => types.replace(',', ", "),
            _ => text.as_deref().map(crate::ingest::preview_of).unwrap_or_default(),
        };
        let mut thumb = None;
        if kind == Kind::Image {
            thumb = crate::ingest::primary_image(&reps).and_then(|p| crate::imageio::thumbnail(&p.data, 760, 300));
        }
        let hash = crate::ingest::content_hash(kind, &reps).ok_or("cannot rebuild the clip's hash")?;
        let files = crate::ingest::file_paths(&reps);
        let source_path = if files.len() == 1 { files.first().cloned() } else { None };
        tx.execute(
            "UPDATE clips SET text_z=?2, hash=?3, preview=?4, thumb=?5, source_path=COALESCE(?6, source_path), \
             ocr_state=CASE WHEN kind='image' THEN 0 ELSE 1 END, \
             stored_size=(SELECT COALESCE(SUM(length(data)),0) FROM reps WHERE clip_id=?1) WHERE id=?1",
            params![id, text_z, hash, preview, thumb, source_path],
        )
        .map_err(|e| match e {
            rusqlite::Error::SqliteFailure(f, _) if f.code == rusqlite::ErrorCode::ConstraintViolation => {
                "the same content is already in the history unsealed".to_string()
            }
            e => e.to_string(),
        })?;
        Ok(())
    }

    // --------------------------------------------------------------- folders

    /// The folder at `path` ("A/B"), created (with its parents) if `create`.
    /// The top level is 0.
    fn folder_at(conn: &Connection, path: &str, create: bool) -> Result<Option<i64>> {
        let mut id = 0;
        for name in folder_parts(path) {
            let found: Option<i64> = conn
                .query_row("SELECT id FROM folders WHERE parent=? AND name=?", params![id, name], |r| r.get(0))
                .optional()?;
            id = match found {
                Some(f) => f,
                None if create => {
                    conn.execute(
                        "INSERT INTO folders(parent, name, pos) \
                         VALUES(?1, ?2, (SELECT COALESCE(MAX(pos), -1) + 1 FROM folders WHERE parent=?1))",
                        params![id, name],
                    )?;
                    conn.last_insert_rowid()
                }
                None => return Ok(None),
            };
        }
        Ok(Some(id))
    }

    pub fn folder_id(&self, path: &str) -> Result<Option<i64>> {
        Self::folder_at(&self.conn, path, false)
    }

    /// The folder at `path` or, if it is gone, its nearest parent still there
    /// (0: none, the top level).
    pub fn nearest_folder(&self, path: &str) -> Result<i64> {
        let mut parts = folder_parts(path);
        while !parts.is_empty() {
            if let Some(id) = self.folder_id(&parts.join("/"))? {
                return Ok(id);
            }
            parts.pop();
        }
        Ok(0)
    }

    /// The folder at `path`, created if needed.
    pub fn make_folder(&self, path: &str) -> Result<i64> {
        Ok(Self::folder_at(&self.conn, path, true)?.unwrap_or(0))
    }

    /// "A/B" for a folder id ("" for the top level).
    pub fn folder_path(&self, id: i64) -> Result<String> {
        Self::path_of(&self.conn, id)
    }

    fn path_of(conn: &Connection, mut id: i64) -> Result<String> {
        let mut names = Vec::new();
        while id != 0 && names.len() < 64 {
            let (parent, name): (i64, String) =
                conn.query_row("SELECT parent, name FROM folders WHERE id=?", [id], |r| Ok((r.get(0)?, r.get(1)?)))?;
            names.push(name);
            id = parent;
        }
        names.reverse();
        Ok(names.join("/"))
    }

    fn folder_paths_of(conn: &Connection, clip: i64) -> Result<Vec<String>> {
        let ids: Vec<i64> = {
            let mut st = conn.prepare_cached("SELECT folder_id FROM snippets WHERE clip_id=?")?;
            st.query_map([clip], |r| r.get(0))?.collect::<Result<_>>()?
        };
        let mut paths = ids.into_iter().map(|f| Self::path_of(conn, f)).collect::<Result<Vec<_>>>()?;
        paths.sort_by_key(|p| p.to_lowercase());
        Ok(paths)
    }

    /// Folder paths a clip is filed in.
    pub fn folders_of(&self, clip: i64) -> Result<Vec<String>> {
        Self::folder_paths_of(&self.conn, clip)
    }

    /// File a clip in exactly these folders (created as needed). Folders left
    /// empty by it are removed. Returns the clip's folder paths.
    pub fn set_folders(&self, clip: i64, paths: &[String]) -> Result<Vec<String>> {
        let tx = rusqlite::Transaction::new_unchecked(&self.conn, rusqlite::TransactionBehavior::Immediate)?;
        let mut want = Vec::new();
        for p in paths {
            if let Some(f) = Self::folder_at(&tx, p, true)?.filter(|f| *f != 0) {
                if !want.contains(&f) {
                    want.push(f);
                }
            }
        }
        let have: Vec<i64> = {
            let mut st = tx.prepare("SELECT folder_id FROM snippets WHERE clip_id=?")?;
            st.query_map([clip], |r| r.get(0))?.collect::<Result<_>>()?
        };
        // Add before removing: moving A/B to A would otherwise prune A (empty
        // once B goes) before the clip lands in it.
        for f in want.iter().filter(|f| !have.contains(f)) {
            Self::add_snippet(&tx, *f, clip, "", None)?;
        }
        for f in have.iter().filter(|f| !want.contains(f)) {
            tx.execute("DELETE FROM snippets WHERE folder_id=? AND clip_id=?", params![f, clip])?;
            Self::prune_folder(&tx, *f)?;
        }
        Self::reindex(&tx, clip)?;
        tx.commit()?;
        self.folders_of(clip)
    }

    /// Put a clip in a folder (or rename it there). `pos` None: at the end.
    pub fn file_snippet(&self, folder: i64, clip: i64, title: &str, pos: Option<i64>) -> Result<()> {
        let tx = rusqlite::Transaction::new_unchecked(&self.conn, rusqlite::TransactionBehavior::Immediate)?;
        Self::add_snippet(&tx, folder, clip, title, pos)?;
        Self::reindex(&tx, clip)?;
        tx.commit()
    }

    fn add_snippet(conn: &Connection, folder: i64, clip: i64, title: &str, pos: Option<i64>) -> Result<()> {
        conn.execute(
            "INSERT INTO snippets(folder_id, clip_id, title, pos) \
             VALUES(?1, ?2, ?3, COALESCE(?4, (SELECT COALESCE(MAX(pos), -1) + 1 FROM snippets WHERE folder_id=?1))) \
             ON CONFLICT(folder_id, clip_id) DO UPDATE SET title=CASE WHEN excluded.title<>'' THEN excluded.title ELSE title END, \
             pos=COALESCE(?4, pos)",
            params![folder, clip, title, pos],
        )?;
        Ok(())
    }

    /// Remove a folder that holds nothing anymore, and its parents likewise.
    fn prune_folder(conn: &Connection, mut id: i64) -> Result<()> {
        while id != 0 {
            let used: bool = conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM snippets WHERE folder_id=?1) OR EXISTS(SELECT 1 FROM folders WHERE parent=?1)",
                [id],
                |r| r.get(0),
            )?;
            if used {
                break;
            }
            let parent: i64 = conn.query_row("SELECT parent FROM folders WHERE id=?", [id], |r| r.get(0))?;
            conn.execute("DELETE FROM folders WHERE id=?", [id])?;
            id = parent;
        }
        Ok(())
    }

    /// Set a folder's place among its siblings (importers keep their order).
    pub fn set_folder_pos(&self, id: i64, pos: i64) -> Result<()> {
        self.conn.execute("UPDATE folders SET pos=? WHERE id=?", params![pos, id])?;
        Ok(())
    }

    /// What `folder` (0: top level) holds: subfolders, then snippets, each in
    /// their order. `filter` keeps the ones whose name contains it (any case).
    pub fn folder_entries(&self, folder: i64, filter: &str) -> Result<Vec<Entry>> {
        let f = filter.to_lowercase();
        let keep = |s: &str| f.is_empty() || s.to_lowercase().contains(&f);
        let mut out = Vec::new();
        let mut st = self.conn.prepare_cached(
            "SELECT id, name, (SELECT COUNT(*) FROM folders c WHERE c.parent=f.id) + \
             (SELECT COUNT(*) FROM snippets s WHERE s.folder_id=f.id) FROM folders f WHERE parent=? ORDER BY pos, name",
        )?;
        for e in st.query_map([folder], |r| Ok(Entry::Folder { id: r.get(0)?, name: r.get(1)?, items: r.get(2)? }))? {
            let e = e?;
            if keep(e.label()) {
                out.push(e);
            }
        }
        let mut st = self.conn.prepare_cached(
            "SELECT s.clip_id, s.title, c.preview FROM snippets s JOIN clips c ON c.id=s.clip_id \
             WHERE s.folder_id=? ORDER BY s.pos, s.title COLLATE NOCASE, c.copied_at DESC",
        )?;
        for e in st.query_map([folder], |r| {
            let title: String = r.get(1)?;
            let preview: String = r.get(2)?;
            let title = if title.is_empty() { preview.lines().next().unwrap_or("").trim().to_string() } else { title };
            Ok(Entry::Snippet { clip_id: r.get(0)?, title })
        })? {
            let e = e?;
            if keep(e.label()) {
                out.push(e);
            }
        }
        Ok(out)
    }

    /// Every folder path with how many snippets it holds, in tree order.
    pub fn all_folder_paths(&self) -> Result<Vec<(String, i64)>> {
        let rows: Vec<(i64, i64, String, i64)> = {
            let mut st = self.conn.prepare(
                "SELECT id, parent, name, (SELECT COUNT(*) FROM snippets s WHERE s.folder_id=f.id) FROM folders f ORDER BY pos, name",
            )?;
            st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?.collect::<Result<_>>()?
        };
        fn walk(rows: &[(i64, i64, String, i64)], parent: i64, prefix: &str, out: &mut Vec<(String, i64)>, depth: usize) {
            if depth > 64 {
                return;
            }
            for (id, p, name, n) in rows {
                if *p == parent {
                    let path = if prefix.is_empty() { name.clone() } else { format!("{prefix}/{name}") };
                    out.push((path.clone(), *n));
                    walk(rows, *id, &path, out, depth + 1);
                }
            }
        }
        let mut out = Vec::new();
        walk(&rows, 0, "", &mut out, 0);
        Ok(out)
    }

    // -------------------------------------------------------- snippet editor

    /// Every folder as (id, parent, name), siblings in their order.
    pub fn folder_tree(&self) -> Result<Vec<(i64, i64, String)>> {
        let mut st = self.conn.prepare("SELECT id, parent, name FROM folders ORDER BY pos, name")?;
        st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect()
    }

    /// A new, empty folder `name` in `parent` (0: the top level).
    pub fn new_folder(&self, parent: i64, name: &str) -> std::result::Result<i64, String> {
        let name = folder_name(name)?;
        let taken: bool = self
            .conn
            .query_row("SELECT EXISTS(SELECT 1 FROM folders WHERE parent=? AND name=?)", params![parent, name], |r| r.get(0))
            .map_err(|e| e.to_string())?;
        if taken {
            return Err(format!("There is already a folder named {name} there."));
        }
        let path = match parent {
            0 => name,
            p => format!("{}/{name}", self.folder_path(p).map_err(|e| e.to_string())?),
        };
        self.make_folder(&path).map_err(|e| e.to_string())
    }

    /// Rename a folder where it is: what it holds goes along. Returns its new path.
    pub fn rename_folder(&self, id: i64, name: &str) -> std::result::Result<String, String> {
        let e = |e: rusqlite::Error| e.to_string();
        let name = folder_name(name)?;
        let parent: i64 = self.conn.query_row("SELECT parent FROM folders WHERE id=?", [id], |r| r.get(0)).map_err(e)?;
        let other: Option<i64> = self
            .conn
            .query_row("SELECT id FROM folders WHERE parent=? AND name=?", params![parent, name], |r| r.get(0))
            .optional()
            .map_err(e)?;
        if other.is_some_and(|o| o != id) {
            return Err(format!("There is already a folder named {name} there."));
        }
        let tx = rusqlite::Transaction::new_unchecked(&self.conn, rusqlite::TransactionBehavior::Immediate).map_err(e)?;
        tx.execute("UPDATE folders SET name=? WHERE id=?", params![name, id]).map_err(e)?;
        // Folder names are searchable words of the clips they hold.
        for clip in Self::clips_below(&tx, id).map_err(e)? {
            Self::reindex(&tx, clip).map_err(e)?;
        }
        tx.commit().map_err(e)?;
        self.folder_path(id).map_err(e)
    }

    /// Clips filed in `folder` or in a folder below it.
    pub fn clips_under(&self, folder: i64) -> Result<Vec<i64>> {
        Self::clips_below(&self.conn, folder)
    }

    fn clips_below(conn: &Connection, folder: i64) -> Result<Vec<i64>> {
        let mut st = conn.prepare(
            "WITH RECURSIVE sub(id) AS (SELECT ?1 UNION SELECT f.id FROM folders f JOIN sub ON f.parent=sub.id) \
             SELECT DISTINCT clip_id FROM snippets WHERE folder_id IN sub",
        )?;
        st.query_map([folder], |r| r.get(0))?.collect()
    }

    /// Remove a folder and everything below it. Its clips stay in the history
    /// (and in other folders they are in).
    pub fn delete_folder(&self, id: i64) -> Result<()> {
        let tx = rusqlite::Transaction::new_unchecked(&self.conn, rusqlite::TransactionBehavior::Immediate)?;
        let clips = Self::clips_below(&tx, id)?;
        tx.execute(
            "WITH RECURSIVE sub(id) AS (SELECT ?1 UNION SELECT f.id FROM folders f JOIN sub ON f.parent=sub.id) \
             DELETE FROM snippets WHERE folder_id IN sub",
            [id],
        )?;
        tx.execute(
            "WITH RECURSIVE sub(id) AS (SELECT ?1 UNION SELECT f.id FROM folders f JOIN sub ON f.parent=sub.id) \
             DELETE FROM folders WHERE id IN sub",
            [id],
        )?;
        for clip in clips {
            Self::reindex(&tx, clip)?;
        }
        tx.commit()
    }

    /// Take a clip out of a folder. The editor's operations leave folders
    /// in place even when empty: they are managed there.
    pub fn unfile(&self, folder: i64, clip: i64) -> Result<()> {
        let tx = rusqlite::Transaction::new_unchecked(&self.conn, rusqlite::TransactionBehavior::Immediate)?;
        tx.execute("DELETE FROM snippets WHERE folder_id=? AND clip_id=?", params![folder, clip])?;
        Self::reindex(&tx, clip)?;
        tx.commit()
    }

    /// Move a clip from one folder to the end of another, keeping its title.
    pub fn move_snippet(&self, from: i64, to: i64, clip: i64) -> Result<()> {
        if from == to {
            return Ok(());
        }
        let tx = rusqlite::Transaction::new_unchecked(&self.conn, rusqlite::TransactionBehavior::Immediate)?;
        let title: String = tx
            .query_row("SELECT title FROM snippets WHERE folder_id=? AND clip_id=?", params![from, clip], |r| r.get(0))
            .optional()?
            .unwrap_or_default();
        // Already there too: it keeps its place and title there.
        tx.execute(
            "INSERT OR IGNORE INTO snippets(folder_id, clip_id, title, pos) \
             VALUES(?1, ?2, ?3, (SELECT COALESCE(MAX(pos), -1) + 1 FROM snippets WHERE folder_id=?1))",
            params![to, clip, title],
        )?;
        tx.execute("DELETE FROM snippets WHERE folder_id=? AND clip_id=?", params![from, clip])?;
        Self::reindex(&tx, clip)?;
        tx.commit()
    }

    /// Put a clip at `index` among the snippets of folder `to` (dragged
    /// there), taking it out of `from` if that is another folder. Its title
    /// goes along.
    pub fn place_snippet(&self, clip: i64, from: i64, to: i64, index: usize) -> Result<()> {
        let tx = rusqlite::Transaction::new_unchecked(&self.conn, rusqlite::TransactionBehavior::Immediate)?;
        let title: String = tx
            .query_row("SELECT title FROM snippets WHERE folder_id=? AND clip_id=?", params![from, clip], |r| r.get(0))
            .optional()?
            .unwrap_or_default();
        let mut order: Vec<i64> = {
            // In the order the folder is shown (folder_entries), ties included.
            let mut st = tx.prepare(
                "SELECT s.clip_id FROM snippets s JOIN clips c ON c.id=s.clip_id WHERE s.folder_id=? AND s.clip_id<>? \
                 ORDER BY s.pos, s.title COLLATE NOCASE, c.copied_at DESC",
            )?;
            st.query_map(params![to, clip], |r| r.get(0))?.collect::<Result<_>>()?
        };
        order.insert(index.min(order.len()), clip);
        if from != to {
            // Already in `to` too: one entry, with the dragged one's title if it has one.
            tx.execute(
                "INSERT INTO snippets(folder_id, clip_id, title) VALUES(?, ?, ?) \
                 ON CONFLICT(folder_id, clip_id) DO UPDATE SET title=CASE WHEN excluded.title<>'' THEN excluded.title ELSE title END",
                params![to, clip, title],
            )?;
            tx.execute("DELETE FROM snippets WHERE folder_id=? AND clip_id=?", params![from, clip])?;
        }
        for (pos, c) in order.iter().enumerate() {
            tx.execute("UPDATE snippets SET pos=? WHERE folder_id=? AND clip_id=?", params![pos as i64, to, c])?;
        }
        Self::reindex(&tx, clip)?;
        tx.commit()
    }

    /// Put folder `id` at `index` among the folders in `parent` (0: the top
    /// level), with what it holds. Returns its new path.
    pub fn place_folder(&self, id: i64, parent: i64, index: usize) -> std::result::Result<String, String> {
        let e = |e: rusqlite::Error| e.to_string();
        // Not into itself or a folder below it.
        let mut p = parent;
        while p != 0 {
            if p == id {
                return Err("A folder can't go inside itself.".into());
            }
            p = self.conn.query_row("SELECT parent FROM folders WHERE id=?", [p], |r| r.get(0)).map_err(e)?;
        }
        let (old_parent, name): (i64, String) =
            self.conn.query_row("SELECT parent, name FROM folders WHERE id=?", [id], |r| Ok((r.get(0)?, r.get(1)?))).map_err(e)?;
        if old_parent != parent {
            let taken: bool = self
                .conn
                .query_row("SELECT EXISTS(SELECT 1 FROM folders WHERE parent=? AND name=?)", params![parent, name], |r| r.get(0))
                .map_err(e)?;
            if taken {
                return Err(format!("There is already a folder named {name} there."));
            }
        }
        let tx = rusqlite::Transaction::new_unchecked(&self.conn, rusqlite::TransactionBehavior::Immediate).map_err(e)?;
        let mut order: Vec<i64> = {
            let mut st = tx.prepare("SELECT id FROM folders WHERE parent=? AND id<>? ORDER BY pos, name").map_err(e)?;
            st.query_map(params![parent, id], |r| r.get(0)).map_err(e)?.collect::<Result<_>>().map_err(e)?
        };
        order.insert(index.min(order.len()), id);
        tx.execute("UPDATE folders SET parent=? WHERE id=?", params![parent, id]).map_err(e)?;
        for (pos, f) in order.iter().enumerate() {
            tx.execute("UPDATE folders SET pos=? WHERE id=?", params![pos as i64, f]).map_err(e)?;
        }
        if old_parent != parent {
            // Its clips are found by folder paths, which changed.
            for clip in Self::clips_below(&tx, id).map_err(e)? {
                Self::reindex(&tx, clip).map_err(e)?;
            }
        }
        tx.commit().map_err(e)?;
        self.folder_path(id).map_err(e)
    }

    /// Clip `clip` is filed in folder `folder`.
    pub fn is_filed(&self, folder: i64, clip: i64) -> bool {
        self.conn
            .query_row("SELECT EXISTS(SELECT 1 FROM snippets WHERE folder_id=? AND clip_id=?)", params![folder, clip], |r| r.get(0))
            .unwrap_or(false)
    }

    /// A snippet's own title in a folder ("" when it shows its text).
    pub fn snippet_title(&self, folder: i64, clip: i64) -> Result<String> {
        self.conn
            .query_row("SELECT title FROM snippets WHERE folder_id=? AND clip_id=?", params![folder, clip], |r| r.get(0))
            .optional()
            .map(|t| t.unwrap_or_default())
    }

    /// Set (or clear, with "") a snippet's title in a folder.
    pub fn set_snippet_title(&self, folder: i64, clip: i64, title: &str) -> Result<()> {
        let tx = rusqlite::Transaction::new_unchecked(&self.conn, rusqlite::TransactionBehavior::Immediate)?;
        tx.execute("UPDATE snippets SET title=? WHERE folder_id=? AND clip_id=?", params![title.trim(), folder, clip])?;
        Self::reindex(&tx, clip)?;
        tx.commit()
    }

    /// When a new snippet is dated: just before the oldest clip that isn't
    /// one, so snippets don't crowd the top of the history.
    pub fn snippet_ts(&self) -> Result<i64> {
        let oldest: Option<i64> = self.conn.query_row(
            "SELECT MIN(created_at) FROM clips WHERE id NOT IN (SELECT clip_id FROM snippets)",
            [],
            |r| r.get(0),
        )?;
        Ok(oldest.unwrap_or_else(crate::timefmt::now_ms) - 60_000)
    }

    /// Give clip `id` the text of `c` (a text clip from ingest::build), where
    /// it is: folders, titles, tags, stars and dates stay. If another clip
    /// already holds that text, that one takes this one's place: it gets its
    /// folders (this one's titles win), tags and star, and this one goes.
    /// Returns the clip that has the text.
    pub fn set_text(&self, id: i64, c: &NewClip, level: i32) -> std::result::Result<i64, String> {
        let e = |e: rusqlite::Error| e.to_string();
        if self.is_sealed(id) {
            return Err("Its text is encrypted (password tag): remove the tag to edit it.".into());
        }
        let other: Option<i64> =
            self.conn.query_row("SELECT id FROM clips WHERE hash=?", [&c.hash], |r| r.get(0)).optional().map_err(e)?;
        if let Some(other) = other {
            if other != id {
                let tx = rusqlite::Transaction::new_unchecked(&self.conn, rusqlite::TransactionBehavior::Immediate).map_err(e)?;
                tx.execute(
                    "INSERT INTO snippets(folder_id, clip_id, title, pos) \
                     SELECT folder_id, ?2, title, pos FROM snippets WHERE clip_id=?1 \
                     ON CONFLICT(folder_id, clip_id) DO UPDATE SET \
                     title=CASE WHEN excluded.title<>'' THEN excluded.title ELSE title END",
                    params![id, other],
                )
                .map_err(e)?;
                tx.execute("INSERT OR IGNORE INTO tags(clip_id, tag) SELECT ?2, tag FROM tags WHERE clip_id=?1", params![id, other])
                    .map_err(e)?;
                tx.execute(
                    "UPDATE clips SET starred=MAX(starred, (SELECT starred FROM clips WHERE id=?1)) WHERE id=?2",
                    params![id, other],
                )
                .map_err(e)?;
                // Its folders now hold the other clip: nothing to prune.
                tx.execute("DELETE FROM snippets WHERE clip_id=?", [id]).map_err(e)?;
                Self::delete_in(&tx, id).map_err(e)?;
                Self::reindex(&tx, other).map_err(e)?;
                tx.commit().map_err(e)?;
            }
            return Ok(other);
        }
        let reps: Vec<(i64, &str, i64, i64, Vec<u8>)> = c
            .reps
            .iter()
            .map(|r| {
                let (codec, data) = compress(&r.data, level);
                (r.item, r.uti.as_str(), codec, r.data.len() as i64, data)
            })
            .collect();
        let text_z = c.text.as_ref().map(|t| zstd::bulk::compress(t.as_bytes(), level.max(3)).unwrap_or_default());
        let raw: i64 = reps.iter().map(|r| r.3).sum();
        let stored: i64 = reps.iter().map(|r| r.4.len() as i64).sum();
        let tx = rusqlite::Transaction::new_unchecked(&self.conn, rusqlite::TransactionBehavior::Immediate).map_err(e)?;
        tx.execute(
            "UPDATE clips SET kind=?2, hash=?3, preview=?4, text_z=?5, text_len=?6, types=?7, raw_size=?8, \
             stored_size=?9, thumb=NULL, img_w=NULL, img_h=NULL, img_scale=NULL, ocr_text=NULL, ocr_state=1 WHERE id=?1",
            params![
                id,
                c.kind.as_str(),
                c.hash,
                c.preview,
                text_z,
                c.text.as_ref().map(|t| t.len() as i64).unwrap_or(0),
                c.types,
                raw,
                stored
            ],
        )
        .map_err(e)?;
        Self::write_reps(&tx, id, &reps, true).map_err(e)?;
        if c.kind == Kind::Url {
            tx.execute("INSERT OR IGNORE INTO tags(clip_id, tag) VALUES(?, ?)", params![id, URL_TAG]).map_err(e)?;
        } else {
            tx.execute("DELETE FROM tags WHERE clip_id=? AND tag=?", params![id, URL_TAG]).map_err(e)?;
        }
        Self::reindex(&tx, id).map_err(e)?;
        tx.commit().map_err(e)?;
        Ok(id)
    }

    /// File clips stored with a file reference URL (`file:///.file/id=…`).
    pub fn file_ref_clips(&self) -> Result<Vec<i64>> {
        let mut st = self.conn.prepare("SELECT id FROM clips WHERE kind='file' AND preview LIKE '%/.file/id=%'")?;
        let ids = st.query_map([], |r| r.get(0))?.collect();
        ids
    }

    /// The source path a file reference URL left, now resolved (and indexed).
    pub fn fix_file_ref_source(&self, id: i64, path: Option<&str>) -> Result<()> {
        let n = self.conn.execute(
            "UPDATE clips SET source_path=?2 WHERE id=?1 AND (source_path IS NULL OR source_path LIKE '/.file/id=%')",
            params![id, path],
        )?;
        if n > 0 {
            Self::reindex(&self.conn, id)?;
        }
        Ok(())
    }

    pub fn count(&self) -> Result<i64> {
        self.conn.query_row("SELECT COUNT(*) FROM clips", [], |r| r.get(0))
    }

    pub fn file_size(&self) -> i64 {
        let Some(p) = &self.path else { return 0 };
        let mut total = 0;
        for suffix in ["", "-wal"] {
            let f = PathBuf::from(format!("{}{}", p.display(), suffix));
            total += std::fs::metadata(f).map(|m| m.len() as i64).unwrap_or(0);
        }
        total
    }

    pub fn get_setting(&self, key: &str) -> Option<String> {
        self.conn
            .query_row("SELECT value FROM settings WHERE key=?", [key], |r| r.get(0))
            .optional()
            .ok()
            .flatten()
    }

    pub fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO settings(key, value) VALUES(?,?) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            params![key, value],
        )?;
        Ok(())
    }
}
