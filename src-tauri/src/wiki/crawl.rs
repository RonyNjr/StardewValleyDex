//! Link-graph closure used by `dump --crawl-missing`: find internal links in
//! stored pages whose target isn't in the database yet.

use std::collections::{BTreeSet, HashSet};

use rusqlite::{Connection, Result};

use super::{is_skipped_namespace, percent_decode, title_key};

const LINK_PREFIX: &str = "href=\"wiki://localhost/";

/// Titles linked from a stored page (`href="wiki://localhost/..."`).
pub fn link_targets(html: &str) -> impl Iterator<Item = String> + '_ {
    html.match_indices(LINK_PREFIX).filter_map(move |(i, _)| {
        let rest = &html[i + LINK_PREFIX.len()..];
        let value = &rest[..rest.find('"')?];
        let path = value.split(['#', '?']).next().unwrap_or("");
        let target = percent_decode(&decode_attr_entities(path))
            .replace('_', " ")
            .trim()
            .to_string();
        let script = target.starts_with("mediawiki/") || target.ends_with(".php");
        (!target.is_empty() && !script && !is_skipped_namespace(&target)).then_some(target)
    })
}

/// Link targets that are neither a stored page nor a known redirect, sorted.
pub fn find_missing_targets(conn: &Connection) -> Result<BTreeSet<String>> {
    let mut known = HashSet::new();
    let mut candidates = BTreeSet::new();

    let mut pages = conn.prepare("SELECT title, html FROM pages")?;
    let mut rows = pages.query([])?;
    while let Some(row) = rows.next()? {
        known.insert(title_key(row.get_ref(0)?.as_str()?));
        candidates.extend(link_targets(row.get_ref(1)?.as_str()?));
    }

    // Redirects resolve already; counting them as missing would refetch
    // them on every round.
    let mut redirects = conn.prepare("SELECT from_title FROM redirects")?;
    for from in redirects.query_map([], |r| r.get::<_, String>(0))? {
        known.insert(title_key(&from?));
    }

    Ok(candidates
        .into_iter()
        .filter(|t| !known.contains(&title_key(t)))
        .collect())
}

/// Undo the escaping the HTML serializer applies to attribute values.
fn decode_attr_entities(s: &str) -> String {
    s.replace("&quot;", "\"")
        .replace("&nbsp;", "\u{a0}")
        .replace("&amp;", "&")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;

    #[test]
    fn extracts_link_targets() {
        let html = concat!(
            r#"<a href="wiki://localhost/Mina_de_Cobre">a</a>"#,
            r#"<a href="wiki://localhost/Minera%C3%A7%C3%A3o#Andares">b</a>"#,
            r#"<a href="wiki://localhost/Peixe_&amp;_Fritas">c</a>"#,
            r#"<a href="wiki://localhost/Categoria:Peixes">skip</a>"#,
            r#"<a href="wiki://localhost/Talk:Mining">skip</a>"#,
            r#"<a href="wiki://localhost/mediawiki/index.php?title=X&amp;action=edit">skip</a>"#,
            r#"<a href="wiki://localhost/">skip</a>"#,
            r#"<img src="wiki://localhost/images/x.png">"#,
        );
        assert_eq!(
            link_targets(html).collect::<Vec<_>>(),
            vec!["Mina de Cobre", "Mineração", "Peixe & Fritas"]
        );
    }

    #[test]
    fn finds_only_targets_not_in_the_db() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        db::create_schema(&conn).unwrap();
        let links = r#"<a href="wiki://localhost/Mina_de_Cobre">x</a>
            <a href="wiki://localhost/%22Barco%22">x</a>
            <a href="wiki://localhost/Velho">x</a>
            <a href="wiki://localhost/Falta">x</a>"#;
        db::upsert_page(&conn, "Pesca", links, "t", "").unwrap();
        db::upsert_page(&conn, "Mina de Cobre", "", "t", "").unwrap();
        db::upsert_page(&conn, "Barco", "", "t", "").unwrap();
        db::upsert_redirect(&conn, "Velho", "Pesca").unwrap();

        let missing = find_missing_targets(&conn).unwrap();
        assert_eq!(missing.into_iter().collect::<Vec<_>>(), vec!["Falta"]);
    }
}
