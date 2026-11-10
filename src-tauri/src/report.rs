use std::{
    collections::HashSet,
    env, fs,
    io::Write,
    path::{Path, PathBuf},
};

use chrono::NaiveDateTime;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use url::Url;

pub type Result<T> = std::result::Result<T, String>;
pub const PENDING: &str = "не обработан";
pub const UNCONFIRMED: &str = "добавление не подтверждено: проверьте плейлист в SoundCloud";
pub const LIMIT: usize = 500;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Track {
    #[serde(default)]
    pub url: String,
    pub title: String,
    #[serde(default)]
    pub artists: Vec<String>,
    #[serde(default = "pending")]
    pub result: String,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

fn pending() -> String {
    PENDING.into()
}

impl Track {
    pub fn label(&self) -> String {
        format!("{} — {}", self.artists.join(", "), self.title)
    }

    pub fn query(&self) -> String {
        format!("{} {}", self.artists.join(" "), self.title)
            .trim()
            .to_owned()
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Part {
    pub name: String,
    pub created: bool,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub occupied: Option<usize>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub dedup_only: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Replacement {
    pub label: String,
    pub url: Option<String>,
    pub phase: ReplacementPhase,
    pub added_slot: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplacementPhase {
    Adding,
    RemovePrevious,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Report {
    pub source: String,
    #[serde(default)]
    pub source_title: String,
    pub playlist: String,
    #[serde(default)]
    pub created: bool,
    #[serde(default)]
    pub playlist_url: Option<String>,
    #[serde(default)]
    pub playlists: Vec<Part>,
    pub tracks: Vec<Track>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Serialize)]
pub struct Summary {
    pub id: String,
    pub date: String,
    pub title: String,
    pub source: String,
    pub added: usize,
    pub added_now: usize,
    pub already_present: usize,
    pub legacy_added: usize,
    pub skipped: usize,
    pub total: usize,
    pub status: &'static str,
    pub resumable: bool,
    pub reconcile: bool,
    pub links: Vec<(String, String)>,
}

#[derive(Serialize)]
pub struct Skipped {
    pub number: usize,
    pub artist: String,
    pub title: String,
    pub reason: &'static str,
}

#[derive(Serialize)]
pub struct Added {
    pub number: usize,
    pub title: String,
    pub artist: String,
    pub version: String,
    pub url: Option<String>,
    pub playlist: Option<String>,
}

#[derive(Serialize)]
pub struct TrackRow {
    pub number: usize,
    pub title: String,
    pub artist: String,
    pub status: &'static str,
    pub method: Option<&'static str>,
    pub detail: String,
    pub source_url: Option<String>,
    pub soundcloud_url: Option<String>,
    pub playlist: Option<(String, Option<String>)>,
}

impl Track {
    pub fn part_index(&self) -> Option<usize> {
        let part = self
            .extra
            .get("playlist_part")
            .map_or(Some(1), Value::as_u64)?;
        usize::try_from(part).ok()?.checked_sub(1)
    }

    pub fn replacement(&self) -> Option<Replacement> {
        serde_json::from_value(self.extra.get("replacement")?.clone()).ok()
    }

    pub fn needs_reconcile(&self) -> bool {
        self.result == UNCONFIRMED || self.replacement().is_some()
    }

    pub fn deferred(&self) -> bool {
        self.extra.get("deferred") == Some(&Value::Bool(true))
    }

    pub fn already_present(&self) -> Option<bool> {
        self.extra.get("already_present").and_then(Value::as_bool)
    }

    pub fn status(&self) -> &'static str {
        if self.needs_reconcile() {
            "reconcile"
        } else if self.deferred() {
            "deferred"
        } else if self.result.starts_with("добавлен:") {
            match self.already_present() {
                Some(true) => "existing",
                Some(false) => "added",
                None => "legacy",
            }
        } else if skip_reason(&self.result).is_some() {
            "skipped"
        } else if self.extra.contains_key("search_error") || self.result.starts_with("ошибка:")
        {
            "error"
        } else {
            "pending"
        }
    }
}

impl Report {
    pub fn new(source: String, title: String, stamp: &str, tracks: Vec<Track>) -> Self {
        let playlist = with_suffix(&title, &format!(" — импорт {stamp}"));
        Self {
            source,
            source_title: title,
            playlist,
            tracks,
            created: false,
            playlist_url: None,
            playlists: vec![],
            extra: Map::new(),
        }
    }

    pub fn ensure_parts(&mut self) {
        if self.playlists.is_empty() {
            self.playlists.push(Part {
                name: self.playlist.clone(),
                created: self.created || self.added() > 0,
                url: self.playlist_url.clone(),
                occupied: None,
                dedup_only: false,
            });
        }
    }

    pub fn add_linked_parts(&mut self, parts: Vec<Part>) -> bool {
        let mut changed = false;
        for part in parts {
            if let Some(existing) = self
                .playlists
                .iter_mut()
                .find(|existing| same_part(existing, &part))
            {
                if existing.url.is_none() && part.url.is_some() {
                    existing.url = part.url;
                    changed = true;
                }
            } else {
                self.playlists.push(part);
                changed = true;
            }
        }
        changed
    }

    pub fn added(&self) -> usize {
        self.tracks
            .iter()
            .filter(|track| track.result.starts_with("добавлен:"))
            .count()
    }

    pub fn added_counts(&self) -> (usize, usize, usize) {
        let (mut new, mut existing, mut legacy) = (0, 0, 0);
        for track in self
            .tracks
            .iter()
            .filter(|track| track.result.starts_with("добавлен:"))
        {
            match track.already_present() {
                Some(true) => existing += 1,
                Some(false) => new += 1,
                None => legacy += 1,
            }
        }
        (new, existing, legacy)
    }

    pub fn track_rows(&self) -> Vec<TrackRow> {
        self.tracks
            .iter()
            .enumerate()
            .map(|(index, track)| {
                let status = track.status();
                let version = track.result.strip_prefix("добавлен:").map(str::trim);
                let detail = if let Some(replacement) = track.replacement() {
                    format!("Замена на «{}» ожидает сверки.", replacement.label)
                } else if let Some(candidate) =
                    track.extra.get("pending_candidate").and_then(Value::as_str)
                {
                    format!("Не подтверждено добавление «{candidate}».")
                } else if let Some(error) = track.extra.get("search_error").and_then(Value::as_str)
                {
                    error.to_owned()
                } else {
                    version
                        .or_else(|| skip_reason(&track.result))
                        .unwrap_or(&track.result)
                        .to_owned()
                };
                let part = track.part_index().and_then(|part| self.playlists.get(part));
                TrackRow {
                    number: index + 1,
                    title: track.title.clone(),
                    artist: track.artists.join(", "),
                    status,
                    detail,
                    method: match track.extra.get("match_method").and_then(Value::as_str) {
                        Some("auto") => Some("auto"),
                        Some("manual") => Some("manual"),
                        _ => None,
                    },
                    source_url: original_url(&track.url),
                    soundcloud_url: track
                        .extra
                        .get("soundcloud_url")
                        .or_else(|| track.extra.get("pending_url"))
                        .and_then(Value::as_str)
                        .and_then(track_url),
                    playlist: part
                        .filter(|part| {
                            part.created && (version.is_some() || track.needs_reconcile())
                        })
                        .map(|part| {
                            (
                                part.name.clone(),
                                part.url.as_deref().and_then(canonical_url),
                            )
                        }),
                }
            })
            .collect()
    }

    pub fn added_tracks(&self) -> Vec<Added> {
        self.tracks
            .iter()
            .enumerate()
            .filter_map(|(index, track)| {
                let version = track.result.strip_prefix("добавлен:")?.trim().to_owned();
                Some(Added {
                    number: index + 1,
                    title: track.title.clone(),
                    artist: track.artists.join(", "),
                    version,
                    url: track
                        .extra
                        .get("soundcloud_url")
                        .and_then(Value::as_str)
                        .and_then(track_url),
                    playlist: track
                        .part_index()
                        .and_then(|part| self.playlists.get(part))
                        .and_then(|part| part.url.as_deref())
                        .and_then(canonical_url),
                })
            })
            .collect()
    }

    pub fn occupied(&self, index: usize) -> usize {
        self.playlists
            .get(index)
            .and_then(|part| part.occupied)
            .unwrap_or_else(|| {
                self.tracks
                    .iter()
                    .filter(|track| {
                        track.result.starts_with("добавлен:")
                            && track.part_index() == Some(index)
                            && track.extra.get("already_present") != Some(&Value::Bool(true))
                    })
                    .count()
            })
    }

    pub fn skipped(&self) -> Vec<Skipped> {
        self.tracks
            .iter()
            .enumerate()
            .filter_map(|(index, track)| {
                Some(Skipped {
                    number: index + 1,
                    artist: track.artists.join(", "),
                    title: track.title.clone(),
                    reason: skip_reason(&track.result)?,
                })
            })
            .collect()
    }

    pub fn summary(&self, id: String) -> Summary {
        let reconcile = self.tracks.iter().any(Track::needs_reconcile);
        let deferred = self.tracks.iter().any(Track::deferred);
        let pending = self.tracks.iter().any(|track| track.result == PENDING);
        let added = self.added();
        let (added_now, already_present, legacy_added) = self.added_counts();
        let skipped = self
            .tracks
            .iter()
            .filter(|track| skip_reason(&track.result).is_some())
            .count();
        let status = if reconcile {
            "Нужна сверка"
        } else if self
            .tracks
            .iter()
            .any(|track| track.result == PENDING && track.extra.contains_key("search_error"))
        {
            "Сбой поиска"
        } else if deferred {
            "Есть отложенные треки"
        } else if pending {
            "Можно продолжить"
        } else if added > 0 && added == self.tracks.len() && already_present > 0 {
            "Все треки учтены"
        } else if added > 0 && added == self.tracks.len() {
            "Все треки добавлены"
        } else if added > 0 {
            "Частично"
        } else {
            "Без добавлений"
        };
        Summary {
            date: report_date(&id)
                .map(|date| date.format("%d.%m.%Y %H:%M").to_string())
                .unwrap_or_default(),
            id,
            title: if self.source_title.is_empty() {
                self.playlist
                    .rsplit_once(" — импорт ")
                    .map_or(self.playlist.as_str(), |(title, _)| title)
                    .to_owned()
            } else {
                self.source_title.clone()
            },
            source: self.source.clone(),
            added,
            added_now,
            already_present,
            legacy_added,
            skipped,
            total: self.tracks.len(),
            status,
            resumable: reconcile || pending || deferred,
            reconcile,
            links: self
                .playlists
                .iter()
                .filter_map(|part| {
                    let url = canonical_url(part.url.as_deref()?)?;
                    part.created.then(|| (part.name.clone(), url))
                })
                .collect(),
        }
    }
}

pub fn skip_reason(result: &str) -> Option<&'static str> {
    match result {
        "пропущен пользователем" => Some("Пропущен вручную"),
        "не найден" => Some("Трек не найден в SoundCloud"),
        result if result.starts_with("пропущен автоматически:") => {
            Some("Нет совпадений среди первых трёх вариантов")
        }
        result if result.starts_with("трек не найден") => {
            Some("Трек не найден в SoundCloud")
        }
        result if result.starts_with("нет доступных") => {
            Some("В SoundCloud нет доступных вариантов")
        }
        _ => None,
    }
}

pub fn with_suffix(title: &str, suffix: &str) -> String {
    let title = title
        .chars()
        .take(100usize.saturating_sub(suffix.chars().count()))
        .collect::<String>();
    format!("{}{suffix}", title.trim_end())
}

pub fn part_name(base: &str, number: usize) -> String {
    let (title, suffix) = base.rsplit_once(" — импорт ").map_or_else(
        || (base, format!(" — часть {number}")),
        |(title, stamp)| (title, format!(" — импорт {stamp} — часть {number}")),
    );
    with_suffix(title, &suffix)
}

pub fn source_url(text: &str) -> Option<Url> {
    yandex_url(text, "playlists")
}

pub fn original_url(text: &str) -> Option<String> {
    let mut url = yandex_url(text, "track")?;
    url.set_query(None);
    url.set_fragment(None);
    Some(url.into())
}

fn yandex_url(text: &str, expected_segment: &str) -> Option<Url> {
    let url = Url::parse(text).ok()?;
    if url.scheme() == "https"
        && url.username().is_empty()
        && url.password().is_none()
        && url.port().is_none()
        && matches!(
            url.host_str(),
            Some(
                "music.yandex.ru"
                    | "music.yandex.by"
                    | "music.yandex.com"
                    | "music.yandex.kz"
                    | "music.yandex.uz"
            )
        )
        && url
            .path_segments()?
            .any(|segment| segment == expected_segment)
    {
        return Some(url);
    }
    None
}

pub fn track_url(text: &str) -> Option<String> {
    let url = Url::parse(text).ok()?;
    let safe = |segment: &str| {
        !segment.is_empty()
            && segment
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    };
    let path: Vec<_> = url.path().trim_matches('/').split('/').collect();
    if url.scheme() == "https"
        && url.host_str() == Some("soundcloud.com")
        && url.port().is_none()
        && url.username().is_empty()
        && url.password().is_none()
        && path.len() == 2
        && path.iter().all(|segment| safe(segment))
        && !matches!(
            path[0],
            "you" | "search" | "discover" | "pages" | "settings"
        )
    {
        return Some(format!("https://soundcloud.com/{}/{}", path[0], path[1]));
    }
    None
}

pub fn canonical_url(text: &str) -> Option<String> {
    let url = Url::parse("https://soundcloud.com").ok()?.join(text).ok()?;
    let segments: Vec<_> = url
        .path()
        .trim_end_matches('/')
        .trim_start_matches('/')
        .split('/')
        .collect();
    let safe = |segment: &str| {
        !segment.is_empty()
            && segment
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    };
    if url.scheme() == "https"
        && url.host_str() == Some("soundcloud.com")
        && url.port().is_none()
        && url.username().is_empty()
        && url.password().is_none()
        && matches!(segments.len(), 3 | 4)
        && safe(segments[0])
        && segments[1] == "sets"
        && safe(segments[2])
        && (segments.len() == 3 || segments[3].strip_prefix("s-").is_some_and(safe))
    {
        return Some(format!(
            "https://soundcloud.com/{}/sets/{}",
            segments[0], segments[2]
        ));
    }
    None
}

pub fn home() -> Result<PathBuf> {
    env::var_os("USERPROFILE")
        .map(PathBuf::from)
        .ok_or_else(|| "Не найдена папка пользователя Windows.".into())
}

pub fn data_dir() -> Result<PathBuf> {
    Ok(env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .map_or_else(home, Ok)?
        .join("MusicTransfer"))
}

pub fn report_date(id: &str) -> Option<NaiveDateTime> {
    let stamp = id.strip_prefix("перенос-")?.strip_suffix(".json")?;
    if stamp.len() != 22
        || !stamp
            .bytes()
            .all(|byte| byte.is_ascii_digit() || byte == b'-')
    {
        return None;
    }
    NaiveDateTime::parse_from_str(stamp, "%Y%m%d-%H%M%S-%6f").ok()
}

fn folders() -> Result<Vec<PathBuf>> {
    let mut folders = vec![data_dir()?.join("Reports")];
    if let Ok(executable) = env::current_exe() {
        if let Some(folder) = executable.parent() {
            folders.push(folder.to_path_buf());
        }
    }
    Ok(folders)
}

pub fn resolve(id: &str) -> Result<PathBuf> {
    report_date(id).ok_or("Некорректное имя отчёта.")?;
    folders()?
        .into_iter()
        .map(|folder| folder.join(id))
        .find(|path| path.is_file())
        .ok_or_else(|| "Отчёт не найден.".into())
}

pub fn delete(id: &str) -> Result<()> {
    delete_from(id, folders()?)
}

fn delete_from(id: &str, folders: Vec<PathBuf>) -> Result<()> {
    report_date(id).ok_or("Некорректное имя отчёта.")?;
    // A legacy copy beside the EXE would otherwise reappear under the same ID.
    for folder in folders.into_iter().rev() {
        match fs::remove_file(folder.join(id)) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(format!(
                    "Не удалось удалить отчёт из {}: {error}",
                    folder.display()
                ));
            }
        }
    }
    Ok(())
}

pub fn read(path: &Path) -> Result<Report> {
    let bytes = fs::read(path).map_err(|error| format!("Не удалось прочитать отчёт: {error}"))?;
    let mut report: Report =
        serde_json::from_slice(bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(&bytes))
            .map_err(|error| format!("Некорректный отчёт: {error}"))?;
    report.ensure_parts();
    Ok(report)
}

pub fn write(path: &Path, report: &mut Report) -> Result<()> {
    report.ensure_parts();
    report.created = report.playlists[0].created;
    report.playlist_url = report.playlists[0].url.clone();
    let folder = path.parent().ok_or("У отчёта нет папки.")?;
    fs::create_dir_all(folder).map_err(|error| error.to_string())?;
    let mut file = tempfile::NamedTempFile::new_in(folder).map_err(|error| error.to_string())?;
    serde_json::to_writer_pretty(&mut file, report).map_err(|error| error.to_string())?;
    file.flush()
        .and_then(|()| file.as_file().sync_all())
        .map_err(|error| error.to_string())?;
    file.persist(path)
        .map_err(|error| format!("Не удалось сохранить отчёт: {error}"))?;
    Ok(())
}

pub fn history() -> Result<(Vec<Summary>, Vec<String>)> {
    let mut seen = HashSet::new();
    let mut rows = Vec::new();
    let mut warnings = Vec::new();
    for folder in folders()? {
        let files = match fs::read_dir(folder) {
            Ok(files) => files,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.to_string()),
        };
        for file in files.flatten() {
            let id = file.file_name().to_string_lossy().into_owned();
            if report_date(&id).is_none() || seen.contains(&id) {
                continue;
            }
            match read(&file.path()) {
                Ok(report) => {
                    seen.insert(id.clone());
                    rows.push(report.summary(id));
                }
                Err(error) => warnings.push(format!("{id} · {error}")),
            }
        }
    }
    rows.sort_by(|left, right| right.id.cmp(&left.id));
    Ok((rows, warnings))
}

