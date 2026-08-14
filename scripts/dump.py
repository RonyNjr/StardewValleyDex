#!/usr/bin/env python3
"""
StardewValleyDex - Initial wiki dump script.

Fetches all pages from https://pt.stardewvalleywiki.com, rewrites internal
links / asset URLs to the app's `wiki://localhost/` custom protocol, downloads
assets (images + CSS) to disk, and stores the processed HTML in the SQLite
database consumed by the Tauri app.

Usage:
    python3 scripts/dump.py                        # full dump
    python3 scripts/dump.py --limit 5              # only 5 pages (for testing)
    python3 scripts/dump.py --resume               # skip pages already in SQLite
    python3 scripts/dump.py --data-dir ./tmp       # override data directory
    python3 scripts/dump.py --workers 16           # override HTTP concurrency

    # Link-graph closure pass: after the main dump, scan stored HTML for
    # wiki://localhost/ links that point at pages not yet in the DB, and
    # download them in rounds until fixpoint (or --max-pages reached):
    python3 scripts/dump.py --resume --crawl-missing
    python3 scripts/dump.py --crawl-missing-only   # skip the main dump entirely

Requirements: see scripts/requirements.txt
"""

from __future__ import annotations

import argparse
import hashlib
import re
import sqlite3
import sys
import threading
import time
from concurrent.futures import ThreadPoolExecutor, as_completed
from pathlib import Path
from typing import Iterable, Optional
from urllib.parse import quote, urljoin, urlparse, unquote

import requests
from bs4 import BeautifulSoup
from requests.adapters import HTTPAdapter
from urllib3.util.retry import Retry

WIKI_BASE = "https://pt.stardewvalleywiki.com"
API_URL = f"{WIKI_BASE}/api.php"
EN_WIKI_BASE = "https://stardewvalleywiki.com"
EN_API_URL = f"{EN_WIKI_BASE}/api.php"
USER_AGENT = "StardewValleyDex/0.1 (offline-wiki-dump; educational)"

# Politeness delays (seconds). With concurrent workers the effective request
# rate is roughly WORKERS / PAGE_SLEEP; keep the per-worker delay small but
# non-zero so we still throttle a bit.
PAGE_SLEEP = 0.05
API_SLEEP = 0.1
DEFAULT_WORKERS = 8

# Elements to strip out of the wiki HTML entirely (original MediaWiki chrome).
STRIP_SELECTORS = ["#mw-head", "#mw-head-base", "#mw-page-base"]

# MediaWiki namespaces we never want to crawl / follow as internal links.
SKIP_NS = (
    "Especial", "Special", "Ficheiro", "File", "Ajuda", "Help",
    "Predefinição", "Template", "Categoria", "Category",
    "Módulo", "Module", "MediaWiki", "Usuário", "User", "Discussão",
)

DEFAULT_DATA_DIR = (
    Path.home()
    / "Library"
    / "Application Support"
    / "com.stardewvalleydex.app"
)

SCHEMA_SQL = """
CREATE TABLE IF NOT EXISTS pages (
    title TEXT PRIMARY KEY,
    html  TEXT NOT NULL,
    last_modified TEXT NOT NULL,
    categories TEXT DEFAULT ''
);
CREATE TABLE IF NOT EXISTS redirects (
    from_title TEXT PRIMARY KEY,
    to_title   TEXT NOT NULL
);
"""


# ---------------------------------------------------------------------------
# HTTP helpers
# ---------------------------------------------------------------------------


def make_session(verify_ssl: bool = True, pool_size: int = 32) -> requests.Session:
    """
    Build a requests.Session with a larger connection pool and basic retries.
    A single Session is safe to share across threads for typical GET workloads.
    """
    session = requests.Session()
    session.headers.update({"User-Agent": USER_AGENT})
    session.verify = verify_ssl

    retry = Retry(
        total=3,
        backoff_factor=0.5,
        status_forcelist=(500, 502, 503, 504),
        allowed_methods=frozenset(["GET"]),
        raise_on_status=False,
    )
    adapter = HTTPAdapter(
        pool_connections=pool_size,
        pool_maxsize=pool_size,
        max_retries=retry,
    )
    session.mount("http://", adapter)
    session.mount("https://", adapter)
    return session


def fetch_all_titles(session: requests.Session) -> list[str]:
    """
    Get all page titles. Tries the MediaWiki API first; if it fails for any
    reason (disabled module, non-JSON response, etc.), falls back to scraping
    Special:AllPages.
    """
    print("Listing pages via Special:AllPages...")
    titles = _fetch_titles_via_special_page(session)
    print(f"  found {len(titles)} pages (via Special:AllPages)")
    return titles


