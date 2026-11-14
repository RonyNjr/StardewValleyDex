//! StardewValleyDex - initial wiki dump.
//!
//! Downloads every article of https://pt.stardewvalleywiki.com (English
//! fallback for missing pages), rewrites links and assets for the app's
//! `wiki://localhost/` protocol, saves assets to disk and the HTML to the
//! SQLite database the app reads. Same code as the in-app sync
//! (`tauri_app_lib::wiki`).
//!
//! Lives in `examples/` rather than `src/bin/` because `tauri build` ships
//! every `src/bin` binary inside the installers.
//!
//! Usage (from `src-tauri/`):
//!     cargo run --release --example dump -- [options]
//!
//! Run with `--help` for the options.

use std::collections::{BTreeSet, HashSet};
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

use futures_util::{stream, StreamExt};
use rusqlite::Connection;
use tauri_app_lib::db::{self, SyncCursor};
use tauri_app_lib::wiki::{
    crawl, html, percent_decode, title_key, utc_now, Error, PageOutcome, PageSource, WikiClient,
    WikiConfig,
};

/// Must match `identifier` in tauri.conf.json: the app reads its data from
/// `<data dir>/<identifier>`.
const APP_IDENTIFIER: &str = "com.stardewvalleydex.app";
const DEFAULT_WORKERS: usize = 8;
const DEFAULT_MAX_PAGES: usize = 5000;

const USAGE: &str = "\
Usage: cargo run --release --example dump -- [options]

