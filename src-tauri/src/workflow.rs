use std::{
    collections::HashSet,
    fs::{self, File, OpenOptions},
    io::Write,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::Duration,
};

use chrono::Local;
use serde::Deserialize;
use serde_json::{Value, json};
use tauri::ipc::Channel;

use crate::{
    chrome::{self, Candidate, Chrome, SaveFailure, Search, SearchFailure},
    matching,
    report::{
        self, LIMIT, PENDING, Part, Replacement, ReplacementPhase, Report, Result, Track,
        UNCONFIRMED,
    },
};

const STOP: &str = "Перенос остановлен.";

fn retry_delay(error: &SearchFailure, attempt: usize) -> Option<u64> {
    match error {
        SearchFailure::Http(429) => [10, 25].get(attempt).copied(),
        SearchFailure::Http(408 | 500..=599)
        | SearchFailure::Browser(_)
        | SearchFailure::Unconfirmed
        | SearchFailure::Challenge => [2, 5].get(attempt).copied(),
        SearchFailure::Http(_) => None,
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Request {
    url: Option<String>,
    id: Option<String>,
    retry: Option<Vec<usize>>,
    review: Option<usize>,
    auto_exact: bool,
    auto_single: bool,
    auto_closest: bool,
    full_auto: bool,
}

#[cfg(test)]
mod tests {
    use super::{SearchFailure, retry_delay};

    #[test]
    fn transient_search_failures_are_bounded() {
        assert_eq!(retry_delay(&SearchFailure::Http(503), 0), Some(2));
        assert_eq!(retry_delay(&SearchFailure::Http(429), 1), Some(25));
        assert_eq!(retry_delay(&SearchFailure::Http(429), 2), None);
        assert_eq!(retry_delay(&SearchFailure::Http(403), 0), None);
    }
}

pub struct Log {
    pub path: Option<PathBuf>,
    file: Mutex<Option<File>>,
    lines: Mutex<Vec<(String, String)>>,
}

impl Log {
    pub fn new() -> Self {
        let path = report::data_dir().ok().map(|folder| {
            folder.join("Logs").join(format!(
                "MusicTransfer-{}.log",
                Local::now().format("%Y%m%d-%H%M%S-%6f")
            ))
        });
        let file = path.as_ref().and_then(|path| {
            fs::create_dir_all(path.parent()?).ok()?;
            OpenOptions::new()
                .create_new(true)
                .append(true)
                .open(path)
                .ok()
        });
        let path = if file.is_some() { path } else { None };
        Self {
            path,
            file: Mutex::new(file),
            lines: Mutex::new(Vec::new()),
        }
    }

    pub fn write(&self, level: &str, message: &str) -> (String, String) {
        match level {
            "ERROR" => log::error!("{message}"),
            "WARNING" => log::warn!("{message}"),
            _ => {}
        }
        let line = format!(
            "{}  {level}  {message}",
            Local::now().format("%Y-%m-%d %H:%M:%S")
        );
        let mut lines = self.lines.lock().unwrap();
        lines.push((level.into(), line.clone()));
        if lines.len() > 500 {
            lines.remove(0);
        }
        if let Some(file) = self.file.lock().unwrap().as_mut() {
            let _ = writeln!(file, "{line}");
        }
        (level.into(), line)
    }

    pub fn lines(&self) -> Vec<(String, String)> {
        self.lines.lock().unwrap().clone()
    }
}

pub struct Control {
    cancelled: AtomicBool,
    pub close_after: AtomicBool,
    prompt: Mutex<Option<(u64, mpsc::Sender<Value>)>>,
}

impl Control {
    pub fn new() -> Self {
        Self {
            cancelled: AtomicBool::new(false),
            close_after: AtomicBool::new(false),
            prompt: Mutex::new(None),
        }
    }

    pub fn answer(&self, id: u64, answer: Value) -> Result<()> {
        let mut prompt = self.prompt.lock().unwrap();
        if !matches!(prompt.as_ref(), Some((expected, _)) if *expected == id) {
            return Err("Это действие уже обработано.".into());
        }
        let (_, reply) = prompt.take().unwrap();
        reply.send(answer).map_err(|_| STOP.into())
    }

    pub fn stop(&self) {
        self.cancelled.store(true, Ordering::Release);
        self.prompt.lock().unwrap().take();
    }

    fn check(&self) -> Result<()> {
        if self.cancelled.load(Ordering::Acquire) {
            Err(STOP.into())
        } else {
            Ok(())
        }
    }
}

pub struct Job {
    request: Request,
    control: Arc<Control>,
    events: Channel<Value>,
    log: Arc<Log>,
    next_prompt: u64,
    record: Option<(PathBuf, Report)>,
    unchecked_parts: HashSet<usize>,
}

impl Job {
    pub fn new(
        request: Request,
        control: Arc<Control>,
        events: Channel<Value>,
        log: Arc<Log>,
    ) -> Self {
        Self {
            request,
            control,
            events,
            log,
            next_prompt: 0,
            record: None,
            unchecked_parts: HashSet::new(),
        }
    }

    fn emit(&self, event: Value) {
        let _ = self.events.send(event);
    }

    fn log(&self, level: &str, message: &str) {
        let (level, line) = self.log.write(level, message);
        self.emit(json!({"kind": "log", "level": level, "line": line}));
    }

    fn work(&self, text: &str) {
        self.log("INFO", text);
        self.emit(json!({"kind": "working", "text": text}));
    }

    fn ask(&mut self, mut event: Value) -> Result<Value> {
        let (sender, receiver) = mpsc::channel();
        self.next_prompt += 1;
        {
            let mut prompt = self.control.prompt.lock().unwrap();
            self.control.check()?;
            *prompt = Some((self.next_prompt, sender));
        }
        event["prompt"] = json!(self.next_prompt);
        if self.events.send(event).is_err() {
            self.control.stop();
            return Err(STOP.into());
        }
        let reply = receiver.recv().map_err(|_| STOP.to_owned())?;
        self.control.check()?;
        Ok(reply)
    }

    fn action(&mut self, title: &str, text: &str, actions: &[(&str, &str)]) -> Result<Value> {
        self.log("INFO", title);
        self.ask(json!({"kind": "action", "title": title, "text": text, "actions": actions}))
    }

    fn check_captcha(&mut self, chrome: &mut Chrome) -> Result<bool> {
        let has_challenge = chrome
            .wait(Duration::from_secs(3), &|| self.control.check(), |chrome| {
                chrome.captcha().map(Some)
            })?
            .unwrap_or(false);
        if !has_challenge {
            return Ok(false);
        }
        loop {
            self.action("Требуется проверка в Chrome",
                "Пройдите капчу в открытом окне Chrome. Если виден ползунок, передвиньте его вправо.",
                &[("Проверка пройдена", "done")])?;
            if !chrome.captcha()? {
                self.log("INFO", "Проверка завершена. Продолжаю перенос.");
                return Ok(true);
            }
        }
    }

    fn launch(&self) -> Result<Chrome> {
        self.work("Открываю Google Chrome…");
        Chrome::launch(&|| self.control.check())
    }

    fn pause(&self, seconds: u64) -> Result<()> {
        for _ in 0..seconds * 5 {
            self.control.check()?;
            thread::sleep(Duration::from_millis(200));
        }
        self.control.check()
    }

    fn recover_chrome(
        &mut self,
        chrome: &mut Chrome,
        address: &str,
        mut reason: String,
    ) -> Result<()> {
        loop {
            self.control.check()?;
            let reply = self.action(
                "Связь с Chrome прервана",
                &format!("{reason} Перенос продолжится с текущего действия."),
                &[
                    ("Открыть Chrome снова", "reopen"),
                    ("Завершить попытку", "stop"),
                ],
            )?;
            if reply != "reopen" {
                return Err(STOP.into());
            }
            self.work("Открываю Chrome снова…");
            match chrome
                .relaunch(&|| self.control.check())
                .and_then(|()| chrome.navigate(address))
            {
                Ok(()) => {
                    self.action(
                        "Chrome открыт снова",
                        "Убедитесь, что вход выполнен, и продолжите.",
                        &[("Продолжить", "continue")],
                    )?;
                    if !chrome.disconnected() {
                        return Ok(());
                    }
                    reason = "Окно Chrome снова закрылось.".into();
                }
                Err(error) => reason = error,
            }
        }
    }

    fn search_with_recovery(&mut self, chrome: &mut Chrome, query: &str) -> Result<Search> {
        let mut attempt = 0;
        loop {
            self.control.check()?;
            self.work(&format!("Ищу: {query}"));
            let error = match chrome.search(query, &|| self.control.check()) {
                Ok(search) => return Ok(search),
                Err(error) => error,
            };
            self.control.check()?;
            if chrome.disconnected() {
                self.recover_chrome(chrome, chrome::SOUNDCLOUD, error.to_string())?;
                attempt = 0;
                continue;
            }
            if self.check_captcha(chrome).unwrap_or(false) {
                continue;
            }
            let delay = retry_delay(&error, attempt);
            if let Some(seconds) = delay {
                self.log("WARNING", &error.to_string());
                self.work(&format!(
                    "Повторю поиск через {seconds} сек. Попытка {} из 3.",
                    attempt + 2
                ));
                self.pause(seconds)?;
                attempt += 1;
                continue;
            }
            self.log("WARNING", &error.to_string());
            let reply = self.action(
                "Поиск SoundCloud прерван",
                &format!("{error} Проверьте вход или соединение в Chrome."),
                &[("Повторить поиск", "retry"), ("Завершить попытку", "stop")],
            )?;
            if reply != "retry" {
                return Err(error.to_string());
            }
            attempt = 0;
        }
    }

    fn choose_candidate(
        &mut self,
        track: &Track,
        query: &str,
        candidates: &[Candidate],
        review: bool,
        editable: bool,
    ) -> Result<Value> {
        let options: Vec<_> = candidates
            .iter()
            .map(|candidate| {
                let mut row = json!(candidate);
                row["tags"] = json!(matching::version_tags(&candidate.title));
                row
            })
            .collect();
        self.ask(json!({
            "kind": "candidates", "label": track.label(), "options": options,
            "query": query, "retry": editable && !review, "review": review,
            "originalTitle": track.title, "defer": !review,
            "previous": review.then(|| track.result.trim_start_matches("добавлен:").trim()),
            "previousUrl": track.extra.get("soundcloud_url").and_then(Value::as_str).and_then(report::track_url),
            "originalUrl": report::original_url(&track.url),
            "originalDuration": track.extra.get("duration").and_then(Value::as_u64),
            "originalTags": matching::version_tags(&track.title),
        }))
    }

    fn login(&mut self, mut browser: Option<Chrome>) -> Result<Chrome> {
        loop {
            let state = browser.take().map_or_else(|| self.launch(), Ok);
            let mut opened = false;
            let mut hint =
                "Войдите в SoundCloud. Когда увидите свой аватар, нажмите «Начать поиск»."
                    .to_owned();
            let mut chrome = match state {
                Ok(mut chrome) => {
                    match chrome.navigate(chrome::SOUNDCLOUD) {
                        Ok(()) => opened = true,
                        Err(error) => hint = error,
                    }
                    Some(chrome)
                }
                Err(error) => {
                    self.control.check()?;
                    hint = error;
                    None
                }
            };
            let mut actions = vec![];
            if opened {
                actions.push(("Начать поиск", "start"));
            } else {
                self.log("WARNING", &hint);
            }
            actions.extend([
                ("Открыть Chrome снова", "reopen"),
                ("Войти в обычном Chrome", "manual"),
            ]);
            let choice = self.action("Вход в SoundCloud", &hint, &actions)?;
            match choice.as_str() {
                Some("start") if opened => {
                    if let Some(mut chrome) = chrome.take() {
                        if chrome.address().is_ok() {
                            self.check_captcha(&mut chrome)?;
                            return Ok(chrome);
                        }
                    }
                    self.log("WARNING", "Окно Chrome закрылось во время ожидания входа.");
                }
                Some("manual") => {
                    drop(chrome);
                    let mut process: Option<std::process::Child> = None;
                    let mut open = true;
                    let mut message = String::new();
                    loop {
                        self.control.check()?;
                        if open {
                            match chrome::manual_login() {
                                Ok(started) => {
                                    if process
                                        .as_mut()
                                        .is_none_or(|active| !matches!(active.try_wait(), Ok(None)))
                                    {
                                        process = Some(started);
                                    }
                                    message = "Войдите в аккаунт, дождитесь аватара и закройте все окна Chrome, открытые для входа.".into();
                                }
                                Err(error) => {
                                    self.log("ERROR", &error);
                                    message = error;
                                }
                            }
                        }
                        let choice = self.action(
                            "Вход в обычном Chrome",
                            &message,
                            &[
                                ("Я вошёл и закрыл Chrome", "done"),
                                ("Открыть окно снова", "reopen"),
                            ],
                        )?;
                        open = choice == "reopen";
                        if !open {
                            if process
                                .as_mut()
                                .is_some_and(|process| matches!(process.try_wait(), Ok(Some(_))))
                            {
                                break;
                            }
                            message = "Закройте окна Chrome, открытые для входа, или откройте окно снова.".into();
                        }
                    }
                }
                _ => {}
            }
        }
    }

    fn save_record(&mut self) -> Result<()> {
        let (path, report) = self.record.as_mut().unwrap();
        report::write(path, report)
    }

    fn choose_destination(&mut self, chrome: &mut Chrome) -> Result<()> {
        let mut choices = vec![];
        let mut error = None;
        loop {
            let reply =
                self.ask(json!({"kind": "destination", "options": choices, "error": error}))?;
            match reply.as_str() {
                Some("new") => break,
                Some("library") => {
                    self.work("Загружаю плейлисты SoundCloud…");
                    match chrome.library(&|| self.control.check()) {
                        Ok(rows) => {
                            choices = rows;
                            error = choices.is_empty().then(|| {
                                "Список пуст. Можно вставить ссылку на свой плейлист.".into()
                            });
                        }
                        Err(message) => {
                            self.control.check()?;
                            if chrome.disconnected() {
                                self.recover_chrome(chrome, chrome::SOUNDCLOUD, message)?;
                            } else {
                                error = Some(message);
                            }
                        }
                    }
                }
                Some(value) => {
                    let Some(url) = report::canonical_url(value.trim()) else {
                        error = Some(
                            "Укажите ссылку вида https://soundcloud.com/имя/sets/плейлист.".into(),
                        );
                        continue;
                    };
                    self.work("Проверяю плейлист и число треков…");
                    match chrome.playlist_details(&url, &|| self.control.check()) {
                        Ok(details) => {
                            let parts = report::linked_parts(&details.url)?;
                            let reply = self.action("Подтвердите плейлист SoundCloud", &format!(
                                "«{}»: сейчас {} треков. Дополнительных частей для проверки: {}. Уже присутствующие записи будут отмечены без повторного добавления.",
                                details.name, details.count, parts.len(),
                            ), &[("Перенести в этот плейлист", "confirm"), ("Выбрать другой", "back")])?;
                            if reply == "confirm" {
                                let report = &mut self.record.as_mut().unwrap().1;
                                report.playlists[0] = Part {
                                    name: details.name,
                                    created: true,
                                    url: Some(details.url),
                                    occupied: Some(details.count),
                                    dedup_only: false,
                                };
                                report.add_linked_parts(parts);
                                report
                                    .extra
                                    .insert("existing_destination".into(), json!(true));
                                break;
                            }
                            error = None;
                        }
                        Err(message) => {
                            self.control.check()?;
                            if chrome.disconnected() {
                                self.recover_chrome(chrome, chrome::SOUNDCLOUD, message)?;
                            } else {
                                self.check_captcha(chrome).ok();
                                error = Some(message);
                            }
                        }
                    }
                }
                None => return Err(STOP.into()),
            }
        }
        self.record
            .as_mut()
            .unwrap()
            .1
            .extra
            .remove("destination_pending");
        self.save_record()
    }

    fn refresh_existing(&mut self, chrome: &mut Chrome) -> Result<()> {
        let report = &self.record.as_ref().unwrap().1;
        if report.extra.get("existing_destination") != Some(&Value::Bool(true)) {
            return Ok(());
        }
        let Some(url) = report.playlists[0]
            .url
            .as_deref()
            .and_then(report::canonical_url)
        else {
            return Err("Не найдена ссылка на выбранный плейлист.".into());
        };
        match chrome.playlist_details(&url, &|| self.control.check()) {
            Ok(details) => {
                let part = &mut self.record.as_mut().unwrap().1.playlists[0];
                part.name = details.name;
                part.occupied = Some(details.count);
                self.save_record()
            }
            Err(error) => {
                self.control.check()?;
                self.log(
                    "WARNING",
                    &format!("Не удалось обновить число треков: {error}"),
                );
                Ok(())
            }
        }
    }

    fn progress(&self, index: usize) {
        let (_, report) = self.record.as_ref().unwrap();
        let (added_now, already_present, legacy_added) = report.added_counts();
        self.emit(
            json!({"kind": "progress", "index": index + 1, "total": report.tracks.len(),
            "added": report.added(), "skipped": report.skipped().len(), "added_now": added_now,
            "already_present": already_present, "legacy_added": legacy_added}),
        );
    }

    fn verify_membership(
        &mut self,
        chrome: &mut Chrome,
        track: Option<&str>,
        part: &mut Part,
    ) -> Result<Option<bool>> {
        let Some(track) = track.and_then(report::track_url) else {
            return Ok(None);
        };
        self.work("Сверяю запись в SoundCloud…");
        let found = (|| {
            if part
                .url
                .as_deref()
                .and_then(report::canonical_url)
                .is_none()
            {
                part.url = chrome
                    .playlist_links(std::slice::from_ref(&part.name), &|| self.control.check())?
                    .into_iter()
                    .next()
                    .flatten();
            }
            chrome.has_track(&track, part, &|| self.control.check())
        })();
        match found {
            Ok(present) => {
                self.log(
                    "INFO",
                    &format!(
                        "Автоматическая сверка «{}»: {}.",
                        part.name,
                        if present {
                            "трек уже есть"
                        } else {
                            "трека нет"
                        }
                    ),
                );
                Ok(Some(present))
            }
            Err(error) => {
                self.control.check()?;
                self.log(
                    "WARNING",
                    &format!("Автоматическая сверка недоступна: {error}"),
                );
                Ok(None)
            }
        }
    }

    fn reconcile(&mut self, chrome: &mut Chrome, index: usize) -> Result<()> {
        let (_, report) = self.record.as_ref().unwrap();
        let track = &report.tracks[index];
        let part_index = track.part_index().ok_or("Неверная часть плейлиста.")?;
        let mut part = report
            .playlists
            .get(part_index)
            .ok_or("В отчёте указана неизвестная часть для сверки.")?
            .clone();
        let track_url = track
            .extra
            .get("pending_url")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let label = track
            .extra
            .get("pending_candidate")
            .and_then(Value::as_str)
            .map_or_else(|| track.label(), str::to_owned);
        let text = format!(
            "Проверьте в SoundCloud, есть ли «{label}» в плейлисте «{}».",
            part.name
        );
        let mut actions = vec![
            ("Трек уже в плейлисте", "added"),
            ("Плейлист есть, трека нет", "existing"),
        ];
        if !part.created {
            actions.push(("Плейлист не создан", "missing"));
        }
        actions.push(("Открыть плейлист в Chrome", "open"));
        let verified = self.verify_membership(chrome, track_url.as_deref(), &mut part)?;
        let url = part.url.as_deref().and_then(report::canonical_url);
        let reply = match verified {
            Some(true) => json!("added"),
            Some(false) => json!("existing"),
            None => loop {
                let reply = self.action("Проверьте последнее добавление", &text, &actions)?;
                if reply != "open" {
                    break reply;
                }
                self.open_for_reconcile(chrome, url.as_deref())?;
            },
        };
        let (_, report) = self.record.as_mut().unwrap();
        let track = &mut report.tracks[index];
        if reply == "added" {
            track.result = format!("добавлен: {label}");
            track.extra.remove("retry_origin");
            track.extra.remove("search_error");
            track.extra.remove("deferred");
            track.extra.remove("deferred_query");
            track.extra.insert("already_present".into(), json!(false));
            if let Some(url) = track.extra.remove("pending_url") {
                track.extra.insert("soundcloud_url".into(), url);
            }
            if let Some(method) = track.extra.remove("pending_method") {
                track.extra.insert("match_method".into(), method);
            }
            if let Some(occupied) = report.playlists[part_index].occupied.as_mut() {
                *occupied = occupied.saturating_add(1);
            }
        } else {
            track.result = track
                .extra
                .get("retry_origin")
                .and_then(Value::as_str)
                .unwrap_or(PENDING)
                .into();
            track.extra.remove("playlist_part");
        }
        track.extra.remove("pending_candidate");
        track.extra.remove("pending_url");
        track.extra.remove("pending_method");
        report.playlists[part_index].created = reply != "missing";
        if reply != "missing" {
            report.playlists[part_index].url = url;
        }
        self.log(
            "INFO",
            &format!(
                "Сверено добавление «{label}»: {}",
                reply.as_str().unwrap_or_default()
            ),
        );
        self.save_record()
    }

    fn source(&mut self, url: &str) -> Result<Chrome> {
        let mut chrome = self.launch()?;
        if let Err(error) = chrome.navigate(url) {
            self.recover_chrome(&mut chrome, url, error)?;
        }
        self.action("Вход в Яндекс Музыку",
            "Войдите в открывшемся окне Chrome и убедитесь, что плейлист открыт и фильтр по трекам пуст.",
            &[("Считать треки", "read")])?;
        if chrome.disconnected() {
            self.recover_chrome(
                &mut chrome,
                url,
                "Окно Chrome закрылось при входе в Яндекс Музыку.".into(),
            )?;
        }
        self.check_captcha(&mut chrome)?;
        if report::source_url(&chrome.address()?).is_none() {
            chrome.navigate(url)?;
        }
        self.work("Считываю треки из Яндекс Музыки…");
        let (title, tracks) = loop {
            match chrome.tracks(&|| self.control.check()) {
                Ok(source) => break source,
                Err(error) => {
                    self.control.check()?;
                    if chrome.disconnected() {
                        self.recover_chrome(&mut chrome, url, error)?;
                        continue;
                    }
                    if !self.check_captcha(&mut chrome)? {
                        return Err(error);
                    }
                    chrome.navigate(url)?;
                }
            }
        };
        let labels: Vec<_> = tracks.iter().map(|track| track.label()).collect();
        self.ask(json!({"kind": "source", "title": title, "tracks": labels}))?;
        let stamp = Local::now().format("%Y%m%d-%H%M%S-%6f").to_string();
        let path = report::data_dir()?
            .join("Reports")
            .join(format!("перенос-{stamp}.json"));
        let mut report = Report::new(url.into(), title, &stamp, tracks);
        report
            .extra
            .insert("destination_pending".into(), json!(true));
        report::write(&path, &mut report)?;
        self.record = Some((path, report));
        self.log("INFO", "Список треков сохранён для продолжения.");
        Ok(chrome)
    }

    fn run(&mut self) -> Result<&'static str> {
        let mut selected = None;
        let mut source_browser = None;
        if let Some(id) = &self.request.id {
            let path = report::resolve(id)?;
            let report = report::read(&path)?;
            if let Some(number) = self.request.review {
                if report.tracks.iter().any(Track::needs_reconcile) {
                    return Err("Сначала выполните сверку через «Продолжить перенос».".into());
                }
                if number
                    .checked_sub(1)
                    .and_then(|index| report.tracks.get(index))
                    .is_none_or(|track| !track.result.starts_with("добавлен:"))
                {
                    return Err("В отчёте нет выбранного добавленного трека.".into());
                }
            } else if let Some(numbers) = &self.request.retry {
                if report.tracks.iter().any(Track::needs_reconcile) {
                    return Err("Сначала выполните сверку через «Продолжить перенос».".into());
                }
                let numbers = numbers
                    .iter()
                    .copied()
                    .filter(|number| {
                        number
                            .checked_sub(1)
                            .and_then(|index| report.tracks.get(index))
                            .is_some_and(|track| {
                                matches!(
                                    track.status(),
                                    "pending" | "skipped" | "error" | "deferred"
                                )
                            })
                    })
                    .collect::<HashSet<_>>();
                if numbers.is_empty() {
                    return Err("Для выбранных треков больше не требуется поиск.".into());
                }
                self.log(
                    "INFO",
                    &format!("Ручной повтор поиска: {} треков · {id}", numbers.len()),
                );
                selected = Some(numbers);
            } else {
                self.log("INFO", &format!("Продолжение переноса · {id}"));
            }
            self.record = Some((path, report));
        } else {
            let url = self
                .request
                .url
                .as_deref()
                .and_then(report::source_url)
                .ok_or("Вставьте HTTPS-ссылку на плейлист Яндекс Музыки.")?;
            source_browser = Some(self.source(url.as_str())?);
        }

        let mut chrome = self.login(source_browser)?;
        if self
            .record
            .as_ref()
            .unwrap()
            .1
            .extra
            .get("destination_pending")
            == Some(&Value::Bool(true))
        {
            self.choose_destination(&mut chrome)?;
        }
        if let Some(number) = self.request.review {
            self.review_track(&mut chrome, number - 1)?;
            return Ok("Проверка трека завершена.");
        }
        let count = self.record.as_ref().unwrap().1.tracks.len();
        for index in 0..count {
            if self.record.as_ref().unwrap().1.tracks[index].result == UNCONFIRMED {
                self.reconcile(&mut chrome, index)?;
            }
            if self.record.as_ref().unwrap().1.tracks[index]
                .replacement()
                .is_some()
            {
                self.finish_replacement(&mut chrome, index)?;
            }
        }
        if self.request.id.is_some() {
            let report = &mut self.record.as_mut().unwrap().1;
            if let Some(root) = report
                .playlists
                .first()
                .and_then(|part| part.url.as_deref())
                .and_then(report::canonical_url)
            {
                if report.add_linked_parts(report::linked_parts(&root)?) {
                    self.save_record()?;
                }
            }
            self.refresh_existing(&mut chrome)?;
        }
        for later in [false, true] {
            for index in 0..count {
                self.control.check()?;
                let track = &self.record.as_ref().unwrap().1.tracks[index];
                let manual = selected.is_some()
                    || track.deferred()
                    || track.extra.contains_key("retry_origin");
                if track.deferred() != later
                    || selected
                        .as_ref()
                        .is_some_and(|numbers| !numbers.contains(&(index + 1)))
                    || (!manual && track.result != PENDING)
                {
                    continue;
                }
                self.progress(index);
                self.transfer_track(&mut chrome, index, manual)?;
                self.progress(index);
            }
        }
        let indices: Vec<_> = self
            .record
            .as_ref()
            .unwrap()
            .1
            .playlists
            .iter()
            .enumerate()
            .filter_map(|(index, part)| (part.created && part.url.is_none()).then_some(index))
            .collect();
        if !indices.is_empty() {
            self.work("Ищу ссылки на готовые плейлисты…");
            let names: Vec<_> = indices
                .iter()
                .map(|&index| {
                    self.record.as_ref().unwrap().1.playlists[index]
                        .name
                        .clone()
                })
                .collect();
            match chrome.playlist_links(&names, &|| self.control.check()) {
                Ok(links) => {
                    for (index, link) in indices.into_iter().zip(links) {
                        self.record.as_mut().unwrap().1.playlists[index].url = link;
                    }
                    self.save_record()?;
                }
                Err(error) => self.log("WARNING", &error),
            }
        }
        self.control.check()?;
        Ok(
            if self
                .record
                .as_ref()
                .unwrap()
                .1
                .tracks
                .iter()
                .any(Track::deferred)
            {
                "Есть отложенные треки. Вернитесь к ним через «Продолжить перенос»."
            } else if selected.is_some() {
                "Повторный поиск завершён."
            } else {
                "Перенос завершён."
            },
        )
    }

    fn transfer_track(&mut self, chrome: &mut Chrome, index: usize, manual: bool) -> Result<()> {
        let track = self.record.as_ref().unwrap().1.tracks[index].clone();
        let label = track.label();
        let mut query = track
            .extra
            .get("deferred_query")
            .and_then(Value::as_str)
            .map_or_else(|| track.query(), str::to_owned);
        let mut user_search = manual;
        loop {
            self.control.check()?;
            let search = match self.search_with_recovery(chrome, &query) {
                Ok(search) => search,
                Err(error) => {
                    self.control.check()?;
                    self.record.as_mut().unwrap().1.tracks[index]
                        .extra
                        .insert("search_error".into(), json!(error));
                    self.save_record()?;
                    return Err(error);
                }
            };
            let candidates = &search.rows;
            self.log(
                "INFO",
                &format!("Доступных вариантов: {}", candidates.len()),
            );
            let mut choice = None;
            if !user_search {
                if self.request.auto_exact {
                    choice = matching::exact_match(&track, candidates);
                }
                if choice.is_none()
                    && ((self.request.auto_single && candidates.len() == 1)
                        || (self.request.auto_closest && candidates.len() > 1))
                {
                    choice = matching::closest(&track, candidates, false);
                }
                if choice.is_none() && self.request.full_auto {
                    choice =
                        matching::closest(&track, &candidates[..candidates.len().min(3)], true);
                }
                if let Some(index) = choice {
                    self.log(
                        "INFO",
                        &format!("Выбран автоматически: {}", candidates[index].label()),
                    );
                }
            }
            let prompted = user_search
                || (choice.is_none() && !candidates.is_empty() && !self.request.full_auto);
            if prompted {
                let answer = self.choose_candidate(&track, &query, candidates, false, manual)?;
                if answer["defer"] == true {
                    let target = &mut self.record.as_mut().unwrap().1.tracks[index];
                    target.extra.insert("deferred".into(), json!(true));
                    target.extra.insert("deferred_query".into(), json!(query));
                    target.extra.remove("search_error");
                    self.save_record()?;
                    self.log("INFO", &format!("Отложен трек: {label}"));
                    return Ok(());
                }
                if let Some(text) = answer
                    .as_str()
                    .map(str::trim)
                    .filter(|text| !text.is_empty())
                {
                    query = text.to_owned();
                    user_search = true;
                    continue;
                }
                choice = answer
                    .as_u64()
                    .map(|number| number as usize)
                    .filter(|number| *number < candidates.len());
                user_search |= choice.is_some();
            }
            let Some(candidate) = choice.and_then(|choice| candidates.get(choice)) else {
                let result = if search.missing {
                    "трек не найден в SoundCloud"
                } else if candidates.is_empty() {
                    "нет доступных вариантов в SoundCloud"
                } else if !user_search && self.request.full_auto {
                    "пропущен автоматически: нет совпадений среди первых трёх"
                } else {
                    "пропущен пользователем"
                };
                self.finish_track(index, result)?;
                self.log(
                    "INFO",
                    &format!("ПРОПУСК · {} · {label} · {result}", index + 1),
                );
                return Ok(());
            };
            self.work(&format!("Добавляю: {label}"));
            let method = if prompted { "manual" } else { "auto" };
            let Some((part_index, part, already_added)) =
                self.prepare_destination(chrome, candidate)?
            else {
                continue;
            };
            if !already_added {
                self.pause(1)?;
                if chrome.disconnected() {
                    self.recover_chrome(
                        chrome,
                        chrome::SOUNDCLOUD,
                        "Chrome закрылся до отправки.".into(),
                    )?;
                    continue;
                }
                if self.check_captcha(chrome)? {
                    continue;
                }
                let target = &mut self.record.as_mut().unwrap().1.tracks[index];
                target.result = UNCONFIRMED.into();
                target
                    .extra
                    .insert("pending_candidate".into(), json!(candidate.label()));
                target.extra.insert("pending_method".into(), json!(method));
                if let Some(url) = &candidate.url {
                    target.extra.insert("pending_url".into(), json!(url));
                }
                target
                    .extra
                    .insert("playlist_part".into(), json!(part_index + 1));
                if manual {
                    target.extra.insert(
                        "retry_origin".into(),
                        json!(
                            track
                                .extra
                                .get("retry_origin")
                                .and_then(Value::as_str)
                                .unwrap_or(&track.result)
                        ),
                    );
                }
                self.save_record()?;
                if let Err(failure) = chrome.save(&part) {
                    match failure {
                        SaveFailure::Unknown(error) => {
                            self.log("WARNING", &error);
                            self.check_unknown(chrome, error)?;
                            self.reconcile(chrome, index)?;
                            if self.record.as_ref().unwrap().1.tracks[index]
                                .result
                                .starts_with("добавлен:")
                            {
                                return Ok(());
                            }
                        }
                        SaveFailure::Rejected(error) => {
                            let target = &mut self.record.as_mut().unwrap().1.tracks[index];
                            *target = track.clone();
                            self.save_record()?;
                            self.retry_action(chrome, "SoundCloud отклонил добавление", error)?;
                            self.refresh_existing(chrome)?;
                        }
                    }
                    continue;
                }
            }
            let report = &mut self.record.as_mut().unwrap().1;
            report.playlists[part_index].created = true;
            if !already_added {
                if let Some(occupied) = report.playlists[part_index].occupied.as_mut() {
                    *occupied = occupied.saturating_add(1);
                }
            }
            let target = &mut report.tracks[index];
            target
                .extra
                .insert("playlist_part".into(), json!(part_index + 1));
            target
                .extra
                .insert("already_present".into(), json!(already_added));
            target.extra.insert("match_method".into(), json!(method));
            if let Some(url) = &candidate.url {
                target.extra.insert("soundcloud_url".into(), json!(url));
            }
            let result = format!("добавлен: {}", candidate.label());
            self.finish_track(index, &result)?;
            self.log(
                "INFO",
                &format!(
                    "{} · {label} · {}",
                    index + 1,
                    if already_added {
                        "Уже был в плейлисте"
                    } else {
                        &result
                    }
                ),
            );
            return Ok(());
        }
    }

    fn retry_action(&mut self, chrome: &mut Chrome, title: &str, error: String) -> Result<()> {
        self.control.check()?;
        self.log("WARNING", &error);
        if chrome.disconnected() {
            return self.recover_chrome(chrome, chrome::SOUNDCLOUD, error);
        }
        if self.check_captcha(chrome).unwrap_or(false) {
            return Ok(());
        }
        let reply = self.action(
            title,
            &error,
            &[
                ("Повторить попытку", "retry"),
                ("Завершить попытку", "stop"),
            ],
        )?;
        if reply == "retry" { Ok(()) } else { Err(error) }
    }

    fn check_unknown(&mut self, chrome: &mut Chrome, error: String) -> Result<()> {
        self.control.check()?;
        if chrome.disconnected() {
            return self.recover_chrome(chrome, chrome::SOUNDCLOUD, error);
        }
        self.check_captcha(chrome).ok();
        self.control.check()
    }

    fn open_for_reconcile(&mut self, chrome: &mut Chrome, url: Option<&str>) -> Result<()> {
        let url = url.unwrap_or("https://soundcloud.com/you/sets");
        if let Err(error) = chrome.navigate(url) {
            self.recover_chrome(chrome, url, error)?;
        }
        self.check_captcha(chrome)?;
        Ok(())
    }

    fn prepare_destination(
        &mut self,
        chrome: &mut Chrome,
        candidate: &Candidate,
    ) -> Result<Option<(usize, Part, bool)>> {
        let report = &mut self.record.as_mut().unwrap().1;
        let part_index = (0..report.playlists.len())
            .find(|&index| !report.playlists[index].dedup_only && report.occupied(index) < LIMIT)
            .unwrap_or(report.playlists.len());
        if part_index == report.playlists.len() {
            report.playlists.push(Part {
                name: report::part_name(&report.playlist, part_index + 1),
                created: false,
                url: None,
                occupied: None,
                dedup_only: false,
            });
        }
        let part = report.playlists[part_index].clone();
        let other_parts: Vec<_> = report
            .playlists
            .iter()
            .enumerate()
            .filter(|(index, part)| {
                part.created && *index != part_index && !self.unchecked_parts.contains(index)
            })
            .map(|(index, part)| {
                (
                    index,
                    Part {
                        occupied: Some(report.occupied(index)),
                        ..part.clone()
                    },
                )
            })
            .collect();
        if let Err(error) = chrome.open_candidate(candidate, &|| self.control.check()) {
            self.retry_action(chrome, "SoundCloud не открыл форму добавления", error)?;
            return Ok(None);
        }
        let mut unavailable = Vec::new();
        for (index, other) in other_parts {
            match chrome.probe_open_part(&other, &|| self.control.check()) {
                Ok(Some(state)) if state.present() => return Ok(Some((index, other, true))),
                Ok(Some(_)) => {}
                Ok(None) => unavailable.push((index, other.name)),
                Err(error) => {
                    self.retry_action(chrome, "Не удалось проверить дубликат", error)?;
                    return Ok(None);
                }
            }
        }
        match chrome.prepare_open_part(&part, &|| self.control.check()) {
            Ok(true) => return Ok(Some((part_index, part, true))),
            Ok(false) => {}
            Err(error) => {
                self.retry_action(chrome, "SoundCloud не открыл форму добавления", error)?;
                return Ok(None);
            }
        }
        if !unavailable.is_empty() {
            let names = unavailable
                .iter()
                .map(|(_, name)| name.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            let reply = self.action("Не удалось проверить части плейлиста", &format!(
                "SoundCloud не показал эти части: {names}. Их можно исключить из проверки дубликатов до конца текущего запуска."),
                &[("Повторить проверку", "retry"), ("Продолжить без этих частей", "exclude")])?;
            if reply != "exclude" {
                return Ok(None);
            }
            self.unchecked_parts
                .extend(unavailable.into_iter().map(|(index, _)| index));
            self.log(
                "WARNING",
                &format!("Части исключены из сверки дубликатов: {names}"),
            );
        }
        Ok(Some((part_index, part, false)))
    }

    fn review_track(&mut self, chrome: &mut Chrome, index: usize) -> Result<()> {
        let report = &self.record.as_ref().unwrap().1;
        let track = report.tracks[index].clone();
        let part_index = track
            .part_index()
            .ok_or("В отчёте нет номера части плейлиста.")?;
        let part = report
            .playlists
            .get(part_index)
            .ok_or("В отчёте отсутствует плейлист трека.")?
            .clone();
        if !part.created {
            return Err("В отчёте не подтверждено создание плейлиста этого трека.".into());
        }
        let previous = track.result.trim_start_matches("добавлен:").trim();
        let previous_url = track.extra.get("soundcloud_url").and_then(Value::as_str);
        let mut query = track.query();
        loop {
            let search = self.search_with_recovery(chrome, &query)?;
            let answer = self.choose_candidate(&track, &query, &search.rows, true, true)?;
            if let Some(text) = answer
                .as_str()
                .map(str::trim)
                .filter(|text| !text.is_empty())
            {
                query = text.to_owned();
                continue;
            }
            let Some(candidate) = answer
                .as_u64()
                .and_then(|number| search.rows.get(number as usize))
            else {
                self.log(
                    "INFO",
                    &format!("Проверено: {} · оставлена текущая версия.", track.label()),
                );
                return Ok(());
            };
            if previous_url.is_some_and(|url| candidate.url.as_deref() == Some(url))
                || (previous_url.is_none() && candidate.label() == previous)
            {
                self.log(
                    "INFO",
                    &format!("Проверено: {} · выбрана текущая версия.", track.label()),
                );
                return Ok(());
            }
            let confirm = self.action("Подтвердите замену", &format!(
                "В «{}» будет добавлен «{}». После этого приложение предложит убрать прежний трек «{previous}» в SoundCloud.",
                part.name, candidate.label(),
            ), &[("Добавить новую версию", "add"), ("Вернуться к поиску", "back"), ("Оставить текущую", "keep")])?;
            if confirm == "keep" {
                return Ok(());
            }
            if confirm != "add" {
                continue;
            }
            let already_added = match chrome.prepare(candidate, &part, &|| self.control.check()) {
                Ok(value) => value,
                Err(error) => {
                    self.retry_action(chrome, "Не удалось открыть форму замены", error)?;
                    continue;
                }
            };
            if !already_added && self.record.as_ref().unwrap().1.occupied(part_index) >= LIMIT {
                return Err("В этой части 500 треков. Для безопасной замены сначала освободите место, сохранив прежнюю версию.".into());
            }
            if !already_added {
                self.pause(1)?;
                if chrome.disconnected() {
                    self.recover_chrome(
                        chrome,
                        chrome::SOUNDCLOUD,
                        "Chrome закрылся до отправки.".into(),
                    )?;
                    continue;
                }
                if self.check_captcha(chrome)? {
                    continue;
                }
            }
            let mut replacement = Replacement {
                label: candidate.label(),
                url: candidate.url.clone(),
                added_slot: !already_added,
                phase: if already_added {
                    ReplacementPhase::RemovePrevious
                } else {
                    ReplacementPhase::Adding
                },
            };
            self.save_replacement(index, Some(&replacement))?;
            if !already_added {
                match chrome.save(&part) {
                    Ok(()) => {
                        replacement.phase = ReplacementPhase::RemovePrevious;
                        self.save_replacement(index, Some(&replacement))?;
                    }
                    Err(SaveFailure::Rejected(error)) => {
                        self.save_replacement(index, None)?;
                        self.retry_action(chrome, "SoundCloud отклонил замену", error)?;
                        continue;
                    }
                    Err(SaveFailure::Unknown(error)) => self.check_unknown(chrome, error)?,
                }
            }
            return self.finish_replacement(chrome, index);
        }
    }

    fn save_replacement(&mut self, index: usize, replacement: Option<&Replacement>) -> Result<()> {
        let extra = &mut self.record.as_mut().unwrap().1.tracks[index].extra;
        if let Some(replacement) = replacement {
            extra.insert("replacement".into(), json!(replacement));
        } else {
            extra.remove("replacement");
        }
        self.save_record()
    }

    fn finish_replacement(&mut self, chrome: &mut Chrome, index: usize) -> Result<()> {
        let report = &self.record.as_ref().unwrap().1;
        let track = &report.tracks[index];
        let Some(mut replacement) = track.replacement() else {
            return Ok(());
        };
        let part_index = track
            .part_index()
            .ok_or("В отчёте нет номера части плейлиста.")?;
        let part = report
            .playlists
            .get(part_index)
            .ok_or("В отчёте отсутствует плейлист трека.")?;
        let previous = track
            .result
            .trim_start_matches("добавлен:")
            .trim()
            .to_owned();
        let part_name = part.name.clone();
        let mut part = part.clone();
        let previous_url = track
            .extra
            .get("soundcloud_url")
            .and_then(Value::as_str)
            .and_then(report::track_url);
        let shared = previous_url.as_ref().is_some_and(|url| {
            report
                .tracks
                .iter()
                .enumerate()
                .any(|(other_index, other)| {
                    index != other_index
                        && other.result.starts_with("добавлен:")
                        && other.part_index() == Some(part_index)
                        && other.extra.get("soundcloud_url").and_then(Value::as_str)
                            == Some(url.as_str())
                })
        });
        let previously_added = track.already_present() == Some(false);
        if replacement.phase == ReplacementPhase::Adding {
            let verified = self.verify_membership(chrome, replacement.url.as_deref(), &mut part)?;
            let present = if let Some(present) = verified {
                present
            } else {
                loop {
                    let reply = self.action(
                        "Проверьте новую версию",
                        &format!(
                            "Убедитесь, что «{}» есть в «{part_name}». Прежний трек пока сохранён.",
                            replacement.label
                        ),
                        &[
                            ("Новая версия есть", "added"),
                            ("Новой версии нет", "missing"),
                            ("Открыть плейлист в Chrome", "open"),
                        ],
                    )?;
                    match reply.as_str() {
                        Some("open") => {
                            let url = part.url.as_deref().and_then(report::canonical_url);
                            self.open_for_reconcile(chrome, url.as_deref())?;
                        }
                        Some("added") => break true,
                        _ => break false,
                    }
                }
            };
            if !present {
                return self.save_replacement(index, None);
            }
            replacement.phase = ReplacementPhase::RemovePrevious;
            self.record.as_mut().unwrap().1.playlists[part_index].url = part.url.clone();
            self.save_replacement(index, Some(&replacement))?;
        }
        let playlist_url = part.url.as_deref().and_then(report::canonical_url);
        let mut auto_remove = previous_url.is_some()
            && replacement.url.is_some()
            && playlist_url.is_some()
            && !shared
            && previously_added;
        let mut warning = String::new();
        let removed = loop {
            let mut actions = vec![("Открыть плейлист в Chrome", "open")];
            if auto_remove {
                actions.insert(0, ("Удалить прежнюю версию", "auto"));
            }
            if !shared {
                actions.push(("Я удалил прежнюю версию", "removed"));
            }
            actions.push(("Оставить обе версии", "keep"));
            let instructions = if shared {
                "Прежняя версия также указана для другого трека из этого переноса. Оставьте обе записи."
            } else if auto_remove {
                "Подтвердите удаление прежней версии из плейлиста или оставьте обе записи."
            } else {
                "Чтобы завершить замену, откройте Edit → Tracks в SoundCloud, удалите прежнюю версию и сохраните."
            };
            let reply = self.action("Завершите замену в SoundCloud",
                &format!("Плейлист «{part_name}». Новая версия: «{}». Прежняя: «{previous}». {instructions}{warning}", replacement.label),
                &actions)?;
            match reply.as_str() {
                Some("open") => self.open_for_reconcile(chrome, playlist_url.as_deref())?,
                Some("auto") if auto_remove => {
                    self.work("Проверяю обе версии перед удалением…");
                    match chrome.remove_previous(
                        previous_url.as_deref().unwrap(),
                        replacement.url.as_deref().unwrap(),
                        &part,
                        &|| self.control.check(),
                    ) {
                        Ok(()) => break true,
                        Err(error) => {
                            self.control.check()?;
                            self.log("WARNING", &error);
                            warning = format!(" {error}");
                            auto_remove = false;
                        }
                    }
                }
                Some("removed") if !shared => break true,
                _ => break false,
            }
        };
        let report = &mut self.record.as_mut().unwrap().1;
        let occupied = report.occupied(part_index) + usize::from(replacement.added_slot);
        report.playlists[part_index].occupied = Some(occupied.saturating_sub(usize::from(removed)));
        let track = &mut report.tracks[index];
        track.result = format!("добавлен: {}", replacement.label);
        track.extra.remove("soundcloud_url");
        track
            .extra
            .insert("already_present".into(), json!(!replacement.added_slot));
        track.extra.insert("match_method".into(), json!("manual"));
        if let Some(url) = replacement.url {
            track.extra.insert("soundcloud_url".into(), json!(url));
        }
        track
            .extra
            .insert("previous_version".into(), json!(previous));
        track
            .extra
            .insert("previous_removed".into(), json!(removed));
        self.save_replacement(index, None)?;
        self.log(
            "INFO",
            if removed {
                "Замена трека подтверждена."
            } else {
                "Обе версии сохранены."
            },
        );
        Ok(())
    }

    fn finish_track(&mut self, index: usize, result: &str) -> Result<()> {
        let track = &mut self.record.as_mut().unwrap().1.tracks[index];
        track.result = result.into();
        for key in [
            "pending_candidate",
            "pending_url",
            "pending_method",
            "retry_origin",
            "search_error",
            "deferred",
            "deferred_query",
        ] {
            track.extra.remove(key);
        }
        self.save_record()
    }

    pub fn execute(&mut self) -> Value {
        let text = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.run())) {
            Ok(Ok(message)) => message.to_owned(),
            Ok(Err(error)) if error == STOP => error,
            Ok(Err(error)) => {
                self.log("ERROR", &error);
                format!("Перенос остановлен: {error}")
            }
            Err(error) => {
                let detail = error
                    .downcast_ref::<String>()
                    .map(String::as_str)
                    .or_else(|| error.downcast_ref::<&str>().copied())
                    .unwrap_or("неизвестная ошибка");
                self.log("ERROR", &format!("Необработанная ошибка: {detail}"));
                "Перенос остановлен из-за ошибки приложения. Подробности в журнале.".into()
            }
        };
        let report = self.record.as_ref().and_then(|(path, _)| {
            let id = path.file_name()?.to_str()?.to_owned();
            report::read(path).ok().map(|report| report.summary(id))
        });
        self.log("INFO", &text);
        json!({"kind": "done", "text": text, "report": report})
    }
}
