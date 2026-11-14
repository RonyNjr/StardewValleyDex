//! Pure HTML processing for wiki pages: strip the MediaWiki chrome, point
//! internal links at `wiki://localhost/`, and swap asset URLs for local
//! copies. No I/O happens here; asset downloads are done by the caller
//! between `collect_assets` and `process_html`.

use std::collections::HashSet;

use dom_query::Document;
use percent_encoding::{utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};

use super::{title_to_slug, utc_now, EN_BASE};

/// Escapes asset paths in `wiki://localhost/` URLs, so names with `#`, `?`,
/// `+`, `'` or non-ASCII characters reach the protocol handler intact.
const ASSET_PATH_ESCAPE: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'/')
    .remove(b'-')
    .remove(b'_')
    .remove(b'.')
    .remove(b'~');

/// MediaWiki chrome and scripts removed from every page.
const STRIP_SELECTOR: &str = "#mw-head, #mw-head-base, #mw-page-base, script";

const ASSET_EXTS: &[&str] = &[
    ".png", ".jpg", ".jpeg", ".gif", ".svg", ".css", ".ttf", ".woff", ".woff2",
];
const SKIP_HREF: &[&str] = &[
    "wiki://",
    "http://",
    "https://",
    "//",
    "#",
    "mailto:",
    "javascript:",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AssetKind {
    Image,
    Stylesheet,
}

/// An asset as referenced in the page (`url` is the raw attribute value).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct AssetRef {
    pub url: String,
    pub kind: AssetKind,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcessedPage {
    pub html: String,
    /// Category names joined with `|`.
    pub categories: String,
    pub last_modified: String,
}

/// Rewrite an internal wiki link to `wiki://localhost/...`. Handles both
/// `/wiki/Foo` and short URLs (`/Foo`). Returns `None` for links that must
/// stay as they are (external, anchors, assets).
pub fn rewrite_wiki_link(href: &str) -> Option<String> {
    if SKIP_HREF.iter().any(|p| href.starts_with(p)) {
        return None;
    }
    if let Some(rest) = href.strip_prefix("/wiki/") {
        return Some(format!("wiki://localhost/{rest}"));
    }
    if href.starts_with('/') {
        let path = href.split(['?', '#']).next().unwrap_or("").to_lowercase();
        if ASSET_EXTS.iter().any(|ext| path.ends_with(ext)) {
            return None;
        }
        return Some(format!("wiki://localhost{href}"));
    }
    None
}

fn parse_clean(html: &str) -> Document {
    let doc = Document::from(html);
    doc.select(STRIP_SELECTOR).remove();
    doc
}

fn is_fetchable(url: &str) -> bool {
    !url.is_empty() && !url.starts_with("data:")
}

fn image_asset(src: &str) -> Option<AssetRef> {
    is_fetchable(src).then(|| AssetRef {
        url: src.to_string(),
        kind: AssetKind::Image,
    })
}

fn stylesheet_asset(rel: Option<&str>, href: &str) -> Option<AssetRef> {
    (rel.is_some_and(|r| r.contains("stylesheet")) && is_fetchable(href)).then(|| AssetRef {
        url: href.to_string(),
        kind: AssetKind::Stylesheet,
    })
}

fn local_url(rel: &str) -> String {
    format!(
        "wiki://localhost/{}",
        utf8_percent_encode(rel, ASSET_PATH_ESCAPE)
    )
}

/// Every image and stylesheet the page references, in document order,
/// without duplicates. The caller decides which ones to store offline.
pub fn collect_assets(html: &str) -> Vec<AssetRef> {
    let doc = parse_clean(html);
    let mut seen = HashSet::new();
    let mut out = Vec::new();

    for img in doc.select("img[src]").nodes() {
        if let Some(asset) = img.attr("src").and_then(|s| image_asset(&s)) {
            if seen.insert(asset.clone()) {
                out.push(asset);
            }
        }
    }
    for link in doc.select("link[href]").nodes() {
        let rel = link.attr("rel");
        if let Some(asset) = link
            .attr("href")
            .and_then(|h| stylesheet_asset(rel.as_deref(), &h))
        {
            if seen.insert(asset.clone()) {
                out.push(asset);
            }
        }
    }
    out
}

/// Clean up and rewrite one page. `resolve` maps an asset to its path under
/// the assets directory; `None` keeps the original URL.
pub fn process_html(html: &str, resolve: impl Fn(&AssetRef) -> Option<String>) -> ProcessedPage {
    let doc = parse_clean(html);

    for a in doc.select("a[href]").nodes() {
        if let Some(new) = a.attr("href").and_then(|h| rewrite_wiki_link(&h)) {
            a.set_attr("href", &new);
        }
    }

    for img in doc.select("img").nodes() {
        // Drop srcset so the webview doesn't ask for resolutions we never stored.
        if img.has_attr("srcset") {
            img.set_attr("srcset", "");
        }
        let local = img
            .attr("src")
            .and_then(|s| image_asset(&s))
            .and_then(|asset| resolve(&asset));
        if let Some(rel) = local {
            img.set_attr("src", &local_url(&rel));
        }
    }

    for link in doc.select("link[href]").nodes() {
        let rel = link.attr("rel");
        let local = link
            .attr("href")
            .and_then(|h| stylesheet_asset(rel.as_deref(), &h))
            .and_then(|asset| resolve(&asset));
        if let Some(path) = local {
            link.set_attr("href", &local_url(&path));
        }
    }

    let categories = doc
        .select("#mw-normal-catlinks li a")
        .nodes()
        .iter()
        .map(|a| collapse_whitespace(&a.text()))
        .filter(|t| !t.is_empty())
        .collect::<Vec<_>>()
        .join("|");

    let last_modified = Some(collapse_whitespace(
        &doc.select("#footer-info-lastmod").text(),
    ))
    .filter(|t| !t.is_empty())
    .unwrap_or_else(utc_now);

    ProcessedPage {
        html: doc.html().to_string(),
        categories,
        last_modified,
    }
}

/// Page stored for link targets that exist on neither wiki, so the crawl
/// doesn't keep retrying them.
pub fn stub_html(title: &str) -> String {
    let title_html = escape_html(title);
    let en_url = format!("{EN_BASE}/{}", title_to_slug(title));
    format!(
        "<!DOCTYPE html><html lang=\"pt-BR\"><head><meta charset=\"UTF-8\">\
         <title>{title_html}</title></head>\
         <body><h1>{title_html}</h1>\
         <p>Esta página não está disponível offline.</p>\
         <p><a href=\"{}\">Ver no wiki em inglês</a></p>\
         </body></html>",
        escape_html(&en_url)
    )
}

fn collapse_whitespace(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAGE: &str = r##"<!DOCTYPE html>
<html><head>
<link rel="stylesheet" href="/mediawiki/load.php?lang=pt&amp;modules=site.styles&amp;only=styles">
<link rel="icon" href="/favicon.ico">
<script>console.log("chrome")</script>
</head><body>
<div id="mw-head"><a href="/Especial:Entrar">Entrar</a><img src="/images/logo.png"></div>
<div id="content">
  <a href="/wiki/Mineração">Mineração</a>
  <a href="/Pesca#Varas">Pesca</a>
  <a href="/images/a/ab/Foo.png">arquivo</a>
  <a href="https://stardewvalleywiki.com/Mining">EN</a>
  <a href="#topo">topo</a>
  <img src="/images/a/ab/Foo.png" srcset="/images/a/ab/Foo2x.png 2x">
  <img src="//stardewvalleywiki.com/images/b/bc/Bar.png">
  <img src="https://stardewvalleywiki.com/mediawiki/images/en/0/07/%27Boat%27_Location.png">
  <img src="https://example.com/ad.png">
  <img src="data:image/png;base64,AAAA">
  <img src="/images/missing.png">
</div>
<div id="mw-normal-catlinks"><ul><li><a href="/Categoria:Peixes"> Peixes </a></li><li><a href="/Categoria:Itens">Itens</a></li></ul></div>
<ul><li id="footer-info-lastmod"> Esta página foi editada pela última vez
   em 3 de março de 2025.</li></ul>
<script>more()</script>
</body></html>"##;

    #[test]
    fn rewrites_internal_links_only() {
        assert_eq!(
            rewrite_wiki_link("/wiki/Foo").as_deref(),
            Some("wiki://localhost/Foo")
        );
        assert_eq!(
            rewrite_wiki_link("/Foo?x=1#y").as_deref(),
            Some("wiki://localhost/Foo?x=1#y")
        );
        assert_eq!(rewrite_wiki_link("/images/Foo.PNG"), None);
        assert_eq!(rewrite_wiki_link("//cdn/x"), None);
        assert_eq!(rewrite_wiki_link("https://x"), None);
        assert_eq!(rewrite_wiki_link("#top"), None);
        assert_eq!(rewrite_wiki_link("mailto:a@b"), None);
        assert_eq!(rewrite_wiki_link("relative"), None);
    }

    #[test]
    fn collects_assets_outside_the_stripped_chrome() {
        let assets = collect_assets(PAGE);
        assert_eq!(
            assets,
            vec![
                AssetRef {
                    url: "/images/a/ab/Foo.png".into(),
                    kind: AssetKind::Image
                },
                AssetRef {
                    url: "//stardewvalleywiki.com/images/b/bc/Bar.png".into(),
                    kind: AssetKind::Image
                },
                AssetRef {
                    url: "https://stardewvalleywiki.com/mediawiki/images/en/0/07/%27Boat%27_Location.png"
                        .into(),
                    kind: AssetKind::Image
                },
                AssetRef {
                    url: "https://example.com/ad.png".into(),
                    kind: AssetKind::Image
                },
                AssetRef {
                    url: "/images/missing.png".into(),
                    kind: AssetKind::Image
                },
                AssetRef {
                    // Entities are decoded, like BeautifulSoup did.
                    url: "/mediawiki/load.php?lang=pt&modules=site.styles&only=styles".into(),
                    kind: AssetKind::Stylesheet
                },
            ]
        );
    }

    #[test]
    fn processes_a_page() {
        let page = process_html(PAGE, |asset| match asset.url.as_str() {
            "/images/a/ab/Foo.png" => Some("images/a/ab/Foo.png".into()),
            "//stardewvalleywiki.com/images/b/bc/Bar.png" => Some("images/b/bc/Bar.png".into()),
            "https://stardewvalleywiki.com/mediawiki/images/en/0/07/%27Boat%27_Location.png" => {
                Some("mediawiki/images/en/0/07/'Boat'_Location #2+.png".into())
            }
            u if u.starts_with("/mediawiki/load.php") => Some("css/abc.css".into()),
            _ => None,
        });
        let html = &page.html;

        assert!(!html.contains("<script"), "scripts must be removed");
        assert!(!html.contains("mw-head"), "chrome must be removed");
        assert!(!html.contains("logo.png"));
        assert!(html.contains(r#"href="wiki://localhost/Mineração""#));
        assert!(html.contains(r#"href="wiki://localhost/Pesca#Varas""#));
        assert!(
            html.contains(r#"href="/images/a/ab/Foo.png""#),
            "asset links stay"
        );
        assert!(html.contains(r#"href="https://stardewvalleywiki.com/Mining""#));
        assert!(html.contains(r##"href="#topo""##));
        assert!(html.contains(r#"src="wiki://localhost/images/a/ab/Foo.png" srcset="""#));
        assert!(html.contains(r#"src="wiki://localhost/images/b/bc/Bar.png""#));
        assert!(html.contains(
            r#"src="wiki://localhost/mediawiki/images/en/0/07/%27Boat%27_Location%20%232%2B.png""#
        ));
        assert!(
            html.contains(r#"src="https://example.com/ad.png""#),
            "unresolved absolute images keep their URL"
        );
        assert!(html.contains(r#"src="data:image/png;base64,AAAA""#));
        assert!(
            html.contains(r#"src="/images/missing.png""#),
            "unresolved assets keep their URL"
        );
        assert!(html.contains(r#"href="wiki://localhost/css/abc.css""#));
        assert!(html.contains(r#"href="/favicon.ico""#));

        assert_eq!(page.categories, "Peixes|Itens");
        assert_eq!(
            page.last_modified,
            "Esta página foi editada pela última vez em 3 de março de 2025."
        );
    }

    #[test]
    fn last_modified_falls_back_to_now() {
        let page = process_html("<html><body><p>x</p></body></html>", |_| None);
        assert_eq!(page.last_modified.len(), "2026-01-01T00:00:00Z".len());
        assert!(page.last_modified.ends_with('Z'));
        assert_eq!(page.categories, "");
    }

    #[test]
    fn stub_escapes_the_title() {
        let html = stub_html("<b>Foo Bar</b>");
        assert!(html.contains("<h1>&lt;b&gt;Foo Bar&lt;/b&gt;</h1>"));
        assert!(html.contains("https://stardewvalleywiki.com/%3Cb%3EFoo_Bar%3C/b%3E"));
    }
}
