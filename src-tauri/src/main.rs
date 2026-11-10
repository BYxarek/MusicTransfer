#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod chrome;
mod desktop;
mod matching;
mod report;
mod workflow;

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

use report::{Result, Skipped, Summary};
use serde_json::Value;
use tauri::{AppHandle, Manager, State, WindowEvent, ipc::Channel};
use workflow::{Control, Job, Log, Request};

struct Desktop {
    active: Mutex<Option<Arc<Control>>>,
    log: Arc<Log>,
    update: Mutex<Option<tauri_plugin_updater::Update>>,
    updating: AtomicBool,
}

fn lock_transfer_files() -> Result<std::fs::File> {
    let path = report::data_dir()?;
    std::fs::create_dir_all(&path)
        .map_err(|error| format!("Не удалось подготовить папку переноса: {error}"))?;
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path.join("transfer.lock"))
        .map_err(|error| format!("Не удалось открыть файл блокировки переноса: {error}"))?;
    lock.try_lock().map_err(|error| match error {
        std::fs::TryLockError::WouldBlock => {
            "Перенос уже запущен в другой копии MusicTransfer.".into()
        }
        std::fs::TryLockError::Error(error) => format!("Не удалось заблокировать перенос: {error}"),
    })?;
    Ok(lock)
}

#[tauri::command]
fn start_transfer(
    app: AppHandle,
    state: State<'_, Desktop>,
    request: Request,
    events: Channel<Value>,
) -> Result<()> {
    let mut active = state.active.lock().unwrap();
    if state.updating.load(Ordering::Acquire) {
        return Err("Дождитесь завершения обновления.".into());
    }
    if active.is_some() {
        return Err("Перенос уже запущен.".into());
    }
    let lock = lock_transfer_files()?;
    let control = Arc::new(Control::new());
    *active = Some(control.clone());
    let mut job = Job::new(request, control.clone(), events.clone(), state.log.clone());
    std::thread::spawn(move || {
        let event = job.execute();
        drop(lock);
        let state = app.state::<Desktop>();
        let close = {
            let mut active = state.active.lock().unwrap();
            let close = control.close_after.load(Ordering::Acquire);
            *active = None;
            close
        };
        let _ = events.send(event);
        if close {
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.destroy();
            }
        }
    });
    Ok(())
}

#[tauri::command]
fn reply_transfer(state: State<'_, Desktop>, prompt: u64, answer: Value) -> Result<()> {
    state
        .active
        .lock()
        .unwrap()
        .as_ref()
        .ok_or("Перенос уже завершён.")?
        .answer(prompt, answer)
}

#[tauri::command]
fn stop_transfer(state: State<'_, Desktop>) {
    if let Some(control) = state.active.lock().unwrap().as_ref() {
        state.log.write("INFO", "Пользователь запросил остановку.");
        control.stop();
    }
}

#[tauri::command]
fn transfer_history(state: State<'_, Desktop>) -> Result<Vec<Summary>> {
    let (entries, warnings) = report::history()?;
    state.log.write("INFO", "Открыта история переносов.");
    for warning in warnings {
        state.log.write("WARNING", &warning);
    }
    Ok(entries)
}

#[tauri::command]
fn delete_transfer_history(state: State<'_, Desktop>, id: String) -> Result<()> {
    let active = state.active.lock().unwrap();
    if active.is_some() {
        return Err("Остановите перенос перед удалением истории.".into());
    }
    let _lock = lock_transfer_files()?;
    report::delete(&id)?;
    state
        .log
        .write("INFO", &format!("Удалена запись истории {id}."));
    Ok(())
}

#[tauri::command]
fn skipped_tracks(id: String) -> Result<Vec<Skipped>> {
    Ok(report::read(&report::resolve(&id)?)?.skipped())
}

#[tauri::command]
fn added_tracks(id: String) -> Result<Vec<report::Added>> {
    Ok(report::read(&report::resolve(&id)?)?.added_tracks())
}

#[tauri::command]
fn transfer_report(id: String) -> Result<Vec<report::TrackRow>> {
    Ok(report::read(&report::resolve(&id)?)?.track_rows())
}

#[tauri::command]
fn open_track(url: String) -> Result<()> {
    let url = report::original_url(&url)
        .or_else(|| report::track_url(&url))
        .ok_or("Некорректная ссылка на трек.")?;
    open::that_detached(url).map_err(|error| format!("Не удалось открыть трек: {error}"))
}

#[tauri::command]
fn open_report(id: String) -> Result<()> {
    open::that_detached(report::resolve(&id)?)
        .map_err(|error| format!("Не удалось открыть отчёт: {error}"))
}

#[tauri::command]
fn open_playlist(url: String) -> Result<()> {
    let url = report::canonical_url(&url).ok_or("Некорректная ссылка на плейлист SoundCloud.")?;
    open::that_detached(url).map_err(|error| format!("Не удалось открыть плейлист: {error}"))
}

