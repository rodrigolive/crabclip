use crabclip::db::{AppInfo, Db, Filter, Inserted, Kind, Rep};
use crabclip::ingest::{Capture, build};
use crabclip::settings::Settings;

fn text_capture(ts: i64, text: &str, app: &str) -> Capture {
    Capture {
        ts,
        reps: vec![
            Rep { item: 0, uti: "public.utf8-plain-text".into(), data: text.as_bytes().to_vec() },
            Rep { item: 0, uti: "NSStringPboardType".into(), data: text.as_bytes().to_vec() },
        ],
        app: AppInfo { bundle: Some(format!("com.test.{app}")), name: Some(app.into()), path: None },
        ..Default::default()
    }
}

fn search(db: &Db, q: &str) -> Vec<i64> {
    db.search(&Filter { query: q, starred: false, app: None }, 1000).unwrap()
}

#[test]
fn insert_dedupe_search_roundtrip() {
    let db = Db::open_memory().unwrap();
    let s = Settings::default();
    let big = "hello world ".repeat(2000);
    let a = db.insert(&build(text_capture(1000, &big, "iTerm2"), &s).unwrap(), 3).unwrap();
    let b = db.insert(&build(text_capture(2000, "Ünïcode snowman ☃ https://x", "Safari"), &s).unwrap(), 3).unwrap();
    let c = db.insert(&build(text_capture(3000, "https://example.com/page", "Chrome"), &s).unwrap(), 3).unwrap();
    assert!(matches!(a, Inserted::New(_)) && matches!(b, Inserted::New(_)));
    assert_eq!(db.detail(c.id()).unwrap().unwrap().kind, Kind::Url);

    // legacy duplicate type dropped, data comes back byte-identical
    let reps = db.reps(a.id()).unwrap();
    assert_eq!(reps.len(), 1);
    assert_eq!(reps[0].data, big.as_bytes());
    let d = db.detail(a.id()).unwrap().unwrap();
    assert!(d.stored_size < d.raw_size / 10, "compressed {} vs {}", d.stored_size, d.raw_size);

    // newest first, substring + unicode + metadata + short terms
    assert_eq!(search(&db, ""), vec![c.id(), b.id(), a.id()]);
    assert_eq!(search(&db, "snowm"), vec![b.id()]);
    assert_eq!(search(&db, "unicode"), vec![b.id()], "trigram is case-insensitive");
    assert_eq!(search(&db, "iterm"), vec![a.id()], "app name is searchable");
    assert_eq!(search(&db, "app:safari"), vec![b.id()]);
    assert_eq!(search(&db, "type:url"), vec![c.id()]);
    assert_eq!(search(&db, "https -example"), vec![b.id()]);
    assert_eq!(search(&db, "☃"), vec![b.id()]);

    // copying the same text again moves it to the top instead of duplicating
    let again = db.insert(&build(text_capture(4000, &big, "Terminal"), &s).unwrap(), 3).unwrap();
    assert_eq!(again, Inserted::Existing(a.id()));
    assert_eq!(search(&db, "")[0], a.id());
    assert_eq!(db.count().unwrap(), 3);

    // stars and filters
    db.set_star(b.id(), true).unwrap();
    assert_eq!(db.search(&Filter { query: "", starred: true, app: None }, 10).unwrap(), vec![b.id()]);
    assert_eq!(db.search(&Filter { query: "", starred: false, app: Some("com.test.Chrome") }, 10).unwrap(), vec![c.id()]);

    // clear keeps starred
    assert_eq!(db.clear(None, false).unwrap(), 2);
    assert_eq!(search(&db, ""), vec![b.id()]);
    assert_eq!(search(&db, "hello"), Vec::<i64>::new(), "fts rows removed with clips");

    // OCR text becomes searchable
    db.set_ocr(b.id(), Some("Save your changes"), 1).unwrap();
    assert_eq!(search(&db, "your chan"), vec![b.id()]);
}

