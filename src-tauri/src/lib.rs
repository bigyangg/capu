mod capture;
mod folder_watch;
mod library;

use rusqlite::{Connection, OptionalExtension};
use serde::Serialize;
use std::{
    path::{Path, PathBuf},
    str::FromStr,
    sync::{Arc, Mutex},
};
use tauri::{
    Emitter, Manager,
    menu::{Menu, MenuItem},
    tray::TrayIconBuilder,
};
use tauri_plugin_global_shortcut::{GlobalShortcutExt, Modifiers, Shortcut, ShortcutState};

pub struct AppState {
    pub db_path: PathBuf,
    pub thumbnail_dir: PathBuf,
    pub capture_dir: PathBuf,
    pub pending_region: Mutex<Option<capture::PendingRegion>>,
    pub shortcut_error: Mutex<Option<String>>,
    pub shortcuts: Mutex<CaptureShortcuts>,
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
            library::ChangeCheck::Quick,
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

/// Imports new and changed screenshots from every watched folder. With `full`, every file
/// is read and compared, which also finds edits that kept the same size and modified time.
#[tauri::command]
async fn sync_monitored_folders(
    state: tauri::State<'_, AppState>,
    full: Option<bool>,
) -> Result<library::ImportSummary, String> {
    let check = if full.unwrap_or(false) {
        library::ChangeCheck::Full
    } else {
        library::ChangeCheck::Quick
    };
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
        let mut report = library::import_monitored_folders(
            &db_path,
            &thumbnail_dir,
            &capture_dir,
            &roots,
            check,
        );
        report.monitored_folders = configured;
        Ok(report)
    })
    .await
    .map_err(|error| format!("Could not sync monitored folders: {error}"))?
}

/// Global capture shortcuts as accelerator strings such as "Alt+Shift+S".
#[derive(Clone, Serialize)]
pub struct CaptureShortcuts {
    pub full_screen: String,
    pub region: String,
}

// Ctrl+Shift+S is "Save As" and Ctrl+Shift+R is a hard reload in many Windows apps, so the
// Windows defaults use Alt+Shift instead. On macOS, Control+Shift combinations are rarely
// claimed by apps, while Cmd+Shift+S is "Save As" and Cmd+Shift+3/4/5 are system captures.
#[cfg(target_os = "macos")]
const DEFAULT_SHORTCUTS: (&str, &str) = ("Control+Shift+S", "Control+Shift+R");
#[cfg(not(target_os = "macos"))]
const DEFAULT_SHORTCUTS: (&str, &str) = ("Alt+Shift+S", "Alt+Shift+R");

const AUTOSTART_INITIALIZED_KEY: &str = "autostart_initialized";
const FULL_SCREEN_SHORTCUT_KEY: &str = "shortcut_full_screen";
const REGION_SHORTCUT_KEY: &str = "shortcut_region";

fn parse_shortcut(value: &str) -> Result<Shortcut, String> {
    let shortcut = Shortcut::from_str(value)
        .map_err(|error| format!("{value} is not a valid shortcut: {error}"))?;
    // Shift alone would take over ordinary typing, such as capital letters.
    if !shortcut
        .mods
        .intersects(Modifiers::CONTROL | Modifiers::ALT | Modifiers::SUPER)
    {
        return Err("Include Ctrl, Alt or Cmd in the shortcut.".into());
    }
    Ok(shortcut)
}

fn stored_shortcuts(db_path: &Path) -> CaptureShortcuts {
    let read = |key: &str, fallback: &str| {
        setting(db_path, key)
            .ok()
            .flatten()
            .filter(|value| parse_shortcut(value).is_ok())
            .unwrap_or_else(|| fallback.to_string())
    };
    CaptureShortcuts {
        full_screen: read(FULL_SCREEN_SHORTCUT_KEY, DEFAULT_SHORTCUTS.0),
        region: read(REGION_SHORTCUT_KEY, DEFAULT_SHORTCUTS.1),
    }
}

/// Registers both capture shortcuts and returns a message for each one that failed.
fn register_capture_shortcuts(app: &tauri::AppHandle, shortcuts: &CaptureShortcuts) -> Vec<String> {
    let mut errors = Vec::new();
    for (value, purpose) in [
        (&shortcuts.full_screen, "full screen"),
        (&shortcuts.region, "region"),
    ] {
        let result = parse_shortcut(value).and_then(|shortcut| {
            app.global_shortcut()
                .register(shortcut)
                .map_err(|error| error.to_string())
        });
        if let Err(error) = result {
            errors.push(format!("{value} ({purpose}) unavailable: {error}"));
        }
    }
    errors
}

#[derive(Serialize)]
struct ShortcutSettings {
    full_screen: String,
    region: String,
    default_full_screen: &'static str,
    default_region: &'static str,
    platform: &'static str,
}