def _fetch_titles_via_api(session: requests.Session) -> list[str]:
    titles: list[str] = []
    params: dict[str, str] = {
        "action": "query",
        "list": "allpages",
        "aplimit": "500",
        "format": "json",
    }

    while True:
        resp = session.get(API_URL, params=params, timeout=30)
        if not resp.ok:
            preview = resp.text[:300].replace("\n", " ")
            raise RuntimeError(
                f"API returned HTTP {resp.status_code} for {resp.url}\n  Body: {preview}"
            )
        data = resp.json()
        for page in data.get("query", {}).get("allpages", []):
            titles.append(page["title"])
        cont = data.get("continue")
        if not cont:
            break
        params.update(cont)
        time.sleep(API_SLEEP)

    return titles


def _fetch_titles_via_sitemap(session: requests.Session) -> list[str]:
    """Try to get all titles from the wiki sitemap XML."""
    import xml.etree.ElementTree as ET

    for sitemap_url in [
        f"{WIKI_BASE}/sitemap.xml",
        f"{WIKI_BASE}/sitemap-index.xml",
        f"{WIKI_BASE}/sitemap_index.xml",
    ]:
        resp = session.get(sitemap_url, timeout=30)
        if not resp.ok:
            continue

        try:
            root = ET.fromstring(resp.text)
        except ET.ParseError:
            continue

        ns = {"sm": "http://www.sitemaps.org/schemas/sitemap/0.9"}
        titles: list[str] = []

        # sitemap index → fetch each child sitemap
        for loc in root.findall(".//sm:loc", ns):
            url = loc.text or ""
            if "sitemap" in url and url != sitemap_url:
                sub = session.get(url, timeout=30)
                if sub.ok:
                    try:
                        sub_root = ET.fromstring(sub.text)
                        for sub_loc in sub_root.findall(".//sm:loc", ns):
                            page_url = sub_loc.text or ""
                            _extract_title_from_url(page_url, titles)
                    except ET.ParseError:
                        pass
            else:
                _extract_title_from_url(url, titles)

        if titles:
            print(f"  (sitemap: {sitemap_url})")
            return titles

    return []


def _extract_title_from_url(url: str, titles: list) -> None:
    for prefix in [f"{WIKI_BASE}/wiki/", f"{WIKI_BASE}/"]:
        if url.startswith(prefix):
            raw = url[len(prefix):].split("#")[0].split("?")[0]
            title = unquote(raw).replace("_", " ").strip().strip('"').strip("'")
            if title and not any(title.startswith(ns + ":") for ns in SKIP_NS):
                titles.append(title)
            return


def _fetch_titles_via_special_page(session: requests.Session) -> list[str]:
    """
    Scrape Special:AllPages. Tries the sitemap first, then the special page
    with a broad CSS selector to handle custom wiki skins.
    """
    # Try sitemap first — it's more reliable
    titles = _fetch_titles_via_sitemap(session)
    if titles:
        return titles

    START_URLS = [
        f"{WIKI_BASE}/Especial:Todas_as_p%C3%A1ginas",
        f"{WIKI_BASE}/Special:AllPages",
        f"{WIKI_BASE}/index.php?title=Special:AllPages",
    ]

    start_url: Optional[str] = None
    for candidate in START_URLS:
        r = session.get(candidate, timeout=30, allow_redirects=True)
        if r.ok:
            start_url = r.url
            break

    if not start_url:
        raise RuntimeError("Could not reach Special:AllPages or sitemap on this wiki.")

    titles = []
    url: Optional[str] = start_url

    while url:
        resp = session.get(url, timeout=30)
        if not resp.ok:
            break

        soup = BeautifulSoup(resp.text, "lxml")

        # Try multiple selectors — different skins use different classes
        candidates = soup.select(
            ".mw-allpages-body a, "
            ".mw-allpages-chunk a, "
            "#mw-content-text li a, "
            "#mw-content-text td a"
        )

        found_on_page = 0
        for a in candidates:
            href = a.get("href", "")
            # Handle both /wiki/X and /X URL formats
            if "/wiki/" in href:
                raw = href.split("/wiki/", 1)[1]
            elif href.startswith("/") and not href.startswith("//"):
                raw = href.lstrip("/")
            else:
                continue

            raw = raw.split("#")[0].split("?")[0]
            title = unquote(raw).replace("_", " ").strip().strip('"').strip("'")

            if not title:
                continue
            if any(title.startswith(ns + ":") for ns in SKIP_NS):
                continue

            titles.append(title)
            found_on_page += 1

        if found_on_page == 0:
            # Debug: print a snippet of the page HTML to help diagnose selector issues
            content = soup.select_one("#mw-content-text")
            if content:
                print(f"\n  [DEBUG] #mw-content-text snippet (first 400 chars):")
                print(f"  {str(content)[:400]}")
            break

        # Follow pagination — PT wiki uses "Página seguinte"; also handle "next"/"próxim"
        url = None
        for nav_a in soup.select(".mw-allpages-nav a, #mw-content-text a"):
            text = nav_a.get_text(strip=True).lower()
            href = nav_a.get("href", "")
            if ("seguinte" in text or "próxim" in text or "next" in text) and "from=" in href:
                url = urljoin(WIKI_BASE, href)
                break

        if url:
            time.sleep(API_SLEEP)

    return titles