#[test]
fn files_and_skips() {
    let s = Settings::default();
    let cap = Capture {
        ts: 1,
        reps: vec![
            Rep { item: 0, uti: "public.file-url".into(), data: b"file:///Users/rod/My%20Doc.txt".to_vec() },
            Rep { item: 1, uti: "public.file-url".into(), data: b"file:///tmp/b.png".to_vec() },
        ],
        ..Default::default()
    };
    let clip = build(cap, &s).unwrap();
    assert_eq!(clip.kind, Kind::File);
    assert_eq!(clip.text.as_deref(), Some("/Users/rod/My Doc.txt\n/tmp/b.png"));
    assert_eq!(clip.preview, "/Users/rod/My Doc.txt\n/tmp/b.png");

    let secret = Capture {
        ts: 1,
        reps: vec![
            Rep { item: 0, uti: "public.utf8-plain-text".into(), data: b"hunter2".to_vec() },
            Rep { item: 0, uti: "org.nspasteboard.ConcealedType".into(), data: b"".to_vec() },
        ],
        ..Default::default()
    };
    assert!(build(secret, &s).is_none());
}

#[test]
fn odd_file_urls_do_not_panic() {
    use crabclip::ingest::{file_url_to_path, percent_decode};
    assert_eq!(percent_decode("a%a\u{20ac}b%2"), "a%a\u{20ac}b%2");
    assert_eq!(percent_decode("%E2%82%AC%20x"), "\u{20ac} x");
    assert_eq!(file_url_to_path("file:///tmp/x%a\u{20ac}.txt").as_deref(), Some("/tmp/x%a\u{20ac}.txt"));
}

#[test]
fn recopy_never_strips_rich_formats() {
    let db = Db::open_memory().unwrap();
    let s = Settings::default();
    let rich = Capture {
        ts: 10,
        reps: vec![
            Rep { item: 0, uti: "public.utf8-plain-text".into(), data: b"hello rich".to_vec() },
            Rep { item: 0, uti: "public.rtf".into(), data: b"{\\rtf1 hello rich}".to_vec() },
        ],
        ..Default::default()
    };
    let id = db.insert(&build(rich, &s).unwrap(), 3).unwrap().id();
    let plain = text_capture(20, "hello rich", "Terminal");
    assert_eq!(db.insert(&build(plain, &s).unwrap(), 3).unwrap(), Inserted::Existing(id));
    assert_eq!(db.reps(id).unwrap().len(), 2, "rtf kept");
    assert_eq!(db.search(&Filter { query: "", starred: false, app: None }, 5).unwrap(), vec![id]);
    assert_eq!(db.detail(id).unwrap().unwrap().app_name.as_deref(), Some("Terminal"));
}

#[test]
fn image_thumbnail_and_dims() {
    // 4x2 red PNG generated through ImageIO itself from a TIFF-less path:
    // use a tiny known PNG.
    let png: &[u8] = &[
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52,
        0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x02, 0x00, 0x00, 0x00, 0x90, 0x77, 0x53,
        0xDE, 0x00, 0x00, 0x00, 0x0C, 0x49, 0x44, 0x41, 0x54, 0x08, 0xD7, 0x63, 0xF8, 0xCF, 0xC0, 0x00,
        0x00, 0x03, 0x01, 0x01, 0x00, 0x18, 0xDD, 0x8D, 0xB0, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E,
        0x44, 0xAE, 0x42, 0x60, 0x82,
    ];
    let info = crabclip::imageio::info(png).expect("info");
    assert_eq!((info.width, info.height), (1, 1));
    let thumb = crabclip::imageio::thumbnail(png, 760, 300).expect("thumb");
    assert_eq!(&thumb[..2], &[0xFF, 0xD8], "jpeg");
    let cap = Capture {
        ts: 5,
        reps: vec![Rep { item: 0, uti: "public.png".into(), data: png.to_vec() }],
        ..Default::default()
    };
    let clip = build(cap, &Settings::default()).unwrap();
    assert_eq!(clip.kind, Kind::Image);
    assert_eq!(clip.img, Some((1, 1, 1.0)));
    let db = Db::open_memory().unwrap();
    let id = db.insert(&clip, 3).unwrap().id();
    assert_eq!(db.reps(id).unwrap()[0].data, png, "image bytes come back as-is");
    assert_eq!(db.pending_ocr(10).unwrap(), vec![id]);
}

