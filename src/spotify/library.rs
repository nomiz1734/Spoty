//! Browsing data through Spotify's internal endpoints (the same ones the official
//! clients use), so no Web API client id is needed.

use std::collections::VecDeque;

use librespot_core::{Error, Session, SpotifyUri};
use librespot_metadata::image::ImageSize;
use librespot_protocol::context_page::ContextPage;
use librespot_protocol::extended_metadata::{BatchedEntityRequest, EntityRequest, ExtensionQuery};
use librespot_protocol::extension_kind::ExtensionKind;
use librespot_protocol::playlist4_external::SelectedListContent;
use protobuf::{EnumOrUnknown, Message};

use super::types::{PlaylistInfo, Source, TrackInfo, TrackList};
use crate::gfx::text::clean;

const MAX_PAGES: usize = 300;
const META_BATCH: usize = 100;

pub fn image_url(session: &Session, hex_id: &str) -> String {
    match session.get_user_attribute("image-url") {
        Some(t) if t.contains("{file_id}") => t.replace("{file_id}", hex_id),
        _ => format!("https://i.scdn.co/image/{hex_id}"),
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub async fn playlists(session: &Session) -> Result<Vec<PlaylistInfo>, Error> {
    let mut out = Vec::new();
    let mut from = 0usize;
    loop {
        let bytes = session.spclient().get_rootlist(from, Some(200)).await?;
        let msg = SelectedListContent::parse_from_bytes(&bytes)?;
        let Some(contents) = msg.contents.as_ref() else {
            break;
        };
        for (i, item) in contents.items.iter().enumerate() {
            let uri = item.uri();
            if !uri.starts_with("spotify:playlist:") {
                continue; // folder markers
            }
            let meta = contents.meta_items.get(i);
            let attrs = meta.and_then(|m| m.attributes.as_ref());
            let name = attrs.map(|a| clean(a.name())).unwrap_or_default();
            let cover = attrs.and_then(|a| {
                a.picture_size
                    .iter()
                    .find(|p| p.target_name() == "default")
                    .or_else(|| a.picture_size.first())
                    .map(|p| p.url().to_string())
                    .filter(|u| !u.is_empty())
                    .or_else(|| {
                        (a.picture().len() >= 16).then(|| image_url(session, &hex(a.picture())))
                    })
            });
            out.push(PlaylistInfo {
                uri: uri.to_string(),
                name: if name.is_empty() {
                    "Playlist".into()
                } else {
                    name
                },
                owner: meta.map(|m| m.owner_username().to_string()).unwrap_or_default(),
                length: meta.map(|m| m.length()).unwrap_or(0),
                cover,
            });
        }
        let got = contents.items.len();
        if !contents.truncated() || got == 0 {
            break;
        }
        from += got;
        if from > 5000 {
            break;
        }
    }
    Ok(out)
}

async fn fetch_page(session: &Session, url: &str) -> Result<ContextPage, Error> {
    let bytes = session.spclient().get_next_page(url).await?;
    let json = String::from_utf8_lossy(&bytes);
    protobuf_json_mapping::parse_from_str::<ContextPage>(&json)
        .map_err(|e| Error::failed_precondition(format!("bad context page: {e}")))
}

/// Lists all track URIs of a playable context, following pagination.
pub async fn load_source(session: &Session, source: Source) -> Result<TrackList, Error> {
    let context_uri = source.context_uri(&session.username());
    // Artist contexts list every album as extra pages; the first few are enough.
    let max_pages = if matches!(source, Source::Artist { .. }) {
        4
    } else {
        MAX_PAGES
    };
    let ctx = session.spclient().get_context(&context_uri).await?;
    let mut uris = Vec::new();
    let mut pages: VecDeque<ContextPage> = ctx.pages.into_iter().collect();
    let mut fetched = 0usize;
    while let Some(page) = pages.pop_front() {
        if fetched > max_pages {
            log::warn!("context {context_uri}: too many pages, stopping");
            break;
        }
        if page.tracks.is_empty() && !page.page_url().is_empty() {
            fetched += 1;
            match fetch_page(session, page.page_url()).await {
                Ok(p) => pages.push_front(p),
                Err(e) => log::warn!("page {}: {e}", page.page_url()),
            }
            continue;
        }
        for t in &page.tracks {
            let uri = t.uri();
            if !uri.is_empty() && !uri.starts_with("spotify:delimiter") {
                uris.push(uri.to_string());
            }
        }
        if !page.next_page_url().is_empty() {
            fetched += 1;
            match fetch_page(session, page.next_page_url()).await {
                Ok(p) => pages.push_back(p),
                Err(e) => log::warn!("next page {}: {e}", page.next_page_url()),
            }
        }
    }
    Ok(TrackList {
        context_uri,
        uris,
    })
}

fn pick_cover(
    session: &Session,
    images: &[librespot_metadata::image::Image],
    prefer: &[ImageSize],
) -> Option<String> {
    for want in prefer {
        if let Some(img) = images.iter().find(|i| i.size == *want) {
            if let Ok(h) = img.id.to_base16() {
                return Some(image_url(session, &h));
            }
        }
    }
    images
        .first()
        .and_then(|i| i.id.to_base16().ok())
        .map(|h| image_url(session, &h))
}

fn track_info(session: &Session, uri: &str, t: &librespot_metadata::Track) -> TrackInfo {
    let covers: Vec<_> = if t.album.covers.is_empty() {
        t.album.cover_group.iter().cloned().collect()
    } else {
        t.album.covers.iter().cloned().collect()
    };
    TrackInfo {
        uri: uri.to_string(),
        name: clean(&t.name),
        artists: clean(
            &t.artists
                .iter()
                .map(|a| a.name.as_str())
                .collect::<Vec<_>>()
                .join(", "),
        ),
        artist_uri: t.artists.first().and_then(|a| a.id.to_uri().ok()),
        album: clean(&t.album.name),
        album_uri: t.album.id.to_uri().ok(),
        duration_ms: t.duration.max(0) as u32,
        cover_small: pick_cover(session, &covers, &[ImageSize::SMALL, ImageSize::DEFAULT]),
        cover_large: pick_cover(
            session,
            &covers,
            &[ImageSize::LARGE, ImageSize::XLARGE, ImageSize::DEFAULT],
        ),
        explicit: t.is_explicit,
        quality: None,
    }
}

/// Entries we can describe without a metadata request (local files, podcasts).
fn placeholder(uri: &str) -> Option<TrackInfo> {
    match SpotifyUri::from_uri(uri) {
        Ok(SpotifyUri::Local {
            artist,
            album_title,
            track_title,
            duration,
        }) => Some(TrackInfo {
            uri: uri.to_string(),
            name: clean(&track_title),
            artists: clean(&artist),
            album: clean(&album_title),
            duration_ms: duration.as_millis() as u32,
            ..Default::default()
        }),
        Ok(SpotifyUri::Track { .. }) => None,
        _ => Some(TrackInfo {
            uri: uri.to_string(),
            name: "Podcast / nội dung khác".into(),
            ..Default::default()
        }),
    }
}

/// Fetches track metadata in batches. Unknown/failed entries are simply absent.
pub async fn track_meta(session: &Session, uris: &[String]) -> Result<Vec<TrackInfo>, Error> {
    let mut out = Vec::with_capacity(uris.len());
    let mut wanted = Vec::new();
    for u in uris {
        match placeholder(u) {
            Some(p) => out.push(p),
            None => wanted.push(u.clone()),
        }
    }
    for chunk in wanted.chunks(META_BATCH) {
        let req = BatchedEntityRequest {
            entity_request: chunk
                .iter()
                .map(|u| EntityRequest {
                    entity_uri: u.clone(),
                    query: vec![ExtensionQuery {
                        extension_kind: EnumOrUnknown::new(ExtensionKind::TRACK_V4),
                        ..Default::default()
                    }],
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        };
        let res = session.spclient().get_extended_metadata(req).await?;
        for arr in &res.extended_metadata {
            for data in &arr.extension_data {
                let Some(any) = data.extension_data.as_ref() else {
                    continue;
                };
                let Ok(msg) = librespot_protocol::metadata::Track::parse_from_bytes(&any.value)
                else {
                    continue;
                };
                match librespot_metadata::Track::try_from(&msg) {
                    Ok(t) => out.push(track_info(session, &data.entity_uri, &t)),
                    Err(e) => log::debug!("track {}: {e}", data.entity_uri),
                }
            }
        }
    }
    Ok(out)
}
