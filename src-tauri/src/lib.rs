mod capture;
mod folder_watch;
mod library;

use rusqlite::{Connection, OptionalExtension};
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
};
use tauri::{
    Emitter, Manager,
    menu::{Menu, MenuItem},
    tray::TrayIconBuilder,
};
use tauri_plugin_global_shortcut::{Code, GlobalShortcutExt, Modifiers, Shortcut, ShortcutState};

pub struct AppState {
    pub db_path: PathBuf,
    pub thumbnail_dir: PathBuf,
    pub capture_dir: PathBuf,
    pub pending_region: Mutex<Option<capture::PendingRegion>>,
    pub shortcut_error: Mutex<Option<String>>,
    pub folder_watcher: Arc<Mutex<folder_watch::FolderWatchService>>,
    pub import_lock: Arc<Mutex<()>>,
}

#[tauri::command]
async fn add_monitored_folder(
    state: tauri::State<'_, AppState>,
    folder: String,
) -> Result<library::ImportSummary, String> {
    let db_path = state.db_path.clone();
    let thumbnail_dir = state.thumbnail_dir.clone();
    let capture_dir = state.capture_dir.clone();
    let watcher = Arc::clone(&state.folder_watcher);
    let import_lock = Arc::clone(&state.import_lock);
    tauri::async_runtime::spawn_blocking(move || {
        let _guard = import_lock
            .lock()
            .map_err(|_| "Folder importer is unavailable.")?;
        let requested = std::fs::canonicalize(&folder)
            .map_err(|error| format!("Cannot access that folder: {error}"))?;
        let managed_captures = std::fs::canonicalize(&capture_dir).unwrap_or(capture_dir.clone());
        if requested.starts_with(&managed_captures) {
            return Err("Screenshot Memory already manages that folder.".to_string());
        }
        let selected = {
            let mut watcher = watcher
                .lock()
                .map_err(|_| "Folder monitor is unavailable.")?;
            watcher.add(&requested)?
        };
        if let Err(error) = library::persist_monitored_folder(&db_path, &selected) {
            let _ = watcher.lock().map(|mut watcher| watcher.remove(&selected));
            return Err(error);
        }
        let mut report = library::import_monitored_folders(
            &db_path,
            &thumbnail_dir,
            &capture_dir,
            std::slice::from_ref(&selected),
        );
        report.monitored_folders = library::monitored_folders_from_db(&db_path)?;
        Ok(report)
    })
    .await
    .map_err(|error| format!("Could not add folder monitor: {error}"))?
}

#[tauri::command]
async fn remove_monitored_folder(
    state: tauri::State<'_, AppState>,
    folder: String,
) -> Result<Vec<String>, String> {
    let db_path = state.db_path.clone();
    let watcher = Arc::clone(&state.folder_watcher);
    let import_lock = Arc::clone(&state.import_lock);
    tauri::async_runtime::spawn_blocking(move || {
        let _guard = import_lock
            .lock()
            .map_err(|_| "Folder importer is unavailable.")?;
        let selected = std::fs::canonicalize(&folder).unwrap_or_else(|_| PathBuf::from(&folder));
        watcher
            .lock()
            .map_err(|_| "Folder monitor is unavailable.")?
            .remove(&selected)?;
        library::remove_monitored_folder_from_db(&db_path, &selected)?;
        library::monitored_folders_from_db(&db_path)
    })
    .await
    .map_err(|error| format!("Could not remove folder monitor: {error}"))?
}

