mod db;
mod protocol;
mod sync;

use std::sync::{Arc, Mutex};

use rusqlite::Connection;
use tauri::Manager;

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

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // Shared reference so the protocol handler can receive the real assets
    // path once setup resolves it.
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

            // Update the shared path so the protocol handler uses the real dir.
            *assets_dir.lock().unwrap() = real_assets;

            let conn = db::init_db(&app_data_dir).expect("failed to initialise database");
            app.manage(Mutex::new(conn));

            sync::start(app.handle().clone());

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![navigate_to])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