def fetch_redirect_target(session: requests.Session, title: str) -> Optional[str]:
    """
    Ask the PT wiki API if `title` is a redirect page and return the redirect
    target title. Returns None if the page is not a redirect or the API fails.
    """
    params = {
        "action": "query",
        "titles": title,
        "redirects": "1",
        "format": "json",
    }
    try:
        resp = session.get(API_URL, params=params, timeout=15)
        if not resp.ok:
            return None
        data = resp.json()
        redirects = data.get("query", {}).get("redirects", [])
        if redirects:
            return redirects[-1].get("to")
    except Exception:
        pass
    return None


def _fetch_en_title(session: requests.Session, pt_title: str) -> Optional[str]:
    """Look up the English wiki equivalent of a PT title via the langlinks API."""
    params = {
        "action": "query",
        "titles": pt_title,
        "prop": "langlinks",
        "lllang": "en",
        "format": "json",
    }
    try:
        resp = session.get(API_URL, params=params, timeout=15)
        if not resp.ok:
            return None
        data = resp.json()
        for page in data.get("query", {}).get("pages", {}).values():
            for ll in page.get("langlinks", []):
                if ll.get("lang") == "en":
                    return ll.get("*")
    except Exception:
        pass
    return None


def _fetch_html(session: requests.Session, title: str, base: str) -> tuple[Optional[str], int]:
    slug = quote(title.replace(" ", "_"), safe="/:@!$&'()*+,;=.-_~")
    url = f"{base}/{slug}"
    try:
        resp = session.get(url, timeout=30)
    except requests.RequestException:
        return None, 0
    if resp.status_code != 200:
        return None, resp.status_code
    return resp.text, 200


def fetch_page_html(session: requests.Session, title: str) -> tuple[Optional[str], int, str]:
    """
    Fetch a wiki page. Tries PT wiki first; on 404 falls back to the EN wiki
    via langlinks. Returns (html, status_code, source) where source is 'pt' or
    'en:<en_title>'.
    """
    html, status = _fetch_html(session, title, WIKI_BASE)
    if html is not None:
        return html, status, "pt"

    if status == 404:
        en_title = _fetch_en_title(session, title) or title
        html, en_status = _fetch_html(session, en_title, EN_WIKI_BASE)
        if html is not None:
            return html, en_status, f"en:{en_title}"
        return None, en_status, "en"

    return None, status, "pt"


# ---------------------------------------------------------------------------
# Asset download
# ---------------------------------------------------------------------------


def _normalize_asset_url(raw_url: str) -> str:
    """
    Turn a possibly-relative URL into an absolute https URL pointing at the
    wiki. Handles:
      - //pt.stardewvalleywiki.com/... (protocol-relative)
      - /images/... (host-relative)
      - full https://... URLs (left alone)
    """
    if raw_url.startswith("//"):
        return "https:" + raw_url
    if raw_url.startswith("/"):
        return WIKI_BASE + raw_url
    return raw_url


def _local_path_for_asset(url: str, assets_dir: Path) -> tuple[Path, str]:
    """
    Compute (absolute_local_path, relative_path_from_assets_dir) for a given
    asset URL. Preserves the URL path structure so wiki://localhost/images/...
    can find the file straight from disk.
    """
    parsed = urlparse(url)
    path = unquote(parsed.path).lstrip("/")
    if not path:
        path = "index"
    local = assets_dir / path
    return local, path


def _local_path_for_css(url: str, assets_dir: Path) -> tuple[Path, str]:
    """Dynamic CSS (load.php) has query strings; hash them into a stable name."""
    digest = hashlib.md5(url.encode("utf-8")).hexdigest()[:12]
    rel = f"css/{digest}.css"
    return assets_dir / rel, rel


