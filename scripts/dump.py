#!/usr/bin/env python3
"""
StardewValleyDex - Initial wiki dump script.

Fetches all pages from https://pt.stardewvalleywiki.com, rewrites internal
links / asset URLs to the app's `wiki://localhost/` custom protocol, downloads
assets (images + CSS) to disk, and stores the processed HTML in the SQLite
database consumed by the Tauri app.

Usage:
    python3 scripts/dump.py                   # full dump
    python3 scripts/dump.py --limit 5         # only 5 pages (for testing)
    python3 scripts/dump.py --resume          # skip pages already in SQLite
    python3 scripts/dump.py --data-dir ./tmp  # override data directory

Requirements: see scripts/requirements.txt
"""

from __future__ import annotations

import argparse
import hashlib
import os
import sqlite3
import sys
import time
from pathlib import Path
from typing import Iterable, Optional
from urllib.parse import urljoin, urlparse, unquote

import requests
from bs4 import BeautifulSoup

WIKI_BASE = "https://pt.stardewvalleywiki.com"
API_URL = f"{WIKI_BASE}/api.php"
EN_WIKI_BASE = "https://stardewvalleywiki.com"
EN_API_URL = f"{EN_WIKI_BASE}/api.php"
USER_AGENT = "StardewValleyDex/0.1 (offline-wiki-dump; educational)"

# Politeness delays (seconds)
PAGE_SLEEP = 0.5
API_SLEEP = 0.1

# Elements to strip out of the wiki HTML entirely (original MediaWiki chrome).
STRIP_SELECTORS = ["#mw-head", "#mw-head-base", "#mw-page-base"]

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


def make_session(verify_ssl: bool = True) -> requests.Session:
    session = requests.Session()
    session.headers.update({"User-Agent": USER_AGENT})
    session.verify = verify_ssl
    return session


def fetch_all_titles(session: requests.Session) -> list[str]:
    """
    Get all page titles. Tries the MediaWiki API first; if the allpages
    module is disabled (common on restricted wikis), falls back to scraping
    Special:AllPages.
    """
    print("Listing pages via MediaWiki API...")
    try:
        titles = _fetch_titles_via_api(session)
        print(f"  found {len(titles)} pages (via API)")
        return titles
    except RuntimeError as e:
        if "404" in str(e):
            print("  allpages API module disabled — falling back to Special:AllPages scraping...")
            titles = _fetch_titles_via_special_page(session)
            print(f"  found {len(titles)} pages (via Special:AllPages)")
            return titles
        raise


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
    SKIP_NS = (
        "Especial", "Special", "Ficheiro", "File", "Ajuda", "Help",
        "Predefinição", "Template", "Categoria", "Category",
        "Módulo", "Module", "MediaWiki", "Usuário", "User", "Discussão",
    )
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

    SKIP_NS = (
        "Especial", "Special", "Ficheiro", "File", "Ajuda", "Help",
        "Predefinição", "Template", "Categoria", "Category",
        "Módulo", "Module", "MediaWiki", "Usuário", "User", "Discussão",
    )

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

        # Follow pagination — look for any link containing "from=" in the nav area
        url = None
        for nav_a in soup.select(".mw-allpages-nav a, #mw-content-text a"):
            text = nav_a.get_text(strip=True).lower()
            href = nav_a.get("href", "")
            if ("próxim" in text or "next" in text) and "from=" in href:
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
    from urllib.parse import quote
    slug = quote(title.replace(" ", "_"), safe="/:@!$&'()*+,;=.-_~")
    url = f"{base}/{slug}"
    resp = session.get(url, timeout=30)
    if resp.status_code != 200:
        return None, resp.status_code
    return resp.text, 200


