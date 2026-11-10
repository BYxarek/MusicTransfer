use std::{
    collections::{HashSet, VecDeque},
    env,
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use chromiumoxide::{
    Browser, Handler, Page,
    cdp::{
        browser_protocol::network::{
            EnableParams, EventRequestWillBeSent, EventResponseReceived, RequestId,
        },
        js_protocol::runtime::EvaluateParams,
    },
    error::CdpError,
    handler::HandlerConfig,
    listeners::EventStream,
};
use futures_util::{FutureExt, StreamExt};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use tokio::{runtime::Runtime, task::JoinHandle, time::timeout};
use url::Url;
use winreg::{HKCU, HKLM};

use crate::report::{self, Part, Result, Track};

pub const SOUNDCLOUD: &str = "https://soundcloud.com/discover";
pub const SEARCH_INTERVAL: Duration = Duration::from_millis(1250);
const DOM: &str = include_str!("chrome_dom.js");
const DELAY: Duration = Duration::from_secs(1);
const TICK: Duration = Duration::from_millis(200);

#[derive(Clone, Deserialize, Serialize)]
pub struct Candidate {
    pub(crate) index: usize,
    pub artist: String,
    pub title: String,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub duration: Option<u64>,
}

impl Candidate {
    pub fn label(&self) -> String {
        format!("{} — {}", self.artist, self.title)
    }
}

#[derive(Deserialize)]
pub struct Search {
    pub missing: bool,
    pub rows: Vec<Candidate>,
}

#[derive(Debug, thiserror::Error)]
pub enum SearchFailure {
    #[error("Поиск SoundCloud недоступен (HTTP 429).")]
    RateLimited(Option<u64>),
    #[error("Поиск SoundCloud недоступен (HTTP {0}).")]
    Http(u16),
    #[error("SoundCloud показал проверку безопасности.")]
    Challenge,
    #[error("SoundCloud не подтвердил результат поиска.")]
    Unconfirmed,
    #[error("{0}")]
    Browser(String),
}

#[derive(Deserialize, Serialize)]
pub struct PlaylistChoice {
    pub name: String,
    pub url: String,
}

#[derive(Deserialize, Serialize)]
pub struct PlaylistDetails {
    pub name: String,
    pub url: String,
    pub count: usize,
}

#[derive(Deserialize)]
struct Source {
    title: String,
    tracks: Vec<Track>,
}

#[derive(Deserialize)]
struct SaveState {
    state: String,
}

#[derive(Deserialize)]
struct Validation {
    message: String,
    invalid: bool,
}

#[derive(Debug)]
pub enum SaveFailure {
    Challenge,
    Rejected(String),
    Unknown(String),
}

struct NetworkResponse {
    id: RequestId,
    url: String,
    status: u16,
    retry_after: Option<u64>,
    challenge: bool,
}

fn challenge_response(status: u16, headers: &Value) -> bool {
    headers.as_object().is_some_and(|headers| {
        headers.iter().any(|(name, value)| {
            (name.eq_ignore_ascii_case("cf-mitigated") && value.as_str() == Some("challenge"))
                || (status == 403
                    && name.eq_ignore_ascii_case("x-datadomeresponse")
                    && value.as_str() == Some("403"))
        })
    })
}

#[derive(Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Membership {
    Present,
    Removable,
    Absent,
}

impl Membership {
    pub fn present(self) -> bool {
        self != Self::Absent
    }
}

fn chrome_exe() -> Result<PathBuf> {
    let registry = r"Software\Microsoft\Windows\CurrentVersion\App Paths\chrome.exe";
    let paths = [HKCU, HKLM].into_iter().filter_map(|hive| {
        let key = hive.open_subkey(registry).ok()?;
        key.get_value::<OsString, _>("").ok().map(PathBuf::from)
    });
    let locations = ["LOCALAPPDATA", "ProgramFiles", "ProgramFiles(x86)"]
        .into_iter()
        .filter_map(env::var_os)
        .map(|folder| PathBuf::from(folder).join(r"Google\Chrome\Application\chrome.exe"));
    paths
        .chain(locations)
        .find(|path| path.is_file())
        .ok_or_else(|| "Установленный Google Chrome не найден.".into())
}

fn profile() -> Result<PathBuf> {
    Ok(report::home()?.join(".yandex-soundcloud-transfer"))
}

fn spawn(profile: &Path, debug: bool, address: &str) -> Result<Child> {
    let mut profile_arg = OsString::from("--user-data-dir=");
    profile_arg.push(profile);
    let mut command = Command::new(chrome_exe()?);
    command
        .arg(profile_arg)
        .arg("--new-window")
        .arg("--lang=en-US")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if debug {
        command.args([
            "--remote-debugging-port=0",
            "--no-first-run",
            "--no-default-browser-check",
        ]);
    }
    command
        .arg(address)
        .spawn()
        .map_err(|error| format!("Chrome не запустился: {error}"))
}