Options:
  --limit N             Process at most N pages (for testing).
  --data-dir PATH       Data directory (default: the app's data directory).
  --resume              Skip pages already in the database.
  --workers N           Concurrent page downloads (default: 8).
  --crawl-missing       After the dump, download pages that stored pages link
                        to but that aren't in the database, until none are
                        left or --max-pages is reached.
  --crawl-missing-only  Skip the main dump and only run the crawl.
  --max-pages N         Cap on pages added by the crawl (default: 5000).
  --verbose-assets      Print every asset download.
  --no-ssl-verify       Don't verify TLS certificates.
  --zip PATH            When done, pack the database and assets into PATH,
                        in the layout the app downloads on first run.
  -h, --help            Show this help.";

struct Args {
    limit: Option<usize>,
    data_dir: PathBuf,
    resume: bool,
    workers: usize,
    crawl_missing: bool,
    crawl_missing_only: bool,
    max_pages: usize,
    verbose_assets: bool,
    no_ssl_verify: bool,
    zip: Option<PathBuf>,
}

enum Parsed {
    Run(Args),
    Help,
}

fn parse_args(mut argv: impl Iterator<Item = String>) -> Result<Parsed, String> {
    let mut args = Args {
        limit: None,
        data_dir: dirs::data_dir()
            .map(|d| d.join(APP_IDENTIFIER))
            .ok_or("could not find the user data directory; pass --data-dir")?,
        resume: false,
        workers: DEFAULT_WORKERS,
        crawl_missing: false,
        crawl_missing_only: false,
        max_pages: DEFAULT_MAX_PAGES,
        verbose_assets: false,
        no_ssl_verify: false,
        zip: None,
    };

    while let Some(arg) = argv.next() {
        // Accept both `--limit 5` and `--limit=5`.
        let split = arg
            .split_once('=')
            .filter(|(f, _)| f.starts_with("--"))
            .map(|(f, v)| (f.to_string(), v.to_string()));
        let (flag, inline) = match split {
            Some((f, v)) => (f, Some(v)),
            None => (arg, None),
        };
        let mut value = |name: &str| {
            inline
                .clone()
                .or_else(|| argv.next())
                .ok_or_else(|| format!("{name} needs a value"))
        };
        let number = |name: &str, v: String| {
            v.parse::<usize>()
                .map_err(|_| format!("{name} expects a number, got {v:?}"))
        };
        match flag.as_str() {
            "-h" | "--help" => return Ok(Parsed::Help),
            "--limit" => args.limit = Some(number("--limit", value("--limit")?)?),
            "--data-dir" => args.data_dir = PathBuf::from(value("--data-dir")?),
            "--resume" => args.resume = true,
            "--workers" => args.workers = number("--workers", value("--workers")?)?.max(1),
            "--crawl-missing" => args.crawl_missing = true,
            "--crawl-missing-only" => args.crawl_missing_only = true,
            "--max-pages" => args.max_pages = number("--max-pages", value("--max-pages")?)?,
            "--verbose-assets" => args.verbose_assets = true,
            "--no-ssl-verify" => args.no_ssl_verify = true,
            "--zip" => args.zip = Some(PathBuf::from(value("--zip")?)),
            other => return Err(format!("unknown option {other:?}")),
        }
    }
    Ok(Parsed::Run(args))
}

#[derive(Default)]
struct Stats {
    written: usize,
    failed: usize,
}

/// Fetch `titles` concurrently and write each result as it arrives.
/// `allow_stub` stores an "unavailable offline" page for titles that exist
/// on neither wiki (crawl mode), so they aren't retried forever.
async fn process_titles(
    client: &WikiClient,
    conn: &Connection,
    titles: Vec<String>,
    workers: usize,
    allow_stub: bool,
) -> Stats {
    let total = titles.len();
    let mut stats = Stats::default();
    let mut done = 0;

    let mut results = stream::iter(titles)
        .map(|title| async move {
            let outcome = client.fetch_page(&title).await;
            (title, outcome)
        })
        .buffer_unordered(workers);

    while let Some((title, outcome)) = results.next().await {
        done += 1;
        let prefix = format!("[{done}/{total}] {title} ...");
        let written = match outcome {
            Ok(PageOutcome::Page { page, source }) => db::upsert_page(
                conn,
                &title,
                &page.html,
                &page.last_modified,
                &page.categories,
            )
            .map(|()| match source {
                PageSource::Pt => "ok".to_string(),
                PageSource::En(en) => format!("ok (EN: {en})"),
            }),
            Ok(PageOutcome::Redirect(target)) => db::upsert_redirect(conn, &title, &target)
                .map(|()| format!("ok (redirect -> {target})")),
            Ok(PageOutcome::Missing) if allow_stub => {
                db::upsert_page(conn, &title, &html::stub_html(&title), &utc_now(), "")
                    .map(|()| "stub (missing on both wikis)".to_string())
            }
            Ok(PageOutcome::Missing) => {
                stats.failed += 1;
                println!("{prefix} x (HTTP 404)");
                continue;
            }
            Err(e) => {
                stats.failed += 1;
                println!("{prefix} x ({e})");
                continue;
            }
        };
        match written {
            Ok(label) => {
                stats.written += 1;
                println!("{prefix} {label}");
            }
            Err(e) => {
                stats.failed += 1;
                println!("{prefix} x (database: {e})");
            }
        }
    }
    stats
}

async fn crawl_missing_pass(
    client: &WikiClient,
    conn: &Connection,
    max_pages: usize,
    workers: usize,
) -> Result<Stats, Error> {
    let mut total = Stats::default();
    let mut round = 0;
    while total.written < max_pages {
        round += 1;
        let missing = crawl::find_missing_targets(conn)?;
        if missing.is_empty() {
            println!("\nNo more missing pages. Crawl done.");
            break;
        }
        let batch: Vec<String> = missing
            .iter()
            .take(max_pages - total.written)
            .cloned()
            .collect();
        println!(
            "\nRound {round}: {} missing pages (downloading {} with {workers} workers)...",
            missing.len(),
            batch.len()
        );
        let stats = process_titles(client, conn, batch, workers, true).await;
        println!(
            "  Round {round} done: {} pages written, {} failed.",
            stats.written, stats.failed
        );
        total.written += stats.written;
        total.failed += stats.failed;
        if stats.written == 0 {
            break;
        }
    }
    Ok(total)
}

async fn run(args: Args) -> Result<bool, Error> {
    let assets_dir = args.data_dir.join("assets");
    std::fs::create_dir_all(&assets_dir)?;
    println!("Data directory: {}", args.data_dir.display());
    println!("Assets directory: {}", assets_dir.display());

    let conn = db::init_db(&args.data_dir)?;
    // Faster bulk writes; the database is only read once the dump is done.
    conn.execute_batch(
        "PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; PRAGMA temp_store=MEMORY;",
    )?;

    let mut cfg = WikiConfig::new(assets_dir);
    cfg.verbose_assets = args.verbose_assets;
    cfg.accept_invalid_certs = args.no_ssl_verify;
    if args.no_ssl_verify {
        println!("WARNING: TLS certificate verification disabled.");
    }
    let client = WikiClient::new(cfg)?;

    let mut failed = 0;
    if !args.crawl_missing_only {
        let started_at = utc_now();
        let start = Instant::now();

        let mut titles = client.list_titles().await?;
        println!("Found {} pages.", titles.len());

        if args.resume {
            let stored: HashSet<String> = db::stored_titles(&conn)?
                .iter()
                .map(|t| title_key(t))
                .collect();
            let before = titles.len();
            titles.retain(|t| !stored.contains(&title_key(t)));
            println!(
                "Resume: skipping {} pages already in the database.",
                before - titles.len()
            );
        }
        if let Some(limit) = args.limit {
            titles.truncate(limit);
            println!("--limit: processing {} page(s).", titles.len());
        }

        let stats = process_titles(&client, &conn, titles, args.workers, false).await;
        failed += stats.failed;

        // The sync fetches wiki changes made after this point. A resumed
        // dump only fills gaps, so it must not move an existing cursor.
        if !args.resume || db::get_sync_cursor(&conn)?.is_none() {
            db::set_sync_cursor(
                &conn,
                &SyncCursor {
                    timestamp: started_at,
                    rcid: 0,
                },
            )?;
        }

        println!();
        println!("{}", "=".repeat(60));
        println!("Main dump done in {:.1}s", start.elapsed().as_secs_f64());
        println!("  pages ok:          {}", stats.written);
        println!("  pages failed:      {}", stats.failed);
        println!("  assets downloaded: {}", client.assets().downloaded());
        println!("  db: {}", args.data_dir.join(db::DB_FILE_NAME).display());
        println!("  assets: {}", client.assets().dir().display());
    }

    if args.crawl_missing || args.crawl_missing_only {
        println!();
        println!("{}", "=".repeat(60));
        println!("Starting link-graph closure pass (--crawl-missing)...");
        let stats = crawl_missing_pass(&client, &conn, args.max_pages, args.workers).await?;
        println!("  crawl added: {}", stats.written);
        println!(
            "  total assets downloaded: {}",
            client.assets().downloaded()
        );
    }

    if let Some(zip_path) = &args.zip {
        let assets = referenced_assets(&conn, &args.data_dir.join("assets"))?;
        // Fold the WAL back in so the packed database is one self-contained file.
        conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE); PRAGMA journal_mode=DELETE; VACUUM;")?;
        drop(conn);
        println!();
        println!("Writing {} ...", zip_path.display());
        write_release_zip(&args.data_dir, &assets, zip_path)?;
        let size = std::fs::metadata(zip_path)?.len();
        println!(
            "  {} assets + database, {:.1} MB",
            assets.len(),
            size as f64 / 1_000_000.0
        );
    }

    Ok(failed == 0)
}

