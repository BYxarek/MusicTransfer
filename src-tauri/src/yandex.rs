use std::{collections::HashSet, time::Duration};

use serde::Deserialize;
use serde_json::json;
use url::Url;

use crate::report::{PENDING, Result, Track};

#[derive(Deserialize)]
struct Response {
    result: Playlist,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Playlist {
    title: String,
    track_count: usize,
    tracks: Vec<Entry>,
}

#[derive(Deserialize)]
struct Entry {
    track: Recording,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Recording {
    id: serde_json::Value,
    title: String,
    version: Option<String>,
    artists: Vec<Artist>,
    duration_ms: Option<u64>,
}

#[derive(Deserialize)]
struct Artist {
    name: String,
}

fn endpoint(source: &Url) -> Option<Url> {
    let path: Vec<_> = source
        .path()
        .trim_end_matches('/')
        .split('/')
        .skip(1)
        .collect();
    let identifier = |value: &str| {
        !value.is_empty()
            && value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    };
    let mut url = Url::parse("https://api.music.yandex.net").ok()?;
    match path.as_slice() {
        ["users", user, "playlists", kind] if identifier(user) && identifier(kind) => {
            url.path_segments_mut()
                .ok()?
                .extend(["users", user, "playlists", kind]);
        }
        ["playlists", id] if identifier(id.strip_prefix("lk.").unwrap_or(id)) => {
            url.path_segments_mut().ok()?.extend(["playlist", id]);
        }
        _ => return None,
    }
    Some(url)
}

fn convert(playlist: Playlist, source: &Url) -> Result<(String, Vec<Track>)> {
    if playlist.title.trim().is_empty()
        || playlist.track_count == 0
        || playlist.track_count != playlist.tracks.len()
    {
        return Err("API Яндекс Музыки вернул неполный плейлист.".into());
    }
    let mut seen = HashSet::new();
    let mut tracks = Vec::with_capacity(playlist.track_count);
    for entry in playlist.tracks {
        let recording = entry.track;
        let id = recording
            .id
            .as_u64()
            .map(|id| id.to_string())
            .or_else(|| {
                recording
                    .id
                    .as_str()
                    .filter(|id| !id.is_empty() && id.bytes().all(|byte| byte.is_ascii_digit()))
                    .map(str::to_owned)
            })
            .ok_or("В API Яндекс Музыки отсутствует идентификатор трека.")?;
        if recording.title.trim().is_empty() {
            return Err("В API Яндекс Музыки отсутствует название трека.".into());
        }
        if !seen.insert(id.clone()) {
            continue;
        }
        let mut url = source
            .join(&format!("/track/{id}"))
            .map_err(|error| error.to_string())?;
        url.set_query(None);
        let mut title = recording.title;
        if let Some(version) = recording
            .version
            .filter(|version| !version.trim().is_empty())
        {
            let version = version.trim();
            if !title.to_lowercase().contains(&version.to_lowercase()) {
                title = format!("{title} ({version})");
            }
        }
        let mut track = Track {
            title,
            url: url.into(),
            artists: recording
                .artists
                .into_iter()
                .map(|artist| artist.name)
                .collect(),
            result: PENDING.into(),
            extra: Default::default(),
        };
        if let Some(duration) = recording.duration_ms {
            track
                .extra
                .insert("duration".into(), json!(duration.div_ceil(1000)));
        }
        tracks.push(track);
    }
    Ok((playlist.title, tracks))
}

pub fn tracks(source: &Url) -> Result<(String, Vec<Track>)> {
    let url = endpoint(source).ok_or("Этот вид ссылки не поддерживается API Яндекс Музыки.")?;
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(12))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|error| error.to_string())?;
    let response = client
        .get(url)
        .send()
        .and_then(|response| response.error_for_status())
        .and_then(|response| response.json::<Response>())
        .map_err(|error| format!("API Яндекс Музыки: {}", error.without_url()))?;
    convert(response.result, source)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_the_liked_playlist_prefix_in_the_api_request() {
        let url = Url::parse(
            "https://music.yandex.by/playlists/lk.12345678-abcd-4321-abcd-123456789abc/?utm_source=share",
        )
        .unwrap();
        assert_eq!(
            endpoint(&url).unwrap().as_str(),
            "https://api.music.yandex.net/playlist/lk.12345678-abcd-4321-abcd-123456789abc",
        );
        for id in ["lk.", "lk.bad%2Fid", "lk..id"] {
            let url = Url::parse(&format!("https://music.yandex.by/playlists/{id}")).unwrap();
            assert!(endpoint(&url).is_none());
        }
    }

    #[test]
    fn checks_completeness_before_removing_repeats() {
        let url = Url::parse("https://music.yandex.by/users/editor/playlists/3?x=1").unwrap();
        assert_eq!(endpoint(&url).unwrap().path(), "/users/editor/playlists/3");
        let track = json!({"track":{"id":"10","title":"Title","version":"Live","artists":[{"name":"Artist"}],"durationMs":200000}});
        let playlist = |count| {
            serde_json::from_value(
                json!({"title":"List","trackCount":count,"tracks":[track,track]}),
            )
            .unwrap()
        };
        assert!(convert(playlist(3), &url).is_err());
        let (_, tracks) = convert(playlist(2), &url).unwrap();
        assert_eq!(tracks.len(), 1);
        assert_eq!(tracks[0].title, "Title (Live)");
        assert_eq!(tracks[0].extra["duration"], 200);
        assert_eq!(tracks[0].url, "https://music.yandex.by/track/10");
    }
}