fn shortcut_settings(shortcuts: CaptureShortcuts) -> ShortcutSettings {
    ShortcutSettings {
        full_screen: shortcuts.full_screen,
        region: shortcuts.region,
        default_full_screen: DEFAULT_SHORTCUTS.0,
        default_region: DEFAULT_SHORTCUTS.1,
        platform: std::env::consts::OS,
    }
}

#[tauri::command]
fn get_capture_shortcuts(state: tauri::State<'_, AppState>) -> Result<ShortcutSettings, String> {
    let shortcuts = state
        .shortcuts
        .lock()
        .map_err(|_| "Shortcut settings are unavailable.".to_string())?
        .clone();
    Ok(shortcut_settings(shortcuts))
}

/// Replaces both capture shortcuts. If either cannot be registered, for example because
/// another app holds it, the previous shortcuts are restored and nothing is saved.
#[tauri::command]
async fn set_capture_shortcuts(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    full_screen: String,
    region: String,
) -> Result<ShortcutSettings, String> {
    let full = parse_shortcut(&full_screen)?;
    let select = parse_shortcut(&region)?;
    if full == select {
        return Err("Choose two different shortcuts.".into());
    }
    let previous = state
        .shortcuts
        .lock()
        .map_err(|_| "Shortcut settings are unavailable.".to_string())?
        .clone();
    let manager = app.global_shortcut();
    for old in [&previous.full_screen, &previous.region] {
        if let Ok(shortcut) = parse_shortcut(old) {
            let _ = manager.unregister(shortcut);
        }
    }
    if let Err(error) = manager
        .register(full)
        .and_then(|()| manager.register(select))
    {
        let _ = manager.unregister(full);
        let _ = manager.unregister(select);
        let restore_errors = register_capture_shortcuts(&app, &previous);
        if let Ok(mut status) = state.shortcut_error.lock() {
            *status = (!restore_errors.is_empty()).then(|| restore_errors.join(". "));
        }
        return Err(format!(
            "That shortcut is unavailable; another app may be using it. ({error})"
        ));
    }
    let updated = CaptureShortcuts {
        full_screen,
        region,
    };
    set_setting(
        &state.db_path,
        FULL_SCREEN_SHORTCUT_KEY,
        &updated.full_screen,
    )?;
    set_setting(&state.db_path, REGION_SHORTCUT_KEY, &updated.region)?;
    *state
        .shortcuts
        .lock()
        .map_err(|_| "Shortcut settings are unavailable.".to_string())? = updated.clone();
    if let Ok(mut status) = state.shortcut_error.lock() {
        *status = None;
    }
    Ok(shortcut_settings(updated))
}

fn set_setting(db_path: &Path, key: &str, value: &str) -> Result<(), String> {
    Connection::open(db_path)
        .map_err(|error| error.to_string())?
        .execute(
            "INSERT INTO settings(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            [key, value],
        )
        .map_err(|error| error.to_string())?;
    Ok(())
}

