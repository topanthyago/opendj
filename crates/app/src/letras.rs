//! Letras' public search, and original plain lyrics from a selected song page.
//! Search returns metadata only. Page text is requested after a user selects
//! a result. No JavaScript, login, translation or timing inference is used.

use std::time::Duration;

use opendj_plugins::{encode, USER_AGENT};
use serde_json::Value;

use crate::lrclib::{Found, Query};

const SEARCH: &str = "https://solr.sscdn.co/letras/m1/";

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Error {
    Offline,
    Blocked,
    Layout,
}

impl Error {
    pub fn message(self) -> &'static str {
        match self {
            Self::Offline => "Couldn't reach Letras. Check the connection and try again.",
            Self::Blocked => "Letras refused access to this page. Open it on the site.",
            Self::Layout => "Letras returned no readable original lyrics. Open it on the site.",
        }
    }
}

fn get(url: &str) -> Result<String, Error> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(12)))
        .user_agent(USER_AGENT)
        // Follow redirects ourselves so a page stays on the allowed hosts.
        .max_redirects(0)
        .build()
        .into();
    let mut url = url.to_string();
    for _ in 0..4 {
        match agent.get(&url).call() {
            Ok(mut response) => {
                if response.status().is_redirection() {
                    let next = response
                        .headers()
                        .get("location")
                        .and_then(|h| h.to_str().ok())
                        .ok_or(Error::Layout)?;
                    let next = if next.starts_with('/') && !next.starts_with("//") {
                        let host = url.split('/').nth(2).ok_or(Error::Layout)?;
                        format!("https://{host}{next}")
                    } else {
                        next.to_string()
                    };
                    if !valid_url(&next) {
                        return Err(Error::Layout);
                    }
                    url = next;
                    continue;
                }
                return response
                    .body_mut()
                    .with_config()
                    .limit(2 << 20)
                    .read_to_string()
                    .map_err(|_| Error::Layout);
            }
            Err(ureq::Error::StatusCode(403 | 429)) => return Err(Error::Blocked),
            Err(ureq::Error::StatusCode(404)) => return Err(Error::Layout),
            Err(_) => return Err(Error::Offline),
        }
    }
    Err(Error::Layout)
}

/// Search by title, then prefer the requested artist in the public results.
/// Combining both names in this endpoint can discard the original recording.
pub fn find(q: &Query) -> Result<Vec<Found>, Error> {
    let mut found = records(&get(&format!(
        "{SEARCH}?q={}&wt=json",
        encode(q.title.trim())
    ))?)?;
    if !q.artist.trim().is_empty() {
        let artist = q.artist.trim().to_lowercase();
        found.sort_by_key(|f| f.artist.trim().to_lowercase() != artist);
    }
    Ok(found)
}

fn records(body: &str) -> Result<Vec<Found>, Error> {
    let body = body.trim();
    // The public endpoint currently sends LetrasSug(JSON), even with wt=json.
    // Parse the fixed envelope as data, never evaluate JSONP.
    let json = body
        .strip_prefix("LetrasSug(")
        .and_then(|s| s.strip_suffix(");").or_else(|| s.strip_suffix(')')))
        .unwrap_or(body);
    let value: Value = serde_json::from_str(json).map_err(|_| Error::Layout)?;
    let docs = value["response"]["docs"].as_array().ok_or(Error::Layout)?;
    let mut found = Vec::new();
    for doc in docs {
        if doc["t"].as_str() != Some("2") {
            continue;
        }
        let (Some(artist), Some(title), Some(dns), Some(slug)) = (
            doc["art"].as_str(),
            doc["txt"].as_str(),
            doc["dns"].as_str(),
            doc["url"].as_str(),
        ) else {
            continue;
        };
        let url = format!("https://www.letras.mus.br/{dns}/{slug}/");
        if artist.trim().is_empty() || title.trim().is_empty() || !valid_url(&url) {
            continue;
        }
        if found.iter().any(|f: &Found| f.url == url) {
            continue;
        }
        found.push(Found {
            source: "Letras",
            url,
            title: decode_entities(title),
            artist: decode_entities(artist),
            album: String::new(),
            duration: 0.0,
            instrumental: false,
            synced: String::new(),
            plain: String::new(),
        });
        if found.len() == 12 {
            break;
        }
    }
    Ok(found)
}

/// Only HTTPS song paths on Letras' own hosts can be fetched or opened.
pub fn valid_url(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("https://") else {
        return false;
    };
    let Some((host, path)) = rest.split_once('/') else {
        return false;
    };
    if !matches!(
        host,
        "www.letras.mus.br" | "letras.mus.br" | "www.letras.com" | "letras.com"
    ) {
        return false;
    }
    let parts: Vec<_> = path.trim_end_matches('/').split('/').collect();
    parts.len() == 2
        && parts.iter().all(|part| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        })
}