# Process-wide asset cache to avoid re-checking disk / redownloading the same
# asset repeatedly across pages. Maps normalized URL -> rel path (or None if
# the download previously failed).
_ASSET_CACHE: dict[str, Optional[str]] = {}
_ASSET_LOCK = threading.Lock()
# Per-URL locks so two concurrent workers don't race on the same download.
_INFLIGHT_LOCKS: dict[str, threading.Lock] = {}


def _asset_lock_for(url: str) -> threading.Lock:
    with _ASSET_LOCK:
        lock = _INFLIGHT_LOCKS.get(url)
        if lock is None:
            lock = threading.Lock()
            _INFLIGHT_LOCKS[url] = lock
        return lock


def download_asset(
    session: requests.Session,
    url: str,
    assets_dir: Path,
    is_css: bool = False,
    verbose: bool = False,
) -> Optional[str]:
    """
    Download an asset to disk, idempotently. Returns the relative path from
    assets_dir (e.g. "images/thumb/a/b/file.png") or None on failure.
    """
    url = _normalize_asset_url(url)
    if not url.startswith("http"):
        return None

    # Fast path: process-wide cache.
    cached = _ASSET_CACHE.get(url)
    if cached is not None or url in _ASSET_CACHE:
        return cached

    if is_css:
        local_path, rel_path = _local_path_for_css(url, assets_dir)
    else:
        local_path, rel_path = _local_path_for_asset(url, assets_dir)

    # Serialize concurrent downloads of the same URL.
    lock = _asset_lock_for(url)
    with lock:
        # Re-check cache once we hold the lock.
        if url in _ASSET_CACHE:
            return _ASSET_CACHE[url]

        try:
            if local_path.exists() and local_path.stat().st_size > 0:
                _ASSET_CACHE[url] = rel_path
                return rel_path
        except OSError:
            pass

        if verbose:
            print(f"  -> downloading {rel_path}")

        try:
            resp = session.get(url, timeout=30, stream=True)
            if resp.status_code != 200:
                _ASSET_CACHE[url] = None
                return None
            local_path.parent.mkdir(parents=True, exist_ok=True)
            with open(local_path, "wb") as fh:
                for chunk in resp.iter_content(chunk_size=64 * 1024):
                    if chunk:
                        fh.write(chunk)
            _ASSET_CACHE[url] = rel_path
            return rel_path
        except requests.RequestException:
            # Clean up any partial file we may have created.
            try:
                if local_path.exists() and local_path.stat().st_size == 0:
                    local_path.unlink()
            except OSError:
                pass
            _ASSET_CACHE[url] = None
            return None


# ---------------------------------------------------------------------------
# HTML processing
# ---------------------------------------------------------------------------


_ASSET_EXTS = {".png", ".jpg", ".jpeg", ".gif", ".svg", ".css", ".ttf", ".woff", ".woff2"}
_SKIP_HREF = ("wiki://", "http://", "https://", "//", "#", "mailto:", "javascript:")


def _rewrite_wiki_link(href: str) -> Optional[str]:
    """
    Rewrite internal wiki links to wiki://localhost/... .
    Handles both /wiki/Foo (standard MediaWiki) and /Foo (short-URL wikis).
    Returns None if the href should be left untouched (assets, external, anchors).
    """
    if href.startswith(_SKIP_HREF):
        return None
    if href.startswith("/wiki/"):
        return "wiki://localhost/" + href[len("/wiki/"):]
    if href.startswith("/"):
        path = href.split("?", 1)[0].split("#", 1)[0]
        low = path.lower()
        for ext in _ASSET_EXTS:
            if low.endswith(ext):
                return None
        return f"wiki://localhost{href}"
    return None


