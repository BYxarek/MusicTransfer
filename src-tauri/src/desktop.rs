use std::{
    collections::HashSet,
    fs,
    io::{self, Write},
    time::Duration,
};

use tauri::{AppHandle, Manager, State, WebviewWindow};
use tauri_plugin_dialog::DialogExt;
use tauri_plugin_notification::NotificationExt;
use tauri_plugin_updater::UpdaterExt;
use url::Url;

use crate::{
    Desktop,
    report::{self, Result, TrackRow},
};

pub fn diagnostics_dir() -> Result<std::path::PathBuf> {
    Ok(report::data_dir()?.join("Diagnostics"))
}

#[tauri::command]
pub fn open_diagnostics(app: AppHandle) -> Result<()> {
    let path = diagnostics_dir()
        .or_else(|_| app.path().app_log_dir().map_err(|error| error.to_string()))?;
    open::that_detached(path)
        .map_err(|error| format!("Не удалось открыть журнал диагностики: {error}"))
}

#[tauri::command]
pub fn desktop_notification(window: WebviewWindow, title: String, text: String) {
    if window.is_focused().unwrap_or(true) {
        return;
    }
    if let Err(error) = window
        .app_handle()
        .notification()
        .builder()
        .title(title)
        .body(text)
        .show()
    {
        log::warn!("Не удалось показать системное уведомление: {error}");
    }
}

fn csv_text(text: &str) -> String {
    if text.starts_with(['\t', '\r', '\n']) || text.trim_start().starts_with(['=', '+', '-', '@']) {
        return format!("'{text}");
    }
    text.to_owned()
}

fn write_csv(mut file: impl Write, rows: impl IntoIterator<Item = TrackRow>) -> csv::Result<()> {
    file.write_all(b"\xEF\xBB\xBF")?;
    let mut csv = csv::WriterBuilder::new().delimiter(b';').from_writer(file);
    csv.write_record([
        "№",
        "Трек",
        "Исполнитель",
        "Статус",
        "Подбор",
        "Результат",
        "Оригинал",
        "SoundCloud",
        "Плейлист",
        "Ссылка на плейлист",
    ])?;
    for row in rows {
        let status = match row.status {
            "added" => "Добавлен",
            "existing" => "Уже был",
            "legacy" => "Без данных о добавлении",
            "skipped" => "Пропущен",
            "deferred" => "Отложен",
            "error" => "Ошибка поиска",
            "reconcile" => "Ожидает сверки",
            _ => "Ожидает переноса",
        };
        let (playlist, playlist_url) = row.playlist.unwrap_or_default();
        csv.write_record([
            row.number.to_string(),
            csv_text(&row.title),
            csv_text(&row.artist),
            status.to_owned(),
            match row.method {
                Some("auto") => "Автовыбор",
                Some("manual") => "Вручную",
                _ => "",
            }
            .to_owned(),
            csv_text(&row.detail),
            row.source_url.unwrap_or_default(),
            row.soundcloud_url.unwrap_or_default(),
            csv_text(&playlist),
            playlist_url.unwrap_or_default(),
        ])?;
    }
    csv.flush()?;
    Ok(())
}

#[tauri::command]
pub async fn export_report_csv(
    window: WebviewWindow,
    id: String,
    numbers: Option<HashSet<usize>>,
) -> Result<bool> {
    let rows = report::read(&report::resolve(&id)?)?.track_rows();
    if numbers.as_ref().is_some_and(|numbers| {
        numbers.is_empty()
            || numbers
                .iter()
                .any(|number| *number == 0 || *number > rows.len())
    }) {
        return Err("В выборке CSV указаны некорректные номера треков.".into());
    }
    let (send, receive) = tokio::sync::oneshot::channel();
    let app = window.app_handle();
    app.dialog()
        .file()
        .set_parent(&window)
        .add_filter("CSV", &["csv"])
        .set_file_name(id.replace(".json", ".csv"))
        .save_file(move |path| {
            let _ = send.send(path);
        });
    let Some(path) = receive
        .await
        .map_err(|_| "Диалог сохранения CSV закрылся.")?
    else {
        return Ok(false);
    };
    let path = path.into_path().map_err(|error| error.to_string())?;
    let save = || -> std::result::Result<(), Box<dyn std::error::Error>> {
        let parent = path.parent().ok_or("Папка назначения недоступна.")?;
        let mut file = tempfile::NamedTempFile::new_in(parent)?;
        write_csv(
            file.as_file_mut(),
            rows.into_iter().filter(|row| {
                numbers
                    .as_ref()
                    .is_none_or(|numbers| numbers.contains(&row.number))
            }),
        )?;
        file.as_file().sync_all()?;
        file.persist(&path)?;
        Ok(())
    };
    save().map_err(|error| format!("Не удалось сохранить CSV: {error}"))?;
    app.state::<Desktop>()
        .log
        .write("INFO", "Отчёт экспортирован в CSV.");
    Ok(true)
}

fn https_url(address: &str) -> Result<Url> {
    Url::parse(address)
        .ok()
        .filter(|url| {
            url.scheme() == "https"
                && url.host_str().is_some()
                && url.username().is_empty()
                && url.password().is_none()
        })
        .ok_or_else(|| "Для канала обновлений нужен HTTPS-адрес.".into())
}