pub fn lyrics(url: &str) -> Result<String, Error> {
    if !valid_url(url) {
        return Err(Error::Layout);
    }
    parse_html(&get(url)?)
}

/// Read only a recognized original-lyrics container. Translations, ads,
/// scripts and surrounding page text never become imported lyrics.
fn parse_html(html: &str) -> Result<String, Error> {
    let mut rest = html;
    while let Some(start) = rest.find('<') {
        rest = &rest[start..];
        if rest.starts_with("<!--") {
            let end = rest.find("-->").ok_or(Error::Layout)?;
            rest = &rest[end + 3..];
            continue;
        }
        let Some(end) = tag_end(rest) else {
            break;
        };
        let tag = &rest[1..end];
        rest = &rest[end + 1..];
        let name = tag.split_whitespace().next().unwrap_or_default();
        if name.eq_ignore_ascii_case("script") || name.eq_ignore_ascii_case("style") {
            let close = format!("</{}", name.to_ascii_lowercase());
            let end = rest
                .to_ascii_lowercase()
                .find(&close)
                .ok_or(Error::Layout)?;
            rest = &rest[end..];
            continue;
        }
        if !name.eq_ignore_ascii_case("div") {
            continue;
        }
        let class = attribute(tag, "class").unwrap_or_default();
        if !class
            .split_whitespace()
            .any(|c| matches!(c, "lyric-original" | "cnt-letra"))
        {
            continue;
        }
        return container_text(rest);
    }
    Err(Error::Layout)
}

// A quote-aware boundary also handles '>' inside attributes.
fn tag_end(tag: &str) -> Option<usize> {
    let mut quote = None;
    for (i, c) in tag.char_indices().skip(1) {
        match (quote, c) {
            (Some(q), c) if q == c => quote = None,
            (None, '\'' | '"') => quote = Some(c),
            (None, '>') => return Some(i),
            _ => {}
        }
    }
    None
}

fn attribute<'a>(tag: &'a str, key: &str) -> Option<&'a str> {
    let mut rest = tag.split_once(char::is_whitespace)?.1.trim_start();
    while !rest.is_empty() {
        let end = rest
            .find(|c: char| c.is_whitespace() || c == '=')
            .unwrap_or(rest.len());
        let name = &rest[..end];
        rest = rest[end..].trim_start();
        if !rest.starts_with('=') {
            if rest.is_empty() {
                break;
            }
            continue;
        }
        rest = rest[1..].trim_start();
        let (value, tail) = if rest.starts_with(['\'', '"']) {
            let quote = rest.chars().next()?;
            let end = rest[1..].find(quote)? + 1;
            (&rest[1..end], &rest[end + 1..])
        } else {
            let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
            (&rest[..end], &rest[end..])
        };
        if name.eq_ignore_ascii_case(key) {
            return Some(value);
        }
        rest = tail.trim_start();
    }
    None
}

fn container_text(mut rest: &str) -> Result<String, Error> {
    let mut depth = 1usize;
    let mut text = String::new();
    while let Some(start) = rest.find('<') {
        // HTML source wrapping is whitespace. Only br/p/div create lines.
        text.extend(
            rest[..start]
                .chars()
                .map(|c| if c.is_whitespace() { ' ' } else { c }),
        );
        rest = &rest[start..];
        let end = tag_end(rest).ok_or(Error::Layout)?;
        let tag = rest[1..end].trim();
        rest = &rest[end + 1..];
        if tag.starts_with("!--") {
            if !tag.ends_with("--") {
                let end = rest.find("-->").ok_or(Error::Layout)?;
                rest = &rest[end + 3..];
            }
            continue;
        }
        let name = tag
            .split_whitespace()
            .next()
            .unwrap_or_default()
            .trim_end_matches('/')
            .to_ascii_lowercase();
        match name.as_str() {
            "div" => depth += 1,
            "/div" => {
                depth -= 1;
                if depth == 0 {
                    let decoded = decode_entities(&text);
                    let mut lines = Vec::new();
                    for line in decoded.lines() {
                        let line = line.split_whitespace().collect::<Vec<_>>().join(" ");
                        if !line.is_empty() || lines.last().is_some_and(|s: &String| !s.is_empty())
                        {
                            lines.push(line);
                        }
                    }
                    let text = lines.join("\n").trim().to_string();
                    return if text.is_empty() {
                        Err(Error::Layout)
                    } else {
                        Ok(text)
                    };
                }
                text.push('\n');
            }
            "br" => text.push('\n'),
            "/p" => text.push_str("\n\n"),
            "script" | "style" | "iframe" => {
                let close = format!("</{name}");
                let end = rest
                    .to_ascii_lowercase()
                    .find(&close)
                    .ok_or(Error::Layout)?;
                rest = &rest[end..];
            }
            _ => {}
        }
    }
    Err(Error::Layout)
}

