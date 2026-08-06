#!/usr/bin/env python3
"""
One-shot fix for missing pages:
  1. Fetches Página_Principal from PT wiki.
  2. For each of the 50 known-failed titles, tries EN wiki, then creates a stub.
  3. Fixes existing_titles quote mismatch in the DB (renames quoted titles).

Run once:  python3 scripts/fix_missing.py --no-ssl-verify
"""

from __future__ import annotations

import argparse
import sqlite3
import time
from pathlib import Path
from typing import Optional
from urllib.parse import quote

import requests
from bs4 import BeautifulSoup

WIKI_PT = "https://pt.stardewvalleywiki.com"
WIKI_EN = "https://stardewvalleywiki.com"
USER_AGENT = "StardewValleyDex/0.1 (offline-wiki-dump; educational)"

DB_PATH = (
    Path.home()
    / "Library"
    / "Application Support"
    / "com.stardewvalleydex.app"
    / "stardewvalleydex.sqlite"
)

# The 50 pages that consistently fail on the PT wiki.
# These are mostly OST tracks and in-game paintings.
MISSING = [
    "Arvorezinha", "Barco", "Básicos da Física", "Caminhos", "Cidade Azul",
    "Colinas de Jade", "Colinas de Jade Extendidas", "Grama dançante",
    "Mestrepipa 95", "Mirtilos", "Movimento Primordial", "O Muzamaro",
    "Oferta Queimada", "Paisagem", "Paraíso VGA", "Pináculos",
    "Rainha do Mar de Joias", "Rodovia 89", "Sol nº 44", "Sol nº 45",
    "Sonhos Congelados", "Uma Noite em Ecolina", "Vila de Baunilha",
    "1000 Anos no Futuro", "A Serpente", "Abstrato", "Binário",
    "Damas", "Descolado", "Espaçonave",
    "Jornada do Rei da Pradaria: O Filme",
    "Maravilhas Naturais: Explorando Nosso Mundo Vibrante",
    "Mysterium", "Nuvens", "OVNI", "O Expresso da Cidade Zuzu",
    "O Milagre no Rancho Coldstar", "O Milagre no Rancho Estrela-fria",
    "O Pequeno Broto", "O Uivo na Chuva", "Peixe Tropical 173",
    "Pequena Lula", "Pequenos Amigos", "Redemoinhos Nulos", "Reino do Sol",
    "Retrato de uma Sereia", "Runas", "Terra do Barro", "Torre do Feiticeiro",
    "Três Árvores", "Wumbus", "Águia Vermelha",
]

STRIP_SELECTORS = ["#mw-head", "#mw-head-base", "#mw-page-base"]


def make_session(verify: bool = True) -> requests.Session:
    s = requests.Session()
    s.headers["User-Agent"] = USER_AGENT
    s.verify = verify
    return s


def fetch(session: requests.Session, title: str, base: str) -> Optional[str]:
    slug = quote(title.replace(" ", "_"), safe="/:@!$&'()*+,;=.-_~")
    resp = session.get(f"{base}/{slug}", timeout=30)
    return resp.text if resp.status_code == 200 else None


def process(session: requests.Session, html: str, base: str) -> str:
    """Strip chrome, rewrite links/assets to wiki:// scheme."""
    from urllib.parse import urlparse, unquote as _unq
    import hashlib

    assets_dir = DB_PATH.parent / "assets"
    soup = BeautifulSoup(html, "lxml")

    for sel in STRIP_SELECTORS:
        for el in soup.select(sel):
            el.decompose()
    for tag in soup.find_all("script"):
        tag.decompose()

    for a in soup.find_all("a", href=True):
        href = a["href"]
        if href.startswith("/wiki/"):
            a["href"] = "wiki://localhost/" + href[len("/wiki/"):]

    for img in soup.find_all("img"):
        img.attrs.pop("srcset", None)
        src = img.get("src", "")
        if src.startswith("data:") or not src:
            continue
        if src.startswith("/") or src.startswith("//"):
            abs_url = ("https:" + src) if src.startswith("//") else (base + src)
            parsed = urlparse(abs_url)
            rel = _unq(parsed.path).lstrip("/")
            local = assets_dir / rel
            if not local.exists():
                try:
                    r = session.get(abs_url, timeout=20, stream=True)
                    if r.status_code == 200:
                        local.parent.mkdir(parents=True, exist_ok=True)
                        with open(local, "wb") as fh:
                            for chunk in r.iter_content(65536):
                                fh.write(chunk)
                except Exception:
                    pass
            if local.exists():
                img["src"] = f"wiki://localhost/{rel}"

    for link in soup.find_all("link", rel=True):
        rels = link["rel"] if isinstance(link["rel"], list) else [link["rel"]]
        if "stylesheet" not in rels:
            continue
        href = link.get("href", "")
        if not (href.startswith("/") or href.startswith("//")):
            continue
        abs_url = ("https:" + href) if href.startswith("//") else (base + href)
        digest = hashlib.md5(abs_url.encode()).hexdigest()[:12]
        rel = f"css/{digest}.css"
        local = assets_dir / rel
        if not local.exists():
            try:
                r = session.get(abs_url, timeout=20)
                if r.status_code == 200:
                    local.parent.mkdir(parents=True, exist_ok=True)
                    local.write_text(r.text, encoding="utf-8")
            except Exception:
                pass
        if local.exists():
            link["href"] = f"wiki://localhost/{rel}"

    return str(soup)


