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


def make_session() -> requests.Session:
    session = requests.Session()
    session.headers.update({"User-Agent": USER_AGENT})
    return session


def fetch_all_titles(session: requests.Session) -> list[str]:
    """
    Enumerate every page title in the main namespace via
    ?action=query&list=allpages, following `continue` tokens until done.
    """
    titles: list[str] = []
    params: dict[str, str] = {
        "action": "query",
        "list": "allpages",
        "aplimit": "500",
        "format": "json",
    }

    print("Listing pages via MediaWiki API...")
    while True:
        resp = session.get(API_URL, params=params, timeout=30)
        resp.raise_for_status()
        data = resp.json()

        for page in data.get("query", {}).get("allpages", []):
            titles.append(page["title"])

        cont = data.get("continue")
        if not cont:
            break

        # Merge every continue key back into the params (usually apcontinue).
        for key, value in cont.items():
            params[key] = value

        time.sleep(API_SLEEP)

    print(f"  found {len(titles)} pages")
    return titles


def fetch_page_html(session: requests.Session, title: str) -> Optional[str]:
    """Fetch the fully-rendered HTML for a single wiki page."""
    # MediaWiki accepts spaces or underscores; use underscores for the URL.
    url = f"{WIKI_BASE}/{title.replace(' ', '_')}"
    resp = session.get(url, timeout=30)
    if resp.status_code != 200:
        return None
    return resp.text


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
    return parser.parse_args(list(argv) if argv is not None else None)


def main(argv: Optional[Iterable[str]] = None) -> int:
    args = parse_args(argv)

    data_dir: Path = args.data_dir
    assets_dir = data_dir / "assets"
    assets_dir.mkdir(parents=True, exist_ok=True)

    print(f"Data directory: {data_dir}")
    print(f"Assets directory: {assets_dir}")

    session = make_session()
    conn = open_db(data_dir)

    already_done: set[str] = existing_titles(conn) if args.resume else set()
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
        if title in already_done:
            skip_count += 1
            print(f"[{idx}/{total}] {title} ... (skip: already in DB)")
            continue

        print(f"[{idx}/{total}] {title} ...", end=" ", flush=True)
        try:
            html = fetch_page_html(session, title)
            if html is None:
                print("x (HTTP error)")
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
            print("ok")
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