def process_html(
    session: requests.Session,
    html: str,
    assets_dir: Path,
    asset_counter: list[int],
    verbose_assets: bool = False,
) -> tuple[str, str, str]:
    """
    Clean up and rewrite one page's HTML. Returns
    (processed_html, categories, last_modified).

    `asset_counter` is a single-element list used to accumulate a running
    total of assets downloaded (for the final summary). We also extract the
    last-modified footer here so we only build one BeautifulSoup tree per page.
    """
    soup = BeautifulSoup(html, "lxml")

    # 1. Strip MediaWiki chrome + scripts.
    for selector in STRIP_SELECTORS:
        for el in soup.select(selector):
            el.decompose()
    for script in soup.find_all("script"):
        script.decompose()

    # 2. Rewrite anchor hrefs (/wiki/Foo -> wiki://localhost/Foo).
    for a in soup.find_all("a", href=True):
        rewritten = _rewrite_wiki_link(a["href"])
        if rewritten is not None:
            a["href"] = rewritten

    # 3. Rewrite images: download the asset, then point src at wiki://localhost.
    for img in soup.find_all("img"):
        # Clear srcset to avoid extra requests we haven't cached.
        if img.has_attr("srcset"):
            img["srcset"] = ""

        src = img.get("src")
        if not src or src.startswith("data:"):
            continue

        # Only rewrite same-origin / protocol-relative wiki assets.
        if src.startswith("/") or src.startswith("//pt.stardewvalleywiki.com"):
            rel = download_asset(
                session, src, assets_dir, is_css=False, verbose=verbose_assets
            )
            if rel is not None:
                img["src"] = f"wiki://localhost/{rel}"
                asset_counter[0] += 1

    # 4. Rewrite stylesheets similarly.
    for link in soup.find_all("link"):
        rel_attr = link.get("rel") or []
        if isinstance(rel_attr, list):
            rel_str = " ".join(rel_attr)
        else:
            rel_str = str(rel_attr)
        if "stylesheet" not in rel_str:
            continue

        href = link.get("href")
        if not href or not (href.startswith("/") or href.startswith("//")):
            continue

        css_rel = download_asset(
            session, href, assets_dir, is_css=True, verbose=verbose_assets
        )
        if css_rel is not None:
            link["href"] = f"wiki://localhost/{css_rel}"
            asset_counter[0] += 1

    # 5. Extract categories from #mw-normal-catlinks li a.
    categories: list[str] = []
    for a in soup.select("#mw-normal-catlinks li a"):
        text = a.get_text(strip=True)
        if text:
            categories.append(text)
    categories_str = "|".join(categories)

    # 6. Pull last-modified out of the footer while we still have the tree.
    footer = soup.select_one("#footer-info-lastmod")
    if footer:
        last_modified = footer.get_text(strip=True) or time.strftime(
            "%Y-%m-%dT%H:%M:%SZ", time.gmtime()
        )
    else:
        last_modified = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())

    processed = str(soup)
    return processed, categories_str, last_modified


# Kept for backward compat with any external caller / earlier scripts.
def extract_last_modified(html: str) -> str:
    """
    Try to pull the 'Última modificação' timestamp out of the footer. Falls
    back to the current UTC timestamp if we can't find it.

    Prefer using the value returned from `process_html` — this re-parses the
    HTML and is only useful if you have raw HTML with no soup already built.
    """
    soup = BeautifulSoup(html, "lxml")
    footer = soup.select_one("#footer-info-lastmod")
    if footer:
        text = footer.get_text(strip=True)
        if text:
            return text
    return time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())


# ---------------------------------------------------------------------------
# Database
# ---------------------------------------------------------------------------


def open_db(data_dir: Path) -> sqlite3.Connection:
    data_dir.mkdir(parents=True, exist_ok=True)
    db_path = data_dir / "stardewvalleydex.sqlite"
    # check_same_thread=False so the writer thread pattern (fetch on workers,
    # write on the main thread) doesn't require a separate connection.
    conn = sqlite3.connect(db_path, check_same_thread=False)
    # WAL + normal sync gives us a large write-throughput boost with negligible
    # risk for a dev dump script.
    conn.execute("PRAGMA journal_mode=WAL")
    conn.execute("PRAGMA synchronous=NORMAL")
    conn.execute("PRAGMA temp_store=MEMORY")
    conn.executescript(SCHEMA_SQL)
    conn.commit()
    return conn


def existing_titles(conn: sqlite3.Connection) -> set[str]:
    return {row[0] for row in conn.execute("SELECT title FROM pages")}


def upsert_redirect(conn: sqlite3.Connection, from_title: str, to_title: str) -> None:
    conn.execute(
        """
        INSERT INTO redirects (from_title, to_title)
        VALUES (?, ?)
        ON CONFLICT(from_title) DO UPDATE SET to_title = excluded.to_title
        """,
        (from_title, to_title),
    )
    conn.commit()


def existing_redirects(conn: sqlite3.Connection) -> set[str]:
    return {row[0] for row in conn.execute("SELECT from_title FROM redirects")}


def upsert_page(
    conn: sqlite3.Connection,
    title: str,
    html: str,
    last_modified: str,
    categories: str,
) -> None:
    conn.execute(
        """
        INSERT INTO pages (title, html, last_modified, categories)
        VALUES (?, ?, ?, ?)
        ON CONFLICT(title) DO UPDATE SET
            html = excluded.html,
            last_modified = excluded.last_modified,
            categories = excluded.categories
        """,
        (title, html, last_modified, categories),
    )
    conn.commit()