fn setting(db_path: &Path, key: &str) -> Result<Option<String>, String> {
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
    set_setting(&state.db_path, "capture_display_mode", &mode)
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

/// Captures the configured displays, hiding the main window first so it is not in the shot.
fn capture_full_screen(app: &tauri::AppHandle) -> Result<Vec<PathBuf>, String> {
    let state = app.state::<AppState>();
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
    Ok(paths)
}

#[tauri::command]
fn capture_screens_now(app: tauri::AppHandle) -> Result<Vec<String>, String> {
    Ok(capture_full_screen(&app)?
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
        // Serves thumbnails as images (thumb://localhost/<hash>, or
        // http://thumb.localhost/<hash> on Windows). Content-addressed, so they cache forever.
        .register_asynchronous_uri_scheme_protocol("thumb", |context, request, responder| {
            let thumbnail_dir = context
                .app_handle()
                .state::<AppState>()
                .thumbnail_dir
                .clone();
            let name = request.uri().path().trim_start_matches('/').to_string();
            tauri::async_runtime::spawn_blocking(move || {
                let response = match library::read_thumbnail(&thumbnail_dir, &name) {
                    Some(bytes) => tauri::http::Response::builder()
                        .header(tauri::http::header::CONTENT_TYPE, "image/jpeg")
                        .header(
                            tauri::http::header::CACHE_CONTROL,
                            "public, max-age=31536000, immutable",
                        )
                        .body(bytes),
                    None => tauri::http::Response::builder()
                        .status(tauri::http::StatusCode::NOT_FOUND)
                        .body(Vec::new()),
                };
                if let Ok(response) = response {
                    responder.respond(response);
                }
            });
        })
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            Some(vec!["--background"]),
        ))
        .setup(|app| {
            // Development builds only: SCREENSHOT_MEMORY_DEV_DATA points the app at a separate
            // data folder, so a test build can run beside the installed app without sharing its
            // library. It also skips the default Snipping Tool folder. Ignored in release builds.
            let dev_data = std::env::var_os("SCREENSHOT_MEMORY_DEV_DATA")
                .filter(|_| cfg!(debug_assertions))
                .map(PathBuf::from);
            let app_data = match &dev_data {
                Some(folder) => folder.clone(),
                None => app.path().app_data_dir()?,
            };
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
            if let (None, Ok(pictures_dir)) = (&dev_data, app.path().picture_dir()) {
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
            let shortcuts = stored_shortcuts(&db_path);
            app.manage(AppState {
                db_path,
                thumbnail_dir: thumbnail_dir.clone(),
                capture_dir: capture_dir.clone(),
                pending_region: Mutex::new(None),
                shortcut_error: Mutex::new(None),
                shortcuts: Mutex::new(shortcuts.clone()),
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
                let converted = library::convert_imports_to_references(
                    &startup_db,
                    &startup_thumbnails,
                    &startup_captures,
                )
                .unwrap_or_else(|error| {
                    eprintln!("Could not convert earlier import copies: {error}");
                    0
                });
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
                    library::ChangeCheck::Quick,
                );
                if report.imported > 0 || report.moved > 0 || converted > 0 {
                    let _ = startup_app.emit("library-changed", report.imported.to_string());
                }
                if report.removed > 0 {
                    let _ = startup_app.emit("library-items-removed", report.removed.to_string());
                }
            });

            app.handle().plugin(
                tauri_plugin_global_shortcut::Builder::new()
                    .with_handler(|app, shortcut, event| {
                        if event.state() != ShortcutState::Pressed {
                            return;
                        }
                        let Ok(current) = app
                            .state::<AppState>()
                            .shortcuts
                            .lock()
                            .map(|shortcuts| shortcuts.clone())
                        else {
                            return;
                        };
                        if parse_shortcut(&current.full_screen).as_ref() == Ok(shortcut) {
                            let app = app.clone();
                            tauri::async_runtime::spawn_blocking(move || {
                                if let Err(error) = capture_full_screen(&app) {
                                    let _ = app.emit("capture-failed", error);
                                }
                            });
                        } else if parse_shortcut(&current.region).as_ref() == Ok(shortcut) {
                            if let Err(error) = capture::open_region_overlay(app) {
                                let _ = app.emit("capture-failed", error);
                            }
                        }
                    })
                    .build(),
            )?;
            let shortcut_errors = register_capture_shortcuts(app.handle(), &shortcuts);
            if !shortcut_errors.is_empty() {
                *app.state::<AppState>()
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
                // Launch at sign-in starts on for a new install. It is turned on only once, so
                // a user who switches it off in Settings is not overridden on the next launch.
                let db_path = &app.state::<AppState>().db_path;
                if setting(db_path, AUTOSTART_INITIALIZED_KEY)
                    .ok()
                    .flatten()
                    .is_none()
                {
                    use tauri_plugin_autostart::ManagerExt;
                    if app.autolaunch().enable().is_ok() {
                        let _ = set_setting(db_path, AUTOSTART_INITIALIZED_KEY, "1");
                    }
                }
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
            library::get_monitored_folders,
            library::set_screenshot_category,
            add_monitored_folder,
            remove_monitored_folder,
            sync_monitored_folders,
            library::index_capture_folder,
            library::list_screenshots,
            library::search_screenshots,
            library::get_ocr_image,
            library::get_screenshot_preview,
            library::copy_screenshot_image_to_clipboard,
            library::save_screenshot_copy,
            library::open_screenshot,
            library::get_storage_usage,
            library::get_monitored_folder_status,
            library::remove_missing_screenshot,
            library::save_ocr_text,
            library::save_semantic_embedding,
            library::get_unembedded_texts,
            library::save_image_embedding,
            library::get_unembedded_images,
            library::mark_ocr_failed,
            get_capture_display_mode,
            set_capture_display_mode,
            get_capture_status,
            get_capture_shortcuts,
            set_capture_shortcuts,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_shortcuts_are_valid_and_distinct() {
        let full = parse_shortcut(DEFAULT_SHORTCUTS.0).expect("full-screen default parses");
        let region = parse_shortcut(DEFAULT_SHORTCUTS.1).expect("region default parses");
        assert_ne!(full, region);
    }

    #[test]
    fn shortcut_needs_ctrl_alt_or_cmd() {
        assert!(parse_shortcut("S").is_err());
        assert!(parse_shortcut("Shift+S").is_err());
        assert!(parse_shortcut("Alt+Shift+S").is_ok());
        assert!(parse_shortcut("Control+F9").is_ok());
        assert!(parse_shortcut("Super+Shift+2").is_ok());
        assert!(parse_shortcut("Alt+NotAKey").is_err());
    }
}
