use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

use rusqlite::Connection;
use serde::Deserialize;
use tauri::Manager;

use crate::db;

/// Interval between two sync passes. The MediaWiki API is polled every
/// 30 minutes for the list of recently changed pages.
const SYNC_INTERVAL: Duration = Duration::from_secs(30 * 60);

/// Background worker that keeps the local SQLite cache in sync with the
/// upstream MediaWiki instance.
pub struct SyncWorker {
    pub wiki_base_url: String,
    #[allow(dead_code)]
    pub db_path: PathBuf,
}

impl SyncWorker {
    pub fn new(wiki_base_url: impl Into<String>, db_path: PathBuf) -> Self {
        Self {
            wiki_base_url: wiki_base_url.into(),
            db_path,
        }
    }
}

/// Spawn the background sync task. Returns immediately; the task keeps
/// running for the lifetime of the application.
pub fn start(app_handle: tauri::AppHandle) {
    let worker = SyncWorker::new(
        "https://pt.stardewvalleywiki.com".to_string(),
        PathBuf::new(),
    );

    tauri::async_runtime::spawn(async move {
        // Small startup delay so the UI can finish loading first.
        tokio::time::sleep(Duration::from_secs(10)).await;

        loop {
            if let Err(e) = run_sync_pass(&app_handle, &worker).await {
                eprintln!("[sync] pass failed: {e}");
            }
            tokio::time::sleep(SYNC_INTERVAL).await;
        }
    });
}

async fn run_sync_pass(
    app_handle: &tauri::AppHandle,
    worker: &SyncWorker,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let client = reqwest::Client::builder()
        .user_agent("StardewValleyDex/0.1 (offline-wiki-client)")
        .build()?;

    let recent_url = format!(
        "{}/api.php?action=query&list=recentchanges&rcprop=title%7Ctimestamp&format=json",
        worker.wiki_base_url
    );

    let resp: RecentChangesResponse = client.get(&recent_url).send().await?.json().await?;

    let changes = resp
        .query
        .map(|q| q.recentchanges)
        .unwrap_or_default();

    println!("[sync] fetched {} recent change(s)", changes.len());

    for change in changes {
        // TODO: fetch parsed HTML for the page and upsert it. For now we
        // record a placeholder row so the pipeline is exercised end-to-end.
        if let Err(e) = upsert_placeholder(app_handle, &change.title, &change.timestamp) {
            eprintln!("[sync] failed to upsert {}: {}", change.title, e);
        }

        // TODO(fase-2): call `api.php?action=parse&page={title}&format=json`,
        // extract `parse.text.*` (HTML) and `parse.categories`, then upsert.
    }

    Ok(())
}

fn upsert_placeholder(
    app_handle: &tauri::AppHandle,
    title: &str,
    timestamp: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let state = app_handle.state::<Mutex<Connection>>();
    let conn = state
        .lock()
        .map_err(|_| "database mutex poisoned".to_string())?;
    db::upsert_page(
        &conn,
        title,
        &format!(
            "<h1>{}</h1><p><em>Page pending sync — last change at {}.</em></p>",
            html_escape(title),
            html_escape(timestamp)
        ),
        timestamp,
        "",
    )?;
    Ok(())
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[derive(Debug, Deserialize)]
struct RecentChangesResponse {
    query: Option<QueryBlock>,
}

#[derive(Debug, Deserialize)]
struct QueryBlock {
    recentchanges: Vec<RecentChange>,
}

#[derive(Debug, Deserialize)]
struct RecentChange {
    title: String,
    timestamp: String,
}