fn decode_entities(text: &str) -> String {
    let mut decoded = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find('&') {
        decoded.push_str(&rest[..start]);
        rest = &rest[start..];
        let Some(end) = rest.find(';').filter(|&i| i <= 12) else {
            decoded.push('&');
            rest = &rest[1..];
            continue;
        };
        let entity = &rest[1..end];
        let value = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" | "#39" => Some('\''),
            "nbsp" => Some(' '),
            "aacute" => Some('á'),
            "Aacute" => Some('Á'),
            "agrave" => Some('à'),
            "Agrave" => Some('À'),
            "acirc" => Some('â'),
            "Acirc" => Some('Â'),
            "atilde" => Some('ã'),
            "Atilde" => Some('Ã'),
            "auml" => Some('ä'),
            "Auml" => Some('Ä'),
            "eacute" => Some('é'),
            "Eacute" => Some('É'),
            "egrave" => Some('è'),
            "Egrave" => Some('È'),
            "ecirc" => Some('ê'),
            "Ecirc" => Some('Ê'),
            "iacute" => Some('í'),
            "Iacute" => Some('Í'),
            "oacute" => Some('ó'),
            "Oacute" => Some('Ó'),
            "ocirc" => Some('ô'),
            "Ocirc" => Some('Ô'),
            "otilde" => Some('õ'),
            "Otilde" => Some('Õ'),
            "ouml" => Some('ö'),
            "Ouml" => Some('Ö'),
            "uacute" => Some('ú'),
            "Uacute" => Some('Ú'),
            "uuml" => Some('ü'),
            "Uuml" => Some('Ü'),
            "ccedil" => Some('ç'),
            "Ccedil" => Some('Ç'),
            "ntilde" => Some('ñ'),
            "Ntilde" => Some('Ñ'),
            "iexcl" => Some('¡'),
            "iquest" => Some('¿'),
            "rsquo" => Some('’'),
            "lsquo" => Some('‘'),
            "rdquo" => Some('”'),
            "ldquo" => Some('“'),
            "ndash" => Some('–'),
            "mdash" => Some('—'),
            "hellip" => Some('…'),
            _ => entity
                .strip_prefix("#x")
                .or_else(|| entity.strip_prefix("#X"))
                .and_then(|v| u32::from_str_radix(v, 16).ok())
                .and_then(char::from_u32)
                .or_else(|| {
                    entity
                        .strip_prefix('#')
                        .and_then(|v| v.parse::<u32>().ok())
                        .and_then(char::from_u32)
                }),
        };
        if let Some(c) = value {
            decoded.push(c);
        } else {
            decoded.push_str(&rest[..=end]);
        }
        rest = &rest[end + 1..];
    }
    decoded.push_str(rest);
    decoded
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_is_metadata_and_filters_other_content() {
        let fixture = r#"LetrasSug({"response":{"docs":[
          {"t":"2","art":"Árvore &amp; Mar","txt":"Canção","dns":"arvore-e-mar","url":"123"},
          {"t":"1","art":"Artist","txt":"Artist","dns":"artist","url":"123"},
          {"t":"2","art":"Bad","txt":"Bad","dns":"evil.test/../../","url":"123"}
        ]}})"#;
        let found = records(fixture).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].artist, "Árvore & Mar");
        assert!(found[0].text().is_empty());
        assert!(records("alert(1)").is_err());
    }

    #[test]
    fn extracts_original_text_preserving_lines_and_accents() {
        let fixture = include_str!("../tests/fixtures/letras-original.html");
        assert_eq!(
            parse_html(fixture).unwrap(),
            "Canção & luar\nCoração <aberto>\n\nOutra voz ’ aqui"
        );
        assert_eq!(parse_html("<html>Access denied</html>"), Err(Error::Layout));
        assert_eq!(
            parse_html("<div class='lyric-original'>truncated"),
            Err(Error::Layout)
        );
        assert_eq!(
            parse_html("<div class='lyric-original'><p>One\nwrapped <span>line</span></p></div>")
                .unwrap(),
            "One wrapped line"
        );
    }

    #[test]
    fn source_links_stay_on_letras_song_pages() {
        assert!(valid_url("https://www.letras.mus.br/artist/song/"));
        assert!(valid_url("https://www.letras.com/artist/123/"));
        for url in [
            "http://www.letras.com/a/b/",
            "https://www.letras.com.evil/a/b/",
            "https://user@www.letras.com/a/b/",
            "https://www.letras.com/a/../",
            "file:///tmp/a",
            "https://www.letras.com/a/b/?url=evil",
            "https://www.letras.com/a%2fb/c/",
        ] {
            assert!(!valid_url(url), "{url}");
        }
    }
}
