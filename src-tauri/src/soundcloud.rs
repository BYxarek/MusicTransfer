use std::{
    collections::{HashMap, HashSet},
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    thread,
    time::{Duration, Instant},
};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use reqwest::{
    Method,
    blocking::{Client, Response},
};
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use url::Url;

use crate::{
    chrome::{
        Candidate, Membership, PlaylistChoice, PlaylistDetails, SaveFailure, Search, SearchFailure,
    },
    report::{self, LIMIT, Part, Result},
};

const API: &str = "https://api.soundcloud.com";
const TOKEN: &str = "https://byxarek.site/music-transfer/soundcloud-token.php";
const CLIENT_ID: &str = "FP6dwOtDaLJbs330tNedpX9CUoAGauBZ";
const LOOPBACK: &str = "127.0.0.1:8766";
const REDIRECT: &str = "http://127.0.0.1:8766/callback";

#[derive(Deserialize)]
struct Token {
    access_token: String,
    refresh_token: String,
    expires_in: u64,
}

#[derive(Deserialize)]
struct Identity {
    urn: String,
}

#[derive(Deserialize)]
struct Recording {
    urn: String,
    title: Option<String>,
    permalink_url: Option<String>,
    metadata_artist: Option<String>,
    duration: Option<u64>,
    user: Option<Uploader>,
}

#[derive(Deserialize)]
struct Uploader {
    username: String,
}

#[derive(Deserialize)]
struct Playlist {
    urn: String,
    title: String,
    permalink_url: String,
    user: Identity,
    track_count: usize,
    sharing: String,
    tracks: Option<Vec<Identity>>,
}

#[derive(Deserialize)]
struct Page<T> {
    collection: Vec<T>,
    next_href: Option<String>,
}

pub struct SoundCloud {
    http: Client,
    token: Token,
    expires: Instant,
    pub owner: String,
    tracks: HashMap<String, String>,
    playlists: HashMap<String, String>,
    selected: Option<String>,
}

fn api_url(url: &Url) -> bool {
    url.scheme() == "https"
        && url.host_str() == Some("api.soundcloud.com")
        && url.port().is_none()
        && url.username().is_empty()
        && url.password().is_none()
}

fn resource(urn: &str, kind: &str) -> bool {
    urn.strip_prefix("soundcloud:")
        .and_then(|value| value.split_once(':'))
        .is_some_and(|(prefix, id)| {
            prefix == kind && !id.is_empty() && id.bytes().all(|byte| byte.is_ascii_digit())
        })
}

fn complete_tracks(tracks: Vec<Identity>, count: usize) -> Result<Vec<String>> {
    if tracks.len() != count || tracks.iter().any(|track| !resource(&track.urn, "tracks")) {
        return Err("SoundCloud вернул неполный состав плейлиста. Изменение отменено.".into());
    }
    Ok(tracks.into_iter().map(|track| track.urn).collect())
}

fn status(response: Response) -> std::result::Result<Response, SearchFailure> {
    if response.status().is_success() {
        Ok(response)
    } else {
        Err(SearchFailure::Http(response.status().as_u16()))
    }
}

fn network(error: reqwest::Error) -> SearchFailure {
    SearchFailure::Browser(format!("API SoundCloud: {}", error.without_url()))
}

fn exchange(http: &Client, body: Value) -> Result<Token> {
    let response = http
        .post(TOKEN)
        .json(&body)
        .send()
        .map_err(|error| network(error).to_string())?;
    status(response)
        .map_err(|error| format!("Не удалось получить токен SoundCloud. {error}"))?
        .json()
        .map_err(|error| network(error).to_string())
}

fn callback(target: &str, state: &str) -> Option<Result<String>> {
    let query = target.strip_prefix("/callback?")?;
    let values: Vec<_> = url::form_urlencoded::parse(query.as_bytes()).collect();
    let parameter = |name: &str| {
        let mut matches = values
            .iter()
            .filter(|(key, _)| key == name)
            .map(|(_, value)| value.as_ref());
        let value = matches.next()?;
        matches.next().is_none().then_some(value)
    };
    if parameter("state") != Some(state) {
        return None;
    }
    Some(
        parameter("code")
            .filter(|code| !code.is_empty())
            .map(str::to_owned)
            .ok_or_else(|| "Вход в SoundCloud не был подтверждён.".into()),
    )
}