# ---------------------------------------------------------------------------
# Link-graph closure pass (formerly crawl_missing.py)
# ---------------------------------------------------------------------------


_HREF_RE = re.compile(r'href="wiki://localhost/([^"#?]*)"', re.IGNORECASE)
_QUOTED_HREF_RE = re.compile(r'href="wiki://localhost/(%22[^"]*%22)"', re.IGNORECASE)


def _stub_html(title: str) -> str:
    """HTML stub written for pages that are missing from both PT and EN wikis."""
    en_url = f"https://stardewvalleywiki.com/{quote(title.replace(' ', '_'))}"
    return (
        f'<!DOCTYPE html><html lang="pt-BR"><head><meta charset="UTF-8">'
        f'<title>{title}</title></head>'
        f'<body><h1>{title}</h1>'
        f'<p>Esta página não está disponível offline.</p>'
        f'<p><a href="{en_url}">Ver no wiki em inglês</a></p>'
        f'</body></html>'
    )


def find_missing_targets(conn: sqlite3.Connection) -> set[str]:
    """
    Scan all stored pages for `href="wiki://localhost/..."` links and return
    the set of target titles that are not present in the pages table.
    Considers common title-form variants (underscores vs spaces, stripped
    quotes) when deciding whether a link is already covered.
    """
    # Single pass over pages: build the `known` set and collect candidates at
    # the same time. Avoids two full table scans and holding all HTML twice.
    known: set[str] = set()
    candidates: set[str] = set()
    skip_prefixes = tuple(ns + ":" for ns in SKIP_NS)

    for (title, html) in conn.execute("SELECT title, html FROM pages"):
        known.add(title)
        known.add(title.replace("_", " "))
        known.add(title.replace(" ", "_"))
        clean = title.strip('"').strip("'")
        known.add(clean)
        known.add(clean.replace("_", " "))

        for m in _HREF_RE.finditer(html):
            raw = unquote(m.group(1)).strip()
            target = raw.replace("_", " ").strip('"').strip("'")
            if not target or target.startswith(skip_prefixes):
                continue
            candidates.add(target)

    return {t for t in candidates if t not in known}


def fix_quoted_links(conn: sqlite3.Connection) -> int:
    """
    Rewrite `wiki://localhost/%22Title%22` (quoted) hrefs in stored HTML to
    `wiki://localhost/Title`. One-time cleanup for pages persisted before the
    quote-stripping fix landed. Returns the number of hrefs rewritten.
    """
    count = 0
    updates: list[tuple[str, str]] = []
    for title, html in conn.execute("SELECT title, html FROM pages"):
        if "%22" not in html:  # fast prefilter
            continue

        def replacer(m: re.Match) -> str:
            nonlocal count
            inner = unquote(m.group(1)).strip('"').strip("'")
            encoded = quote(inner.replace(" ", "_"), safe="/:@!$&'()*+,;=.-_~")
            count += 1
            return f'href="wiki://localhost/{encoded}"'

        new_html = _QUOTED_HREF_RE.sub(replacer, html)
        if new_html != html:
            updates.append((new_html, title))
    if updates:
        conn.executemany("UPDATE pages SET html = ? WHERE title = ?", updates)
        conn.commit()
    return count


def crawl_missing_pass(
    session: requests.Session,
    conn: sqlite3.Connection,
    assets_dir: Path,
    asset_counter: list[int],
    max_pages: int,
    verbose_assets: bool = False,
    workers: int = DEFAULT_WORKERS,
) -> int:
    """
    Repeatedly scan the DB for missing internal link targets and download them
    until fixpoint (or `max_pages` new pages have been added). Missing pages
    that 404 on both PT and EN wikis are recorded as offline stubs so we
    don't keep retrying them.

    Returns the total number of pages added (real + stubs).
    """
    fixed = fix_quoted_links(conn)
    if fixed:
        print(f"Fixed {fixed} quoted links in stored HTML.")

    round_num = 0
    total_added = 0

    while total_added < max_pages:
        round_num += 1
        missing = find_missing_targets(conn)
        if not missing:
            print("\nNo more missing pages. Closure pass done.")
            break

        remaining = max_pages - total_added
        ordered = sorted(missing)[:remaining]
        print(f"\nRound {round_num}: {len(missing)} missing pages "
              f"(downloading up to {len(ordered)} with {workers} workers)...")

        added_this_round = _process_titles_concurrent(
            session=session,
            conn=conn,
            titles=ordered,
            assets_dir=assets_dir,
            asset_counter=asset_counter,
            verbose_assets=verbose_assets,
            workers=workers,
            allow_stub_on_miss=True,
        )
        total_added += added_this_round
        print(f"  Round {round_num} done: {added_this_round} pages processed.")
        if added_this_round == 0:
            break

    print(f"\nCrawl-missing total pages added: {total_added}")
    return total_added


