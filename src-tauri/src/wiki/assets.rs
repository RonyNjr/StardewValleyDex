//! Downloads images and stylesheets referenced by wiki pages into the
//! assets directory served by the `wiki://` protocol.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use md5::{Digest, Md5};
use tokio::sync::OnceCell;

use super::client::WikiClient;
use super::html::{AssetKind, AssetRef};
use super::percent_decode;

/// Per-run asset cache. Each URL is downloaded at most once, even when
/// several pages that reference it are processed concurrently.
pub struct AssetStore {
    dir: PathBuf,
    cells: Mutex<HashMap<String, Arc<OnceCell<Option<String>>>>>,
    downloaded: AtomicUsize,
    temp_counter: AtomicUsize,
}

impl AssetStore {
    pub fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            cells: Mutex::new(HashMap::new()),
            downloaded: AtomicUsize::new(0),
            temp_counter: AtomicUsize::new(0),
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Number of files actually written to disk by this store.
    pub fn downloaded(&self) -> usize {
        self.downloaded.load(Ordering::Relaxed)
    }

    /// Make sure `asset` (as referenced from a page hosted on `page_base`) is
    /// on disk. Returns its path relative to the assets directory, or `None`
    /// if it can't be stored (the page then keeps the original URL).
    pub async fn fetch(
        &self,
        client: &WikiClient,
        asset: &AssetRef,
        page_base: &str,
    ) -> Option<String> {
        let url = normalize_asset_url(&asset.url, page_base)?;
        let rel = local_rel_path(&url, asset.kind)?;
        let cell = {
            let mut cells = self.cells.lock().unwrap_or_else(|e| e.into_inner());
            cells.entry(url.clone()).or_default().clone()
        };
        cell.get_or_init(|| self.download(client, &url, rel))
            .await
            .clone()
    }

    async fn download(&self, client: &WikiClient, url: &str, rel: String) -> Option<String> {
        let path = self.dir.join(&rel);
        if let Ok(meta) = tokio::fs::metadata(&path).await {
            if meta.len() > 0 {
                return Some(rel);
            }
        }

        if client.config().verbose_assets {
            println!("  -> downloading {rel}");
        }
        let resp = client.get(url).await.ok()?;
        if resp.status() != reqwest::StatusCode::OK {
            return None;
        }
        let bytes = resp.bytes().await.ok()?;

        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await.ok()?;
        }
        // Write to a temporary file first so the protocol never serves a
        // half-written asset.
        let n = self.temp_counter.fetch_add(1, Ordering::Relaxed);
        let mut tmp = path.clone().into_os_string();
        tmp.push(format!(".part{n}"));
        let tmp = PathBuf::from(tmp);
        if tokio::fs::write(&tmp, &bytes).await.is_err() {
            let _ = tokio::fs::remove_file(&tmp).await;
            return None;
        }
        if tokio::fs::rename(&tmp, &path).await.is_err() {
            let _ = tokio::fs::remove_file(&tmp).await;
            return None;
        }
        self.downloaded.fetch_add(1, Ordering::Relaxed);
        Some(rel)
    }
}

/// Absolute URL for an asset reference found on a page hosted at `base`.
/// Handles protocol-relative (`//host/x`) and host-relative (`/x`) URLs.
pub fn normalize_asset_url(raw: &str, base: &str) -> Option<String> {
    let url = if let Some(rest) = raw.strip_prefix("//") {
        format!("https://{rest}")
    } else if raw.starts_with('/') {
        format!("{}{raw}", base.trim_end_matches('/'))
    } else {
        raw.to_string()
    };
    (url.starts_with("http://") || url.starts_with("https://")).then_some(url)
}

/// Where an asset lives under the assets directory.
///
/// Images keep their URL path (`images/a/ab/Foo.png`) so `wiki://localhost/`
/// can serve them straight from disk. Stylesheets come from `load.php` with
/// query strings, so they are named after the MD5 of the URL (same names as
/// the old Python dump, which keeps existing asset folders valid).
pub fn local_rel_path(url: &str, kind: AssetKind) -> Option<String> {
    match kind {
        AssetKind::Stylesheet => {
            let digest = Md5::digest(url.as_bytes());
            let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
            Some(format!("css/{}.css", &hex[..12]))
        }
        AssetKind::Image => {
            let after_scheme = url.split_once("://").map_or(url, |(_, rest)| rest);
            let path = after_scheme.find('/').map_or("", |i| &after_scheme[i..]);
            let path = path.split(['?', '#']).next().unwrap_or("");
            let decoded = percent_decode(path);
            let mut parts = Vec::new();
            for part in decoded.split('/').filter(|p| !p.is_empty()) {
                // Never let a crafted URL escape the assets directory.
                if part == "." || part == ".." || part.contains(['\\', ':', '\0']) {
                    return None;
                }
                parts.push(part);
            }
            if parts.is_empty() {
                Some("index".to_string())
            } else {
                Some(parts.join("/"))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PT: &str = "https://pt.stardewvalleywiki.com";

    #[test]
    fn normalizes_relative_urls() {
        assert_eq!(
            normalize_asset_url("/mediawiki/images/a.png", PT).as_deref(),
            Some("https://pt.stardewvalleywiki.com/mediawiki/images/a.png")
        );
        assert_eq!(
            normalize_asset_url("//stardewvalleywiki.com/x.png", PT).as_deref(),
            Some("https://stardewvalleywiki.com/x.png")
        );
        assert_eq!(normalize_asset_url("data:image/png;base64,AA", PT), None);
    }

    #[test]
    fn image_paths_follow_the_url() {
        assert_eq!(
            local_rel_path(
                "https://pt.stardewvalleywiki.com/mediawiki/images/thumb/1/1a/Foo_Bar.png/24px-Foo_Bar.png?abc",
                AssetKind::Image
            )
            .as_deref(),
            Some("mediawiki/images/thumb/1/1a/Foo_Bar.png/24px-Foo_Bar.png")
        );
        assert_eq!(
            local_rel_path("https://host/images/Ma%C3%A7%C3%A3.png", AssetKind::Image).as_deref(),
            Some("images/Maçã.png")
        );
        assert_eq!(
            local_rel_path("https://host/", AssetKind::Image).as_deref(),
            Some("index")
        );
    }

    #[test]
    fn image_paths_cannot_escape_the_assets_dir() {
        assert_eq!(
            local_rel_path("https://host/images/../../etc/passwd", AssetKind::Image),
            None
        );
        assert_eq!(
            local_rel_path("https://host/images/%2e%2e/x.png", AssetKind::Image),
            None
        );
        assert_eq!(
            local_rel_path("https://host/C:/x.png", AssetKind::Image),
            None
        );
    }

    #[test]
    fn stylesheet_names_match_the_python_dump() {
        // md5("https://pt...load.php?...")[:12], same as hashlib in dump.py.
        assert_eq!(
            local_rel_path(
                "https://pt.stardewvalleywiki.com/mediawiki/load.php?lang=pt&modules=site.styles&only=styles&skin=vector-2022",
                AssetKind::Stylesheet
            )
            .as_deref(),
            Some("css/2e364f62ee03.css")
        );
    }
}
