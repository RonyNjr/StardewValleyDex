mod db;
mod protocol;
mod sync;

use std::sync::{Arc, Mutex};

use rusqlite::Connection;
use tauri::{Emitter, Manager};

#[tauri::command]
fn open_url(url: String, app: tauri::AppHandle) -> Result<(), String> {
    use tauri_plugin_opener::OpenerExt;
    app.opener()
        .open_url(url, None::<String>)
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn navigate_to(
    title: String,
    state: tauri::State<'_, Mutex<Connection>>,
) -> Result<String, String> {
    let conn = state
        .lock()
        .map_err(|_| "database mutex poisoned".to_string())?;

    match db::get_page(&conn, &title) {
        Some(html) => Ok(html),
        None => Err(format!("Page not found: {}", title)),
    }
}

/// Returns true if the local DB already has wiki pages (setup done).
#[tauri::command]
fn check_setup(state: tauri::State<'_, Mutex<Connection>>) -> Result<bool, String> {
    let conn = state.lock().map_err(|_| "mutex poisoned".to_string())?;
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM pages", [], |r| r.get(0))
        .unwrap_or(0);
    Ok(count > 0)
}

/// Downloads the data zip from `url`, extracts the SQLite DB and assets,
/// then imports all pages into the managed connection.
/// Emits `setup_progress` events: { step: string, pct: number }.
#[tauri::command]
async fn download_and_setup(
    url: String,
    app: tauri::AppHandle,
    state: tauri::State<'_, Mutex<Connection>>,
) -> Result<(), String> {
    use std::io::Cursor;

    // Phase 1 – download
    app.emit(
        "setup_progress",
        serde_json::json!({ "step": "downloading", "pct": 0 }),
    )
    .ok();

    let bytes = reqwest::get(&url)
        .await
        .map_err(|e| format!("Erro ao descarregar: {e}"))?
        .bytes()
        .await
        .map_err(|e| format!("Erro ao ler resposta: {e}"))?;

    // Phase 2 – extract SQLite from zip
    app.emit(
        "setup_progress",
        serde_json::json!({ "step": "extracting", "pct": 50 }),
    )
    .ok();

    let temp_sqlite = std::env::temp_dir().join("svdex_import.sqlite");

    {
        let cursor = Cursor::new(bytes.as_ref());
        let mut archive = zip::ZipArchive::new(cursor).map_err(|e| e.to_string())?;

        let mut found = false;
        for i in 0..archive.len() {
            let mut entry = archive.by_index(i).map_err(|e| e.to_string())?;
            if entry.name().ends_with(".sqlite") {
                let mut out =
                    std::fs::File::create(&temp_sqlite).map_err(|e| e.to_string())?;
                std::io::copy(&mut entry, &mut out).map_err(|e| e.to_string())?;
                found = true;
                break;
            }
        }
        if !found {
            return Err("Ficheiro .sqlite não encontrado no zip".to_string());
        }
    }

    // Phase 3 – extract assets (optional; no error if absent)
    {
        let app_data_dir = app
            .path()
            .app_data_dir()
            .map_err(|e| e.to_string())?;
        let assets_dir = app_data_dir.join("assets");
        std::fs::create_dir_all(&assets_dir).ok();

        let cursor = Cursor::new(bytes.as_ref());
        if let Ok(mut archive) = zip::ZipArchive::new(cursor) {
            for i in 0..archive.len() {
                if let Ok(mut entry) = archive.by_index(i) {
                    let name = entry.name().to_string();
                    if name.starts_with("assets/") && !entry.is_dir() {
                        let rel = name.strip_prefix("assets/").unwrap_or(&name);
                        let dest = assets_dir.join(rel);
                        if let Some(parent) = dest.parent() {
                            std::fs::create_dir_all(parent).ok();
                        }
                        if let Ok(mut out) = std::fs::File::create(&dest) {
                            std::io::copy(&mut entry, &mut out).ok();
                        }
                    }
                }
            }
        }
    }

    // Phase 4 – import into managed DB via ATTACH
    app.emit(
        "setup_progress",
        serde_json::json!({ "step": "importing", "pct": 75 }),
    )
    .ok();

    {
        let conn = state.lock().map_err(|_| "mutex poisoned".to_string())?;
        let src = temp_sqlite
            .to_str()
            .ok_or("caminho temporário inválido")?
            .replace('\'', "''"); // basic SQL escape
        conn.execute_batch(&format!(
            "ATTACH DATABASE '{src}' AS src;
             INSERT OR REPLACE INTO pages SELECT * FROM src.pages;
             INSERT OR REPLACE INTO redirects SELECT * FROM src.redirects;
             DETACH DATABASE src;"
        ))
        .map_err(|e| format!("Falha ao importar: {e}"))?;
    }

    std::fs::remove_file(&temp_sqlite).ok();

    app.emit(
        "setup_progress",
        serde_json::json!({ "step": "done", "pct": 100 }),
    )
    .ok();

    Ok(())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let assets_dir: Arc<Mutex<std::path::PathBuf>> =
        Arc::new(Mutex::new(std::path::PathBuf::new()));

    let builder = tauri::Builder::default().plugin(tauri_plugin_opener::init());
    let builder = protocol::register(builder, Arc::clone(&assets_dir));

    builder
        .setup(move |app| {
            let app_data_dir = app
                .path()
                .app_data_dir()
                .expect("failed to resolve app data dir");

            std::fs::create_dir_all(&app_data_dir).ok();

            let real_assets = app_data_dir.join("assets");
            std::fs::create_dir_all(&real_assets).ok();

            *assets_dir.lock().unwrap() = real_assets;

            let conn = db::init_db(&app_data_dir).expect("failed to initialise database");
            app.manage(Mutex::new(conn));

            sync::start(app.handle().clone());

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            navigate_to,
            open_url,
            check_setup,
            download_and_setup
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