pub fn linked_parts(root: &str) -> Result<Vec<Part>> {
    let (rows, _) = history()?;
    let mut parts: Vec<Part> = Vec::new();
    for row in rows
        .into_iter()
        .filter(|row| row.links.iter().any(|(_, url)| url == root))
    {
        let Ok(report) = resolve(&row.id).and_then(|path| read(&path)) else {
            continue;
        };
        if report
            .playlists
            .first()
            .and_then(|part| part.url.as_deref())
            .and_then(canonical_url)
            .as_deref()
            != Some(root)
        {
            continue;
        }
        for (index, mut part) in report.playlists.into_iter().enumerate().skip(1) {
            part.url = part.url.as_deref().and_then(canonical_url);
            if !part.created
                || part.url.as_deref() == Some(root)
                || (part.url.is_none() && part.name != part_name(&report.playlist, index + 1))
            {
                continue;
            }
            if let Some(known) = parts.iter_mut().find(|known| same_part(known, &part)) {
                if known.url.is_none() {
                    known.url = part.url;
                }
            } else {
                part.dedup_only = true;
                parts.push(part);
            }
        }
    }
    Ok(parts)
}

fn same_part(left: &Part, right: &Part) -> bool {
    match (
        left.url.as_deref().and_then(canonical_url),
        right.url.as_deref().and_then(canonical_url),
    ) {
        (Some(left), Some(right)) => left == right,
        _ => left.name == right.name,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_deletion_removes_both_copies_of_one_report() {
        let current = tempfile::tempdir().unwrap();
        let legacy = tempfile::tempdir().unwrap();
        let folders = vec![current.path().to_owned(), legacy.path().to_owned()];
        let id = "перенос-20261109-222224-076520.json";
        let keep = "перенос-20261109-222225-076520.json";
        for folder in &folders {
            fs::write(folder.join(id), b"report").unwrap();
            fs::write(folder.join(keep), b"other report").unwrap();
        }
        assert!(delete_from(&format!("../{id}"), folders.clone()).is_err());
        assert!(folders.iter().all(|folder| folder.join(id).is_file()));
        delete_from(id, folders.clone()).unwrap();
        assert!(folders.iter().all(|folder| !folder.join(id).exists()));
        assert!(folders.iter().all(|folder| folder.join(keep).is_file()));
    }

    #[test]
    fn old_report_and_atomic_update() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("перенос-20261109-222224-076520.json");
        fs::write(&path, r#"{"source":"https://music.yandex.by/playlists/1","playlist":"Плейлист","tracks":[{"title":"Запись","artists":["Артист"],"result":"добавлен: трек"},{"title":"Пропуск","result":"не найден"}]}"#).unwrap();
        let mut report = read(&path).unwrap();
        assert!(report.playlists[0].created);
        assert_eq!(report.occupied(0), 1);
        assert_eq!(report.skipped()[0].number, 2);
        assert_eq!(report.added_tracks()[0].version, "трек");
        assert_eq!(report.added_counts(), (0, 0, 1));
        assert_eq!(report.track_rows()[0].status, "legacy");
        let replacement = Replacement {
            label: "Новая версия".into(),
            url: None,
            phase: ReplacementPhase::RemovePrevious,
            added_slot: true,
        };
        report.tracks[0]
            .extra
            .insert("replacement".into(), serde_json::json!(replacement));
        write(&path, &mut report).unwrap();
        let saved = read(&path).unwrap();
        assert_eq!(
            saved.tracks[0].replacement().unwrap().phase,
            ReplacementPhase::RemovePrevious
        );
        assert_eq!(saved.added_tracks()[0].version, "трек");
        assert!(
            saved
                .summary("перенос-20261109-222224-076520.json".into())
                .reconcile
        );
        report.tracks[0].extra.remove("replacement");
        report.tracks[1].result = "добавлен: найден".into();
        report.tracks[1]
            .extra
            .insert("already_present".into(), Value::Bool(true));
        report.tracks[1]
            .extra
            .insert("match_method".into(), Value::String("manual".into()));
        let part = Part {
            name: "Другая часть".into(),
            url: Some("https://soundcloud.com/user/sets/second".into()),
            created: true,
            occupied: Some(500),
            dedup_only: true,
        };
        assert!(report.add_linked_parts(vec![part.clone()]));
        assert!(!report.add_linked_parts(vec![Part {
            name: "Новое название".into(),
            ..part
        }]));
        report.tracks[1]
            .extra
            .insert("playlist_part".into(), Value::from(2));
        write(&path, &mut report).unwrap();
        let saved = read(&path).unwrap();
        assert_eq!(saved.added(), 2);
        assert_eq!(saved.added_counts(), (0, 1, 1));
        let rows = saved.track_rows();
        assert_eq!(rows[1].status, "existing");
        assert_eq!(rows[1].method, Some("manual"));
        assert_eq!(rows[1].playlist.as_ref().unwrap().0, "Другая часть");
        assert!(report_date("перенос-20261109-222224-076520.json").is_some());
        assert!(report_date("../перенос-20261109-222224-076520.json").is_none());
        assert!(source_url("https://evil.test/playlists/1").is_none());
        assert_eq!(
            canonical_url("/user/sets/list/s-secret").unwrap(),
            "https://soundcloud.com/user/sets/list"
        );
    }
}