fn update_endpoint() -> Result<Url> {
    let path = report::data_dir()?.join("update-channel.json");
    let endpoint = match fs::read(&path) {
        Ok(bytes) => {
            #[derive(serde::Deserialize)]
            struct Channel {
                endpoint: String,
            }
            serde_json::from_slice::<Channel>(&bytes)
                .map_err(|error| format!("Ошибка в update-channel.json: {error}"))?
                .endpoint
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            "https://raw.githubusercontent.com/BYxarek/MusicTransfer/updates/latest.json".into()
        }
        Err(error) => return Err(format!("Не удалось прочитать update-channel.json: {error}")),
    };
    https_url(&endpoint)
}

#[derive(serde::Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum UpdateStatus {
    Current,
    Available { version: String },
}

#[tauri::command]
pub async fn check_update(app: AppHandle, state: State<'_, Desktop>) -> Result<UpdateStatus> {
    *state.update.lock().unwrap() = None;
    let endpoint = update_endpoint()?;
    let updater = app
        .updater_builder()
        .timeout(Duration::from_secs(45))
        .configure_client(|client| client.https_only(true))
        .endpoints(vec![endpoint])
        .and_then(|builder| builder.build())
        .map_err(|error| format!("Ошибка канала обновлений: {error}"))?;
    let update = updater
        .check()
        .await
        .map_err(|error| format!("Не удалось проверить обновления: {error}"))?;
    let status = if let Some(update) = update.as_ref() {
        https_url(update.download_url.as_str())?;
        UpdateStatus::Available {
            version: update.version.clone(),
        }
    } else {
        UpdateStatus::Current
    };
    *state.update.lock().unwrap() = update;
    Ok(status)
}

#[tauri::command]
pub async fn install_update(state: State<'_, Desktop>, version: String) -> Result<()> {
    {
        let active = state.active.lock().unwrap();
        if active.is_some() {
            return Err("Завершите перенос перед установкой обновления.".into());
        }
        if state
            .updating
            .swap(true, std::sync::atomic::Ordering::AcqRel)
        {
            return Err("Обновление уже устанавливается.".into());
        }
    }
    let update = state
        .update
        .lock()
        .unwrap()
        .as_ref()
        .filter(|update| update.version == version)
        .cloned();
    let result = match update {
        Some(update) => update
            .download_and_install(|_, _| {}, || {})
            .await
            .map_err(|error| format!("Не удалось установить обновление: {error}")),
        None => Err("Сначала проверьте наличие обновления.".into()),
    };
    state
        .updating
        .store(false, std::sync::atomic::Ordering::Release);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csv_preserves_quotes_and_blocks_formulas() {
        let mut bytes = Vec::new();
        write_csv(
            &mut bytes,
            [TrackRow {
                number: 1,
                title: "Название; \"версия\"\nстрока".into(),
                artist: "  =2+2".into(),
                status: "added",
                method: Some("manual"),
                detail: "\t123".into(),
                source_url: None,
                soundcloud_url: None,
                playlist: None,
            }],
        )
        .unwrap();
        let mut reader = csv::ReaderBuilder::new()
            .delimiter(b';')
            .from_reader(&bytes[..]);
        let row = reader.records().next().unwrap().unwrap();
        assert_eq!(&row[1], "Название; \"версия\"\nстрока");
        assert_eq!(&row[2], "'  =2+2");
        assert_eq!(&row[5], "'\t123");
    }

    #[test]
    fn updater_requires_https_without_credentials() {
        assert!(https_url("http://example.com/latest.json").is_err());
        assert!(https_url("https://user:pass@example.com/latest.json").is_err());
        assert!(https_url("https://example.com/latest.json").is_ok());
    }

    #[test]
    fn updater_rejects_bad_signature_from_local_feed() {
        use std::{io::Read, net::TcpListener, thread};
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let manifest = serde_json::json!({
            "version": "2.4.0-alpha.1",
            "platforms": {
                "windows-x86_64": {"url": format!("{base}/setup.exe"), "signature": "invalid"}
            }
        })
        .to_string();
        let server = thread::spawn(move || {
            for body in [manifest.as_bytes(), b"Invalid installer".as_slice()] {
                let (mut request, _) = listener.accept().unwrap();
                request
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut input = [0; 4096];
                assert!(request.read(&mut input).unwrap() > 0);
                write!(
                    request,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                )
                .unwrap();
                request.write_all(body).unwrap();
            }
        });
        let mut context = tauri::generate_context!();
        context.config_mut().app.windows.clear();
        let app = tauri::Builder::default()
            .runtime(tauri_runtime_wry::Wry::default())
            .any_thread()
            .plugin(tauri_plugin_updater::Builder::new().build())
            .build(context)
            .unwrap();
        let updater = app
            .updater_builder()
            .no_proxy()
            .timeout(Duration::from_secs(5))
            .endpoints(vec![Url::parse(&format!("{base}/latest.json")).unwrap()])
            .unwrap()
            .build()
            .unwrap();
        tauri::async_runtime::block_on(async {
            let update = updater.check().await.unwrap().unwrap();
            assert_eq!(update.version, "2.4.0-alpha.1");
            let mut downloaded = false;
            assert!(
                update
                    .download(|size, _| downloaded |= size > 0, || {})
                    .await
                    .is_err()
            );
            assert!(downloaded);
        });
        server.join().unwrap();
    }
}