#[tauri::command]
fn open_journal(state: State<'_, Desktop>) -> Result<()> {
    open::that_detached(
        state
            .log
            .path
            .as_ref()
            .ok_or("Не удалось создать файл журнала.")?,
    )
    .map_err(|error| format!("Не удалось открыть журнал: {error}"))
}

#[tauri::command]
fn journal(state: State<'_, Desktop>) -> Vec<(String, String)> {
    state.log.lines()
}

#[tauri::command]
fn report_ui_error(state: State<'_, Desktop>, message: String) {
    state.log.write("ERROR", &message);
}

#[tauri::command]
fn set_window_theme(window: tauri::WebviewWindow, theme: &str) -> Result<()> {
    let theme = match theme {
        "light" => tauri::Theme::Light,
        "dark" => tauri::Theme::Dark,
        _ => return Err("Неизвестная тема.".into()),
    };
    window
        .set_theme(Some(theme))
        .map_err(|error| error.to_string())
}

#[tauri::command]
fn paste_link() -> Result<String> {
    arboard::Clipboard::new()
        .and_then(|mut clipboard| clipboard.get_text())
        .map_err(|_| "В буфере обмена нет текста.".into())
}

#[tauri::command]
fn copy_link(id: String) -> Result<()> {
    let report = report::read(&report::resolve(&id)?)?;
    arboard::Clipboard::new()
        .and_then(|mut clipboard| clipboard.set_text(report.source))
        .map_err(|error| format!("Не удалось скопировать ссылку: {error}"))
}

fn main() {
    let log = Arc::new(Log::new());
    log.write(
        "INFO",
        concat!("MusicTransfer ", env!("CARGO_PKG_VERSION"), " запущен."),
    );
    if log.path.is_none() {
        log.write(
            "WARNING",
            "Файл журнала недоступен. Записи видны в приложении.",
        );
    }
    let panic_log = log.clone();
    std::panic::set_hook(Box::new(move |panic| {
        panic_log.write(
            "ERROR",
            &format!("{panic}\n{}", std::backtrace::Backtrace::force_capture()),
        );
    }));
    use tauri_plugin_log::{RotationStrategy, Target, TargetKind, TimezoneStrategy};
    use tauri_plugin_window_state::StateFlags;
    let diagnostics = match desktop::diagnostics_dir() {
        Ok(path) if std::fs::create_dir_all(&path).is_ok() => TargetKind::Folder {
            path,
            file_name: Some("MusicTransfer".into()),
        },
        _ => TargetKind::Stderr,
    };
    let result = tauri::Builder::default()
        .runtime(tauri_runtime_wry::Wry::default())
        .plugin(tauri_plugin_single_instance::init(|app, _, _| {
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.show();
                let _ = window.unminimize();
                let _ = window.set_focus();
            }
        }))
        .plugin(
            tauri_plugin_log::Builder::default()
                .targets([Target::new(diagnostics)])
                .filter(|meta| meta.target().starts_with("music_transfer"))
                .level(log::LevelFilter::Debug)
                .timezone_strategy(TimezoneStrategy::UseLocal)
                .rotation_strategy(RotationStrategy::KeepSome(3))
                .max_file_size(2_000_000)
                .build(),
        )
        .plugin(
            tauri_plugin_window_state::Builder::default()
                .with_state_flags(StateFlags::SIZE | StateFlags::POSITION | StateFlags::MAXIMIZED)
                .build(),
        )
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .manage(Desktop {
            active: Mutex::new(None),
            log: log.clone(),
            update: Mutex::new(None),
            updating: AtomicBool::new(false),
        })
        .setup(|app| {
            let state = app.state::<Desktop>();
            log::info!("MusicTransfer {} запущен.", env!("CARGO_PKG_VERSION"));
            match app.webview_version() {
                Ok(version) => state.log.write("INFO", &format!("WebView2: {version}")),
                Err(error) => state.log.write(
                    "WARNING",
                    &format!("Не удалось определить версию WebView2: {error}"),
                ),
            };
            Ok(())
        })
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                let state = window.state::<Desktop>();
                if state.updating.load(Ordering::Acquire) {
                    api.prevent_close();
                }
                if let Some(control) = state.active.lock().unwrap().as_ref() {
                    api.prevent_close();
                    control.close_after.store(true, Ordering::Release);
                    control.stop();
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            start_transfer,
            reply_transfer,
            stop_transfer,
            transfer_history,
            delete_transfer_history,
            skipped_tracks,
            added_tracks,
            transfer_report,
            open_track,
            open_report,
            open_playlist,
            open_journal,
            journal,
            report_ui_error,
            set_window_theme,
            paste_link,
            copy_link,
            desktop::open_diagnostics,
            desktop::desktop_notification,
            desktop::export_report_csv,
            desktop::check_update,
            desktop::install_update,
        ])
        .run(tauri::generate_context!());
    if let Err(error) = result {
        log.write("ERROR", &format!("Не удалось запустить окно: {error}"));
    }
}