fn receive(stream: &mut TcpStream, state: &str) -> Option<Result<String>> {
    stream
        .set_read_timeout(Some(Duration::from_millis(500)))
        .ok()?;
    stream
        .set_write_timeout(Some(Duration::from_millis(500)))
        .ok()?;
    let mut request = String::new();
    BufReader::new(&mut *stream)
        .take(4096)
        .read_line(&mut request)
        .ok()?;
    let (target, _) = request.strip_prefix("GET ")?.split_once(' ')?;
    let result = callback(target, state);
    let (status, message) = match result.as_ref() {
        Some(Ok(_)) => (
            "200 OK",
            "SoundCloud confirmed the sign-in. Return to MusicTransfer.",
        ),
        Some(Err(_)) => (
            "400 Bad Request",
            "SoundCloud sign-in was cancelled. Return to MusicTransfer.",
        ),
        None => (
            "404 Not Found",
            "No active SoundCloud sign-in matches this request.",
        ),
    };
    let _ = write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: text/plain; charset=utf-8\r\nCache-Control: no-store\r\nReferrer-Policy: no-referrer\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{message}",
        message.len()
    );
    result
}

impl SoundCloud {
    pub fn connect(check: &impl Fn() -> Result<()>) -> Result<Self> {
        let http = Client::builder()
            .timeout(Duration::from_secs(15))
            .redirect(reqwest::redirect::Policy::custom(|attempt| {
                if attempt.previous().len() < 4
                    && attempt.previous().iter().all(api_url)
                    && api_url(attempt.url())
                {
                    attempt.follow()
                } else {
                    attempt.stop()
                }
            }))
            .build()
            .map_err(|error| network(error).to_string())?;
        status(
            http.get(TOKEN)
                .send()
                .map_err(|error| network(error).to_string())?,
        )
        .map_err(|error| format!("Сервер входа SoundCloud недоступен. {error}"))?;
        let listener = TcpListener::bind(LOOPBACK).map_err(|_| {
            "Не удалось открыть локальный порт 8766 для входа в SoundCloud.".to_owned()
        })?;
        listener
            .set_nonblocking(true)
            .map_err(|error| error.to_string())?;
        let state = URL_SAFE_NO_PAD.encode(rand::random::<[u8; 32]>());
        let verifier = URL_SAFE_NO_PAD.encode(rand::random::<[u8; 32]>());
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        let mut url = Url::parse("https://secure.soundcloud.com/authorize").unwrap();
        url.query_pairs_mut().extend_pairs([
            ("client_id", CLIENT_ID),
            ("redirect_uri", REDIRECT),
            ("response_type", "code"),
            ("code_challenge_method", "S256"),
            ("code_challenge", &challenge),
            ("state", &state),
        ]);
        check()?;
        open::that(url.as_str())
            .map_err(|_| "Не удалось открыть браузер для входа в SoundCloud.".to_owned())?;
        let deadline = Instant::now() + Duration::from_secs(180);
        let code = loop {
            check()?;
            match listener.accept() {
                Ok((mut stream, _)) => {
                    if let Some(result) = receive(&mut stream, &state) {
                        break result?;
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(error) => return Err(error.to_string()),
            }
            if Instant::now() >= deadline {
                return Err("Истекло время ожидания входа в SoundCloud.".into());
            }
            thread::sleep(Duration::from_millis(100));
        };
        check()?;
        let token = exchange(
            &http,
            json!({
                "grant_type": "authorization_code", "code": code, "code_verifier": verifier,
            }),
        )?;
        let expires = Instant::now() + Duration::from_secs(token.expires_in.saturating_sub(60));
        let mut api = Self {
            http,
            token,
            expires,
            owner: String::new(),
            tracks: HashMap::new(),
            playlists: HashMap::new(),
            selected: None,
        };
        let account: Identity = api.get("/me")?;
        if !resource(&account.urn, "users") {
            return Err("SoundCloud не подтвердил аккаунт.".into());
        }
        api.owner = account.urn;
        Ok(api)
    }

    fn request(
        &mut self,
        method: Method,
        url: Url,
        body: Option<&Value>,
    ) -> std::result::Result<Response, SearchFailure> {
        if !api_url(&url) {
            return Err(SearchFailure::Unconfirmed);
        }
        if Instant::now() >= self.expires {
            self.token = exchange(
                &self.http,
                json!({
                    "grant_type": "refresh_token", "refresh_token": self.token.refresh_token,
                }),
            )
            .map_err(SearchFailure::Browser)?;
            self.expires =
                Instant::now() + Duration::from_secs(self.token.expires_in.saturating_sub(60));
        }
        let mut request = self
            .http
            .request(method, url)
            .header(
                "Authorization",
                format!("OAuth {}", self.token.access_token),
            )
            .header("Accept", "application/json");
        if let Some(body) = body {
            request = request.json(body);
        }
        status(request.send().map_err(network)?)
    }

    fn get<T: DeserializeOwned>(&mut self, path: &str) -> Result<T> {
        self.request(
            Method::GET,
            Url::parse(&format!("{API}{path}")).unwrap(),
            None,
        )
        .and_then(|response| response.json().map_err(network))
        .map_err(|error| error.to_string())
    }

    fn pages<T: DeserializeOwned>(
        &mut self,
        path: &str,
        check: &impl Fn() -> Result<()>,
    ) -> Result<Vec<T>> {
        let mut next = Some(format!("{API}{path}"));
        let mut seen = HashSet::new();
        let mut rows = Vec::new();
        while let Some(address) = next {
            check()?;
            if !seen.insert(address.clone()) || seen.len() > 100 {
                return Err("SoundCloud не подтвердил конец списка.".into());
            }
            let url = Url::parse(&address)
                .map_err(|_| "SoundCloud вернул неверную страницу.".to_owned())?;
            let page: Page<T> = self
                .request(Method::GET, url, None)
                .and_then(|response| response.json().map_err(network))
                .map_err(|error| error.to_string())?;
            rows.extend(page.collection);
            next = page.next_href;
        }
        Ok(rows)
    }

    pub fn search(
        &mut self,
        query: &str,
        check: &impl Fn() -> Result<()>,
    ) -> std::result::Result<Search, SearchFailure> {
        check().map_err(SearchFailure::Browser)?;
        let mut url = Url::parse(&format!("{API}/tracks")).unwrap();
        url.query_pairs_mut().extend_pairs([
            ("q", query),
            ("limit", "10"),
            ("linked_partitioning", "true"),
        ]);
        let page: Page<Recording> = self
            .request(Method::GET, url, None)?
            .json()
            .map_err(network)?;
        let missing = page.collection.is_empty();
        let mut rows = Vec::new();
        for item in page.collection {
            let Some(url) = item.permalink_url.as_deref().and_then(report::track_url) else {
                continue;
            };
            let Some(title) = item.title.filter(|title| !title.trim().is_empty()) else {
                continue;
            };
            if !resource(&item.urn, "tracks") {
                continue;
            }
            self.tracks.insert(url.clone(), item.urn);
            rows.push(Candidate {
                index: rows.len(),
                artist: item
                    .metadata_artist
                    .filter(|artist| !artist.trim().is_empty())
                    .or_else(|| item.user.map(|user| user.username))
                    .unwrap_or_default(),
                title,
                url: Some(url),
                duration: item.duration.map(|duration| duration.div_ceil(1000)),
            });
        }
        if rows.is_empty() && !missing {
            return Err(SearchFailure::Unconfirmed);
        }
        Ok(Search { missing, rows })
    }

    fn resolve<T: DeserializeOwned>(&mut self, address: &str) -> Result<T> {
        let mut url = Url::parse(&format!("{API}/resolve")).unwrap();
        url.query_pairs_mut().append_pair("url", address);
        self.request(Method::GET, url, None)
            .and_then(|response| response.json().map_err(network))
            .map_err(|error| error.to_string())
    }

    fn track_urn(&mut self, url: &str) -> Result<String> {
        let url = report::track_url(url).ok_or("В отчёте отсутствует ссылка на трек.")?;
        if let Some(urn) = self.tracks.get(&url) {
            return Ok(urn.clone());
        }
        let track: Identity = self.resolve(&url)?;
        if !resource(&track.urn, "tracks") {
            return Err("SoundCloud не подтвердил идентификатор трека.".into());
        }
        self.tracks.insert(url, track.urn.clone());
        Ok(track.urn)
    }

    fn remember(&mut self, playlist: &Playlist) -> Result<String> {
        let url = report::canonical_url(&playlist.permalink_url)
            .ok_or("SoundCloud не вернул ссылку на плейлист.")?;
        if playlist.user.urn != self.owner || !resource(&playlist.urn, "playlists") {
            return Err("SoundCloud не подтвердил право редактировать этот плейлист.".into());
        }
        self.playlists.insert(url.clone(), playlist.urn.clone());
        Ok(url)
    }

    fn owned_playlists(&mut self, check: &impl Fn() -> Result<()>) -> Result<Vec<Playlist>> {
        self.pages(
            "/me/playlists?show_tracks=false&linked_partitioning=true&limit=100",
            check,
        )
    }

    fn owned_playlist(&mut self, url: &str, check: &impl Fn() -> Result<()>) -> Result<Playlist> {
        let url = report::canonical_url(url).ok_or("В отчёте отсутствует ссылка на плейлист.")?;
        check()?;
        let playlist = if let Some(urn) = self.playlists.get(&url) {
            self.get(&format!("/playlists/{urn}?show_tracks=false"))?
        } else {
            match self.resolve::<Playlist>(&url) {
                Ok(playlist) => playlist,
                Err(error) => self
                    .owned_playlists(check)?
                    .into_iter()
                    .find(|playlist| {
                        report::canonical_url(&playlist.permalink_url).as_deref()
                            == Some(url.as_str())
                    })
                    .ok_or(error)?,
            }
        };
        self.remember(&playlist)?;
        Ok(playlist)
    }

    pub fn library(&mut self, check: &impl Fn() -> Result<()>) -> Result<Vec<PlaylistChoice>> {
        let mut choices = Vec::new();
        for playlist in self.owned_playlists(check)? {
            if let Ok(url) = self.remember(&playlist) {
                choices.push(PlaylistChoice {
                    name: playlist.title,
                    url,
                });
            }
        }
        Ok(choices)
    }

    pub fn playlist_links(
        &mut self,
        names: &[String],
        check: &impl Fn() -> Result<()>,
    ) -> Result<Vec<Option<String>>> {
        let choices = self.library(check)?;
        Ok(names
            .iter()
            .map(|name| {
                let mut rows = choices.iter().filter(|row| row.name == *name);
                let url = rows.next()?.url.clone();
                rows.next().is_none().then_some(url)
            })
            .collect())
    }

    pub fn playlist_details(
        &mut self,
        url: &str,
        check: &impl Fn() -> Result<()>,
    ) -> Result<PlaylistDetails> {
        let playlist = self.owned_playlist(url, check)?;
        self.member_urns(&playlist, check)?;
        let url = self.remember(&playlist)?;
        Ok(PlaylistDetails {
            url,
            name: playlist.title,
            count: playlist.track_count,
        })
    }

    fn member_urns(
        &mut self,
        playlist: &Playlist,
        check: &impl Fn() -> Result<()>,
    ) -> Result<Vec<String>> {
        let path = format!(
            "/playlists/{}/tracks?access=playable,preview,blocked&linked_partitioning=true&limit=200",
            playlist.urn
        );
        let tracks = self.pages::<Identity>(&path, check)?;
        complete_tracks(tracks, playlist.track_count)
    }

    pub fn has_track(
        &mut self,
        track: &str,
        part: &Part,
        check: &impl Fn() -> Result<()>,
    ) -> Result<bool> {
        let track = self.track_urn(track)?;
        let playlist = self.owned_playlist(
            part.url
                .as_deref()
                .ok_or("В отчёте отсутствует ссылка на плейлист.")?,
            check,
        )?;
        Ok(self.member_urns(&playlist, check)?.contains(&track))
    }

    pub fn open_candidate(
        &mut self,
        candidate: &Candidate,
        check: &impl Fn() -> Result<()>,
    ) -> Result<()> {
        check()?;
        self.selected = Some(
            self.track_urn(
                candidate
                    .url
                    .as_deref()
                    .ok_or("Нет ссылки на найденный трек.")?,
            )?,
        );
        Ok(())
    }

    pub fn probe_open_part(
        &mut self,
        part: &Part,
        check: &impl Fn() -> Result<()>,
    ) -> Result<Option<Membership>> {
        if !part.created {
            return Ok(Some(Membership::Absent));
        }
        let selected = self.selected.clone().ok_or("Не выбран трек SoundCloud.")?;
        let playlist = self.owned_playlist(
            part.url
                .as_deref()
                .ok_or("В отчёте отсутствует ссылка на плейлист.")?,
            check,
        )?;
        let present = self.member_urns(&playlist, check)?.contains(&selected);
        Ok(Some(if present {
            Membership::Present
        } else {
            Membership::Absent
        }))
    }

    pub fn prepare_open_part(
        &mut self,
        part: &Part,
        check: &impl Fn() -> Result<()>,
    ) -> Result<bool> {
        Ok(self
            .probe_open_part(part, check)?
            .is_some_and(Membership::present))
    }

    pub fn prepare(
        &mut self,
        candidate: &Candidate,
        part: &Part,
        check: &impl Fn() -> Result<()>,
    ) -> Result<bool> {
        self.open_candidate(candidate, check)?;
        self.prepare_open_part(part, check)
    }

    fn write_playlist(
        &mut self,
        playlist: Option<&Playlist>,
        name: &str,
        tracks: &[String],
        check: &impl Fn() -> Result<()>,
    ) -> std::result::Result<Playlist, SaveFailure> {
        check().map_err(SaveFailure::Rejected)?;
        let tracks_body: Vec<_> = tracks.iter().map(|urn| json!({"urn": urn})).collect();
        let (method, path, body) = match playlist {
            Some(playlist) => (
                Method::PUT,
                format!("/playlists/{}", playlist.urn),
                json!({"tracks": tracks_body}),
            ),
            None => (
                Method::POST,
                "/playlists".to_owned(),
                json!({"title": name, "sharing": "private", "tracks": tracks_body}),
            ),
        };
        let result = self.request(
            method,
            Url::parse(&format!("{API}{path}")).unwrap(),
            Some(&json!({"playlist": body})),
        );
        let saved: Playlist = result
            .map_err(|error| {
                let message = error.to_string();
                match error {
                    SearchFailure::Http(400 | 401 | 403 | 404 | 422) => {
                        SaveFailure::Rejected(message)
                    }
                    _ => SaveFailure::Unknown(message),
                }
            })?
            .json()
            .map_err(|error| SaveFailure::Unknown(network(error).to_string()))?;
        self.remember(&saved).map_err(SaveFailure::Unknown)?;
        let confirmed = saved
            .tracks
            .as_ref()
            .is_some_and(|items| items.iter().map(|track| &track.urn).eq(tracks.iter()));
        if (playlist.is_none() && saved.sharing != "private")
            || (playlist.is_some_and(|playlist| {
                playlist.urn != saved.urn || playlist.sharing != saved.sharing
            }))
            || saved.track_count != tracks.len()
            || (!confirmed
                && self
                    .member_urns(&saved, check)
                    .map_err(SaveFailure::Unknown)?
                    != tracks)
        {
            return Err(SaveFailure::Unknown(
                "SoundCloud не подтвердил ожидаемый состав и доступ к плейлисту.".into(),
            ));
        }
        Ok(saved)
    }

    pub fn save(
        &mut self,
        part: &mut Part,
        check: &impl Fn() -> Result<()>,
    ) -> std::result::Result<(), SaveFailure> {
        let track = self
            .selected
            .clone()
            .ok_or_else(|| SaveFailure::Rejected("Не выбран трек SoundCloud.".into()))?;
        let mut tracks = Vec::new();
        let playlist = if part.created {
            let url = part.url.as_deref().ok_or_else(|| {
                SaveFailure::Rejected("В отчёте отсутствует ссылка на плейлист.".into())
            })?;
            let playlist = self
                .owned_playlist(url, check)
                .map_err(SaveFailure::Rejected)?;
            tracks = self
                .member_urns(&playlist, check)
                .map_err(SaveFailure::Rejected)?;
            if tracks.contains(&track) {
                return Err(SaveFailure::Unknown(
                    "Трек уже появился в плейлисте. Требуется сверка.".into(),
                ));
            }
            if tracks.len() >= LIMIT {
                return Err(SaveFailure::Rejected(
                    "Достигнут предел треков в плейлисте.".into(),
                ));
            }
            Some(playlist)
        } else {
            None
        };
        tracks.push(track);
        let saved = self.write_playlist(playlist.as_ref(), &part.name, &tracks, check)?;
        part.url = Some(self.remember(&saved).map_err(SaveFailure::Unknown)?);
        part.occupied = Some(tracks.len() - 1);
        Ok(())
    }

    pub fn remove_previous(
        &mut self,
        previous: &str,
        replacement: &str,
        part: &Part,
        check: &impl Fn() -> Result<()>,
    ) -> Result<()> {
        let previous = self.track_urn(previous)?;
        let replacement = self.track_urn(replacement)?;
        let playlist = self.owned_playlist(
            part.url
                .as_deref()
                .ok_or("В отчёте отсутствует ссылка на плейлист.")?,
            check,
        )?;
        let mut tracks = self.member_urns(&playlist, check)?;
        if previous == replacement || !tracks.contains(&replacement) {
            return Err(
                "SoundCloud не подтвердил наличие отдельной новой версии в плейлисте.".into(),
            );
        }
        if let Some(index) = tracks.iter().position(|urn| urn == &previous) {
            tracks.remove(index);
            self.write_playlist(Some(&playlist), &part.name, &tracks, check)
                .map_err(|failure| match failure {
                    SaveFailure::Rejected(error) | SaveFailure::Unknown(error) => error,
                    SaveFailure::Challenge => "SoundCloud запросил проверку.".into(),
                })?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_only_the_callback_with_one_matching_state() {
        assert!(callback("/callback?state=other&code=abc", "expected").is_none());
        assert!(callback("/callback?state=expected&state=other&code=abc", "expected").is_none());
        assert_eq!(
            callback("/callback?state=expected&code=a%2Bb", "expected")
                .unwrap()
                .unwrap(),
            "a+b"
        );
        assert!(
            callback("/callback?state=expected&error=access_denied", "expected")
                .unwrap()
                .is_err()
        );
    }

    #[test]
    fn playlist_update_requires_every_original_track_in_order() {
        let original = || {
            Vec::from(
                [
                    "soundcloud:tracks:33",
                    "soundcloud:tracks:12",
                    "soundcloud:tracks:33",
                ]
                .map(|urn| Identity { urn: urn.into() }),
            )
        };
        assert!(complete_tracks(original(), 4).is_err());
        assert_eq!(
            complete_tracks(original(), 3).unwrap(),
            [
                "soundcloud:tracks:33",
                "soundcloud:tracks:12",
                "soundcloud:tracks:33",
            ]
        );
    }
}
