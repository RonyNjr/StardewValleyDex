//! Background sync: every `SYNC_INTERVAL`, ask the wiki which articles
//! changed since the last pass and refresh them with the same pipeline the
//! dump uses (`wiki::WikiClient::fetch_page`).
//!
//! A page already in the database is only ever replaced by real, processed
//! HTML. When the wiki can't be reached, the page vanished upstream, or
//! anything else goes wrong, the stored page stays exactly as it was.

use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

use futures_util::{stream, StreamExt};
use rusqlite::Connection;
use tauri::Manager;

use crate::db::{self, SyncCursor};
use crate::wiki::{format_utc, now_unix, Error, PageOutcome, WikiClient, WikiConfig};

/// Interval between two sync passes.
const SYNC_INTERVAL: Duration = Duration::from_secs(30 * 60);
/// Delay before the first pass so the UI can finish loading.
const STARTUP_DELAY: Duration = Duration::from_secs(10);
/// Most pages refreshed in one pass; the rest waits for the next pass.
const MAX_PAGES_PER_PASS: usize = 200;
/// Concurrent page downloads during a pass.
const SYNC_WORKERS: usize = 4;
/// Without a stored cursor (database made by the old Python dump), look
/// this far back on the first pass.
const FIRST_SYNC_LOOKBACK_SECS: i64 = 30 * 24 * 60 * 60;

/// Spawn the background sync task. Returns immediately; the task keeps
/// running for the lifetime of the application.
pub fn start(app_handle: tauri::AppHandle, assets_dir: PathBuf) {
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(STARTUP_DELAY).await;

        let client = match WikiClient::new(WikiConfig::new(assets_dir)) {
            Ok(client) => client,
            Err(e) => {
                eprintln!("[sync] disabled, could not build HTTP client: {e}");
                return;
            }
        };

        loop {
            let db = app_handle.state::<Mutex<Connection>>();
            match run_sync_pass(&client, &db).await {
                Ok(report) => println!("[sync] {report:?}"),
                Err(e) => eprintln!("[sync] pass failed: {e}"),
            }
            tokio::time::sleep(SYNC_INTERVAL).await;
        }
    });
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct SyncReport {
    /// True when the pass did nothing because the data package is missing
    /// or outdated (a fresh download is pending).
    pub skipped_no_data: bool,
    pub updated: usize,
    /// Pages left untouched on purpose (gone upstream or now a redirect).
    pub kept: usize,
    /// Pages left untouched because fetching them failed.
    pub failed: usize,
}

pub async fn run_sync_pass(
    client: &WikiClient,
    db: &Mutex<Connection>,
) -> Result<SyncReport, Error> {
    let cursor = {
        let conn = lock(db)?;
        // Before the data package is imported (or while an outdated one is
        // waiting to be replaced) there's nothing worth syncing. Writing a
        // few pages into an empty database would also make `check_setup`
        // report the app as ready and skip the real download.
        if !db::data_up_to_date(&conn)? {
            return Ok(SyncReport {
                skipped_no_data: true,
                ..Default::default()
            });
        }
        db::get_sync_cursor(&conn)?.unwrap_or_else(|| SyncCursor {
            timestamp: format_utc(now_unix() - FIRST_SYNC_LOOKBACK_SECS),
            rcid: 0,
        })
    };

    let batch = client.recent_changes(&cursor, MAX_PAGES_PER_PASS).await?;
    let mut report = SyncReport::default();

    let mut results = stream::iter(batch.titles)
        .map(|title| async move {
            let outcome = client.fetch_page(&title).await;
            (title, outcome)
        })
        .buffer_unordered(SYNC_WORKERS);

    while let Some((title, outcome)) = results.next().await {
        match outcome {
            Ok(PageOutcome::Page { page, .. }) => {
                let conn = lock(db)?;
                // Update the row the app serves, whatever its spelling.
                let stored = db::canonical_title(&conn, &title)?.unwrap_or(title);
                db::upsert_page(
                    &conn,
                    &stored,
                    &page.html,
                    &page.last_modified,
                    &page.categories,
                )?;
                report.updated += 1;
            }
            Ok(PageOutcome::Redirect(_) | PageOutcome::Missing) => report.kept += 1,
            Err(e) => {
                eprintln!("[sync] keeping the stored copy of {title}: {e}");
                report.failed += 1;
            }
        }
    }

    // If every fetch failed the wiki is probably unreachable: keep the
    // cursor so the next pass retries the same changes. Isolated failures
    // don't hold the cursor back; those pages keep their current HTML and
    // are refreshed on their next edit.
    let all_failed = report.failed > 0 && report.updated == 0 && report.kept == 0;
    if let (Some(next), false) = (batch.cursor, all_failed) {
        db::set_sync_cursor(&*lock(db)?, &next)?;
    }

    Ok(report)
}

