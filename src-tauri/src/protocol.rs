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

    // Empty path -> Portuguese main page. Underscores are spaces in MediaWiki.
    let target = if decoded.is_empty() {
        "Página Principal".to_string()
    } else {
        decoded.replace('_', " ")
    };

    // Assets: files with a recognised extension are served from disk.
    if let Some(ext) = extension_of(&target) {
        if is_asset_extension(&ext) {
            if target == "stardewbackground.png" {
                static BG: &[u8] = include_bytes!("../resources/stardewbackground.png");
                return Response::builder()
                    .status(StatusCode::OK)
                    .header("Content-Type", "image/png")
                    .header("Access-Control-Allow-Origin", "*")
                    .body(BG.to_vec())
                    .unwrap_or_else(|_| internal_error("Failed to build response"));
            }
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
        Some(html) => {
            let page = inject_header(&html, &target);
            Response::builder()
                .status(StatusCode::OK)
                .header("Content-Type", "text/html; charset=utf-8")
                .header("Access-Control-Allow-Origin", "*")
                .body(page.into_bytes())
                .unwrap_or_else(|_| internal_error("Failed to build response"))
        }
        None => not_found(&target),
    }
}

const HEADER_HTML: &str = r#"<style>
html{background-image:url('wiki://localhost/stardewbackground.png')!important;background-size:cover!important;background-attachment:fixed!important;background-position:center!important;background-color:#a7c4d4!important}body{background:transparent!important}
#svd-q::placeholder{color:rgba(255,255,255,0.5)}
#svd-q:focus{outline:none;background:rgba(255,255,255,0.2)!important;border-color:rgba(255,255,255,0.45)!important}
#svd-back:hover{background:rgba(255,255,255,0.2)!important}
#svd-go:hover{opacity:.9}
</style>
<script>
document.addEventListener('click',function(e){
  var a=e.target.closest('a');
  if(!a)return;
  var h=a.getAttribute('href')||'';
  if(h.startsWith('http://')||h.startsWith('https://')){
    e.preventDefault();
    if(window.__TAURI_INTERNALS__)window.__TAURI_INTERNALS__.invoke('open_url',{url:h}).catch(function(){});
  }
},true);
</script>
<div id="svd-header" style="display:flex;align-items:center;gap:14px;padding:10px 18px;margin:12px auto 0;max-width:960px;width:calc(100% - 32px);background:rgba(30,60,15,0.5);backdrop-filter:blur(28px) saturate(180%);-webkit-backdrop-filter:blur(28px) saturate(180%);color:#fff;position:sticky;top:12px;z-index:9999;border-radius:22px;border:1px solid rgba(255,255,255,0.18);box-shadow:0 4px 32px rgba(0,0,0,0.2),inset 0 1px 0 rgba(255,255,255,0.18);font-family:-apple-system,BlinkMacSystemFont,'Segoe UI',Helvetica,Arial,sans-serif">
  <button id="svd-back" onclick="history.back()" style="display:flex;align-items:center;gap:6px;background:rgba(255,255,255,0.12);border:1px solid rgba(255,255,255,0.18);color:#fff;border-radius:50px;padding:6px 14px;cursor:pointer;font-size:.82rem;font-family:inherit;white-space:nowrap">← Voltar</button>
  <span style="font-weight:700;font-size:.95rem;cursor:pointer;white-space:nowrap;letter-spacing:-.3px" onclick="window.location='wiki://localhost/'">🌾 StardewValleyDex</span>
  <form style="display:flex;flex:1;gap:8px;max-width:580px" onsubmit="event.preventDefault();var t=document.getElementById('svd-q').value.trim().replace(/\s+/g,'_');if(t)window.location='wiki://localhost/'+encodeURIComponent(t)">
    <input id="svd-q" type="text" placeholder="Buscar página..." style="flex:1;padding:8px 16px;border:1px solid rgba(255,255,255,0.18);border-radius:50px;font-size:.88rem;background:rgba(255,255,255,0.13);color:#fff;font-family:inherit"/>
    <button id="svd-go" type="submit" style="padding:8px 20px;background:rgba(240,178,38,.9);color:#1a1a1a;border:none;border-radius:50px;cursor:pointer;font-size:.88rem;font-weight:600;font-family:inherit;white-space:nowrap">Buscar</button>
  </form>
</div>"#;

fn inject_header(html: &str, _title: &str) -> String {
    let header = HEADER_HTML;
    // Insert right after the opening <body> tag (case-insensitive search)
    let lower = html.to_lowercase();
    if let Some(pos) = lower.find("<body") {
        if let Some(close) = lower[pos..].find('>') {
            let insert_at = pos + close + 1;
            let mut out = String::with_capacity(html.len() + header.len());
            out.push_str(&html[..insert_at]);
            out.push_str(header);
            out.push_str(&html[insert_at..]);
            return out;
        }
    }
    // Fallback: prepend
    format!("{}{}", header, html)
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
