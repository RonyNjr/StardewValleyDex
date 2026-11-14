use std::path::Path;

use rusqlite::{params, Connection, OptionalExtension, Result};

/// Name of the SQLite file inside the app data directory.
pub const DB_FILE_NAME: &str = "stardewvalleydex.sqlite";

/// Compares a stored title with the `?1` parameter the way the app resolves
/// links: underscores equal spaces, surrounding double quotes are ignored.
/// Keep in sync with `wiki::title_key`.
const TITLE_MATCHES_PARAM: &str =
    "REPLACE(TRIM(title,'\"'),'_',' ') = REPLACE(TRIM(?1,'\"'),'_',' ')";

/// Initialize (or open) the SQLite database used to cache wiki pages.
///
/// The database file is created inside `app_data_dir`. All required
/// tables are created if they don't already exist.
pub fn init_db(app_data_dir: &Path) -> Result<Connection> {
    // Best-effort creation of the parent directory. Any real error will be
    // surfaced by `Connection::open` below.
    let _ = std::fs::create_dir_all(app_data_dir);

    let conn = Connection::open(app_data_dir.join(DB_FILE_NAME))?;
    create_schema(&conn)?;
    Ok(conn)
}

pub fn create_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        r#"
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
        CREATE TABLE IF NOT EXISTS sync_state (
            key   TEXT PRIMARY KEY,
            value TEXT NOT NULL
        );
        "#,
    )
}

/// Look up a page by title.
///
/// If `title` matches a redirect entry, the redirect is followed once
/// to resolve the destination page. Returns the HTML body if found.
pub fn get_page(conn: &Connection, title: &str) -> Option<String> {
    // First: try to resolve a redirect.
    let redirect_target: Option<String> = conn
        .query_row(
            "SELECT to_title FROM redirects WHERE REPLACE(from_title,'_',' ') = REPLACE(?1,'_',' ')",
            params![title],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .ok()
        .flatten();

    let effective_title = redirect_target.unwrap_or_else(|| title.to_string());

    conn.query_row(
        &format!("SELECT html FROM pages WHERE {TITLE_MATCHES_PARAM}"),
        params![effective_title],
        |row| row.get::<_, String>(0),
    )
    .optional()
    .ok()
    .flatten()
}

/// The stored title `get_page` would serve for `title`, if any. Lets the
/// sync update the row the app actually reads, even when it was stored with
/// a different spelling (quotes, underscores).
pub fn canonical_title(conn: &Connection, title: &str) -> Result<Option<String>> {
    conn.query_row(
        &format!("SELECT title FROM pages WHERE {TITLE_MATCHES_PARAM}"),
        params![title],
        |row| row.get(0),
    )
    .optional()
}

pub fn page_count(conn: &Connection) -> Result<i64> {
    conn.query_row("SELECT COUNT(*) FROM pages", [], |r| r.get(0))
}

/// Every title that already resolves locally: pages and redirect sources.
pub fn stored_titles(conn: &Connection) -> Result<Vec<String>> {
    let mut stmt =
        conn.prepare("SELECT title FROM pages UNION SELECT from_title FROM redirects ORDER BY 1")?;
    let titles = stmt.query_map([], |r| r.get(0))?.collect();
    titles
}

/// Insert or update a page.
pub fn upsert_page(
    conn: &Connection,
    title: &str,
    html: &str,
    last_modified: &str,
    categories: &str,
) -> Result<()> {
    conn.execute(
        r#"
        INSERT INTO pages (title, html, last_modified, categories)
        VALUES (?1, ?2, ?3, ?4)
        ON CONFLICT(title) DO UPDATE SET
            html = excluded.html,
            last_modified = excluded.last_modified,
            categories = excluded.categories
        "#,
        params![title, html, last_modified, categories],
    )?;
    Ok(())
}

/// Insert or update a redirect entry.
pub fn upsert_redirect(conn: &Connection, from_title: &str, to_title: &str) -> Result<()> {
    conn.execute(
        r#"
        INSERT INTO redirects (from_title, to_title)
        VALUES (?1, ?2)
        ON CONFLICT(from_title) DO UPDATE SET
            to_title = excluded.to_title
        "#,
        params![from_title, to_title],
    )?;
    Ok(())
}

/// Position in the wiki's recent-changes feed up to which the local pages
/// are known to be current.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncCursor {
    /// MediaWiki timestamp (`2026-11-14T12:00:00Z`).
    pub timestamp: String,
    /// Last change id already applied at `timestamp` (0 = none).
    pub rcid: i64,
}

const CURSOR_TIMESTAMP: &str = "rc_timestamp";
const CURSOR_RCID: &str = "rc_id";

pub fn get_sync_cursor(conn: &Connection) -> Result<Option<SyncCursor>> {
    let get = |key: &str| -> Result<Option<String>> {
        conn.query_row(
            "SELECT value FROM sync_state WHERE key = ?1",
            params![key],
            |r| r.get(0),
        )
        .optional()
    };
    Ok(get(CURSOR_TIMESTAMP)?.map(|timestamp| SyncCursor {
        timestamp,
        rcid: get(CURSOR_RCID)
            .ok()
            .flatten()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0),
    }))
}

pub fn set_sync_cursor(conn: &Connection, cursor: &SyncCursor) -> Result<()> {
    let mut stmt = conn.prepare(
        "INSERT INTO sync_state (key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
    )?;
    stmt.execute(params![CURSOR_TIMESTAMP, cursor.timestamp])?;
    stmt.execute(params![CURSOR_RCID, cursor.rcid.to_string()])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        create_schema(&conn).unwrap();
        conn
    }

    #[test]
    fn get_page_ignores_quotes_and_underscores() {
        let conn = conn();
        upsert_page(&conn, "\"Barco\"", "<p>barco</p>", "t", "").unwrap();
        upsert_page(&conn, "Mina de Cobre", "<p>mina</p>", "t", "").unwrap();

        assert_eq!(get_page(&conn, "Barco").as_deref(), Some("<p>barco</p>"));
        assert_eq!(
            get_page(&conn, "\"Barco\"").as_deref(),
            Some("<p>barco</p>")
        );
        assert_eq!(
            get_page(&conn, "Mina_de_Cobre").as_deref(),
            Some("<p>mina</p>")
        );
        assert_eq!(
            canonical_title(&conn, "Barco").unwrap().as_deref(),
            Some("\"Barco\"")
        );
        assert_eq!(get_page(&conn, "Nada"), None);
    }

    #[test]
    fn get_page_follows_redirects() {
        let conn = conn();
        upsert_page(&conn, "Novo", "<p>novo</p>", "t", "").unwrap();
        upsert_redirect(&conn, "Velho", "Novo").unwrap();
        assert_eq!(get_page(&conn, "Velho").as_deref(), Some("<p>novo</p>"));
        assert_eq!(stored_titles(&conn).unwrap(), vec!["Novo", "Velho"]);
    }

    #[test]
    fn sync_cursor_round_trip() {
        let conn = conn();
        assert_eq!(get_sync_cursor(&conn).unwrap(), None);
        let cursor = SyncCursor {
            timestamp: "2026-11-14T12:00:00Z".into(),
            rcid: 42,
        };
        set_sync_cursor(&conn, &cursor).unwrap();
        assert_eq!(get_sync_cursor(&conn).unwrap(), Some(cursor));
    }
}
