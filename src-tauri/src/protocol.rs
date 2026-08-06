use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use rusqlite::Connection;
use tauri::http::{Request, Response, StatusCode};
use tauri::{Builder, Manager, Runtime};

use crate::db;

pub fn register<R: Runtime>(
    builder: Builder<R>,
    assets_dir: Arc<Mutex<PathBuf>>,
) -> Builder<R> {
    builder.register_uri_scheme_protocol("wiki", move |ctx, request| {
        let dir = assets_dir.lock().unwrap().clone();
        handle_request(ctx.app_handle(), request, &dir)
    })
}

fn handle_request<R: Runtime>(
    app: &tauri::AppHandle<R>,
    request: Request<Vec<u8>>,
    assets_dir: &PathBuf,
) -> Response<Vec<u8>> {
    let uri = request.uri();
    let raw_path = uri.path();

    // Strip a leading slash so we get "Mining" instead of "/Mining".
    let trimmed = raw_path.trim_start_matches('/');
    let decoded = url_decode(trimmed);

    // Empty path -> route to Main_Page for convenience.
    let target = if decoded.is_empty() {
        "Main_Page".to_string()
    } else {
        decoded
    };

    // Assets: files with a recognised extension are served from disk.
    if let Some(ext) = extension_of(&target) {
        if is_asset_extension(&ext) {
            return serve_asset(assets_dir, &target, &ext);
        }
    }

    // Otherwise: look up the page HTML in SQLite.
    let state = app.state::<Mutex<Connection>>();
    let conn = match state.lock() {
        Ok(c) => c,
        Err(_) => return internal_error("Database mutex poisoned"),
    };

    match db::get_page(&conn, &target) {
        Some(html) => Response::builder()
            .status(StatusCode::OK)
            .header("Content-Type", "text/html; charset=utf-8")
            .header("Access-Control-Allow-Origin", "*")
            .body(html.into_bytes())
            .unwrap_or_else(|_| internal_error("Failed to build response")),
        None => not_found(&target),
    }
}

fn serve_asset(assets_dir: &PathBuf, rel_path: &str, ext: &str) -> Response<Vec<u8>> {
    // Prevent path traversal.
    if rel_path.contains("..") {
        return not_found(rel_path);
    }

    let full = assets_dir.join(rel_path);
    match std::fs::read(&full) {
        Ok(bytes) => Response::builder()
            .status(StatusCode::OK)
            .header("Content-Type", content_type_for(ext))
            .header("Access-Control-Allow-Origin", "*")
            .body(bytes)
            .unwrap_or_else(|_| internal_error("Failed to build asset response")),
        Err(_) => not_found(rel_path),
    }
}

fn not_found(target: &str) -> Response<Vec<u8>> {
    let body = format!("Page not found: {}", target).into_bytes();
    Response::builder()
        .status(StatusCode::NOT_FOUND)
        .header("Content-Type", "text/plain; charset=utf-8")
        .header("Access-Control-Allow-Origin", "*")
        .body(body)
        .unwrap()
}

fn internal_error(msg: &str) -> Response<Vec<u8>> {
    Response::builder()
        .status(StatusCode::INTERNAL_SERVER_ERROR)
        .header("Content-Type", "text/plain; charset=utf-8")
        .header("Access-Control-Allow-Origin", "*")
        .body(msg.as_bytes().to_vec())
        .unwrap()
}

fn extension_of(path: &str) -> Option<String> {
    let last = path.rsplit('/').next().unwrap_or("");
    last.rsplit_once('.').map(|(_, ext)| ext.to_lowercase())
}

fn is_asset_extension(ext: &str) -> bool {
    matches!(
        ext,
        "png" | "jpg" | "jpeg" | "gif" | "svg" | "css" | "ttf" | "woff" | "woff2"
    )
}

fn content_type_for(ext: &str) -> &'static str {
    match ext {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "svg" => "image/svg+xml",
        "css" => "text/css; charset=utf-8",
        "ttf" => "font/ttf",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        _ => "application/octet-stream",
    }
}

/// Very small percent-decoding helper so we don't need an extra dep.
fn url_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hi = hex_val(bytes[i + 1]);
                let lo = hex_val(bytes[i + 2]);
                match (hi, lo) {
                    (Some(h), Some(l)) => {
                        out.push((h << 4) | l);
                        i += 3;
                    }
                    _ => {
                        out.push(b'%');
                        i += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}
