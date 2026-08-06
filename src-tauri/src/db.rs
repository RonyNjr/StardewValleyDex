use std::path::Path;

use rusqlite::{params, Connection, OptionalExtension, Result};

/// Initialize (or open) the SQLite database used to cache wiki pages.
///
/// The database file is created inside `app_data_dir` (the caller is
/// responsible for making sure the directory exists). All required
/// tables are created if they don't already exist.
pub fn init_db(app_data_dir: &Path) -> Result<Connection> {
    // Best-effort creation of the parent directory. Any real error will be
    // surfaced by `Connection::open` below.
    let _ = std::fs::create_dir_all(app_data_dir);

    let db_path = app_data_dir.join("stardewvalleydex.sqlite");
    let conn = Connection::open(&db_path)?;

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
        "#,
    )?;

    Ok(conn)
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
        "SELECT html FROM pages WHERE REPLACE(TRIM(title,'\"'),'_',' ') = REPLACE(?1,'_',' ')",
        params![effective_title],
        |row| row.get::<_, String>(0),
    )
    .optional()
    .ok()
    .flatten()
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
#[allow(dead_code)]
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