#[tauri::command]
async fn sync_monitored_folders(
    state: tauri::State<'_, AppState>,
) -> Result<library::ImportSummary, String> {
    let db_path = state.db_path.clone();
    let thumbnail_dir = state.thumbnail_dir.clone();
    let capture_dir = state.capture_dir.clone();
    let import_lock = Arc::clone(&state.import_lock);
    tauri::async_runtime::spawn_blocking(move || {
        let _guard = import_lock
            .lock()
            .map_err(|_| "Folder importer is unavailable.")?;
        let configured = library::monitored_folders_from_db(&db_path)?;
        let roots = configured
            .iter()
            .filter_map(|path| std::fs::canonicalize(path).ok())
            .filter(|path| path.is_dir())
            .collect::<Vec<_>>();
        let mut report =
            library::import_monitored_folders(&db_path, &thumbnail_dir, &capture_dir, &roots);
        report.monitored_folders = configured;
        Ok(report)
    })
    .await
    .map_err(|error| format!("Could not sync monitored folders: {error}"))?
}

fn setting(db_path: &PathBuf, key: &str) -> Result<Option<String>, String> {
    Connection::open(db_path)
        .map_err(|error| error.to_string())?
        .query_row("SELECT value FROM settings WHERE key=?1", [key], |row| {
            row.get(0)
        })
        .optional()
        .map_err(|error| error.to_string())
}

#[tauri::command]
fn get_capture_display_mode(state: tauri::State<'_, AppState>) -> Result<String, String> {
    Ok(setting(&state.db_path, "capture_display_mode")?.unwrap_or_else(|| "active".into()))
}

#[tauri::command]
fn set_capture_display_mode(state: tauri::State<'_, AppState>, mode: String) -> Result<(), String> {
    if mode != "active" && mode != "all" {
        return Err("Choose active or all displays.".into());
    }
    Connection::open(&state.db_path).map_err(|error| error.to_string())?
        .execute("INSERT INTO settings(key,value) VALUES('capture_display_mode',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value", [mode])
        .map_err(|error| error.to_string())?;
    Ok(())
}

#[tauri::command]
fn get_capture_status(state: tauri::State<'_, AppState>) -> Result<Option<String>, String> {
    Ok(state
        .shortcut_error
        .lock()
        .map_err(|_| "Capture status is unavailable.".to_string())?
        .clone())
}

#[tauri::command]
fn get_capture_folder(state: tauri::State<'_, AppState>) -> String {
    state.capture_dir.to_string_lossy().to_string()
}

#[tauri::command]
fn capture_screens_now(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
) -> Result<Vec<String>, String> {
    let mode = setting(&state.db_path, "capture_display_mode")?.unwrap_or_else(|| "active".into());
    let main = app.get_webview_window("main");
    let was_visible = main
        .as_ref()
        .and_then(|window| window.is_visible().ok())
        .unwrap_or(false);
    if was_visible {
        if let Some(window) = &main {
            let _ = window.hide();
        }
        std::thread::sleep(std::time::Duration::from_millis(120));
    }
    let result = capture::capture_full(&state, &mode);
    if was_visible {
        if let Some(window) = &main {
            let _ = window.show();
        }
    }
    let paths = result?;
    let _ = app.emit(
        "capture-created",
        state.capture_dir.to_string_lossy().to_string(),
    );
    Ok(paths
        .into_iter()
        .map(|path| path.to_string_lossy().to_string())
        .collect())
}

#[tauri::command]
fn get_autostart_enabled(app: tauri::AppHandle) -> Result<bool, String> {
    use tauri_plugin_autostart::ManagerExt;
    app.autolaunch()
        .is_enabled()
        .map_err(|error| error.to_string())
}

#[tauri::command]
fn set_autostart_enabled(app: tauri::AppHandle, enabled: bool) -> Result<(), String> {
    use tauri_plugin_autostart::ManagerExt;
    let manager = app.autolaunch();
    if enabled {
        manager.enable()
    } else {
        manager.disable()
    }
    .map_err(|error| error.to_string())
}