pub fn manual_login() -> Result<Child> {
    spawn(&profile()?, false, SOUNDCLOUD)
}

fn endpoint(file: &Path) -> Option<(u16, String)> {
    let data = fs::read_to_string(file).ok()?;
    let mut lines = data.lines();
    let port = lines.next()?.parse().ok()?;
    let path = lines.next()?.strip_prefix("/devtools/browser/")?;
    if path.is_empty()
        || !path
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() || byte == b'-')
    {
        return None;
    }
    Some((
        port,
        format!("ws://127.0.0.1:{port}/devtools/browser/{path}"),
    ))
}

fn connect(runtime: &Runtime, file: &Path) -> Option<(Browser, Handler)> {
    let (_, address) = endpoint(file)?;
    let config = HandlerConfig {
        ignore_https_errors: false,
        request_timeout: Duration::from_secs(8),
        ..Default::default()
    };
    runtime.block_on(async {
        timeout(
            Duration::from_millis(900),
            Browser::connect_with_config(address, config),
        )
        .await
        .ok()?
        .ok()
    })
}

pub struct Chrome {
    process: Child,
    profile: PathBuf,
    runtime: Runtime,
    browser: Browser,
    page: Page,
    handler: JoinHandle<()>,
    request_stream: EventStream<EventRequestWillBeSent>,
    response_stream: EventStream<EventResponseReceived>,
    requests: VecDeque<(RequestId, String, String)>,
    responses: VecDeque<NetworkResponse>,
    next_search: Instant,
}

impl Chrome {
    pub fn launch(check: &impl Fn() -> Result<()>) -> Result<Self> {
        Self::launch_profile(&profile()?, check)
    }

