//! HTTP access to the wiki: page HTML (Portuguese with English fallback),
//! the MediaWiki API (page list, recent changes, redirects, langlinks), and
//! the fetch -> download assets -> rewrite pipeline used by dump and sync.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use futures_util::{stream, StreamExt};
use reqwest::{StatusCode, Url};
use serde::de::DeserializeOwned;
use serde::Deserialize;

use super::assets::AssetStore;
use super::html::{self, AssetRef, ProcessedPage};
use super::{title_to_slug, Error, API_PATH, EN_BASE, PT_BASE, USER_AGENT};
use crate::db::SyncCursor;

pub struct WikiConfig {
    pub pt_base: String,
    pub en_base: String,
    pub assets_dir: PathBuf,
    pub accept_invalid_certs: bool,
    pub verbose_assets: bool,
    /// Pause after each page fetch (per worker), to go easy on the wiki.
    pub page_delay: Duration,
    /// Pause between paginated API requests.
    pub api_delay: Duration,
    /// Retries for connection errors, timeouts and HTTP 429/500/502/503/504.
    pub retries: u32,
    /// Backoff before the first retry; doubles on each attempt.
    pub retry_delay: Duration,
    /// Concurrent asset downloads per page.
    pub assets_per_page: usize,
}

impl WikiConfig {
    pub fn new(assets_dir: PathBuf) -> Self {
        Self {
            pt_base: PT_BASE.to_string(),
            en_base: EN_BASE.to_string(),
            assets_dir,
            accept_invalid_certs: false,
            verbose_assets: false,
            page_delay: Duration::from_millis(50),
            api_delay: Duration::from_millis(100),
            retries: 3,
            retry_delay: Duration::from_millis(500),
            assets_per_page: 4,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PageSource {
    Pt,
    /// Fetched from the English wiki under this title.
    En(String),
}

#[derive(Debug)]
pub enum PageOutcome {
    Page {
        page: ProcessedPage,
        source: PageSource,
    },
    /// 404 on both wikis, but the API says the title redirects here.
    Redirect(String),
    /// 404 on both wikis and not a redirect.
    Missing,
}

/// One entry of `list=recentchanges`.
#[derive(Debug, Clone, Deserialize)]
pub struct Change {
    pub rcid: i64,
    pub title: String,
    pub timestamp: String,
}

/// Titles to refresh plus the position to resume from next time.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ChangeBatch {
    pub titles: Vec<String>,
    /// Last change included in this batch; `None` if there was nothing new.
    pub cursor: Option<SyncCursor>,
}

impl ChangeBatch {
    /// Add changes (oldest first) that come after `after`. Stops before the
    /// first change that would push the batch past `max_titles` distinct
    /// titles, so nothing is skipped: the next batch starts there. Returns
    /// `false` once the batch is full.
    pub fn absorb(
        &mut self,
        changes: impl IntoIterator<Item = Change>,
        after: &SyncCursor,
        max_titles: usize,
    ) -> bool {
        for change in changes {
            if change.rcid <= after.rcid {
                continue;
            }
            if !self.titles.contains(&change.title) {
                if self.titles.len() >= max_titles {
                    return false;
                }
                self.titles.push(change.title);
            }
            self.cursor = Some(SyncCursor {
                timestamp: change.timestamp,
                rcid: change.rcid,
            });
        }
        true
    }
}

/// Envelope of a MediaWiki `action=query` response.
#[derive(Deserialize)]
struct Query<Q> {
    query: Option<Q>,
    #[serde(rename = "continue")]
    cont: Option<HashMap<String, String>>,
}

enum RawPage {
    Html(String),
    NotFound,
}

pub struct WikiClient {
    http: reqwest::Client,
    cfg: WikiConfig,
    assets: AssetStore,
}

impl WikiClient {
    pub fn new(cfg: WikiConfig) -> Result<Self, Error> {
        let http = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .timeout(Duration::from_secs(30))
            .connect_timeout(Duration::from_secs(10))
            .danger_accept_invalid_certs(cfg.accept_invalid_certs)
            .build()?;
        let assets = AssetStore::new(cfg.assets_dir.clone());
        Ok(Self { http, cfg, assets })
    }

    pub fn config(&self) -> &WikiConfig {
        &self.cfg
    }

    pub fn assets(&self) -> &AssetStore {
        &self.assets
    }

    /// GET with retries on transient failures.
    pub(crate) async fn get(&self, url: &str) -> reqwest::Result<reqwest::Response> {
        let mut attempt = 0;
        loop {
            let result = self.http.get(url).send().await;
            let transient = match &result {
                Ok(resp) => matches!(resp.status().as_u16(), 429 | 500 | 502 | 503 | 504),
                Err(e) => e.is_timeout() || e.is_connect(),
            };
            if !transient || attempt >= self.cfg.retries {
                return result;
            }
            tokio::time::sleep(self.cfg.retry_delay * 2u32.pow(attempt)).await;
            attempt += 1;
        }
    }

    async fn api<T: DeserializeOwned>(&self, params: &[(&str, &str)]) -> Result<T, Error> {
        let mut url = Url::parse(&format!("{}{API_PATH}", self.cfg.pt_base))?;
        url.query_pairs_mut()
            .extend_pairs(params.iter().copied())
            .append_pair("format", "json")
            .append_pair("formatversion", "2");

        let resp = self.get(url.as_str()).await?;
        let status = resp.status();
        if !status.is_success() {
            return Err(format!("API answered HTTP {} for {url}", status.as_u16()).into());
        }
        let value: serde_json::Value = serde_json::from_str(&resp.text().await?)
            .map_err(|e| format!("API answered with something that isn't JSON ({e}) for {url}"))?;
        if let Some(err) = value.get("error") {
            return Err(format!("API error for {url}: {err}").into());
        }
        Ok(serde_json::from_value(value)?)
    }

    /// Walk a paginated `action=query` list. `each` returns `false` to stop.
    async fn api_pages<Q: DeserializeOwned>(
        &self,
        params: &[(&str, &str)],
        mut each: impl FnMut(Q) -> bool,
    ) -> Result<(), Error> {
        let mut cont: HashMap<String, String> = HashMap::new();
        loop {
            let page: Query<Q> = {
                let mut all = params.to_vec();
                all.extend(cont.iter().map(|(k, v)| (k.as_str(), v.as_str())));
                self.api(&all).await?
            };
            let keep_going = page.query.is_none_or(&mut each);
            match page.cont {
                Some(next) if keep_going => {
                    cont = next;
                    tokio::time::sleep(self.cfg.api_delay).await;
                }
                _ => return Ok(()),
            }
        }
    }

    /// Every article title (main namespace, redirects included).
    pub async fn list_titles(&self) -> Result<Vec<String>, Error> {
        #[derive(Deserialize)]
        struct AllPages {
            #[serde(default)]
            allpages: Vec<Item>,
        }
        #[derive(Deserialize)]
        struct Item {
            title: String,
        }

        let mut titles = Vec::new();
        self.api_pages::<AllPages>(
            &[
                ("action", "query"),
                ("list", "allpages"),
                ("apnamespace", "0"),
                ("aplimit", "max"),
            ],
            |q| {
                titles.extend(q.allpages.into_iter().map(|p| p.title));
                true
            },
        )
        .await?;
        Ok(titles)
    }

    /// Article edits and creations after `after`, oldest first, capped at
    /// `max_titles` distinct pages.
    pub async fn recent_changes(
        &self,
        after: &SyncCursor,
        max_titles: usize,
    ) -> Result<ChangeBatch, Error> {
        #[derive(Deserialize)]
        struct RecentChanges {
            #[serde(default)]
            recentchanges: Vec<Change>,
        }

        let mut batch = ChangeBatch::default();
        self.api_pages::<RecentChanges>(
            &[
                ("action", "query"),
                ("list", "recentchanges"),
                ("rcnamespace", "0"),
                ("rctype", "edit|new"),
                ("rcprop", "title|timestamp|ids"),
                ("rcdir", "newer"),
                ("rcstart", after.timestamp.as_str()),
                ("rclimit", "max"),
            ],
            |q| batch.absorb(q.recentchanges, after, max_titles),
        )
        .await?;
        Ok(batch)
    }

    /// Fetch one page and turn it into what the app stores: Portuguese page
    /// first, English page if it doesn't exist in Portuguese, assets
    /// downloaded and HTML rewritten. `Err` means a transient failure
    /// (network, HTTP 5xx...), so callers keep whatever they already have.
    pub async fn fetch_page(&self, title: &str) -> Result<PageOutcome, Error> {
        let outcome = self.fetch_page_inner(title).await;
        tokio::time::sleep(self.cfg.page_delay).await;
        outcome
    }

    async fn fetch_page_inner(&self, title: &str) -> Result<PageOutcome, Error> {
        let pt_base = self.cfg.pt_base.as_str();
        if let RawPage::Html(raw) = self.fetch_raw(pt_base, title).await? {
            let page = self.process(&raw, pt_base).await;
            return Ok(PageOutcome::Page {
                page,
                source: PageSource::Pt,
            });
        }

        let en_base = self.cfg.en_base.as_str();
        let en_title = self
            .en_title(title)
            .await
            .unwrap_or_else(|| title.to_string());
        if let RawPage::Html(raw) = self.fetch_raw(en_base, &en_title).await? {
            let page = self.process(&raw, en_base).await;
            return Ok(PageOutcome::Page {
                page,
                source: PageSource::En(en_title),
            });
        }

        Ok(match self.redirect_target(title).await {
            Some(target) => PageOutcome::Redirect(target),
            None => PageOutcome::Missing,
        })
    }

    async fn fetch_raw(&self, base: &str, title: &str) -> Result<RawPage, Error> {
        let url = format!("{}/{}", base.trim_end_matches('/'), title_to_slug(title));
        let resp = self.get(&url).await?;
        match resp.status() {
            StatusCode::OK => Ok(RawPage::Html(resp.text().await?)),
            StatusCode::NOT_FOUND => Ok(RawPage::NotFound),
            status => Err(format!("HTTP {} for {url}", status.as_u16()).into()),
        }
    }

    /// Assets kept offline: relative and protocol-relative URLs (as the old
    /// dump did) and anything hosted on either wiki. Most content images on
    /// the Portuguese wiki are absolute URLs on the English one.
    fn is_wiki_asset(&self, url: &str) -> bool {
        url.starts_with('/')
            || [&self.cfg.pt_base, &self.cfg.en_base].iter().any(|base| {
                url.strip_prefix(base.trim_end_matches('/'))
                    .is_some_and(|rest| rest.starts_with('/'))
            })
    }

    async fn process(&self, raw: &str, page_base: &str) -> ProcessedPage {
        let assets = html::collect_assets(raw)
            .into_iter()
            .filter(|asset| self.is_wiki_asset(&asset.url));
        let resolved: HashMap<AssetRef, String> = stream::iter(assets)
            .map(|asset| async move {
                let rel = self.assets.fetch(self, &asset, page_base).await;
                rel.map(|rel| (asset, rel))
            })
            .buffer_unordered(self.cfg.assets_per_page.max(1))
            .filter_map(|entry| async move { entry })
            .collect()
            .await;
        html::process_html(raw, |asset| resolved.get(asset).cloned())
    }

    /// English title linked from the Portuguese page, if any.
    async fn en_title(&self, title: &str) -> Option<String> {
        #[derive(Deserialize)]
        struct Pages {
            #[serde(default)]
            pages: Vec<Page>,
        }
        #[derive(Deserialize)]
        struct Page {
            #[serde(default)]
            langlinks: Vec<LangLink>,
        }
        #[derive(Deserialize)]
        struct LangLink {
            lang: String,
            title: String,
        }

        let resp: Query<Pages> = self
            .api(&[
                ("action", "query"),
                ("titles", title),
                ("prop", "langlinks"),
                ("lllang", "en"),
            ])
            .await
            .ok()?;
        resp.query?
            .pages
            .into_iter()
            .flat_map(|p| p.langlinks)
            .find(|l| l.lang == "en")
            .map(|l| l.title)
    }

    async fn redirect_target(&self, title: &str) -> Option<String> {
        #[derive(Deserialize)]
        struct Redirects {
            #[serde(default)]
            redirects: Vec<Redirect>,
        }
        #[derive(Deserialize)]
        struct Redirect {
            to: String,
        }

        let resp: Query<Redirects> = self
            .api(&[("action", "query"), ("titles", title), ("redirects", "1")])
            .await
            .ok()?;
        resp.query?.redirects.pop().map(|r| r.to)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wiki::test_server::{test_config, Reply, TempDir, TestServer};

    fn change(rcid: i64, title: &str) -> Change {
        Change {
            rcid,
            title: title.to_string(),
            timestamp: format!("2026-11-01T00:00:{rcid:02}Z"),
        }
    }

    fn cursor(rcid: i64) -> SyncCursor {
        SyncCursor {
            timestamp: format!("2026-11-01T00:00:{rcid:02}Z"),
            rcid,
        }
    }

    #[test]
    fn batch_skips_changes_already_synced() {
        let mut batch = ChangeBatch::default();
        assert!(batch.absorb(vec![change(5, "A"), change(6, "B")], &cursor(5), 10));
        assert_eq!(batch.titles, vec!["B"]);
        assert_eq!(batch.cursor, Some(cursor(6)));
    }

    #[test]
    fn batch_stops_at_the_cap_without_losing_changes() {
        let mut batch = ChangeBatch::default();
        let full = !batch.absorb(
            vec![
                change(1, "A"),
                change(2, "B"),
                change(3, "A"),
                change(4, "C"),
                change(5, "B"),
            ],
            &cursor(0),
            2,
        );
        assert!(full);
        assert_eq!(batch.titles, vec!["A", "B"]);
        // C (rcid 4) didn't fit, so the next pass must start right before it.
        assert_eq!(batch.cursor, Some(cursor(3)));
    }

    #[test]
    fn batch_without_new_changes_has_no_cursor() {
        let mut batch = ChangeBatch::default();
        batch.absorb(vec![change(3, "A")], &cursor(3), 10);
        assert_eq!(batch, ChangeBatch::default());
    }

    #[tokio::test]
    async fn lists_titles_across_api_pages() {
        let server = TestServer::start(|target| {
            if !target.starts_with("/mediawiki/api.php") || !target.contains("list=allpages") {
                return Reply::status(404);
            }
            if target.contains("apcontinue=B") {
                Reply::json(r#"{"query":{"allpages":[{"title":"B"},{"title":"\"C\""}]}}"#)
            } else {
                Reply::json(r#"{"continue":{"apcontinue":"B","continue":"-||"},"query":{"allpages":[{"title":"A"}]}}"#)
            }
        })
        .await;
        let tmp = TempDir::new();
        let client = WikiClient::new(test_config(&server.base, tmp.path())).unwrap();

        assert_eq!(client.list_titles().await.unwrap(), vec!["A", "B", "\"C\""]);
    }

    #[tokio::test]
    async fn api_html_error_page_is_an_error() {
        // What the wiki does for the wrong API path: HTML with HTTP 404.
        let server = TestServer::start(|_| Reply::html(404, "<html>not found</html>")).await;
        let tmp = TempDir::new();
        let client = WikiClient::new(test_config(&server.base, tmp.path())).unwrap();

        assert!(client.list_titles().await.is_err());
    }

    #[tokio::test]
    async fn fetches_portuguese_page_and_its_assets() {
        let server = TestServer::start(|target| match target {
            "/Abacaxi" => Reply::html(
                200,
                r#"<html><body><a href="/Morango">M</a><img src="/images/a/ab/Abacaxi.png"><img src="/images/nope.png"></body></html>"#,
            ),
            "/images/a/ab/Abacaxi.png" => Reply::bytes("image/png", b"PNGDATA"),
            _ => Reply::status(404),
        })
        .await;
        let tmp = TempDir::new();
        let client = WikiClient::new(test_config(&server.base, tmp.path())).unwrap();

        let PageOutcome::Page { page, source } = client.fetch_page("Abacaxi").await.unwrap() else {
            panic!("expected a page");
        };
        assert_eq!(source, PageSource::Pt);
        assert!(page.html.contains(r#"href="wiki://localhost/Morango""#));
        assert!(page
            .html
            .contains(r#"src="wiki://localhost/images/a/ab/Abacaxi.png""#));
        assert!(page.html.contains(r#"src="/images/nope.png""#));
        assert_eq!(
            std::fs::read(tmp.path().join("images/a/ab/Abacaxi.png")).unwrap(),
            b"PNGDATA"
        );
        assert_eq!(client.assets().downloaded(), 1);
    }

    #[tokio::test]
    async fn stores_images_hosted_on_the_english_wiki() {
        // The page needs the server's address, known only after it starts.
        let base = std::sync::Arc::new(std::sync::OnceLock::<String>::new());
        let page_base = std::sync::Arc::clone(&base);
        let server = TestServer::start(move |target| match target {
            "/Pesca" => {
                let base = page_base.get().unwrap();
                Reply::html(
                    200,
                    &format!(
                        r#"<html><body><img src="{base}/en/mediawiki/images/en/0/07/%27Boat%27_Location.png"><img src="https://example.invalid/ad.png"></body></html>"#
                    ),
                )
            }
            "/en/mediawiki/images/en/0/07/%27Boat%27_Location.png" => {
                Reply::bytes("image/png", b"BOAT")
            }
            _ => Reply::status(404),
        })
        .await;
        base.set(server.base.clone()).unwrap();
        let tmp = TempDir::new();
        let client = WikiClient::new(test_config(&server.base, tmp.path())).unwrap();

        let PageOutcome::Page { page, .. } = client.fetch_page("Pesca").await.unwrap() else {
            panic!("expected a page");
        };
        assert!(page.html.contains(
            r#"src="wiki://localhost/en/mediawiki/images/en/0/07/%27Boat%27_Location.png""#
        ));
        assert!(
            page.html
                .contains(r#"src="https://example.invalid/ad.png""#),
            "images on other hosts are left alone"
        );
        assert_eq!(
            std::fs::read(
                tmp.path()
                    .join("en/mediawiki/images/en/0/07/'Boat'_Location.png")
            )
            .unwrap(),
            b"BOAT"
        );
        assert_eq!(client.assets().downloaded(), 1);
    }

    #[tokio::test]
    async fn falls_back_to_the_english_page() {
        let server = TestServer::start(|target| {
            if target.contains("prop=langlinks") {
                return Reply::json(
                    r#"{"query":{"pages":[{"title":"Mineração","langlinks":[{"lang":"en","title":"Mining"}]}]}}"#,
                );
            }
            match target {
                "/en/Mining" => Reply::html(200, "<html><body><p>Mining</p></body></html>"),
                _ => Reply::status(404),
            }
        })
        .await;
        let tmp = TempDir::new();
        let client = WikiClient::new(test_config(&server.base, tmp.path())).unwrap();

        let PageOutcome::Page { page, source } = client.fetch_page("Mineração").await.unwrap()
        else {
            panic!("expected a page");
        };
        assert_eq!(source, PageSource::En("Mining".into()));
        assert!(page.html.contains("<p>Mining</p>"));
    }

    #[tokio::test]
    async fn reports_redirects_and_missing_pages() {
        let server = TestServer::start(|target| {
            if target.contains("redirects=1") && target.contains("titles=Velho") {
                return Reply::json(r#"{"query":{"redirects":[{"from":"Velho","to":"Novo"}]}}"#);
            }
            if target.starts_with("/mediawiki/api.php") {
                return Reply::json(r#"{"query":{"pages":[{"title":"x","missing":true}]}}"#);
            }
            Reply::status(404)
        })
        .await;
        let tmp = TempDir::new();
        let client = WikiClient::new(test_config(&server.base, tmp.path())).unwrap();

        assert!(matches!(
            client.fetch_page("Velho").await.unwrap(),
            PageOutcome::Redirect(t) if t == "Novo"
        ));
        assert!(matches!(
            client.fetch_page("Nada").await.unwrap(),
            PageOutcome::Missing
        ));
    }

    #[tokio::test]
    async fn server_errors_are_retried_then_reported() {
        let server = TestServer::start(|_| Reply::status(503)).await;
        let tmp = TempDir::new();
        let client = WikiClient::new(test_config(&server.base, tmp.path())).unwrap();

        assert!(client.fetch_page("Abacaxi").await.is_err());
        // 1 attempt + `retries` retries.
        assert_eq!(
            server.requests().len(),
            1 + client.config().retries as usize
        );
    }
}
