use std::{
    collections::{HashMap, HashSet},
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    sync::Mutex,
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use reqwest::{
    Method,
    blocking::{Client, Response},
    header::{HeaderMap, LOCATION, RETRY_AFTER},
};
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use url::Url;

use crate::{
    chrome::{
        Candidate, Membership, PlaylistChoice, PlaylistDetails, SEARCH_INTERVAL, SaveFailure,
        Search, SearchFailure,
    },
    report::{self, LIMIT, Part, Result},
};

const API: &str = "https://api.soundcloud.com";
const TOKEN: &str = "https://byxarek.site/music-transfer/soundcloud-token.php";
const CLIENT_ID: &str = "FP6dwOtDaLJbs330tNedpX9CUoAGauBZ";
const LOOPBACK: &str = "127.0.0.1:8766";
const REDIRECT: &str = "http://127.0.0.1:8766/callback";
const API_INTERVAL: Duration = Duration::from_millis(750);
const DEFAULT_COOLDOWN: u64 = 60;

#[derive(Default)]
struct RateLimits {
    api: Option<Instant>,
    search: Option<Instant>,
}

// shortcut: this process tracks its own traffic, use a shared limiter to budget across installations.
static RATE_LIMITS: Mutex<RateLimits> = Mutex::new(RateLimits {
    api: None,
    search: None,
});

impl RateLimits {
    fn delay(&self, now: Instant, search: bool) -> Duration {
        let deadline = if search {
            self.api.max(self.search)
        } else {
            self.api
        };
        deadline.map_or(Duration::ZERO, |deadline| {
            deadline.saturating_duration_since(now)
        })
    }

    fn sent(&mut self, now: Instant, search: bool) {
        self.api = self.api.max(Some(now + API_INTERVAL));
        if search {
            self.search = self.search.max(Some(now + SEARCH_INTERVAL));
        }
    }

    fn observe(&mut self, headers: &HeaderMap, limited: bool, now: Instant, unix_now: u64) {
        if !limited && seconds_header(headers, "x-ratelimit-remaining") != Some(0) {
            return;
        }
        let reset = seconds_header(headers, "x-ratelimit-reset")
            .filter(|&reset| reset >= unix_now)
            .map(|reset| (reset - unix_now).saturating_add(1));
        let retry_after = if limited {
            seconds_header(headers, RETRY_AFTER.as_str())
        } else {
            None
        };
        let seconds = reset.max(retry_after).unwrap_or(DEFAULT_COOLDOWN);
        let deadline = now
            .checked_add(Duration::from_secs(seconds))
            .unwrap_or(now + Duration::from_secs(DEFAULT_COOLDOWN));
        // A search response can report the shared quota, so pause all API traffic.
        self.api = self.api.max(Some(deadline));
    }
}

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
    on_wait: Box<dyn Fn(u64)>,
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

fn seconds_header(headers: &HeaderMap, name: &str) -> Option<u64> {
    headers.get(name)?.to_str().ok()?.trim().parse().ok()
}

fn status(response: Response) -> std::result::Result<Response, SearchFailure> {
    match response.status().as_u16() {
        200..=299 => Ok(response),
        429 => Err(SearchFailure::RateLimited(seconds_header(
            response.headers(),
            RETRY_AFTER.as_str(),
        ))),
        status => Err(SearchFailure::Http(status)),
    }
}

fn playlist_request(
    urn: Option<&str>,
    name: &str,
    sharing: &str,
    tracks: &[String],
) -> (Method, String, Value) {
    let tracks: Vec<_> = tracks.iter().map(|urn| json!({"urn": urn})).collect();
    match urn {
        Some(urn) => (
            Method::PUT,
            format!("/playlists/{urn}"),
            json!({"playlist": {"tracks": tracks}}),
        ),
        None => (
            Method::POST,
            "/playlists".to_owned(),
            json!({"playlist": {
                "title": name,
                "sharing": sharing,
                "tracks": tracks,
            }}),
        ),
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
    pub fn connect(
        check: &impl Fn() -> Result<()>,
        on_wait: impl Fn(u64) + 'static,
    ) -> Result<Self> {
        let http = Client::builder()
            .timeout(Duration::from_secs(15))
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
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
            on_wait: Box::new(on_wait),
        };
        let account: Identity = api.get("/me", check)?;
        if !resource(&account.urn, "users") {
            return Err("SoundCloud не подтвердил аккаунт.".into());
        }
        api.owner = account.urn;
        Ok(api)
    }

    fn wait_for_slot(&self, search: bool, check: &impl Fn() -> Result<()>) -> Result<()> {
        let mut notified = false;
        loop {
            check()?;
            let delay = RATE_LIMITS.lock().unwrap().delay(Instant::now(), search);
            if delay.is_zero() {
                return Ok(());
            }
            if !notified && delay >= Duration::from_secs(2) {
                (self.on_wait)(delay.as_secs() + 1);
                notified = true;
            }
            thread::sleep(delay.min(Duration::from_millis(200)));
        }
    }

    fn request(
        &mut self,
        mut method: Method,
        mut url: Url,
        mut body: Option<&Value>,
        check: &impl Fn() -> Result<()>,
    ) -> std::result::Result<Response, SearchFailure> {
        if !api_url(&url) {
            return Err(SearchFailure::Unconfirmed);
        }
        let mut redirects = 0;
        loop {
            let search = method == Method::GET
                && matches!(url.path(), "/tracks" | "/search" | "/search/tracks");
            self.wait_for_slot(search, check)
                .map_err(SearchFailure::Browser)?;
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
                .request(method.clone(), url.clone())
                .header(
                    "Authorization",
                    format!("OAuth {}", self.token.access_token),
                )
                .header("Accept", "application/json");
            if let Some(body) = body {
                request = request.json(body);
            }
            check().map_err(SearchFailure::Browser)?;
            RATE_LIMITS.lock().unwrap().sent(Instant::now(), search);
            let response = request.send().map_err(network)?;
            let code = response.status().as_u16();
            let unix_now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            RATE_LIMITS.lock().unwrap().observe(
                response.headers(),
                code == 429,
                Instant::now(),
                unix_now,
            );
            // Resolve redirects within the same API host, pacing each HTTP request.
            if redirects < 3
                && matches!(code, 301 | 302 | 303 | 307 | 308)
                && let Some(location) = response
                    .headers()
                    .get(LOCATION)
                    .and_then(|value| value.to_str().ok())
                && let Ok(next) = url.join(location)
                && api_url(&next)
            {
                if code == 303 || (matches!(code, 301 | 302) && method == Method::POST) {
                    if method != Method::HEAD {
                        method = Method::GET;
                    }
                    body = None;
                }
                redirects += 1;
                url = next;
                continue;
            }
            return status(response);
        }
    }

    fn get<T: DeserializeOwned>(
        &mut self,
        path: &str,
        check: &impl Fn() -> Result<()>,
    ) -> Result<T> {
        self.request(
            Method::GET,
            Url::parse(&format!("{API}{path}")).unwrap(),
            None,
            check,
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
                .request(Method::GET, url, None, check)
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
            .request(Method::GET, url, None, check)?
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

    fn resolve<T: DeserializeOwned>(
        &mut self,
        address: &str,
        check: &impl Fn() -> Result<()>,
    ) -> Result<T> {
        let mut url = Url::parse(&format!("{API}/resolve")).unwrap();
        url.query_pairs_mut().append_pair("url", address);
        self.request(Method::GET, url, None, check)
            .and_then(|response| response.json().map_err(network))
            .map_err(|error| error.to_string())
    }

    fn track_urn(&mut self, url: &str, check: &impl Fn() -> Result<()>) -> Result<String> {
        let url = report::track_url(url).ok_or("В отчёте отсутствует ссылка на трек.")?;
        if let Some(urn) = self.tracks.get(&url) {
            return Ok(urn.clone());
        }
        let track: Identity = self.resolve(&url, check)?;
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
            self.get(&format!("/playlists/{urn}?show_tracks=false"), check)?
        } else {
            match self.resolve::<Playlist>(&url, check) {
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
        let track = self.track_urn(track, check)?;
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
                check,
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
        part: &Part,
        tracks: &[String],
        check: &impl Fn() -> Result<()>,
    ) -> std::result::Result<Playlist, SaveFailure> {
        check().map_err(SaveFailure::Rejected)?;
        let sharing = playlist.map_or(if part.public { "public" } else { "private" }, |playlist| {
            playlist.sharing.as_str()
        });
        let (method, path, body) = playlist_request(
            playlist.map(|playlist| playlist.urn.as_str()),
            &part.name,
            sharing,
            tracks,
        );
        let result = self.request(
            method,
            Url::parse(&format!("{API}{path}")).unwrap(),
            Some(&body),
            check,
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
        if saved.sharing != sharing
            || playlist.is_some_and(|playlist| playlist.urn != saved.urn)
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
        let saved = self.write_playlist(playlist.as_ref(), part, &tracks, check)?;
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
        let previous = self.track_urn(previous, check)?;
        let replacement = self.track_urn(replacement, check)?;
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
            self.write_playlist(Some(&playlist), part, &tracks, check)
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
    fn spaces_api_and_search_requests_independently() {
        let now = Instant::now();
        let mut limits = RateLimits::default();
        limits.sent(now, true);
        assert_eq!(limits.delay(now, false), Duration::from_millis(750));
        assert_eq!(limits.delay(now, true), Duration::from_millis(1250));

        let next = now + Duration::from_millis(750);
        limits.sent(next, false);
        assert_eq!(limits.delay(next, true), Duration::from_millis(750));
        assert_eq!(
            limits.delay(next + Duration::from_millis(750), true),
            Duration::ZERO
        );
    }

    #[test]
    fn pauses_before_the_next_request_when_the_server_quota_is_empty() {
        let now = Instant::now();
        let mut limits = RateLimits::default();
        let mut headers = HeaderMap::new();
        headers.insert("x-ratelimit-remaining", "1".parse().unwrap());
        headers.insert("x-ratelimit-reset", "1045".parse().unwrap());
        limits.observe(&headers, false, now, 1000);
        assert_eq!(limits.delay(now, false), Duration::ZERO);

        headers.insert("x-ratelimit-remaining", "0".parse().unwrap());
        limits.observe(&headers, false, now, 1000);
        assert_eq!(limits.delay(now, false), Duration::from_secs(46));
        assert_eq!(limits.delay(now, true), Duration::from_secs(46));

        headers.insert(RETRY_AFTER, "70".parse().unwrap());
        limits.observe(&headers, true, now, 1000);
        assert_eq!(limits.delay(now, true), Duration::from_secs(70));
        assert_eq!(
            limits.delay(now + Duration::from_secs(70), true),
            Duration::ZERO
        );
    }

    #[test]
    fn public_playlist_creation_and_update_send_the_ordered_tracks() {
        let tracks = ["soundcloud:tracks:12".into(), "soundcloud:tracks:33".into()];
        let expected = json!([{"urn": tracks[0]}, {"urn": tracks[1]}]);
        let (method, path, body) = playlist_request(None, "Test", "public", &tracks);
        assert_eq!(method, Method::POST);
        assert_eq!(path, "/playlists");
        assert_eq!(
            body,
            json!({"playlist": {"title": "Test", "sharing": "public", "tracks": expected}})
        );

        let (method, path, body) =
            playlist_request(Some("soundcloud:playlists:1"), "Test", "public", &tracks);
        assert_eq!(method, Method::PUT);
        assert_eq!(path, "/playlists/soundcloud:playlists:1");
        assert_eq!(body, json!({"playlist": {"tracks": expected}}));
    }

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