fn lock(db: &Mutex<Connection>) -> Result<std::sync::MutexGuard<'_, Connection>, Error> {
    db.lock().map_err(|_| "database mutex poisoned".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wiki::test_server::{test_config, Reply, TempDir, TestServer};

    const ORIGINAL: &str =
        "<html><body><h1>Abacaxi</h1><p>conteúdo baixado pelo dump</p></body></html>";

    fn db_with_page() -> Mutex<Connection> {
        let conn = Connection::open_in_memory().unwrap();
        db::create_schema(&conn).unwrap();
        db::upsert_page(&conn, "Abacaxi", ORIGINAL, "dump", "Frutas").unwrap();
        db::set_data_version(&conn, db::DATA_VERSION).unwrap();
        db::set_sync_cursor(
            &conn,
            &SyncCursor {
                timestamp: "2026-11-01T00:00:00Z".into(),
                rcid: 100,
            },
        )
        .unwrap();
        Mutex::new(conn)
    }

    fn stored_html(db: &Mutex<Connection>, title: &str) -> Option<String> {
        db::get_page(&db.lock().unwrap(), title)
    }

    fn cursor(db: &Mutex<Connection>) -> SyncCursor {
        db::get_sync_cursor(&db.lock().unwrap()).unwrap().unwrap()
    }

    /// Wiki whose recent changes list `titles` and whose pages are served by `page`.
    async fn wiki(
        titles: &'static [&'static str],
        page: impl Fn(&str) -> Reply + Send + Sync + 'static,
    ) -> TestServer {
        TestServer::start(move |target| {
            if target.contains("list=recentchanges") {
                let changes: Vec<String> = titles
                    .iter()
                    .enumerate()
                    .map(|(i, t)| {
                        format!(
                            r#"{{"type":"edit","ns":0,"title":"{t}","rcid":{},"timestamp":"2026-11-02T00:00:0{i}Z"}}"#,
                            101 + i
                        )
                    })
                    .collect();
                return Reply::json(&format!(r#"{{"query":{{"recentchanges":[{}]}}}}"#, changes.join(",")));
            }
            if target.starts_with("/mediawiki/api.php") {
                // langlinks / redirects lookups: nothing known.
                return Reply::json(r#"{"query":{"pages":[]}}"#);
            }
            page(target)
        })
        .await
    }

    #[tokio::test]
    async fn keeps_the_original_page_when_the_wiki_fails() {
        let server = wiki(&["Abacaxi"], |_| Reply::status(500)).await;
        let tmp = TempDir::new();
        let client = WikiClient::new(test_config(&server.base, tmp.path())).unwrap();
        let db = db_with_page();

        let report = run_sync_pass(&client, &db).await.unwrap();

        assert_eq!(
            report,
            SyncReport {
                failed: 1,
                ..Default::default()
            }
        );
        assert_eq!(stored_html(&db, "Abacaxi").as_deref(), Some(ORIGINAL));
        // Everything failed: retry the same changes next time.
        assert_eq!(cursor(&db).rcid, 100);
    }

    #[tokio::test]
    async fn offline_pass_changes_nothing() {
        // Grab a free port and close it again: connections get refused, like
        // a machine with no network.
        let addr = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap();
        let tmp = TempDir::new();
        let client = WikiClient::new(test_config(&format!("http://{addr}"), tmp.path())).unwrap();
        let db = db_with_page();

        assert!(run_sync_pass(&client, &db).await.is_err());
        assert_eq!(stored_html(&db, "Abacaxi").as_deref(), Some(ORIGINAL));
        assert_eq!(cursor(&db).rcid, 100);
    }

    #[tokio::test]
    async fn keeps_the_original_page_when_it_is_gone_upstream() {
        let server = wiki(&["Abacaxi"], |_| Reply::status(404)).await;
        let tmp = TempDir::new();
        let client = WikiClient::new(test_config(&server.base, tmp.path())).unwrap();
        let db = db_with_page();

        let report = run_sync_pass(&client, &db).await.unwrap();

        assert_eq!(
            report,
            SyncReport {
                kept: 1,
                ..Default::default()
            }
        );
        assert_eq!(stored_html(&db, "Abacaxi").as_deref(), Some(ORIGINAL));
        assert_eq!(cursor(&db).rcid, 101);
    }

    #[tokio::test]
    async fn never_stores_a_placeholder() {
        // Regression for the old sync, which replaced recently edited pages
        // with "Page pending sync".
        let server = wiki(&["Abacaxi", "Morango"], |target| match target {
            "/Morango" => Reply::html(200, "<html><body><p>novo</p></body></html>"),
            _ => Reply::status(503),
        })
        .await;
        let tmp = TempDir::new();
        let client = WikiClient::new(test_config(&server.base, tmp.path())).unwrap();
        let db = db_with_page();

        run_sync_pass(&client, &db).await.unwrap();

        let conn = db.lock().unwrap();
        let placeholders: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pages WHERE html LIKE '%pending sync%'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(placeholders, 0);
        assert_eq!(db::get_page(&conn, "Abacaxi").as_deref(), Some(ORIGINAL));
    }

    #[tokio::test]
    async fn refreshes_changed_pages_with_processed_html() {
        let server = wiki(&["Abacaxi", "Morango"], |target| match target {
            "/Abacaxi" => Reply::html(
                200,
                r#"<html><body><h1>Abacaxi</h1><p>editado</p><a href="/Morango">M</a><script>x()</script></body></html>"#,
            ),
            "/Morango" => Reply::html(200, "<html><body><p>página nova</p></body></html>"),
            _ => Reply::status(404),
        })
        .await;
        let tmp = TempDir::new();
        let client = WikiClient::new(test_config(&server.base, tmp.path())).unwrap();
        let db = db_with_page();

        let report = run_sync_pass(&client, &db).await.unwrap();

        assert_eq!(
            report,
            SyncReport {
                updated: 2,
                ..Default::default()
            }
        );
        let abacaxi = stored_html(&db, "Abacaxi").unwrap();
        assert!(abacaxi.contains("<p>editado</p>"));
        assert!(abacaxi.contains(r#"href="wiki://localhost/Morango""#));
        assert!(!abacaxi.contains("<script"));
        assert!(stored_html(&db, "Morango").unwrap().contains("página nova"));
        assert_eq!(
            cursor(&db),
            SyncCursor {
                timestamp: "2026-11-02T00:00:01Z".into(),
                rcid: 102
            }
        );
    }

    #[tokio::test]
    async fn updates_the_row_the_app_serves() {
        // Old dumps stored some titles without their quotes.
        let server = wiki(&["\\\"Barco\\\""], |target| match target {
            "/%22Barco%22" => Reply::html(200, "<html><body><p>barco novo</p></body></html>"),
            _ => Reply::status(404),
        })
        .await;
        let tmp = TempDir::new();
        let client = WikiClient::new(test_config(&server.base, tmp.path())).unwrap();
        let db = db_with_page();
        db::upsert_page(
            &db.lock().unwrap(),
            "Barco",
            "<p>barco velho</p>",
            "dump",
            "",
        )
        .unwrap();

        run_sync_pass(&client, &db).await.unwrap();

        let conn = db.lock().unwrap();
        assert_eq!(db::page_count(&conn).unwrap(), 2, "no duplicate row");
        assert!(db::get_page(&conn, "Barco").unwrap().contains("barco novo"));
    }

    #[tokio::test]
    async fn does_nothing_before_the_initial_import() {
        let server = wiki(&["Abacaxi"], |_| {
            Reply::html(200, "<html><body>x</body></html>")
        })
        .await;
        let tmp = TempDir::new();
        let client = WikiClient::new(test_config(&server.base, tmp.path())).unwrap();
        let conn = Connection::open_in_memory().unwrap();
        db::create_schema(&conn).unwrap();
        let db = Mutex::new(conn);

        let report = run_sync_pass(&client, &db).await.unwrap();

        assert!(report.skipped_no_data);
        assert_eq!(db::page_count(&db.lock().unwrap()).unwrap(), 0);
        assert!(server.requests().is_empty());
    }

    #[tokio::test]
    async fn does_nothing_while_the_data_is_outdated() {
        let server = wiki(&["Abacaxi"], |_| {
            Reply::html(200, "<html><body>x</body></html>")
        })
        .await;
        let tmp = TempDir::new();
        let client = WikiClient::new(test_config(&server.base, tmp.path())).unwrap();
        let db = db_with_page();
        db::set_data_version(&db.lock().unwrap(), db::DATA_VERSION - 1).unwrap();

        let report = run_sync_pass(&client, &db).await.unwrap();

        assert!(report.skipped_no_data);
        assert_eq!(stored_html(&db, "Abacaxi").as_deref(), Some(ORIGINAL));
        assert!(server.requests().is_empty());
    }
}