def fetch_page_html(session: requests.Session, title: str) -> tuple[Optional[str], int, str]:
    """
    Fetch a wiki page. Tries PT wiki first; on 404 falls back to the EN wiki
    via langlinks. Returns (html, status_code, source) where source is 'pt' or 'en'.
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

    if is_css:
        local_path, rel_path = _local_path_for_css(url, assets_dir)
    else:
        local_path, rel_path = _local_path_for_asset(url, assets_dir)

    if local_path.exists() and local_path.stat().st_size > 0:
        return rel_path

    if verbose:
        print(f"  -> downloading {rel_path}")

    try:
        resp = session.get(url, timeout=30, stream=True)
        if resp.status_code != 200:
            return None
        local_path.parent.mkdir(parents=True, exist_ok=True)
        with open(local_path, "wb") as fh:
            for chunk in resp.iter_content(chunk_size=64 * 1024):
                if chunk:
                    fh.write(chunk)
        return rel_path
    except requests.RequestException:
        # Clean up any partial file we may have created.
        try:
            if local_path.exists() and local_path.stat().st_size == 0:
                local_path.unlink()
        except OSError:
            pass
        return None


# ---------------------------------------------------------------------------
# HTML processing
# ---------------------------------------------------------------------------


def _rewrite_wiki_link(href: str) -> Optional[str]:
    """
    Rewrite `/wiki/Foo` or `/wiki/Foo#section` into `wiki://localhost/Foo...`.
    Returns None if the href isn't a wiki link we can rewrite.
    """
    if not href.startswith("/wiki/"):
        return None
    remainder = href[len("/wiki/"):]
    return f"wiki://localhost/{remainder}"


def process_html(
    session: requests.Session,
    html: str,
    assets_dir: Path,
    asset_counter: list[int],
    verbose_assets: bool = False,
) -> tuple[str, str]:
    """
    Clean up and rewrite one page's HTML. Returns (processed_html, categories).
    `asset_counter` is a single-element list used to accumulate a running
    total of assets downloaded (for the final summary).
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
        if not src:
            continue
        if src.startswith("data:"):
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
        rel = link.get("rel") or []
        if isinstance(rel, list):
            rel_str = " ".join(rel)
        else:
            rel_str = str(rel)
        if "stylesheet" not in rel_str:
            continue

        href = link.get("href")
        if not href:
            continue
        if not (href.startswith("/") or href.startswith("//")):
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

    processed = str(soup)
    return processed, categories_str


def extract_last_modified(html: str) -> str:
    """
    Try to pull the 'Última modificação' timestamp out of the footer. Falls
    back to the current UTC timestamp if we can't find it.
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
    conn = sqlite3.connect(db_path)
    conn.executescript(SCHEMA_SQL)
    conn.commit()
    return conn


def existing_titles(conn: sqlite3.Connection) -> set[str]:
    cur = conn.execute("SELECT title FROM pages")
    return {row[0] for row in cur.fetchall()}


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
    cur = conn.execute("SELECT from_title FROM redirects")
    return {row[0] for row in cur.fetchall()}


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
    session = make_session(verify_ssl=not args.no_ssl_verify)
    conn = open_db(data_dir)

    already_done: set[str] = existing_titles(conn) if args.resume else set()
    already_redirected: set[str] = existing_redirects(conn) if args.resume else set()
    if args.resume:
        print(f"Resume mode: {len(already_done)} pages already in DB will be skipped.")

    titles = fetch_all_titles(session)
    if args.limit is not None:
        titles = titles[: args.limit]
        print(f"--limit set: processing first {len(titles)} title(s) only.")

    total = len(titles)
    asset_counter = [0]
    ok_count = 0
    fail_count = 0
    skip_count = 0
    start_ts = time.time()

    for idx, title in enumerate(titles, start=1):
        if title in already_done or title in already_redirected:
            skip_count += 1
            print(f"[{idx}/{total}] {title} ... (skip: already in DB)")
            continue

        print(f"[{idx}/{total}] {title} ...", end=" ", flush=True)
        try:
            html, status, source = fetch_page_html(session, title)
            if html is None:
                if status == 404:
                    # Check if the PT wiki knows this as a redirect page
                    target = fetch_redirect_target(session, title)
                    if target:
                        upsert_redirect(conn, title, target)
                        ok_count += 1
                        print(f"ok (redirect → {target})")
                        time.sleep(PAGE_SLEEP)
                        continue
                print(f"x (HTTP {status})")
                fail_count += 1
                time.sleep(PAGE_SLEEP)
                continue

            processed, categories = process_html(
                session,
                html,
                assets_dir,
                asset_counter,
                verbose_assets=args.verbose_assets,
            )
            last_modified = extract_last_modified(html)
            upsert_page(conn, title, processed, last_modified, categories)
            ok_count += 1
            label = f"ok (EN: {source[3:]})" if source.startswith("en:") else "ok"
            print(label)
        except Exception as e:  # noqa: BLE001 - dump script, keep going
            fail_count += 1
            print(f"x ({type(e).__name__}: {e})")

        time.sleep(PAGE_SLEEP)

    elapsed = time.time() - start_ts
    print()
    print("=" * 60)
    print(f"Done in {elapsed:.1f}s")
    print(f"  pages ok:      {ok_count}")
    print(f"  pages failed:  {fail_count}")
    print(f"  pages skipped: {skip_count}")
    print(f"  assets downloaded: {asset_counter[0]}")
    print(f"  db: {data_dir / 'stardewvalleydex.sqlite'}")
    print(f"  assets: {assets_dir}")

    conn.close()
    return 0 if fail_count == 0 else 1


if __name__ == "__main__":
    sys.exit(main())