    fn launch_profile(profile: &Path, check: &impl Fn() -> Result<()>) -> Result<Self> {
        let endpoint_file = profile.join("DevToolsActivePort");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| format!("Не удалось запустить управление Chrome: {error}"))?;
        if connect(&runtime, &endpoint_file).is_some() {
            return Err(
                "Закройте другое окно Chrome, открытое приложением для переноса, и повторите."
                    .into(),
            );
        }
        let mut process = spawn(profile, true, "about:blank")?;
        let deadline = Instant::now() + Duration::from_secs(18);
        let (mut browser, mut events) = loop {
            if let Err(error) = check() {
                let _ = process.kill();
                let _ = process.wait();
                return Err(error);
            }
            if let Ok(Some(_)) = process.try_wait() {
                return Err("Chrome уже использует профиль переноса. Закройте окна этого профиля и повторите.".into());
            }
            if let Some(connection) = connect(&runtime, &endpoint_file) {
                break connection;
            }
            if Instant::now() >= deadline {
                let _ = process.kill();
                let _ = process.wait();
                return Err("Chrome открылся без управления. Закройте окна профиля переноса и попробуйте снова.".into());
            }
            thread::sleep(TICK);
        };
        let handler = runtime.spawn(async move {
            while let Some(event) = events.next().await {
                if event.is_err() {
                    log::debug!("Chrome returned a protocol error.");
                }
            }
        });
        let connected = runtime.block_on(async {
            timeout(Duration::from_secs(10), async {
                let targets = browser.fetch_targets().await?;
                let mut page = None;
                if let Some(target) = targets
                    .into_iter()
                    .find(|target| target.r#type == "page" && target.url == "about:blank")
                {
                    for _ in 0..5 {
                        if let Ok(tab) = browser.get_page(target.target_id.clone()).await {
                            page = Some(tab);
                            break;
                        }
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                }
                let page = match page {
                    Some(page) => page,
                    None => browser.new_page("about:blank").await?,
                };
                let requests = page.event_listener::<EventRequestWillBeSent>().await?;
                let responses = page.event_listener::<EventResponseReceived>().await?;
                page.execute(EnableParams::default()).await?;
                Ok::<_, CdpError>((page, requests, responses))
            })
            .await
            .map_err(|_| chrome_error(CdpError::Timeout))?
            .map_err(chrome_error)
        });
        let (page, request_stream, response_stream) = match connected {
            Ok(connected) => connected,
            Err(error) => {
                close_process(&runtime, &mut browser, &mut process);
                return Err(error);
            }
        };
        Ok(Self {
            process,
            profile: profile.to_path_buf(),
            runtime,
            browser,
            page,
            handler,
            request_stream,
            response_stream,
            requests: VecDeque::new(),
            responses: VecDeque::new(),
            next_search: Instant::now(),
        })
    }

    fn collect_network(&mut self) {
        while let Some(Some(event)) = self.request_stream.next().now_or_never() {
            if !matches!(event.request.method.as_str(), "POST" | "PUT" | "PATCH") {
                continue;
            }
            if self.requests.len() == 2048 {
                self.requests.pop_front();
            }
            self.requests.push_back((
                event.request_id.clone(),
                event.request.method.clone(),
                event.request.url.clone(),
            ));
        }
        while let Some(Some(event)) = self.response_stream.next().now_or_never() {
            if self.responses.len() == 2048 {
                self.responses.pop_front();
            }
            if let Ok(status) = u16::try_from(event.response.status) {
                let headers = event.response.headers.inner();
                self.responses.push_back(NetworkResponse {
                    id: event.request_id.clone(),
                    url: event.response.url.clone(),
                    status,
                    retry_after: headers.as_object().and_then(|headers| {
                        headers
                            .iter()
                            .find(|(name, _)| name.eq_ignore_ascii_case("retry-after"))
                            .and_then(|(_, value)| value.as_str()?.trim().parse().ok())
                    }),
                    challenge: challenge_response(status, headers),
                });
            }
        }
    }

    fn reset_network(&mut self) {
        self.collect_network();
        self.requests.clear();
        self.responses.clear();
    }

    pub fn navigate(&mut self, url: &str) -> Result<()> {
        self.reset_network();
        // A failed HTTP navigation can omit the load event Chromiumoxide waits for.
        let (previous_url, previous_start): (String, f64) = self.evaluate(&format!(
            "(setTimeout(() => location.assign({}), 0), [location.href, performance.timeOrigin])",
            json!(url)
        ))?;
        self.wait(Duration::from_secs(15), &|| Ok(()), |chrome| {
            let (address, start): (String, f64) =
                chrome.evaluate("[location.href, performance.timeOrigin]")?;
            if address.starts_with("chrome-error:") {
                return Err("Chrome не открыл страницу.".into());
            }
            Ok(
                (start != previous_start || (address == url && address != previous_url))
                    .then_some(()),
            )
        })?
        .ok_or_else(|| "Chrome не подтвердил открытие страницы.".into())
    }

    fn search_http_error(&self, url: &str) -> Option<SearchFailure> {
        self.responses.iter().find_map(|response| {
            (response.url == url && response.status >= 400).then(|| match response.status {
                429 => SearchFailure::RateLimited(response.retry_after),
                status => SearchFailure::Http(status),
            })
        })
    }

    fn search_start(&mut self, url: &str) -> std::result::Result<(), SearchFailure> {
        let opened = self.navigate(url);
        if let Some(error) = self.search_http_error(url) {
            return Err(error);
        }
        opened.map_err(SearchFailure::Browser)
    }

    fn evaluate<T: DeserializeOwned>(&mut self, expression: &str) -> Result<T> {
        let params = EvaluateParams::builder()
            .expression(expression)
            .return_by_value(true)
            .build()
            .map_err(|error| error.to_string())?;
        let response = self
            .runtime
            .block_on(self.page.execute(params))
            .map_err(chrome_error)?
            .result;
        self.collect_network();
        if response.exception_details.is_some() {
            return Err("Chrome не смог прочитать интерфейс страницы.".into());
        }
        serde_json::from_value(response.result.value.unwrap_or(Value::Null))
            .map_err(|error| format!("Chrome вернул неожиданный ответ: {error}"))
    }

    fn dom<T: DeserializeOwned>(&mut self, method: &str, args: &[Value]) -> Result<T> {
        let args = args
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join(",");
        self.evaluate(&format!("{DOM}.{method}({args})"))
    }

    pub fn address(&mut self) -> Result<String> {
        self.evaluate("location.href")
    }

    pub fn disconnected(&mut self) -> bool {
        self.handler.is_finished() || self.address().is_err_and(|error| !transient(&error))
    }

    pub fn relaunch(&mut self, check: &impl Fn() -> Result<()>) -> Result<()> {
        close_process(&self.runtime, &mut self.browser, &mut self.process);
        *self = Self::launch_profile(&self.profile, check)?;
        Ok(())
    }

    pub fn captcha(&mut self) -> Result<bool> {
        self.dom("captcha", &[])
    }

    pub fn wait<T>(
        &mut self,
        duration: Duration,
        check: &impl Fn() -> Result<()>,
        mut ready: impl FnMut(&mut Self) -> Result<Option<T>>,
    ) -> Result<Option<T>> {
        let until = Instant::now() + duration;
        loop {
            check()?;
            match ready(self) {
                Ok(Some(value)) => return Ok(Some(value)),
                Ok(None) => {}
                Err(error) if transient(&error) => {}
                Err(error) => return Err(error),
            }
            if Instant::now() >= until {
                return Ok(None);
            }
            thread::sleep(TICK);
        }
    }

    pub fn tracks(&mut self, check: &impl Fn() -> Result<()>) -> Result<(String, Vec<Track>)> {
        let first = self
            .wait(Duration::from_secs(30), check, |chrome| {
                if chrome.captcha()? {
                    return Err("Требуется проверка в Chrome.".into());
                }
                let source: Source = chrome.dom("source", &[])?;
                Ok((!source.title.is_empty() && !source.tracks.is_empty()).then_some(source))
            })?
            .ok_or("Не удалось считать плейлист. Проверьте доступ в Яндекс Музыке.")?;
        let mut seen = HashSet::new();
        let mut tracks = Vec::new();
        let mut source = first;
        let mut stable = 0;
        loop {
            let old = tracks.len();
            tracks.extend(
                source
                    .tracks
                    .into_iter()
                    .filter(|track| seen.insert(track.url.clone())),
            );
            stable = if tracks.len() == old { stable + 1 } else { 0 };
            if stable >= 4 {
                return Ok((source.title, tracks));
            }
            let _: bool = self.dom("scroll", &[])?;
            thread::sleep(Duration::from_millis(1200));
            check()?;
            if self.captcha()? {
                return Err("Требуется проверка в Chrome.".into());
            }
            source = self.dom("source", &[])?;
        }
    }

    pub fn search(
        &mut self,
        query: &str,
        check: &impl Fn() -> Result<()>,
    ) -> std::result::Result<Search, SearchFailure> {
        let next_search = self.next_search;
        self.wait(
            next_search.saturating_duration_since(Instant::now()),
            check,
            |_| Ok((Instant::now() >= next_search).then_some(())),
        )
        .map_err(SearchFailure::Browser)?;
        let mut url = Url::parse("https://soundcloud.com/search/sounds").unwrap();
        url.query_pairs_mut().append_pair("q", query);
        self.next_search = Instant::now() + SEARCH_INTERVAL;
        self.search_start(url.as_str())?;
        let mut result = self
            .wait(Duration::from_secs(20), check, |chrome| {
                if let Some(error @ SearchFailure::RateLimited(_)) =
                    chrome.search_http_error(url.as_str())
                {
                    return Ok(Some(Err(error)));
                }
                let result: Option<Search> = chrome.dom("search", &[json!(query)])?;
                if chrome.captcha()? {
                    return Ok(Some(Err(SearchFailure::Challenge)));
                }
                if let Some(error) = chrome.search_http_error(url.as_str()) {
                    return Ok(Some(Err(error)));
                }
                Ok(result.map(Ok))
            })
            .map_err(SearchFailure::Browser)?
            .ok_or(SearchFailure::Unconfirmed)??;
        for candidate in &mut result.rows {
            candidate.url = candidate.url.as_deref().and_then(report::track_url);
        }
        Ok(result)
    }

    pub fn library(&mut self, check: &impl Fn() -> Result<()>) -> Result<Vec<PlaylistChoice>> {
        self.navigate("https://soundcloud.com/you/sets")?;
        let mut seen = HashSet::new();
        let choices = self
            .wait(Duration::from_secs(6), check, |chrome| {
                let choices: Vec<PlaylistChoice> = chrome.dom("library", &[])?;
                Ok((!choices.is_empty()).then_some(choices))
            })?
            .unwrap_or_default();
        Ok(choices
            .into_iter()
            .filter(|choice| {
                report::canonical_url(&choice.url).as_deref() == Some(&choice.url)
                    && seen.insert(choice.url.clone())
            })
            .collect())
    }

    pub fn playlist_details(
        &mut self,
        url: &str,
        check: &impl Fn() -> Result<()>,
    ) -> Result<PlaylistDetails> {
        self.navigate(url)?;
        self.wait(Duration::from_secs(10), check, |chrome| chrome.dom("playlistDetails", &[json!(url)]))?
            .ok_or_else(|| "Не удалось подтвердить право редактирования или число треков. Откройте свой плейлист в Chrome и проверьте вход.".into())
    }

    fn action(
        &mut self,
        method: &str,
        args: &[Value],
        check: &impl Fn() -> Result<()>,
    ) -> Result<()> {
        thread::sleep(DELAY);
        self.wait(Duration::from_secs(12), check, |chrome| {
            if chrome.captcha()? {
                return Err("SoundCloud показал проверку безопасности.".into());
            }
            Ok(chrome.dom::<bool>(method, args)?.then_some(()))
        })?
        .ok_or_else(|| format!("SoundCloud не показал нужный элемент ({method})."))
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

    pub fn open_candidate(
        &mut self,
        candidate: &Candidate,
        check: &impl Fn() -> Result<()>,
    ) -> Result<()> {
        self.action(
            "more",
            &[
                json!(candidate.index),
                json!(candidate.artist),
                json!(candidate.title),
                json!(candidate.url),
            ],
            check,
        )?;
        self.action("addMenu", &[], check)?;
        self.action("dialog", &[], check)
    }

    pub fn prepare_open_part(
        &mut self,
        part: &Part,
        check: &impl Fn() -> Result<()>,
    ) -> Result<bool> {
        let (name, created, url) = (
            &part.name,
            part.created,
            part.url.as_deref().and_then(report::canonical_url),
        );
        if created {
            self.filter_part(name, check)?;
        } else {
            self.action("createTab", &[], check)?;
            self.action("title", &[json!(name)], check)?;
            self.action("private", &[], check)?;
        }
        self.wait(Duration::from_secs(10), check, |chrome| {
            if chrome.captcha()? {
                return Err("SoundCloud показал проверку безопасности.".into());
            }
            let full = part.occupied.is_some_and(|count| count >= report::LIMIT);
            let state: SaveState = chrome.dom(
                "saveState",
                &[json!(name), json!(created), json!(url), json!(full)],
            )?;
            Ok(match state.state.as_str() {
                "added" => Some(true),
                "ready" => Some(false),
                _ => None,
            })
        })?
        .ok_or_else(|| "SoundCloud не подтвердил закрытый режим или не нашёл плейлист.".into())
    }

    fn filter_part(&mut self, name: &str, check: &impl Fn() -> Result<()>) -> Result<()> {
        self.wait(Duration::from_secs(6), check, |chrome| {
            if chrome.captcha()? {
                return Err("SoundCloud показал проверку безопасности.".into());
            }
            Ok(chrome.dom::<bool>("filter", &[json!(name)])?.then_some(()))
        })?
        .ok_or_else(|| "SoundCloud не показал фильтр плейлистов.".into())
    }

    pub fn probe_open_part(
        &mut self,
        part: &Part,
        check: &impl Fn() -> Result<()>,
    ) -> Result<Option<Membership>> {
        self.probe_membership(
            part,
            part.occupied.is_some_and(|count| count >= report::LIMIT),
            check,
        )
    }

    fn probe_membership(
        &mut self,
        part: &Part,
        allow_full: bool,
        check: &impl Fn() -> Result<()>,
    ) -> Result<Option<Membership>> {
        self.filter_part(&part.name, check)?;
        let args = [
            json!(part.name),
            json!(part.url.as_deref().and_then(report::canonical_url)),
            json!(allow_full),
        ];
        self.wait(Duration::from_secs(5), check, |chrome| {
            if chrome.captcha()? {
                return Err("SoundCloud показал проверку безопасности.".into());
            }
            chrome.dom("membership", &args)
        })
    }

    fn membership(
        &mut self,
        track: &str,
        part: &Part,
        check: &impl Fn() -> Result<()>,
    ) -> Result<(Membership, [Value; 2])> {
        let url = part
            .url
            .as_deref()
            .and_then(report::canonical_url)
            .ok_or("В отчёте отсутствует ссылка на плейлист.")?;
        let track = report::track_url(track).ok_or("В отчёте отсутствует ссылка на трек.")?;
        self.navigate(&track)?;
        self.action("trackMore", &[json!(track)], check)?;
        self.action("addMenu", &[], check)?;
        self.action("dialog", &[], check)?;
        let args = [json!(part.name), json!(url)];
        let state = self
            .probe_membership(part, false, check)?
            .ok_or("SoundCloud не подтвердил состояние трека в плейлисте.")?;
        Ok((state, args))
    }

    pub fn has_track(
        &mut self,
        track: &str,
        part: &Part,
        check: &impl Fn() -> Result<()>,
    ) -> Result<bool> {
        self.membership(track, part, check)
            .map(|(state, _)| state.present())
    }

    pub fn remove_previous(
        &mut self,
        previous: &str,
        replacement: &str,
        part: &Part,
        check: &impl Fn() -> Result<()>,
    ) -> Result<()> {
        if previous == replacement {
            return Err("Выбрана одна и та же запись.".into());
        }
        let (new_state, _) = self.membership(replacement, part, check)?;
        if !new_state.present() {
            return Err("SoundCloud не подтвердил наличие новой версии в плейлисте.".into());
        }
        let (old_state, args) = self.membership(previous, part, check)?;
        if old_state == Membership::Absent {
            return Ok(());
        }
        if old_state != Membership::Removable {
            return Err(
                "SoundCloud не показал кнопку «Remove from Playlist» для прежней версии.".into(),
            );
        }
        thread::sleep(DELAY);
        check()?;
        if self.captcha()? || !self.dom::<bool>("remove", &args)? {
            return Err("SoundCloud не принял удаление прежней версии.".into());
        }
        self.wait(Duration::from_secs(12), check, |chrome| {
            let state: Option<String> = chrome.dom("membership", &args)?;
            Ok((state.as_deref() == Some("absent")).then_some(()))
        })?
        .ok_or_else(|| "SoundCloud не подтвердил удаление. Проверьте плейлист вручную.".into())
    }

    pub fn save(
        &mut self,
        part: &Part,
        check: &impl Fn() -> Result<()>,
    ) -> std::result::Result<(), SaveFailure> {
        let created = part.created;
        let url = part.url.as_deref().and_then(report::canonical_url);
        let args = [json!(part.name), json!(created), json!(url)];
        check().map_err(SaveFailure::Unknown)?;
        if self.captcha().map_err(SaveFailure::Unknown)? {
            return Err(SaveFailure::Challenge);
        }
        self.reset_network();
        if !self
            .dom::<bool>("save", &args)
            .map_err(SaveFailure::Unknown)?
        {
            return Err(SaveFailure::Rejected(
                "SoundCloud не показал кнопку сохранения.".into(),
            ));
        }
        let mut observed = None;
        self.wait(Duration::from_secs(20), check, |chrome| {
            if chrome.captcha()? {
                return Ok(Some(Err(SaveFailure::Challenge)));
            }
            let (sent, response) = chrome.save_network(created);
            if response.is_some_and(|response| response.challenge) {
                return Ok(Some(Err(SaveFailure::Challenge)));
            }
            if sent && response.is_none() {
                observed = None;
                return Ok(None);
            }
            let status = response.map(|response| response.status);
            let outcome = match status {
                Some(200 | 201 | 204) => Some(Ok(())),
                Some(status @ 400..) => {
                    let message =
                        format!("SoundCloud вернул HTTP {status} при сохранении плейлиста.");
                    Some(Err(
                        if (400..500).contains(&status) && !matches!(status, 403 | 408 | 409 | 429)
                        {
                            SaveFailure::Rejected(message)
                        } else {
                            SaveFailure::Unknown(message)
                        },
                    ))
                }
                _ => {
                    let state: SaveState = chrome.dom("saveState", &args)?;
                    if (!created && state.state == "hidden") || (created && state.state == "added")
                    {
                        Some(Ok(()))
                    } else {
                        chrome
                            .dom::<Option<Validation>>("validation", &[])?
                            .map(|validation| {
                                let message =
                                    format!("Ошибка формы SoundCloud: {}", validation.message);
                                Err(if validation.invalid {
                                    SaveFailure::Rejected(message)
                                } else {
                                    SaveFailure::Unknown(message)
                                })
                            })
                    }
                }
            };
            let Some(outcome) = outcome else {
                observed = None;
                return Ok(None);
            };
            // The button and HTTP response can change before SoundCloud renders the challenge.
            let (previous_status, since) = observed.get_or_insert_with(|| (status, Instant::now()));
            if *previous_status != status {
                (*previous_status, *since) = (status, Instant::now());
            }
            let grace = if matches!(status, Some(403 | 429)) {
                3
            } else {
                2
            };
            Ok((since.elapsed() >= Duration::from_secs(grace)).then_some(outcome))
        })
        .map_err(SaveFailure::Unknown)?
        .unwrap_or_else(|| {
            Err(SaveFailure::Unknown(
                "SoundCloud не подтвердил сохранение. Проверьте плейлист.".into(),
            ))
        })
    }

    fn save_network(&self, created: bool) -> (bool, Option<&NetworkResponse>) {
        let requests: HashSet<_> = self
            .requests
            .iter()
            .filter_map(|(id, method, url)| {
                let address = Url::parse(url).ok()?;
                let path = address.path().trim_end_matches('/');
                let matching_path = if created {
                    path.strip_prefix("/playlists/").is_some_and(|rest| {
                        !rest.is_empty()
                            && rest.split('/').count()
                                == if rest.ends_with("/tracks") { 2 } else { 1 }
                    })
                } else {
                    method == "POST" && path == "/playlists"
                };
                (matching_path
                    && matches!(
                        address.host_str(),
                        Some("api.soundcloud.com" | "api-v2.soundcloud.com" | "soundcloud.com")
                    ))
                .then_some(id)
            })
            .collect();
        let response = self
            .responses
            .iter()
            .enumerate()
            .filter(|(_, response)| requests.contains(&response.id))
            .max_by_key(|(index, response)| (response.challenge, *index))
            .map(|(_, response)| response);
        (!requests.is_empty(), response)
    }

    pub fn playlist_links(
        &mut self,
        names: &[String],
        check: &impl Fn() -> Result<()>,
    ) -> Result<Vec<Option<String>>> {
        self.navigate("https://soundcloud.com/you/sets")?;
        self.wait(Duration::from_secs(8), check, |chrome| {
            let mut urls = Vec::with_capacity(names.len());
            for name in names {
                let links: Vec<String> = chrome.dom("links", &[json!(name)])?;
                let links = links
                    .iter()
                    .filter_map(|link| report::canonical_url(link))
                    .collect::<HashSet<_>>();
                urls.push(if links.len() == 1 {
                    links.into_iter().next()
                } else {
                    None
                });
            }
            Ok(urls.iter().any(Option::is_some).then_some(urls))
        })
        .map(|urls| urls.unwrap_or_default())
    }
}

fn chrome_error(error: CdpError) -> String {
    match error {
        CdpError::NoResponse | CdpError::ChannelSendError(_) => "Окно Chrome закрыто.".into(),
        CdpError::Timeout => "Chrome не отвечает.".into(),
        error => format!("Ошибка Chrome: {error}"),
    }
}

fn transient(error: &str) -> bool {
    error.contains("Cannot find context")
        || error.contains("Execution context was destroyed")
        || error.contains("Inspected target navigated")
}

fn close_process(runtime: &Runtime, browser: &mut Browser, process: &mut Child) {
    if !matches!(process.try_wait(), Ok(None)) {
        return;
    }
    let _ = runtime.block_on(async { timeout(Duration::from_secs(2), browser.close()).await });
    let until = Instant::now() + Duration::from_secs(3);
    while matches!(process.try_wait(), Ok(None)) && Instant::now() < until {
        thread::sleep(TICK);
    }
    if matches!(process.try_wait(), Ok(None)) {
        let _ = process.kill();
        let _ = process.wait();
    }
}

impl Drop for Chrome {
    fn drop(&mut self) {
        close_process(&self.runtime, &mut self.browser, &mut self.process);
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn network_challenge_requires_explicit_signal() {
        assert!(challenge_response(
            403,
            &json!({"X-DataDomeResponse": "403"})
        ));
        assert!(challenge_response(
            403,
            &json!({"Cf-Mitigated": "challenge"})
        ));
        assert!(!challenge_response(
            403,
            &json!({"cf-mitigated": "guardrails"})
        ));
        assert!(!challenge_response(429, &json!({"Retry-After": "10"})));
    }

    #[test]
    #[ignore = "requires an installed Google Chrome"]
    fn local_chrome_private_playlist_and_existing_playlist() {
        let profile = tempfile::tempdir().unwrap();
        let mut chrome = Chrome::launch_profile(profile.path(), &|| Ok(())).unwrap();
        let html = r#"
          <h1>Local test</h1>
          <div><a href="/track/1">Track</a><a href="/artist/2">Artist</a><time datetime="PT3M21S">3:21</time></div>
          <div class="searchList__item"><div class="searchItem__trackItem">
            <span class="soundTitle__username">Artist</span><span class="soundTitle__title">Track</span>
            <button aria-label="More" onclick="document.querySelector('#menu').hidden = false">…</button>
          </div></div>
          <button id="menu" hidden onclick="document.querySelector('.modal').hidden = false; this.hidden = true">Add to Playlist</button>
          <div class="modal showBackground" hidden>
            <button class="tabs__tab">Create a playlist</button>
            <label>Playlist title<input type="text"></label>
            <label>Private<input type="radio" value="private" name="privacy"></label>
            <label>Public<input type="radio" value="public" name="privacy" checked></label>
            <button onclick="document.querySelector('.modal').hidden = true">Save</button>
            <input placeholder="Filter playlists">
            <div class="addToPlaylistList__item">
              <a class="addToPlaylistItem__titleLink" href="https://soundcloud.com/test/sets/other">Local playlist</a>
              <button id="other" onclick="this.innerText = 'Added'">Add to Playlist</button>
            </div>
            <div class="addToPlaylistList__item">
              <a class="addToPlaylistItem__titleLink" href="https://soundcloud.com/test/sets/target/s-private">Local playlist</a>
              <button id="target" onclick="this.innerText = this.innerText === 'Add to Playlist' ? 'Remove from Playlist' : 'Add to Playlist'">Add to Playlist</button>
            </div>
          </div>
        "#;
        let set_fixture = format!("(document.body.innerHTML = {}, true)", json!(html));
        assert!(chrome.evaluate::<bool>(&set_fixture).unwrap());
        let source: Source = chrome.dom("source", &[]).unwrap();
        assert_eq!(source.tracks[0].title, "Track");
        assert_eq!(source.tracks[0].extra["duration"], 201);
        assert!(!chrome.captcha().unwrap());

        let candidate = Candidate {
            index: 0,
            artist: "Artist".into(),
            title: "Track".into(),
            url: None,
            duration: None,
        };
        let mut part = Part {
            name: "Local playlist".into(),
            created: false,
            url: None,
            occupied: None,
            dedup_only: false,
            public: false,
        };
        assert!(!chrome.prepare(&candidate, &part, &|| Ok(())).unwrap());
        assert!(
            chrome
                .evaluate::<bool>(r#"document.querySelector('input[value="private"]').checked"#)
                .unwrap()
        );
        assert!(chrome.save(&part, &|| Ok(())).is_ok());
        assert!(chrome.evaluate::<bool>(&set_fixture).unwrap());
        part.created = true;
        part.url = Some("https://soundcloud.com/test/sets/target".into());
        part.occupied = Some(499);
        assert!(!chrome.prepare(&candidate, &part, &|| Ok(())).unwrap());
        assert!(chrome.save(&part, &|| Ok(())).is_ok());
        let args = [json!(part.name), json!(part.url)];
        assert_eq!(
            chrome.dom::<String>("membership", &args).unwrap(),
            "removable"
        );
        assert_eq!(
            chrome
                .evaluate::<String>("document.querySelector('#other').innerText")
                .unwrap(),
            "Add to Playlist"
        );
        assert!(chrome.dom::<bool>("remove", &args).unwrap());
        assert_eq!(chrome.dom::<String>("membership", &args).unwrap(), "absent");
        assert!(
            chrome
                .evaluate::<bool>(
                    r#"(
            window.addClicks = 0,
            document.querySelector('#target').onclick = function () {
                window.addClicks += 1;
                this.innerText = 'Added';
                setTimeout(() => {
                    const prompt = document.createElement('div');
                    prompt.textContent = 'Slide the slider to the right';
                    document.body.append(prompt);
                }, 500);
            },
            true
        )"#
                )
                .unwrap()
        );
        assert!(matches!(
            chrome.save(&part, &|| Ok(())),
            Err(SaveFailure::Challenge)
        ));
        assert_eq!(chrome.evaluate::<u32>("window.addClicks").unwrap(), 1);
        assert!(chrome.evaluate::<bool>(&set_fixture).unwrap());
        assert!(
            chrome
                .evaluate::<bool>(
                    "(document.querySelector('#target').innerText = 'Remove from Playlist', true)"
                )
                .unwrap()
        );
        part.occupied = Some(500);
        let other = Part {
            url: Some("https://soundcloud.com/test/sets/other".into()),
            ..part.clone()
        };
        let unknown = Part {
            url: Some("https://soundcloud.com/test/sets/missing".into()),
            ..part.clone()
        };
        chrome.open_candidate(&candidate, &|| Ok(())).unwrap();
        assert!(matches!(
            chrome.probe_open_part(&other, &|| Ok(())).unwrap(),
            Some(Membership::Absent)
        ));
        assert!(matches!(
            chrome.probe_open_part(&part, &|| Ok(())).unwrap(),
            Some(Membership::Removable)
        ));
        assert!(
            chrome
                .probe_open_part(&unknown, &|| Ok(()))
                .unwrap()
                .is_none()
        );
        assert_eq!(
            chrome
                .evaluate::<String>("document.querySelector('#other').innerText")
                .unwrap(),
            "Add to Playlist"
        );
        close_process(&chrome.runtime, &mut chrome.browser, &mut chrome.process);
        assert!(chrome.disconnected());
        chrome.relaunch(&|| Ok(())).unwrap();
        assert_eq!(chrome.address().unwrap(), "about:blank");

        use std::{
            io::{Read, Write},
            net::TcpListener,
        };
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/fixture", listener.local_addr().unwrap());
        let server = thread::spawn(move || {
            let (mut connection, _) = listener.accept().unwrap();
            connection
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut request = [0; 4096];
            connection.read(&mut request).unwrap();
            connection.write_all(b"HTTP/1.1 429 Too Many Requests\r\nRetry-After: 17\r\nContent-Type: text/html\r\nContent-Length: 4\r\nConnection: close\r\n\r\nTest").unwrap();
        });
        assert!(matches!(
            chrome.search_start(&url),
            Err(SearchFailure::RateLimited(Some(17)))
        ));
        assert_eq!(chrome.address().unwrap(), url);
        server.join().unwrap();
    }
}