# ---------------------------------------------------------------------------
# Concurrent page pipeline
# ---------------------------------------------------------------------------


def _fetch_task(
    session: requests.Session, title: str
) -> tuple[str, Optional[str], int, str]:
    """Worker fn: fetch one page's raw HTML. Sleeps briefly to be polite."""
    html, status, source = fetch_page_html(session, title)
    if PAGE_SLEEP:
        time.sleep(PAGE_SLEEP)
    return title, html, status, source


def _process_titles_concurrent(
    session: requests.Session,
    conn: sqlite3.Connection,
    titles: list[str],
    assets_dir: Path,
    asset_counter: list[int],
    verbose_assets: bool,
    workers: int,
    allow_stub_on_miss: bool,
    already_done: Optional[set[str]] = None,
    already_redirected: Optional[set[str]] = None,
) -> int:
    """
    Fetch a list of titles concurrently, then serially process the HTML and
    persist to SQLite. Network I/O (the actual bottleneck) is parallelized;
    HTML parsing and DB writes stay on the main thread to keep BeautifulSoup
    and SQLite happy without extra locking.

    Returns the number of pages successfully written (including redirects
    and stubs, matching the previous behavior of `ok_count`/`added_this_round`).
    """
    if not titles:
        return 0

    already_done = already_done or set()
    already_redirected = already_redirected or set()

    total = len(titles)
    written = 0
    fail = 0
    skip = 0
    completed = 0

    # Submit all fetches to the pool up front. Results are consumed in
    # completion order so slow requests don't block faster ones.
    with ThreadPoolExecutor(max_workers=workers, thread_name_prefix="fetch") as pool:
        futures = {}
        for title in titles:
            if title in already_done or title in already_redirected:
                skip += 1
                completed += 1
                print(f"[{completed}/{total}] {title} ... (skip: already in DB)")
                continue
            futures[pool.submit(_fetch_task, session, title)] = title

        for fut in as_completed(futures):
            completed += 1
            title = futures[fut]
            try:
                _t, html, status, source = fut.result()
            except Exception as e:  # noqa: BLE001 - dump script, keep going
                fail += 1
                print(f"[{completed}/{total}] {title} ... x ({type(e).__name__}: {e})")
                continue

            prefix = f"[{completed}/{total}] {title} ..."

            if html is None:
                if status == 404:
                    target = fetch_redirect_target(session, title)
                    if target:
                        upsert_redirect(conn, title, target)
                        written += 1
                        print(f"{prefix} ok (redirect → {target})")
                        continue
                if allow_stub_on_miss:
                    ts = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())
                    upsert_page(conn, title, _stub_html(title), ts, "")
                    written += 1
                    print(f"{prefix} stub (HTTP {status})")
                else:
                    fail += 1
                    print(f"{prefix} x (HTTP {status})")
                continue

            try:
                processed, categories, last_modified = process_html(
                    session, html, assets_dir, asset_counter,
                    verbose_assets=verbose_assets,
                )
                # Free the raw HTML ref early — pages can be a few hundred KB.
                html = None  # noqa: F841
                upsert_page(conn, title, processed, last_modified, categories)
            except Exception as e:  # noqa: BLE001
                fail += 1
                print(f"{prefix} x ({type(e).__name__}: {e})")
                continue

            written += 1
            label = f"ok (EN: {source[3:]})" if source.startswith("en:") else "ok"
            print(f"{prefix} {label}")

    # For the main-dump caller we want a breakdown; pack extras into globals
    # via attributes on the function so the caller can read them without
    # changing the return signature (kept for compatibility with the old
    # crawl_missing_pass contract).
    _process_titles_concurrent.last_ok = written  # type: ignore[attr-defined]
    _process_titles_concurrent.last_fail = fail  # type: ignore[attr-defined]
    _process_titles_concurrent.last_skip = skip  # type: ignore[attr-defined]
    return written


# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------


