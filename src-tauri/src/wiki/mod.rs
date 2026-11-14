//! Wiki logic shared by the initial dump (`examples/dump.rs`) and the
//! background sync (`sync.rs`): HTTP access to the MediaWiki instance, HTML
//! clean-up / link rewriting, and asset download. Keep a single
//! implementation here; neither caller should grow its own copy.

pub mod assets;
pub mod client;
pub mod crawl;
pub mod html;

#[cfg(test)]
pub(crate) mod test_server;

use std::time::{SystemTime, UNIX_EPOCH};

use percent_encoding::{percent_decode_str, utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};

pub use client::{PageOutcome, PageSource, WikiClient, WikiConfig};

pub type Error = Box<dyn std::error::Error + Send + Sync>;

/// Portuguese wiki: the primary source.
pub const PT_BASE: &str = "https://pt.stardewvalleywiki.com";
/// English wiki: fallback for pages that don't exist in Portuguese.
pub const EN_BASE: &str = "https://stardewvalleywiki.com";
/// MediaWiki lives under `/mediawiki/` on these hosts. The root `/api.php`
/// answers with the wiki's HTML 404 page, not the API.
pub const API_PATH: &str = "/mediawiki/api.php";
pub const USER_AGENT: &str = "StardewValleyDex/0.1 (offline-wiki-client)";

/// MediaWiki namespaces we never crawl or follow as internal links.
pub const SKIP_NS: &[&str] = &[
    "Especial",
    "Special",
    "Ficheiro",
    "File",
    "Ajuda",
    "Help",
    "Predefinição",
    "Template",
    "Categoria",
    "Category",
    "Módulo",
    "Module",
    "MediaWiki",
    "Usuário",
    "Usuária",
    "Utilizador",
    "Utilizadora",
    "User",
    "Discussão",
    "Talk",
    "Stardew Valley Wiki",
];

/// Characters left unescaped in page URLs (same set as the old Python
/// `quote(..., safe="/:@!$&'()*+,;=.-_~")`).
const SLUG_ESCAPE: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'/')
    .remove(b':')
    .remove(b'@')
    .remove(b'!')
    .remove(b'$')
    .remove(b'&')
    .remove(b'\'')
    .remove(b'(')
    .remove(b')')
    .remove(b'*')
    .remove(b'+')
    .remove(b',')
    .remove(b';')
    .remove(b'=')
    .remove(b'.')
    .remove(b'-')
    .remove(b'_')
    .remove(b'~');

/// URL path segment for a title: `Mina de Cobre` -> `Mina_de_Cobre`.
pub fn title_to_slug(title: &str) -> String {
    utf8_percent_encode(&title.replace(' ', "_"), SLUG_ESCAPE).to_string()
}

pub fn percent_decode(s: &str) -> String {
    percent_decode_str(s).decode_utf8_lossy().into_owned()
}

/// Normalised form used to decide whether two titles name the same page:
/// underscores as spaces, surrounding whitespace and double quotes removed.
/// Mirrors the SQL used by `db::get_page`.
pub fn title_key(title: &str) -> String {
    title.replace('_', " ").trim().trim_matches('"').to_string()
}

pub fn is_skipped_namespace(title: &str) -> bool {
    let Some((ns, _)) = title.split_once(':') else {
        return false;
    };
    // Talk namespaces ("User talk", "Utilizador Discussão", ...) are never content.
    SKIP_NS.contains(&ns) || ns.ends_with(" talk") || ns.ends_with(" Discussão")
}

pub fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// `YYYY-MM-DDTHH:MM:SSZ`, the timestamp format MediaWiki uses.
pub fn format_utc(unix: i64) -> String {
    let days = unix.div_euclid(86_400);
    let secs = unix.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        secs / 3600,
        (secs % 3600) / 60,
        secs % 60
    )
}

pub fn utc_now() -> String {
    format_utc(now_unix())
}

/// Days since 1970-01-01 -> (year, month, day). Howard Hinnant's algorithm.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_matches_python_quote() {
        assert_eq!(title_to_slug("Mina de Cobre"), "Mina_de_Cobre");
        assert_eq!(title_to_slug("Mineração"), "Minera%C3%A7%C3%A3o");
        assert_eq!(title_to_slug("\"Barco\""), "%22Barco%22");
        assert_eq!(title_to_slug("A/B (c)"), "A/B_(c)");
    }

    #[test]
    fn title_key_ignores_underscores_and_quotes() {
        assert_eq!(title_key("\"Barco\""), "Barco");
        assert_eq!(title_key("Mina_de_Cobre"), "Mina de Cobre");
        assert_eq!(title_key("'O Milagre'"), "'O Milagre'");
    }

    #[test]
    fn skipped_namespaces() {
        assert!(is_skipped_namespace("Ficheiro:Foo.png"));
        assert!(is_skipped_namespace("Categoria:Peixes"));
        assert!(is_skipped_namespace("Talk:Mining"));
        assert!(is_skipped_namespace("User talk:Alguém"));
        assert!(is_skipped_namespace("Modding talk:Index"));
        assert!(is_skipped_namespace("Utilizadora Discussão:Alguém"));
        assert!(is_skipped_namespace("Stardew Valley Wiki:Sobre"));
        assert!(!is_skipped_namespace("Modding:Index"));
        assert!(!is_skipped_namespace("Warp Totem: Beach"));
        assert!(!is_skipped_namespace("Filetes"));
        assert!(!is_skipped_namespace("Mineração"));
    }

    #[test]
    fn utc_formatting() {
        assert_eq!(format_utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(format_utc(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(format_utc(1_000_000_000), "2001-09-09T01:46:40Z");
        assert_eq!(format_utc(1_798_761_599), "2026-12-31T23:59:59Z");
    }
}