def stub_page(title: str) -> str:
    """Create a minimal wiki-like HTML stub for a page with no content."""
    return f"""<!DOCTYPE html>
<html lang="pt-BR">
<head><meta charset="UTF-8"><title>{title} - StardewValleyDex</title></head>
<body>
<div id="content" class="mw-body">
  <h1 id="firstHeading">{title}</h1>
  <div id="mw-content-text">
    <p>Esta página ainda não possui conteúdo traduzido no wiki em português.</p>
    <p>Para informações completas, consulte o
    <a href="https://stardewvalleywiki.com/{quote(title.replace(' ','_'))}">wiki oficial em inglês</a>.
    </p>
  </div>
</div>
</body>
</html>"""


def upsert_page(conn: sqlite3.Connection, title: str, html: str) -> None:
    import time as _time
    ts = _time.strftime("%Y-%m-%dT%H:%M:%SZ", _time.gmtime())
    conn.execute(
        """INSERT INTO pages (title, html, last_modified, categories)
           VALUES (?, ?, ?, '')
           ON CONFLICT(title) DO UPDATE SET
               html = excluded.html,
               last_modified = excluded.last_modified""",
        (title, html, ts),
    )


def fix_quoted_titles(conn: sqlite3.Connection) -> int:
    """Rename rows whose title is stored with surrounding quote chars."""
    cur = conn.execute("SELECT title FROM pages")
    rows = cur.fetchall()
    fixed = 0
    for (raw,) in rows:
        clean = raw.strip('"').strip("'")
        if clean != raw:
            exists = conn.execute(
                "SELECT 1 FROM pages WHERE title = ?", (clean,)
            ).fetchone()
            if not exists:
                conn.execute(
                    "UPDATE pages SET title = ? WHERE title = ?", (clean, raw)
                )
                fixed += 1
            else:
                conn.execute("DELETE FROM pages WHERE title = ?", (raw,))
                fixed += 1
    return fixed


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--no-ssl-verify", action="store_true")
    args = ap.parse_args()

    if args.no_ssl_verify:
        import urllib3
        urllib3.disable_warnings()

    session = make_session(verify=not args.no_ssl_verify)
    conn = sqlite3.connect(DB_PATH)

    # 1. Fix quoted titles
    fixed = fix_quoted_titles(conn)
    conn.commit()
    print(f"Fixed {fixed} quoted title(s) in DB.")

    # 2. Fetch Página_Principal — try Stardew_Valley_Wiki (actual main page on PT wiki)
    print("\nFetching Página_Principal...")
    home_html = None
    for candidate in ["Stardew_Valley_Wiki", "Página_Principal"]:
        home_html = fetch(session, candidate, WIKI_PT)
        if home_html and "no text" not in home_html and len(home_html) > 2000:
            processed = process(session, home_html, WIKI_PT)
            upsert_page(conn, "Página_Principal", processed)
            conn.commit()
            print(f"  ok (PT wiki: {candidate})")
            break
    else:
        print("  not found on PT wiki — creating stub")
        upsert_page(conn, "Página_Principal", stub_page("Página Principal"))
        conn.commit()

    # 3. Handle the 50 known-missing pages
    print(f"\nProcessing {len(MISSING)} known-missing pages...")
    ok_en, ok_stub, skipped = 0, 0, 0

    existing = {
        row[0].strip('"')
        for row in conn.execute("SELECT title FROM pages").fetchall()
    }

    for title in MISSING:
        if title in existing:
            skipped += 1
            print(f"  {title} ... skip (already in DB)")
            continue

        print(f"  {title} ...", end=" ", flush=True)
        html = fetch(session, title, WIKI_EN)
        if html:
            processed = process(session, html, WIKI_EN)
            upsert_page(conn, title, processed)
            conn.commit()
            ok_en += 1
            print("ok (EN wiki)")
        else:
            upsert_page(conn, title, stub_page(title))
            conn.commit()
            ok_stub += 1
            print("ok (stub)")
        time.sleep(0.3)

    print(f"\nDone.")
    print(f"  EN wiki pages: {ok_en}")
    print(f"  Stub pages:    {ok_stub}")
    print(f"  Skipped:       {skipped}")
    conn.close()


if __name__ == "__main__":
    main()