def parse_args(argv: Optional[Iterable[str]] = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Dump the pt.stardewvalleywiki into SQLite + assets on disk.")
    parser.add_argument(
        "--limit",
        type=int,
        default=None,
        help="Process at most N pages (useful for testing).",
    )
    parser.add_argument(
        "--data-dir",
        type=Path,
        default=DEFAULT_DATA_DIR,
        help=f"Override the data directory (default: {DEFAULT_DATA_DIR}).",
    )
    parser.add_argument(
        "--resume",
        action="store_true",
        help="Skip pages already present in the SQLite database.",
    )
    parser.add_argument(
        "--verbose-assets",
        action="store_true",
        help="Print a line for each asset that gets downloaded.",
    )
    parser.add_argument(
        "--no-ssl-verify",
        action="store_true",
        help="Disable SSL certificate verification (workaround for LibreSSL on macOS).",
    )
    parser.add_argument(
        "--crawl-missing",
        action="store_true",
        help=(
            "After the main dump completes, scan stored HTML for wiki://localhost "
            "links pointing at pages not yet in the DB and download them in rounds "
            "until fixpoint (or --max-pages)."
        ),
    )
    parser.add_argument(
        "--crawl-missing-only",
        action="store_true",
        help="Skip the main dump entirely and only run the link-graph closure pass.",
    )
    parser.add_argument(
        "--max-pages",
        type=int,
        default=5000,
        help="Cap on pages added during the --crawl-missing pass (default: 5000).",
    )
    parser.add_argument(
        "--workers",
        type=int,
        default=DEFAULT_WORKERS,
        help=f"Number of concurrent HTTP fetch workers (default: {DEFAULT_WORKERS}).",
    )
    return parser.parse_args(list(argv) if argv is not None else None)


def main(argv: Optional[Iterable[str]] = None) -> int:
    args = parse_args(argv)

    data_dir: Path = args.data_dir
    assets_dir = data_dir / "assets"
    assets_dir.mkdir(parents=True, exist_ok=True)

    print(f"Data directory: {data_dir}")
    print(f"Assets directory: {assets_dir}")

    if args.no_ssl_verify:
        import urllib3
        urllib3.disable_warnings(urllib3.exceptions.InsecureRequestWarning)
        print("WARNING: SSL verification disabled.")

    workers = max(1, args.workers)
    # Give the connection pool enough capacity for the workers plus asset
    # downloads that they trigger.
    session = make_session(
        verify_ssl=not args.no_ssl_verify,
        pool_size=max(32, workers * 4),
    )
    conn = open_db(data_dir)

    asset_counter = [0]
    ok_count = 0
    fail_count = 0
    skip_count = 0
    run_main_dump = not args.crawl_missing_only

    if run_main_dump:
        already_done: set[str] = existing_titles(conn) if args.resume else set()
        already_redirected: set[str] = existing_redirects(conn) if args.resume else set()
        if args.resume:
            print(f"Resume mode: {len(already_done)} pages already in DB will be skipped.")

        titles = fetch_all_titles(session)
        if args.limit is not None:
            titles = titles[: args.limit]
            print(f"--limit set: processing first {len(titles)} title(s) only.")

        start_ts = time.time()
        ok_count = _process_titles_concurrent(
            session=session,
            conn=conn,
            titles=titles,
            assets_dir=assets_dir,
            asset_counter=asset_counter,
            verbose_assets=args.verbose_assets,
            workers=workers,
            allow_stub_on_miss=False,
            already_done=already_done,
            already_redirected=already_redirected,
        )
        fail_count = getattr(_process_titles_concurrent, "last_fail", 0)
        skip_count = getattr(_process_titles_concurrent, "last_skip", 0)

        elapsed = time.time() - start_ts
        print()
        print("=" * 60)
        print(f"Main dump done in {elapsed:.1f}s")
        print(f"  pages ok:      {ok_count}")
        print(f"  pages failed:  {fail_count}")
        print(f"  pages skipped: {skip_count}")
        print(f"  assets downloaded: {asset_counter[0]}")
        print(f"  db: {data_dir / 'stardewvalleydex.sqlite'}")
        print(f"  assets: {assets_dir}")

    crawl_added = 0
    if args.crawl_missing or args.crawl_missing_only:
        print()
        print("=" * 60)
        print("Starting link-graph closure pass (--crawl-missing)...")
        crawl_added = crawl_missing_pass(
            session,
            conn,
            assets_dir,
            asset_counter,
            max_pages=args.max_pages,
            verbose_assets=args.verbose_assets,
            workers=workers,
        )
        print(f"  crawl-missing added: {crawl_added}")
        print(f"  total assets downloaded: {asset_counter[0]}")

    conn.close()
    return 0 if fail_count == 0 else 1


if __name__ == "__main__":
    sys.exit(main())
