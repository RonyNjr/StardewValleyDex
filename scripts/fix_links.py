#!/usr/bin/env python3
"""
Rewrite all host-relative links (/PageTitle) in stored HTML pages to
wiki://localhost/PageTitle so in-app navigation works when clicking links.

Run once: python3 scripts/fix_links.py
"""

import re
import sqlite3
from pathlib import Path

DB = (
    Path.home()
    / "Library"
    / "Application Support"
    / "com.stardewvalleydex.app"
    / "stardewvalleydex.sqlite"
)

ASSET_EXTS = {".png", ".jpg", ".jpeg", ".gif", ".svg", ".css", ".ttf", ".woff", ".woff2"}
SKIP_PREFIXES = (
    "wiki://", "http://", "https://", "//", "#", "mailto:", "javascript:",
)

# Namespaces that should NOT become wiki:// links (keep as-is / let not-found handle it)
# We actually DO want them to navigate so we can show not-found gracefully.
# Only skip pure fragment anchors.

HREF_RE = re.compile(r'href="(/[^"]*)"', re.IGNORECASE)


def should_rewrite(href: str) -> bool:
    if any(href.startswith(p) for p in SKIP_PREFIXES):
        return False
    # Has a file extension that's an asset
    path = href.split("?")[0].split("#")[0]
    for ext in ASSET_EXTS:
        if path.lower().endswith(ext):
            return False
    return True


def rewrite_html(html: str) -> tuple[str, int]:
    count = 0

    def replace(m: re.Match) -> str:
        nonlocal count
        href = m.group(1)
        if not should_rewrite(href):
            return m.group(0)
        count += 1
        return f'href="wiki://localhost{href}"'

    new_html = HREF_RE.sub(replace, html)
    return new_html, count


def main() -> None:
    conn = sqlite3.connect(DB, timeout=30)
    cur = conn.execute("SELECT title, html FROM pages")
    rows = cur.fetchall()

    total_pages = 0
    total_links = 0

    updates = []
    for title, html in rows:
        new_html, count = rewrite_html(html)
        if count > 0:
            updates.append((new_html, title))
            total_pages += 1
            total_links += count

    if updates:
        conn.executemany("UPDATE pages SET html = ? WHERE title = ?", updates)
        conn.commit()

    conn.close()
    print(f"Fixed {total_links} links across {total_pages} pages.")


if __name__ == "__main__":
    main()
