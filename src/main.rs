use crabclip::db::Db;
use crabclip::settings::Settings;

fn usage() {
    println!(
        "crabclip {} — forever clipboard history\n\n\
         Usage:\n  crabclip                         run the menu bar app\n  \
         crabclip --import-collective [archive...]   import Collective.app history (default: its container)\n  \
         crabclip --import-clipy [file]   import Clipy's history, snippets and shortcuts (its sqlite.db, or a snippets XML export)\n  \
         crabclip --stats                 print database stats\n  \
         crabclip --search <query>        search from the terminal",
        env!("CARGO_PKG_VERSION")
    );
}

fn main() {
    std::panic::set_hook(Box::new(|info| {
        crabclip::log!("PANIC: {info}");
    }));
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        None => crabclip::ui::run(),
        Some(a) if a.starts_with("-psn") || a == "--takeover" => crabclip::ui::run(),
        Some("--import-collective") => {
            let db = Db::open(&Db::default_path()).expect("open db");
            let settings = Settings::load(&db);
            let paths: Vec<std::path::PathBuf> = if args.len() > 1 {
                args[1..].iter().map(Into::into).collect()
            } else {
                crabclip::collective::default_paths()
            };
            if paths.is_empty() {
                println!("No Collective archive found.");
                std::process::exit(1);
            }
            let mut summary = Vec::new();
            for p in paths {
                let mut last = std::time::Instant::now();
                let r = crabclip::collective::import(&p, &db, &settings, |st| {
                    if last.elapsed().as_secs() >= 2 {
                        last = std::time::Instant::now();
                        eprintln!("  {}/{} added {} merged {}", st.added + st.merged + st.skipped, st.total, st.added, st.merged);
                    }
                });
                match r {
                    Ok(st) => summary.push(format!(
                        "{}: {} clips, {} added, {} already there, {} skipped",
                        p.file_name().unwrap_or_default().to_string_lossy(),
                        st.total,
                        st.added,
                        st.merged,
                        st.skipped
                    )),
                    Err(e) => summary.push(format!("{}: {e}", p.display())),
                }
            }
            let _ = db.vacuum_some();
            println!("{}", summary.join("\n"));
        }
        Some("--import-clipy") => {
            let db = Db::open(&Db::default_path()).expect("open db");
            let settings = Settings::load(&db);
            let Some(path) = args.get(1).map(std::path::PathBuf::from).or_else(|| crabclip::clipy::default_db(true)) else {
                println!("No Clipy database found. Open the current Clipy once (it converts older data), or pass a snippets XML export.");
                std::process::exit(1);
            };
            let prefs = crabclip::clipy::default_prefs();
            let mut last = std::time::Instant::now();
            let r = crabclip::clipy::import(&path, prefs.as_ref(), &db, &settings, |st| {
                if last.elapsed().as_secs() >= 2 {
                    last = std::time::Instant::now();
                    eprintln!("  {}/{} history clips", st.added + st.merged + st.skipped, st.history);
                }
            });
            let _ = db.vacuum_some();
            match r {
                Ok(st) => {
                    if st.shortcuts > 0 {
                        // A running crabclip takes them now (and won't save its old ones over them).
                        crabclip::ui::post_distributed(crabclip::ui::SHORTCUTS_NOTIFICATION);
                    }
                    println!("{}", st.summary())
                }
                Err(e) => {
                    println!("{}: {e}", path.display());
                    std::process::exit(1);
                }
            }
        }
        Some("--stats") => {
            let db = Db::open(&Db::default_path()).expect("open db");
            println!(
                "{} clips, {} on disk ({})",
                db.count().unwrap_or(0),
                crabclip::timefmt::bytes(db.file_size()),
                Db::default_path().display()
            );
        }
        Some("--search") => {
            let db = Db::open(&Db::default_path()).expect("open db");
            let q = args[1..].join(" ");
            let t0 = std::time::Instant::now();
            let ids = db
                .search(&crabclip::db::Filter { query: &q, starred: false, app: None }, 50)
                .unwrap_or_default();
            let dt = t0.elapsed();
            for id in &ids {
                if let Ok(Some(r)) = db.row(*id) {
                    let line: String = r.preview.lines().next().unwrap_or("").chars().take(90).collect();
                    println!(
                        "{:>6} {:<20} {:<6} {}",
                        id,
                        crabclip::timefmt::human(r.copied_at),
                        r.kind.as_str(),
                        line
                    );
                }
            }
            eprintln!("{} results in {:?}", ids.len(), dt);
        }
        _ => usage(),
    }
}
