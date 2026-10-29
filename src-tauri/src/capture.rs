use crate::AppState;
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use image::{DynamicImage, ImageFormat};
use mouse_position::mouse_position::Mouse;
use std::{
    fs,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};
use tauri::{Emitter, Manager, WebviewUrl, WebviewWindowBuilder};
use xcap::Monitor;

pub struct PendingRegion {
    pub preview_path: PathBuf,
}

fn timestamp() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

fn write_capture(state: &AppState, image: &DynamicImage, suffix: &str) -> Result<PathBuf, String> {
    fs::create_dir_all(&state.capture_dir).map_err(|error| error.to_string())?;
    let path = state
        .capture_dir
        .join(format!("Screenshot-{}-{suffix}.png", timestamp()));
    image
        .save_with_format(&path, ImageFormat::Png)
        .map_err(|error| error.to_string())?;
    Ok(path)
}

fn cursor_position() -> Option<(i32, i32)> {
    match Mouse::get_mouse_position() {
        Mouse::Position { x, y } => Some((x, y)),
        Mouse::Error => None,
    }
}

fn active_monitor() -> Result<Monitor, String> {
    let monitors = Monitor::all().map_err(|error| error.to_string())?;
    if monitors.is_empty() {
        return Err("No displays were found.".into());
    }
    if let Some((x, y)) = cursor_position() {
        if let Ok(monitor) = Monitor::from_point(x, y) {
            return Ok(monitor);
        }
    }
    monitors
        .into_iter()
        .find(|monitor| monitor.is_primary().unwrap_or(false))
        .ok_or_else(|| "No active display was found.".into())
}

pub fn capture_full(state: &AppState, mode: &str) -> Result<Vec<PathBuf>, String> {
    let monitors = Monitor::all().map_err(|error| error.to_string())?;
    if monitors.is_empty() {
        return Err("No displays were found.".into());
    }
    let selected = if mode == "all" {
        monitors
    } else {
        vec![active_monitor()?]
    };
    selected
        .into_iter()
        .enumerate()
        .map(|(index, monitor)| {
            let image = monitor.capture_image().map_err(|error| error.to_string())?;
            write_capture(
                state,
                &DynamicImage::ImageRgba8(image),
                &format!("screen-{}", index + 1),
            )
        })
        .collect()
}

pub fn open_region_overlay(app: &tauri::AppHandle) -> Result<(), String> {
    if app.get_webview_window("capture-region").is_some() {
        return Ok(());
    }
    let monitor = active_monitor()?;
    let preview_path = app
        .path()
        .app_data_dir()
        .map_err(|error| error.to_string())?
        .join("region-preview.png");
    monitor
        .capture_image()
        .map_err(|error| error.to_string())?
        .save(&preview_path)
        .map_err(|error| error.to_string())?;
    let (width, height) = (
        monitor.width().map_err(|error| error.to_string())?,
        monitor.height().map_err(|error| error.to_string())?,
    );
    let (x, y) = (
        monitor.x().map_err(|error| error.to_string())?,
        monitor.y().map_err(|error| error.to_string())?,
    );
    let scale = app
        .get_webview_window("main")
        .and_then(|window| window.available_monitors().ok())
        .unwrap_or_default()
        .into_iter()
        .find(|screen| {
            let position = screen.position();
            let size = screen.size();
            x >= position.x
                && y >= position.y
                && x < position.x + size.width as i32
                && y < position.y + size.height as i32
        })
        .map(|screen| screen.scale_factor())
        .unwrap_or(1.0)
        .max(1.0);
    *app.state::<AppState>()
        .pending_region
        .lock()
        .map_err(|_| "Capture state is unavailable.".to_string())? =
        Some(PendingRegion { preview_path });
    WebviewWindowBuilder::new(
        app,
        "capture-region",
        WebviewUrl::App("index.html?capture=region".into()),
    )
    .title("Select screenshot region")
    .decorations(false)
    .always_on_top(true)
    .skip_taskbar(true)
    .resizable(false)
    .position(x as f64 / scale, y as f64 / scale)
    .inner_size(width as f64 / scale, height as f64 / scale)
    .build()
    .map_err(|error| error.to_string())?;
    Ok(())
}

#[tauri::command]
pub fn get_region_preview(state: tauri::State<'_, AppState>) -> Result<String, String> {
    let guard = state
        .pending_region
        .lock()
        .map_err(|_| "Capture state is unavailable.".to_string())?;
    let region = guard
        .as_ref()
        .ok_or_else(|| "The region selection has expired.".to_string())?;
    let bytes = fs::read(&region.preview_path).map_err(|error| error.to_string())?;
    Ok(format!("data:image/png;base64,{}", BASE64.encode(bytes)))
}

#[tauri::command]
pub fn finish_region_capture(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    x: f64,
    y: f64,
    width: f64,
    height: f64,
) -> Result<String, String> {
    if ![x, y, width, height].into_iter().all(f64::is_finite) || width <= 0.0 || height <= 0.0 {
        return Err("Select a region before saving.".into());
    }
    let mut guard = state
        .pending_region
        .lock()
        .map_err(|_| "Capture state is unavailable.".to_string())?;
    let region = guard
        .take()
        .ok_or_else(|| "The region selection has expired.".to_string())?;
    let source = image::open(&region.preview_path).map_err(|error| error.to_string())?;
    let left = (x.clamp(0.0, 1.0) * source.width() as f64).floor() as u32;
    let top = (y.clamp(0.0, 1.0) * source.height() as f64).floor() as u32;
    let right = ((x + width).clamp(0.0, 1.0) * source.width() as f64).ceil() as u32;
    let bottom = ((y + height).clamp(0.0, 1.0) * source.height() as f64).ceil() as u32;
    let crop_width = right.saturating_sub(left);
    let crop_height = bottom.saturating_sub(top);
    if crop_width < 2 || crop_height < 2 {
        return Err("The selected region is too small.".into());
    }
    let cropped = source.crop_imm(left, top, crop_width, crop_height);
    let path = write_capture(&state, &cropped, "region")?;
    let _ = fs::remove_file(&region.preview_path);
    let _ = app.emit(
        "capture-created",
        state.capture_dir.to_string_lossy().to_string(),
    );
    if let Some(window) = app.get_webview_window("capture-region") {
        let _ = window.close();
    }
    Ok(path.to_string_lossy().to_string())
}

#[tauri::command]
pub fn cancel_region_capture(app: tauri::AppHandle, state: tauri::State<'_, AppState>) {
    if let Ok(mut guard) = state.pending_region.lock() {
        if let Some(region) = guard.take() {
            let _ = fs::remove_file(region.preview_path);
        }
    }
    if let Some(window) = app.get_webview_window("capture-region") {
        let _ = window.close();
    }
}
