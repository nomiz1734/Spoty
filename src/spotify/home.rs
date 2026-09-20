//! Spotify's home feed ("Dành cho bạn"): the same `home` GraphQL query the
//! web player runs on api-partner.spotify.com (pathfinder), authenticated with
//! the session's Login5 token and client token.
//!
//! The query is a persisted query identified by a hash that Spotify rotates when
//! the web player is updated. We ship the current hash, remember the last one
//! that worked, and when it stops working we read the new one from the web
//! player's JavaScript.

use std::path::Path;

use bytes::Bytes;
use hyper::Method;
use librespot_core::Session;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::gfx::text::clean;
use crate::net;

/// `home` persisted-query hash from the web player (2026-09).
const DEFAULT_HASH: &str = "76243c78b0e20ecdbe41b794dec8cbe73f75e585b0a7201b8d2e84578412847a";
const PATHFINDER: &str = "https://api-partner.spotify.com/pathfinder/v2/query";
const BROWSER_UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
     (KHTML, like Gecko) Chrome/140.0.0.0 Safari/537.36";
const MAX_SECTIONS: usize = 14;
const MAX_ITEMS: usize = 20;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum FeedKind {
    Playlist,
    Album,
    Artist,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FeedItem {
    pub uri: String,
    pub kind: FeedKind,
    pub title: String,
    pub subtitle: String,
    pub image: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FeedSection {
    pub title: String,
    pub items: Vec<FeedItem>,
}

/// A request with a browser user agent: Spotify's web endpoints reject the rest.
async fn fetch(
    method: Method,
    url: &str,
    headers: &[(&str, String)],
    body: Option<Vec<u8>>,
) -> Result<(u16, Bytes), String> {
    let mut all = vec![("user-agent", BROWSER_UA.to_string())];
    all.extend(headers.iter().map(|(k, v)| (*k, v.clone())));
    net::fetch(method, url, &all, body, net::CALL_TIMEOUT).await
}

fn time_zone(configured: &str) -> String {
    if !configured.trim().is_empty() {
        return configured.trim().to_string();
    }
    if let Ok(tz) = std::env::var("TZ") {
        if tz.contains('/') {
            return tz.trim_start_matches(':').to_string();
        }
    }
    if let Ok(tz) = std::fs::read_to_string("/etc/timezone") {
        if tz.trim().contains('/') {
            return tz.trim().to_string();
        }
    }
    "Asia/Ho_Chi_Minh".into()
}

async fn query_home(session: &Session, hash: &str, tz: &str) -> Result<Vec<FeedSection>, String> {
    let token = session
        .login5()
        .auth_token()
        .await
        .map_err(|e| format!("token: {e}"))?
        .access_token;
    let client_token = session
        .spclient()
        .client_token()
        .await
        .map_err(|e| format!("client token: {e}"))?;
    let body = json!({
        "variables": {
            "homeEndUserIntegration": "INTEGRATION_WEB_PLAYER",
            "timeZone": tz,
            "sp_t": "",
            "facet": "",
            "sectionItemsLimit": 12,
            "includeEpisodeContentRatingsV2": false
        },
        "operationName": "home",
        "extensions": { "persistedQuery": { "version": 1, "sha256Hash": hash } }
    });
    let headers = [
        ("authorization", format!("Bearer {token}")),
        ("client-token", client_token),
        ("content-type", "application/json;charset=UTF-8".to_string()),
        ("accept", "application/json".to_string()),
        ("accept-language", "vi".to_string()),
        ("app-platform", "WebPlayer".to_string()),
        ("origin", "https://open.spotify.com".to_string()),
        ("referer", "https://open.spotify.com/".to_string()),
    ];
    let (status, bytes) = fetch(
        Method::POST,
        PATHFINDER,
        &headers,
        Some(serde_json::to_vec(&body).unwrap_or_default()),
    )
    .await?;
    if status != 200 {
        let snippet = String::from_utf8_lossy(&bytes[..bytes.len().min(200)]).to_string();
        return Err(format!("HTTP {status}: {snippet}"));
    }
    let v: Value = serde_json::from_slice(&bytes).map_err(|e| format!("JSON: {e}"))?;
    let mut sections = Vec::new();
    if let Some(data) = v.get("data") {
        collect_sections(data, &mut sections);
    }
    if sections.is_empty() {
        let err = v
            .pointer("/errors/0/message")
            .and_then(|m| m.as_str())
            .unwrap_or("trang chủ trống");
        return Err(err.to_string());
    }
    sections.truncate(MAX_SECTIONS);
    Ok(sections)
}

/// Finds the current `home` query hash in the web player's JavaScript.
pub async fn discover_hash() -> Result<String, String> {
    let (_, html) = fetch(Method::GET, "https://open.spotify.com/", &[], None).await?;
    let html = String::from_utf8_lossy(&html);
    // The page links web-player.<hash>.css before the .js; take the first quoted
    // URL that ends in .js.
    let marker = "/web-player/web-player.";
    let js_url = html
        .match_indices(marker)
        .filter_map(|(i, _)| {
            let start = html[..i].rfind('"')? + 1;
            let end = i + html[i..].find('"')?;
            let url = &html[start..end];
            (url.ends_with(".js") && url.starts_with("https://")).then_some(url)
        })
        .next()
        .ok_or("không thấy web player")?;
    log::info!("home: reading query hash from {js_url}");
    let (_, js) = fetch(Method::GET, js_url, &[], None).await?;
    let js = String::from_utf8_lossy(&js);
    let key = "\"home\",\"query\",\"";
    let i = js.find(key).ok_or("không thấy truy vấn home")? + key.len();
    let hash: String = js[i..].chars().take(64).collect();
    if hash.len() == 64 && hash.chars().all(|c| c.is_ascii_hexdigit()) {
        Ok(hash)
    } else {
        Err("hash không hợp lệ".into())
    }
}

#[derive(Serialize, Deserialize, Default)]
struct Cache {
    hash: String,
    sections: Vec<FeedSection>,
}

fn read_cache(path: &Path) -> Cache {
    std::fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

/// The feed saved from the last successful load (shown instantly at start).
pub fn cached(path: &Path) -> Vec<FeedSection> {
    read_cache(path).sections
}

pub async fn load(session: &Session, cache_path: &Path, tz_setting: &str) -> Result<Vec<FeedSection>, String> {
    let tz = time_zone(tz_setting);
    let cache = read_cache(cache_path);
    let hash = if cache.hash.len() == 64 {
        cache.hash.clone()
    } else {
        DEFAULT_HASH.to_string()
    };
    let result = match query_home(session, &hash, &tz).await {
        Ok(s) => Ok((hash, s)),
        Err(first) => {
            log::warn!("home: {first}; looking up the current query hash");
            match discover_hash().await {
                Ok(new_hash) if new_hash != hash => query_home(session, &new_hash, &tz)
                    .await
                    .map(|s| (new_hash, s)),
                Ok(_) => Err(first),
                Err(e) => Err(format!("{first} ({e})")),
            }
        }
    };
    match result {
        Ok((hash, sections)) => {
            let c = Cache {
                hash,
                sections: sections.clone(),
            };
            if let Ok(b) = serde_json::to_vec(&c) {
                let _ = std::fs::write(cache_path, b);
            }
            Ok(sections)
        }
        Err(e) => Err(e),
    }
}

// ------------------------------------------------------------------ parsing
// The response is walked generically so small schema changes don't break it:
// a section is any object with `sectionItems.items`; an item's entity is at
// `content.data` (or `data`).

fn str_at<'a>(v: &'a Value, path: &str) -> Option<&'a str> {
    v.pointer(path).and_then(|x| x.as_str()).filter(|s| !s.trim().is_empty())
}

fn strip_html(s: &str) -> String {
    let mut out = String::new();
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out.replace("&amp;", "&").replace("&#x27;", "'").replace("&quot;", "\"")
}

fn best_image(data: &Value) -> Option<String> {
    let candidates = [
        data.pointer("/images/items/0/sources"),
        data.pointer("/coverArt/sources"),
        data.pointer("/visuals/avatarImage/sources"),
        data.pointer("/albumOfTrack/coverArt/sources"),
    ];
    for sources in candidates.into_iter().flatten() {
        let Some(list) = sources.as_array() else { continue };
        // Prefer ~300 px: sharp at card size without a huge download.
        let best = list
            .iter()
            .filter_map(|s| {
                let url = s.get("url")?.as_str()?;
                let w = s.get("width").and_then(|w| w.as_u64()).unwrap_or(300);
                Some((url, (w as i64 - 300).abs()))
            })
            .min_by_key(|(_, d)| *d);
        if let Some((url, _)) = best {
            return Some(url.to_string());
        }
    }
    None
}

fn parse_item(item: &Value) -> Option<FeedItem> {
    let data = item
        .pointer("/content/data")
        .or_else(|| item.get("data"))
        .unwrap_or(item);
    let uri = str_at(data, "/uri").or_else(|| str_at(item, "/uri"))?;
    let kind = if uri.starts_with("spotify:playlist:") {
        FeedKind::Playlist
    } else if uri.starts_with("spotify:album:") {
        FeedKind::Album
    } else if uri.starts_with("spotify:artist:") {
        FeedKind::Artist
    } else {
        return None; // podcasts, episodes, …
    };
    let title = str_at(data, "/name").or_else(|| str_at(data, "/profile/name"))?;
    let subtitle = match kind {
        FeedKind::Playlist => {
            let desc = str_at(data, "/description").map(strip_html).unwrap_or_default();
            if desc.trim().is_empty() {
                str_at(data, "/ownerV2/data/name").unwrap_or("Playlist").to_string()
            } else {
                desc
            }
        }
        FeedKind::Album => {
            let artists: Vec<&str> = data
                .pointer("/artists/items")
                .and_then(|a| a.as_array())
                .map(|a| a.iter().filter_map(|x| str_at(x, "/profile/name")).collect())
                .unwrap_or_default();
            if artists.is_empty() {
                "Album".into()
            } else {
                format!("Album • {}", artists.join(", "))
            }
        }
        FeedKind::Artist => "Nghệ sĩ".into(),
    };
    Some(FeedItem {
        uri: uri.to_string(),
        kind,
        title: clean(title),
        subtitle: clean(subtitle.trim()),
        image: best_image(data),
    })
}

fn section_title(section: &Value) -> String {
    for p in [
        "/data/title/transformedLabel",
        "/data/title/text",
        "/data/title/originalLabel",
        "/title/text",
        "/title",
    ] {
        if let Some(t) = str_at(section, p) {
            return clean(t);
        }
    }
    "Dành cho bạn".into()
}

fn collect_sections(v: &Value, out: &mut Vec<FeedSection>) {
    match v {
        Value::Object(m) => {
            if let Some(items) = m
                .get("sectionItems")
                .and_then(|s| s.get("items"))
                .and_then(|i| i.as_array())
            {
                let items: Vec<FeedItem> = items.iter().filter_map(parse_item).take(MAX_ITEMS).collect();
                if !items.is_empty() {
                    out.push(FeedSection {
                        title: section_title(v),
                        items,
                    });
                }
                return;
            }
            for x in m.values() {
                collect_sections(x, out);
            }
        }
        Value::Array(a) => {
            for x in a {
                collect_sections(x, out);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_generic_home() {
        let v = json!({"home": {"sectionContainer": {"sections": {"items": [
            {"data": {"title": {"transformedLabel": "Gần đây"}},
             "sectionItems": {"items": [
                {"content": {"data": {"__typename": "Playlist", "uri": "spotify:playlist:1",
                    "name": "Daily Mix 3", "description": "<a href=\"x\">Hngle</a>, Low G",
                    "images": {"items": [{"sources": [{"url": "https://i/640", "width": 640}, {"url": "https://i/300", "width": 300}]}]}}}},
                {"content": {"data": {"__typename": "Episode", "uri": "spotify:episode:9", "name": "x"}}},
                {"content": {"data": {"__typename": "Artist", "uri": "spotify:artist:2",
                    "profile": {"name": "Nhuộm Collective"},
                    "visuals": {"avatarImage": {"sources": [{"url": "https://a/1"}]}}}}}
             ]}}
        ]}}}});
        let mut out = Vec::new();
        collect_sections(&v, &mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].title, "Gần đây");
        assert_eq!(out[0].items.len(), 2);
        assert_eq!(out[0].items[0].subtitle, "Hngle, Low G");
        assert_eq!(out[0].items[0].image.as_deref(), Some("https://i/300"));
        assert_eq!(out[0].items[1].kind, FeedKind::Artist);
    }
}