fn show_main(app: &tauri::AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.set_focus();
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            Some(vec!["--background"]),
        ))
        .setup(|app| {
            let app_data = app.path().app_data_dir()?;
            std::fs::create_dir_all(&app_data)?;
            let thumbnail_dir = app_data.join("thumbnails");
            let capture_dir = app_data.join("captures");
            std::fs::create_dir_all(&thumbnail_dir)?;
            std::fs::create_dir_all(&capture_dir)?;
            let db_path = app_data.join("screenshot-memory.sqlite3");
            library::initialize_database(&db_path).map_err(std::io::Error::other)?;

            // Windows Snipping Tool auto-saves to Pictures/Screenshots by default.
            // Watch that dedicated folder so new snips enter the managed library
            // without asking users to refresh or manually add the common location.
            #[cfg(target_os = "windows")]
            if let Ok(pictures_dir) = app.path().picture_dir() {
                let snipping_folder = pictures_dir.join("Screenshots");
                match std::fs::create_dir_all(&snipping_folder)
                    .and_then(|()| std::fs::canonicalize(&snipping_folder))
                {
                    Ok(snipping_folder) => {
                        if let Err(error) =
                            library::persist_monitored_folder(&db_path, &snipping_folder)
                        {
                            eprintln!("Could not enable Snipping Tool import: {error}");
                        }
                    }
                    Err(error) => {
                        eprintln!("Could not prepare the Snipping Tool Screenshots folder: {error}")
                    }
                }
            }

            let import_lock = Arc::new(Mutex::new(()));
            let folder_watcher = folder_watch::FolderWatchService::new(
                app.handle().clone(),
                db_path.clone(),
                thumbnail_dir.clone(),
                capture_dir.clone(),
                Arc::clone(&import_lock),
            )
            .map_err(std::io::Error::other)?;
            app.manage(AppState {
                db_path,
                thumbnail_dir: thumbnail_dir.clone(),
                capture_dir: capture_dir.clone(),
                pending_region: Mutex::new(None),
                shortcut_error: Mutex::new(None),
                folder_watcher: Arc::new(Mutex::new(folder_watcher)),
                import_lock: Arc::clone(&import_lock),
            });

            let startup_app = app.handle().clone();
            let startup_db = app_data.join("screenshot-memory.sqlite3");
            let startup_thumbnails = thumbnail_dir.clone();
            let startup_captures = capture_dir.clone();
            let startup_lock = Arc::clone(&import_lock);
            tauri::async_runtime::spawn_blocking(move || {
                let Ok(_guard) = startup_lock.lock() else {
                    return;
                };
                let Ok(configured) = library::monitored_folders_from_db(&startup_db) else {
                    return;
                };
                let roots = configured
                    .into_iter()
                    .filter_map(|path| std::fs::canonicalize(path).ok())
                    .filter(|path| path.is_dir())
                    .collect::<Vec<_>>();
                let report = library::import_monitored_folders(
                    &startup_db,
                    &startup_thumbnails,
                    &startup_captures,
                    &roots,
                );
                if report.imported > 0 {
                    let _ = startup_app.emit("library-changed", report.imported.to_string());
                }
            });

            let full_screen =
                Shortcut::new(Some(Modifiers::CONTROL | Modifiers::SHIFT), Code::KeyS);
            let select_region =
                Shortcut::new(Some(Modifiers::CONTROL | Modifiers::SHIFT), Code::KeyR);
            let full_screen_for_register = full_screen.clone();
            let select_region_for_register = select_region.clone();
            let handle = app.handle().clone();
            app.handle().plugin(
                tauri_plugin_global_shortcut::Builder::new()
                    .with_handler(move |app, shortcut, event| {
                        if event.state() != ShortcutState::Pressed {
                            return;
                        }
                        if shortcut == &full_screen {
                            let app = app.clone();
                            tauri::async_runtime::spawn_blocking(move || {
                                let state = app.state::<AppState>();
                                let mode = setting(&state.db_path, "capture_display_mode")
                                    .ok()
                                    .flatten()
                                    .unwrap_or_else(|| "active".into());
                                let main = app.get_webview_window("main");
                                let was_visible = main
                                    .as_ref()
                                    .and_then(|window| window.is_visible().ok())
                                    .unwrap_or(false);
                                if was_visible {
                                    if let Some(window) = &main {
                                        let _ = window.hide();
                                    }
                                    std::thread::sleep(std::time::Duration::from_millis(120));
                                }
                                let result = capture::capture_full(&state, &mode);
                                if was_visible {
                                    if let Some(window) = &main {
                                        let _ = window.show();
                                    }
                                }
                                match result {
                                    Ok(_) => {
                                        let _ = app.emit(
                                            "capture-created",
                                            state.capture_dir.to_string_lossy().to_string(),
                                        );
                                    }
                                    Err(error) => {
                                        let _ = app.emit("capture-failed", error);
                                    }
                                }
                            });
                        } else if shortcut == &select_region {
                            if let Err(error) = capture::open_region_overlay(app) {
                                let _ = app.emit("capture-failed", error);
                            }
                        }
                    })
                    .build(),
            )?;
            let mut shortcut_errors = Vec::new();
            if let Err(error) = app.global_shortcut().register(full_screen_for_register) {
                shortcut_errors.push(format!("Ctrl + Shift + S unavailable: {error}"));
            }
            if let Err(error) = app.global_shortcut().register(select_region_for_register) {
                shortcut_errors.push(format!("Ctrl + Shift + R unavailable: {error}"));
            }
            if !shortcut_errors.is_empty() {
                *handle
                    .state::<AppState>()
                    .shortcut_error
                    .lock()
                    .map_err(|_| "Capture status is unavailable.")? =
                    Some(shortcut_errors.join(". "));
            }

            let show =
                MenuItem::with_id(app, "show", "Open Screenshot Memory", true, None::<&str>)?;
            let quit =
                MenuItem::with_id(app, "quit", "Quit Screenshot Memory", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&show, &quit])?;
            TrayIconBuilder::new()
                .icon(
                    app.default_window_icon()
                        .ok_or("Default app icon is missing")?
                        .clone(),
                )
                .tooltip("Screenshot Memory")
                .menu(&menu)
                .on_menu_event(|app, event| match event.id().as_ref() {
                    "show" => show_main(app),
                    "quit" => app.exit(0),
                    _ => {}
                })
                .on_tray_icon_event(|tray, event| {
                    if let tauri::tray::TrayIconEvent::Click {
                        button: tauri::tray::MouseButton::Left,
                        button_state: tauri::tray::MouseButtonState::Up,
                        ..
                    } = event
                    {
                        show_main(tray.app_handle());
                    }
                })
                .build(app)?;

            if std::env::args().any(|argument| argument == "--background") {
                if let Some(window) = app.get_webview_window("main") {
                    let _ = window.hide();
                }
            } else if !cfg!(debug_assertions) {
                use tauri_plugin_autostart::ManagerExt;
                let _ = app.autolaunch().enable();
            }

            if !std::env::args().any(|argument| argument == "--background") {
                show_main(app.handle());
            }

            if let Some(window) = app.get_webview_window("main") {
                let background_window = window.clone();
                window.on_window_event(move |event| {
                    if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                        api.prevent_close();
                        let _ = background_window.hide();
                    }
                });
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            library::get_library_folder,
            library::set_library_folder,
            library::get_monitored_folders,
            library::set_screenshot_category,
            add_monitored_folder,
            remove_monitored_folder,
            sync_monitored_folders,
            library::index_folder,
            library::list_screenshots,
            library::search_screenshots,
            library::get_ocr_image,
            library::get_screenshot_preview,
            library::copy_screenshot_image_to_clipboard,
            library::save_screenshot_copy,
            library::save_ocr_text,
            library::save_semantic_embedding,
            library::get_unembedded_texts,
            library::mark_ocr_failed,
            get_capture_display_mode,
            set_capture_display_mode,
            get_capture_status,
            get_capture_folder,
            capture_screens_now,
            get_autostart_enabled,
            set_autostart_enabled,
            capture::get_region_preview,
            capture::finish_region_capture,
            capture::cancel_region_capture
        ])
        .run(tauri::generate_context!())
        .expect("error while running Screenshot Memory");
}