#[test]
fn insert_waits_for_a_concurrent_writer() {
    let dir = std::env::temp_dir().join(format!("crabclip-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("busy.db");
    let a = Db::open(&path).unwrap();
    let b = Db::open(&path).unwrap();
    a.conn.execute_batch("BEGIN IMMEDIATE; UPDATE settings SET value=value;").unwrap();
    let t = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(400));
        a.conn.execute_batch("COMMIT").unwrap();
    });
    let clip = build(text_capture(1, "while locked", "x"), &Settings::default()).unwrap();
    assert!(matches!(b.insert(&clip, 3).unwrap(), Inserted::New(_)));
    t.join().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn file_reference_urls_become_paths() {
    let dir = std::env::temp_dir().join(format!("crabclip-fileref-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("Short demo.pptx");
    std::fs::write(&file, b"slides").unwrap();
    let path = std::fs::canonicalize(&file).unwrap().to_string_lossy().into_owned();
    // What some apps put on the pasteboard: file:///.file/id=…
    let url = objc2_foundation::NSURL::fileURLWithPath(&objc2_foundation::NSString::from_str(&path));
    let reference = url.fileReferenceURL().and_then(|u| u.absoluteString()).unwrap().to_string();
    assert!(reference.starts_with("file:///.file/id="), "{reference}");

    let cap = Capture {
        ts: 1,
        reps: vec![Rep { item: 0, uti: "public.file-url".into(), data: reference.into_bytes() }],
        app: AppInfo::default(),
        window_title: None,
        source_path: None,
        starred: false,
    };
    let clip = build(cap, &Settings::default()).unwrap();
    assert_eq!(clip.kind, Kind::File);
    assert_eq!(clip.preview, path);
    assert_eq!(clip.source_path.as_deref(), Some(path.as_str()));
    let rep = String::from_utf8(clip.reps[0].data.clone()).unwrap();
    assert!(rep.starts_with("file:///") && !rep.contains("/.file/id="), "{rep}");

    // A stored clip from before is resolved in place.
    let db = Db::open_memory().unwrap();
    let mut old = clip.clone();
    old.reps[0].data = url.fileReferenceURL().and_then(|u| u.absoluteString()).unwrap().to_string().into_bytes();
    old.preview = String::from_utf8(old.reps[0].data.clone()).unwrap().trim_start_matches("file://").to_string();
    old.hash = vec![7; 32]; // hashed from the reference, as it was stored
    old.source_path = Some(old.preview.clone());
    let id = db.insert(&old, 3).unwrap().id();
    assert_eq!(db.file_ref_clips().unwrap(), vec![id]);
    let mut reps = db.reps(id).unwrap();
    assert!(crabclip::ingest::resolve_file_refs(&mut reps));
    // As the ingest thread does at start: rebuilt in place, path indexed.
    let cap = Capture { ts: 1, reps, app: AppInfo::default(), window_title: None, source_path: None, starred: false };
    let fixed = build(cap, &Settings::default()).unwrap();
    let kept = db.set_text(id, &fixed, 3).unwrap();
    db.fix_file_ref_source(kept, fixed.source_path.as_deref()).unwrap();
    assert_eq!(kept, id);
    assert!(db.file_ref_clips().unwrap().is_empty());
    assert_eq!(db.detail(id).unwrap().unwrap().source_path.as_deref(), Some(path.as_str()));
    std::fs::remove_dir_all(&dir).unwrap();
}