/// Asset files (paths relative to `assets_dir`) that some stored page
/// points at. Files no page uses, such as leftovers from earlier runs or
/// half-written downloads, stay out of the release zip.
fn referenced_assets(conn: &Connection, assets_dir: &Path) -> Result<Vec<String>, Error> {
    const PREFIX: &str = "wiki://localhost/";
    let mut found = BTreeSet::new();
    let mut stmt = conn.prepare("SELECT html FROM pages")?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let html = row.get_ref(0)?.as_str()?;
        for (i, _) in html.match_indices(PREFIX) {
            let rest = &html[i + PREFIX.len()..];
            let end = rest
                .find(['"', '\'', '(', ')', '<', '>', ' ', '?', '#'])
                .unwrap_or(rest.len());
            let rel = percent_decode(&rest[..end].replace("&amp;", "&"));
            if !rel.is_empty() && !rel.split('/').any(|part| part == "..") {
                found.insert(rel);
            }
        }
    }
    Ok(found
        .into_iter()
        .filter(|rel| assets_dir.join(rel).is_file())
        .collect())
}

/// Pack the database and `assets` the way the app's `download_and_setup`
/// reads them: `stardewvalleydex.sqlite` plus `assets/...`.
fn write_release_zip(data_dir: &Path, assets: &[String], out: &Path) -> Result<(), Error> {
    use zip::write::SimpleFileOptions;
    use zip::CompressionMethod;

    let deflated = SimpleFileOptions::default()
        .compression_method(CompressionMethod::Deflated)
        .large_file(true);
    // Images are already compressed: storing them is as small and much faster.
    let stored = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);

    if let Some(parent) = out.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    // Build next to the target and rename at the end, so an interrupted run
    // never leaves a truncated zip behind under the final name.
    let tmp = out.with_extension("zip.part");
    let mut zip = zip::ZipWriter::new(BufWriter::new(File::create(&tmp)?));

    zip.start_file(db::DB_FILE_NAME, deflated)?;
    std::io::copy(&mut File::open(data_dir.join(db::DB_FILE_NAME))?, &mut zip)?;

    let assets_dir = data_dir.join("assets");
    for rel in assets {
        let compressible = rel.ends_with(".css") || rel.ends_with(".svg");
        zip.start_file(
            format!("assets/{rel}"),
            if compressible { deflated } else { stored },
        )?;
        std::io::copy(&mut File::open(assets_dir.join(rel))?, &mut zip)?;
    }

    zip.finish()?.flush()?;
    std::fs::rename(&tmp, out)?;
    Ok(())
}

#[tokio::main]
async fn main() -> ExitCode {
    let args = match parse_args(std::env::args().skip(1)) {
        Ok(Parsed::Run(args)) => args,
        Ok(Parsed::Help) => {
            println!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Err(msg) => {
            eprintln!("error: {msg}\n\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    match run(args).await {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}
