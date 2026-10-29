use crate::AppState;
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use image::ImageFormat;
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::fs::{self, File};
use std::io::{BufReader, Cursor, Read};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tauri::State;

const IMAGE_EXTENSIONS: &[&str] = &["png", "jpg", "jpeg", "webp", "bmp", "gif", "tif", "tiff"];

#[derive(Serialize)]
pub struct Screenshot {
    id: i64,
    path: String,
    file_name: String,
    captured_at: i64,
    file_size: u64,
    status: String,
    ocr_text: String,
    last_error: Option<String>,
    thumbnail_data_url: Option<String>,
    category: Option<String>,
    subcategory: Option<String>,
    suggested_category: Option<String>,
    suggested_subcategory: Option<String>,
    category_is_manual: bool,
    match_reason: Option<String>,
}

#[derive(Serialize)]
pub struct SemanticText {
    pub id: i64,
    pub text: String,
}

#[derive(Serialize)]
pub struct IndexReport {
    discovered: usize,
    indexed: usize,
    duplicates: usize,
    failed: usize,
    screenshots: Vec<Screenshot>,
}

#[derive(Serialize)]
pub struct ImportSummary {
    pub imported: usize,
    pub duplicates: usize,
    pub failed: usize,
    pub monitored_folders: Vec<String>,
}

fn connection(db_path: &Path) -> Result<Connection, String> {
    let conn = Connection::open(db_path).map_err(|error| error.to_string())?;
    conn.busy_timeout(Duration::from_secs(5))
        .map_err(|error| error.to_string())?;
    Ok(conn)
}

pub fn initialize_database(path: &Path) -> Result<(), String> {
    let conn = connection(path)?;
    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA foreign_keys = ON;
         CREATE TABLE IF NOT EXISTS settings (
             key TEXT PRIMARY KEY NOT NULL,
             value TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS screenshots (
             id INTEGER PRIMARY KEY,
             path TEXT NOT NULL UNIQUE,
             file_hash TEXT,
             file_name TEXT NOT NULL,
             captured_at INTEGER NOT NULL,
             file_size INTEGER NOT NULL DEFAULT 0,
             status TEXT NOT NULL DEFAULT 'pending',
             ocr_text TEXT NOT NULL DEFAULT '',
             last_error TEXT,
             thumbnail_path TEXT,
             category TEXT,
             subcategory TEXT,
             suggested_category TEXT,
             suggested_subcategory TEXT,
             category_is_manual INTEGER NOT NULL DEFAULT 0
         );
         CREATE TABLE IF NOT EXISTS screenshot_embeddings (
             screenshot_id INTEGER PRIMARY KEY REFERENCES screenshots(id) ON DELETE CASCADE,
             vector BLOB NOT NULL
         );
         CREATE TABLE IF NOT EXISTS monitored_folders (
             path TEXT PRIMARY KEY NOT NULL
         );
         CREATE INDEX IF NOT EXISTS screenshots_captured_at ON screenshots(captured_at DESC);
         CREATE INDEX IF NOT EXISTS screenshots_hash ON screenshots(file_hash);
         CREATE VIRTUAL TABLE IF NOT EXISTS screenshot_fts USING fts5(
             screenshot_id UNINDEXED,
             ocr_text,
             tokenize = 'unicode61 remove_diacritics 2'
         );",
    )
    .map_err(|error| error.to_string())?;

    for (column, definition) in [
        ("category", "TEXT"),
        ("subcategory", "TEXT"),
        ("suggested_category", "TEXT"),
        ("suggested_subcategory", "TEXT"),
        ("category_is_manual", "INTEGER NOT NULL DEFAULT 0"),
    ] {
        let exists: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM pragma_table_info('screenshots') WHERE name=?1)",
                [column],
                |row| row.get(0),
            )
            .map_err(|error| error.to_string())?;
        if !exists {
            conn.execute_batch(&format!(
                "ALTER TABLE screenshots ADD COLUMN {column} {definition}"
            ))
            .map_err(|error| error.to_string())?;
        }
    }

    let mut statement = conn
        .prepare(
            "SELECT id,ocr_text FROM screenshots WHERE ocr_text != '' AND category_is_manual=0",
        )
        .map_err(|error| error.to_string())?;
    let rows = statement
        .query_map([], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(|error| error.to_string())?;
    let mut backfill = Vec::new();
    for row in rows {
        backfill.push(row.map_err(|error| error.to_string())?);
    }
    drop(statement);
    for (id, text) in backfill {
        let (suggestion, subcategory) = suggest_category(&text);
        conn.execute(
            "UPDATE screenshots SET suggested_category=?1,suggested_subcategory=?2,
             category=CASE WHEN category_is_manual=1 THEN category ELSE ?1 END,
             subcategory=CASE WHEN category_is_manual=1 THEN subcategory ELSE ?2 END WHERE id=?3",
            params![suggestion, subcategory, id],
        )
        .map_err(|error| error.to_string())?;
    }
    Ok(())
}

pub const CATEGORIES: &[&str] = &[
    "Projects",
    "Finance",
    "Meetings",
    "Development",
    "Reference",
];

pub fn subcategories(category: &str) -> &'static [&'static str] {
    match category {
        "Projects" => &[
            "Websites & landing pages",
            "Client projects",
            "Product planning",
            "Campaigns",
        ],
        "Finance" => &[
            "Budgets",
            "Invoices & receipts",
            "Payments & statements",
            "Taxes",
        ],
        "Meetings" => &["Notes & agendas", "Decisions", "Follow-ups"],
        "Development" => &[
            "Code & terminal",
            "Errors & debugging",
            "Hosting & domains",
            "Configuration",
        ],
        _ => &["Design inspiration", "Documentation", "Saved references"],
    }
}

fn validate_category_selection(
    category: Option<&str>,
    subcategory: Option<&str>,
) -> Result<(), String> {
    if let Some(value) = category
        && !CATEGORIES.contains(&value)
    {
        return Err("Choose one of the available categories.".to_string());
    }
    if let Some(value) = subcategory {
        let valid = category.is_some_and(|parent| subcategories(parent).contains(&value));
        if !valid {
            return Err("Choose a subcategory that belongs to the selected category.".to_string());
        }
    }
    Ok(())
}

fn suggest_category(text: &str) -> (Option<String>, Option<String>) {
    if text.trim().is_empty() {
        return (None, None);
    }
    let normalized = text.to_lowercase();
    let tokens: std::collections::HashSet<&str> = normalized
        .split(|character: char| !character.is_alphanumeric())
        .filter(|token| !token.is_empty())
        .collect();
    let score = |terms: &[&str], phrases: &[&str]| -> usize {
        terms.iter().filter(|term| tokens.contains(**term)).count() * 2
            + phrases
                .iter()
                .filter(|phrase| normalized.contains(**phrase))
                .count()
                * 2
    };
    let scores = [
        (
            "Projects",
            score(
                &[
                    "project",
                    "sprint",
                    "roadmap",
                    "milestone",
                    "backlog",
                    "ticket",
                    "jira",
                    "kanban",
                ],
                &[
                    "project plan",
                    "release plan",
                    "landing page",
                    "home page",
                    "website",
                    "web design",
                    "digital presence",
                    "services about contact",
                ],
            ),
        ),
        (
            "Finance",
            score(
                &[
                    "budget",
                    "invoice",
                    "receipt",
                    "expense",
                    "finance",
                    "financial",
                    "tax",
                    "payment",
                    "paid",
                    "subtotal",
                    "usd",
                    "eur",
                    "balance",
                ],
                &[
                    "total due",
                    "payment method",
                    "account statement",
                    "credit card",
                ],
            ),
        ),
        (
            "Meetings",
            score(
                &[
                    "meeting",
                    "agenda",
                    "minutes",
                    "attendees",
                    "zoom",
                    "teams",
                    "calendar",
                    "conference",
                    "standup",
                ],
                &["action items", "call with", "meeting notes"],
            ),
        ),
        (
            "Development",
            score(
                &[
                    "code",
                    "error",
                    "terminal",
                    "stack",
                    "traceback",
                    "compiler",
                    "debug",
                    "function",
                    "class",
                    "const",
                    "npm",
                    "git",
                    "rust",
                    "python",
                    "typescript",
                    "javascript",
                    "localhost",
                    "exception",
                ],
                &["stack trace", "pull request", "command line"],
            ),
        ),
    ];
    let category = scores
        .into_iter()
        .max_by_key(|(_, points)| *points)
        .filter(|(_, points)| *points >= 2)
        .map(|(category, _)| category.to_string())
        .or_else(|| Some("Reference".to_string()));
    let subcategory = match category.as_deref() {
        Some("Projects")
            if [
                "website",
                "homepage",
                "landing",
                "domain",
                "web design",
                "digital presence",
                "services",
            ]
            .iter()
            .any(|term| normalized.contains(term))
                || (normalized.contains("services")
                    && normalized.contains("about")
                    && normalized.contains("contact")) =>
        {
            Some("Websites & landing pages")
        }
        Some("Projects")
            if ["campaign", "advertising", "creative", "sponsored"]
                .iter()
                .any(|term| normalized.contains(term)) =>
        {
            Some("Campaigns")
        }
        Some("Projects") => Some("Client projects"),
        Some("Finance")
            if ["invoice", "receipt"]
                .iter()
                .any(|term| tokens.contains(term)) =>
        {
            Some("Invoices & receipts")
        }
        Some("Finance") if ["tax", "taxes"].iter().any(|term| tokens.contains(term)) => {
            Some("Taxes")
        }
        Some("Finance")
            if ["budget", "expense"]
                .iter()
                .any(|term| tokens.contains(term)) =>
        {
            Some("Budgets")
        }
        Some("Finance") => Some("Payments & statements"),
        Some("Meetings")
            if ["agenda", "minutes", "notes"]
                .iter()
                .any(|term| tokens.contains(term)) =>
        {
            Some("Notes & agendas")
        }
        Some("Meetings")
            if ["decision", "decisions"]
                .iter()
                .any(|term| tokens.contains(term)) =>
        {
            Some("Decisions")
        }
        Some("Meetings") => Some("Follow-ups"),
        Some("Development")
            if ["error", "exception", "traceback", "debug"]
                .iter()
                .any(|term| tokens.contains(term)) =>
        {
            Some("Errors & debugging")
        }
        Some("Development")
            if ["domain", "nginx", "hosting", "ssl", "dns"]
                .iter()
                .any(|term| tokens.contains(term)) =>
        {
            Some("Hosting & domains")
        }
        Some("Development")
            if ["config", "configuration", "setting"]
                .iter()
                .any(|term| tokens.contains(term)) =>
        {
            Some("Configuration")
        }
        Some("Development") => Some("Code & terminal"),
        Some("Reference")
            if ["design", "website", "landing", "inspiration"]
                .iter()
                .any(|term| normalized.contains(term)) =>
        {
            Some("Design inspiration")
        }
        Some("Reference")
            if ["docs", "documentation", "manual"]
                .iter()
                .any(|term| tokens.contains(term)) =>
        {
            Some("Documentation")
        }
        Some("Reference") => Some("Saved references"),
        _ => None,
    }
    .map(str::to_string);
    (category, subcategory)
}

#[tauri::command]
pub fn get_library_folder(state: State<'_, AppState>) -> Result<Option<String>, String> {
    let conn = connection(&state.db_path)?;
    conn.query_row(
        "SELECT value FROM settings WHERE key = 'library_folder'",
        [],
        |row| row.get(0),
    )
    .optional()
    .map_err(|error| error.to_string())
}

#[tauri::command]
pub fn set_library_folder(state: State<'_, AppState>, folder: String) -> Result<(), String> {
    let selected =
        fs::canonicalize(&folder).map_err(|error| format!("Cannot access that folder: {error}"))?;
    if !selected.is_dir() {
        return Err("Choose an existing folder.".to_string());
    }
    let conn = connection(&state.db_path)?;
    conn.execute(
        "INSERT INTO settings(key, value) VALUES('library_folder', ?1)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        [selected.to_string_lossy().as_ref()],
    )
    .map_err(|error| error.to_string())?;
    Ok(())
}

pub fn monitored_folders_from_db(db_path: &Path) -> Result<Vec<String>, String> {
    let conn = connection(db_path)?;
    let mut statement = conn
        .prepare("SELECT path FROM monitored_folders ORDER BY path")
        .map_err(|error| error.to_string())?;
    let rows = statement
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(|error| error.to_string())?;
    rows.map(|row| row.map_err(|error| error.to_string()))
        .collect()
}

#[tauri::command]
pub fn get_monitored_folders(state: State<'_, AppState>) -> Result<Vec<String>, String> {
    monitored_folders_from_db(&state.db_path)
}

pub fn persist_monitored_folder(db_path: &Path, folder: &Path) -> Result<(), String> {
    connection(db_path)?
        .execute(
            "INSERT OR IGNORE INTO monitored_folders(path) VALUES(?1)",
            [folder.to_string_lossy().as_ref()],
        )
        .map_err(|error| error.to_string())?;
    Ok(())
}

pub fn remove_monitored_folder_from_db(db_path: &Path, folder: &Path) -> Result<(), String> {
    connection(db_path)?
        .execute(
            "DELETE FROM monitored_folders WHERE path=?1",
            [folder.to_string_lossy().as_ref()],
        )
        .map_err(|error| error.to_string())?;
    Ok(())
}

#[tauri::command]
pub async fn set_screenshot_category(
    state: State<'_, AppState>,
    id: i64,
    category: Option<String>,
    subcategory: Option<String>,
) -> Result<(), String> {
    validate_category_selection(category.as_deref(), subcategory.as_deref())?;
    let db_path = state.db_path.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let conn = connection(&db_path)?;
        conn.execute(
            "UPDATE screenshots SET category_is_manual=?1,
             category=CASE WHEN ?1=1 THEN ?2 ELSE suggested_category END,
             subcategory=CASE WHEN ?1=1 THEN ?3 ELSE suggested_subcategory END WHERE id=?4",
            params![category.is_some(), category, subcategory, id],
        )
        .map_err(|error| error.to_string())?;
        Ok(())
    })
    .await
    .map_err(|error| format!("Category update failed: {error}"))?
}

#[tauri::command]
pub async fn index_folder(
    state: State<'_, AppState>,
    folder: String,
) -> Result<IndexReport, String> {
    let db_path = state.db_path.clone();
    let thumbnail_dir = state.thumbnail_dir.clone();
    tauri::async_runtime::spawn_blocking(move || scan_folder(&db_path, &thumbnail_dir, &folder))
        .await
        .map_err(|error| format!("Indexing task failed: {error}"))?
}

#[tauri::command]
pub async fn list_screenshots(state: State<'_, AppState>) -> Result<Vec<Screenshot>, String> {
    let db_path = state.db_path.clone();
    let thumbnail_dir = state.thumbnail_dir.clone();
    tauri::async_runtime::spawn_blocking(move || load_screenshots(&db_path, &thumbnail_dir, None))
        .await
        .map_err(|error| format!("Library task failed: {error}"))?
}

#[tauri::command]
pub async fn search_screenshots(
    state: State<'_, AppState>,
    query: String,
    embedding: Vec<f32>,
) -> Result<Vec<Screenshot>, String> {
    let db_path = state.db_path.clone();
    let thumbnail_dir = state.thumbnail_dir.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let terms: Vec<String> = query
            .split(|character: char| !character.is_alphanumeric())
            .filter(|term| !term.is_empty())
            .map(|term| format!("\"{}\"*", term.replace('"', "")))
            .collect();
        let mut search_terms = terms.clone();
        let normalized_query = query.to_lowercase();
        if normalized_query.contains("landing page") || normalized_query.contains("landing site") {
            search_terms.extend(
                [
                    "website",
                    "homepage",
                    "home page",
                    "web design",
                    "digital presence",
                    "online presence",
                    "services",
                    "hero section",
                ]
                .iter()
                .map(|term| format!("\"{term}\"*")),
            );
        }
        if normalized_query.contains("homepage") || normalized_query.contains("home page") {
            search_terms.extend(
                ["website", "landing", "digital presence", "services"]
                    .iter()
                    .map(|term| format!("\"{term}\"*")),
            );
        }
        if search_terms.is_empty() && embedding.len() != 384 {
            return load_screenshots(&db_path, &thumbnail_dir, None);
        }
        let text_matches = if search_terms.is_empty() {
            Vec::new()
        } else {
            load_screenshots(&db_path, &thumbnail_dir, Some(search_terms.join(" OR ")))?
        };
        let mut scores = std::collections::HashMap::<i64, f32>::new();
        let mut reasons = std::collections::HashMap::<i64, String>::new();
        for item in text_matches {
            let normalized = item.ocr_text.to_lowercase();
            let matched = query
                .split_whitespace()
                .filter(|word| word.chars().any(char::is_alphanumeric))
                .filter(|word| normalized.contains(&word.to_lowercase()))
                .count();
            scores.insert(item.id, 1.1 + matched as f32 / terms.len().max(1) as f32);
            reasons.insert(
                item.id,
                if matched > 0 {
                    "Text match"
                } else {
                    "Related phrase"
                }
                .to_string(),
            );
        }
        if embedding.len() == 384 && embedding.iter().all(|value| value.is_finite()) {
            let conn = connection(&db_path)?;
            let mut statement = conn
                .prepare("SELECT screenshot_id,vector FROM screenshot_embeddings")
                .map_err(|error| error.to_string())?;
            let mut rows = statement.query([]).map_err(|error| error.to_string())?;
            while let Some(row) = rows.next().map_err(|error| error.to_string())? {
                let id: i64 = row.get(0).map_err(|error| error.to_string())?;
                let bytes: Vec<u8> = row.get(1).map_err(|error| error.to_string())?;
                if bytes.len() != 384 * 4 {
                    continue;
                }
                let similarity = bytes
                    .chunks_exact(4)
                    .zip(&embedding)
                    .map(|(bytes, query)| {
                        f32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) * query
                    })
                    .sum::<f32>();
                if similarity >= 0.24 {
                    scores
                        .entry(id)
                        .and_modify(|score| *score += similarity)
                        .or_insert(similarity);
                    reasons
                        .entry(id)
                        .or_insert_with(|| "Related meaning".to_string());
                }
            }
        }
        let mut ranked = scores.into_iter().collect::<Vec<_>>();
        ranked.sort_by(|left, right| right.1.total_cmp(&left.1));
        ranked.truncate(150);
        let ids = ranked.iter().map(|(id, _)| *id).collect::<Vec<_>>();
        let mut results = load_screenshots_by_ids(&db_path, &thumbnail_dir, &ids)?;
        let mut by_id = results
            .drain(..)
            .map(|item| (item.id, item))
            .collect::<std::collections::HashMap<_, _>>();
        Ok(ranked
            .into_iter()
            .filter_map(|(id, _)| {
                let mut item = by_id.remove(&id)?;
                item.match_reason = reasons.remove(&id);
                Some(item)
            })
            .collect())
    })
    .await
    .map_err(|error| format!("Search task failed: {error}"))?
}

#[tauri::command]
pub async fn get_ocr_image(state: State<'_, AppState>, id: i64) -> Result<String, String> {
    let db_path = state.db_path.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let conn = connection(&db_path)?;
        let path: String = conn
            .query_row("SELECT path FROM screenshots WHERE id = ?1", [id], |row| {
                row.get(0)
            })
            .map_err(|error| error.to_string())?;
        let image =
            image::open(&path).map_err(|error| format!("Cannot read screenshot: {error}"))?;
        let scaled = image::DynamicImage::ImageRgb8(image.thumbnail(2600, 2600).to_rgb8());
        let mut bytes = Cursor::new(Vec::new());
        scaled
            .write_to(&mut bytes, ImageFormat::Jpeg)
            .map_err(|error| error.to_string())?;
        Ok(format!(
            "data:image/jpeg;base64,{}",
            BASE64.encode(bytes.into_inner())
        ))
    })
    .await
    .map_err(|error| format!("Image read task failed: {error}"))?
}

fn screenshot_path_for_id(db_path: &Path, id: i64) -> Result<PathBuf, String> {
    let conn = connection(db_path)?;
    let path: String = conn
        .query_row("SELECT path FROM screenshots WHERE id = ?1", [id], |row| {
            row.get(0)
        })
        .map_err(|_| "That screenshot is no longer in the library.".to_string())?;
    let path = fs::canonicalize(path)
        .map_err(|_| "The original screenshot is unavailable.".to_string())?;
    if !path.is_file() {
        return Err("The original screenshot is unavailable.".to_string());
    }
    Ok(path)
}

#[tauri::command]
pub async fn get_screenshot_preview(state: State<'_, AppState>, id: i64) -> Result<String, String> {
    let db_path = state.db_path.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let path = screenshot_path_for_id(&db_path, id)?;
        let image =
            image::open(&path).map_err(|error| format!("Cannot preview screenshot: {error}"))?;
        let preview = image.thumbnail(3200, 3200);
        let mut bytes = Cursor::new(Vec::new());
        preview
            .write_to(&mut bytes, ImageFormat::Png)
            .map_err(|error| format!("Cannot prepare screenshot preview: {error}"))?;
        Ok(format!(
            "data:image/png;base64,{}",
            BASE64.encode(bytes.into_inner())
        ))
    })
    .await
    .map_err(|error| format!("Preview task failed: {error}"))?
}

#[tauri::command]
pub async fn copy_screenshot_image_to_clipboard(
    state: State<'_, AppState>,
    id: i64,
) -> Result<(), String> {
    let db_path = state.db_path.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let path = screenshot_path_for_id(&db_path, id)?;
        let image =
            image::open(&path).map_err(|error| format!("Cannot read screenshot: {error}"))?;
        let rgba = image.to_rgba8();
        let (width, height) = rgba.dimensions();
        let data = arboard::ImageData {
            width: width as usize,
            height: height as usize,
            bytes: std::borrow::Cow::Owned(rgba.into_raw()),
        };
        arboard::Clipboard::new()
            .and_then(|mut clipboard| clipboard.set_image(data))
            .map_err(|error| format!("Could not copy image to clipboard: {error}"))
    })
    .await
    .map_err(|error| format!("Clipboard task failed: {error}"))?
}

#[tauri::command]
pub async fn save_screenshot_copy(
    state: State<'_, AppState>,
    id: i64,
    destination: String,
) -> Result<(), String> {
    let db_path = state.db_path.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let source = screenshot_path_for_id(&db_path, id)?;
        let destination = PathBuf::from(destination);
        let name = destination
            .file_name()
            .ok_or_else(|| "Choose a valid destination filename.".to_string())?;
        let parent = destination
            .parent()
            .ok_or_else(|| "Choose a destination folder.".to_string())?;
        let parent = fs::canonicalize(parent)
            .map_err(|error| format!("Cannot access the destination folder: {error}"))?;
        let destination = parent.join(name);
        if destination.exists() && fs::canonicalize(&destination).ok().as_ref() == Some(&source) {
            return Err("Choose a different destination so the original stays untouched.".into());
        }
        fs::copy(&source, &destination)
            .map_err(|error| format!("Could not save screenshot copy: {error}"))?;
        Ok(())
    })
    .await
    .map_err(|error| format!("Save task failed: {error}"))?
}

#[tauri::command]
pub fn save_ocr_text(
    state: State<'_, AppState>,
    id: i64,
    text: String,
    embedding: Option<Vec<f32>>,
) -> Result<(), String> {
    let mut conn = connection(&state.db_path)?;
    let transaction = conn.transaction().map_err(|error| error.to_string())?;
    let (suggestion, suggested_subcategory) = suggest_category(&text);
    transaction
        .execute(
            "UPDATE screenshots SET ocr_text=?1, status='indexed', last_error=NULL,
             suggested_category=?2,suggested_subcategory=?3,
             category=CASE WHEN category_is_manual=1 THEN category ELSE ?2 END,
             subcategory=CASE WHEN category_is_manual=1 THEN subcategory ELSE ?3 END WHERE id=?4",
            params![text, suggestion, suggested_subcategory, id],
        )
        .map_err(|error| error.to_string())?;
    if let Some(vector) = embedding {
        store_embedding(&transaction, id, &vector)?;
    }
    transaction
        .execute("DELETE FROM screenshot_fts WHERE screenshot_id = ?1", [id])
        .map_err(|error| error.to_string())?;
    transaction
        .execute(
            "INSERT INTO screenshot_fts(screenshot_id, ocr_text)
             SELECT id, ocr_text FROM screenshots WHERE id = ?1 AND ocr_text != ''",
            [id],
        )
        .map_err(|error| error.to_string())?;
    transaction.commit().map_err(|error| error.to_string())
}

fn store_embedding(conn: &Connection, id: i64, vector: &[f32]) -> Result<(), String> {
    if vector.len() != 384 || vector.iter().any(|value| !value.is_finite()) {
        return Err("The local meaning model returned an invalid vector.".to_string());
    }
    let bytes = vector
        .iter()
        .flat_map(|value| value.to_le_bytes())
        .collect::<Vec<_>>();
    conn.execute(
        "INSERT INTO screenshot_embeddings(screenshot_id,vector) VALUES(?1,?2)
         ON CONFLICT(screenshot_id) DO UPDATE SET vector=excluded.vector",
        params![id, bytes],
    )
    .map_err(|error| error.to_string())?;
    Ok(())
}

#[tauri::command]
pub fn save_semantic_embedding(
    state: State<'_, AppState>,
    id: i64,
    embedding: Vec<f32>,
) -> Result<(), String> {
    let conn = connection(&state.db_path)?;
    store_embedding(&conn, id, &embedding)
}

#[tauri::command]
pub fn get_unembedded_texts(state: State<'_, AppState>) -> Result<Vec<SemanticText>, String> {
    let conn = connection(&state.db_path)?;
    let mut statement = conn
        .prepare(
            "SELECT id,ocr_text FROM screenshots s
         WHERE status='indexed' AND ocr_text!='' AND NOT EXISTS (
             SELECT 1 FROM screenshot_embeddings e WHERE e.screenshot_id=s.id
         ) ORDER BY captured_at DESC LIMIT 1000",
        )
        .map_err(|error| error.to_string())?;
    let rows = statement
        .query_map([], |row| {
            Ok(SemanticText {
                id: row.get(0)?,
                text: row.get(1)?,
            })
        })
        .map_err(|error| error.to_string())?;
    rows.map(|row| row.map_err(|error| error.to_string()))
        .collect()
}

#[tauri::command]
pub fn mark_ocr_failed(state: State<'_, AppState>, id: i64, error: String) -> Result<(), String> {
    let clipped: String = error.chars().take(500).collect();
    let conn = connection(&state.db_path)?;
    conn.execute(
        "UPDATE screenshots SET status = 'error', last_error = ?1 WHERE id = ?2",
        params![clipped, id],
    )
    .map_err(|error| error.to_string())?;
    Ok(())
}

fn scan_folder(db_path: &Path, thumbnail_dir: &Path, folder: &str) -> Result<IndexReport, String> {
    let root =
        fs::canonicalize(folder).map_err(|error| format!("Cannot access that folder: {error}"))?;
    if !root.is_dir() {
        return Err("Choose an existing folder.".to_string());
    }
    let mut paths = Vec::new();
    walk_directory(&root, &mut paths)?;
    paths.sort();
    let conn = connection(db_path)?;
    let mut indexed = 0;
    let mut duplicates = 0;
    let mut failed = 0;

    for path in &paths {
        match index_one(&conn, thumbnail_dir, path) {
            Ok(ScanOutcome::Indexed) => indexed += 1,
            Ok(ScanOutcome::Skipped) => {}
            Ok(ScanOutcome::Duplicate) => duplicates += 1,
            Err(error) => {
                failed += 1;
                let _ = record_scan_error(&conn, path, &error);
            }
        }
    }
    let screenshots = load_screenshots(db_path, thumbnail_dir, None)?;
    Ok(IndexReport {
        discovered: paths.len(),
        indexed,
        duplicates,
        failed,
        screenshots,
    })
}

enum ScanOutcome {
    Indexed,
    Skipped,
    Duplicate,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ImportOutcome {
    Imported,
    Duplicate,
    Skipped,
}

fn is_supported_image(path: &Path) -> bool {
    path.extension()
        .and_then(|value| value.to_str())
        .map(|value| IMAGE_EXTENSIONS.contains(&value.to_ascii_lowercase().as_str()))
        .unwrap_or(false)
}

pub fn import_monitored_file(
    db_path: &Path,
    thumbnail_dir: &Path,
    capture_dir: &Path,
    monitored_roots: &[PathBuf],
    source: &Path,
) -> Result<ImportOutcome, String> {
    if !is_supported_image(source) {
        return Ok(ImportOutcome::Skipped);
    }
    let source = match fs::canonicalize(source) {
        Ok(path) => path,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ImportOutcome::Skipped);
        }
        Err(error) => return Err(error.to_string()),
    };
    if !monitored_roots.iter().any(|root| source.starts_with(root)) {
        return Ok(ImportOutcome::Skipped);
    }
    let managed_root = fs::canonicalize(capture_dir).unwrap_or_else(|_| capture_dir.to_path_buf());
    if source.starts_with(&managed_root) || !source.is_file() {
        return Ok(ImportOutcome::Skipped);
    }

    let (size, modified) = wait_for_import_stable(&source)?;
    let hash = hash_file(&source)?;
    let conn = connection(db_path)?;
    let already_imported: Option<i64> = conn
        .query_row(
            "SELECT id FROM screenshots WHERE file_hash=?1 LIMIT 1",
            [&hash],
            |row| row.get(0),
        )
        .optional()
        .map_err(|error| error.to_string())?;
    if already_imported.is_some() {
        return Ok(ImportOutcome::Duplicate);
    }

    fs::create_dir_all(capture_dir).map_err(|error| error.to_string())?;
    let ext = source
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or("png")
        .to_ascii_lowercase();
    let stem = source
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("Screenshot");
    let readable_stem: String = stem
        .chars()
        .filter(|character| {
            character.is_alphanumeric()
                || *character == '-'
                || *character == '_'
                || *character == ' '
        })
        .take(48)
        .collect();
    let file_name = format!(
        "Imported-{}-{}.{}",
        &hash[..12],
        if readable_stem.is_empty() {
            "Screenshot"
        } else {
            &readable_stem
        },
        ext
    );
    let destination = capture_dir.join(file_name);
    if destination.exists() {
        if hash_file(&destination)? == hash {
            return match index_one(&conn, thumbnail_dir, &destination)? {
                ScanOutcome::Indexed => Ok(ImportOutcome::Imported),
                ScanOutcome::Duplicate => Ok(ImportOutcome::Duplicate),
                ScanOutcome::Skipped => Ok(ImportOutcome::Skipped),
            };
        }
        return Err("A managed screenshot with the same fingerprint already exists.".to_string());
    }
    let temporary = capture_dir.join(format!(".import-{hash}.part"));
    fs::copy(&source, &temporary).map_err(|error| error.to_string())?;
    let copied_size = fs::metadata(&temporary)
        .map_err(|error| error.to_string())?
        .len();
    if copied_size != size || hash_file(&temporary)? != hash {
        let _ = fs::remove_file(&temporary);
        return Err(
            "The screenshot changed while it was being imported. It will be retried.".to_string(),
        );
    }
    fs::rename(&temporary, &destination).map_err(|error| {
        let _ = fs::remove_file(&temporary);
        error.to_string()
    })?;
    if let Err(error) =
        filetime::set_file_mtime(&destination, filetime::FileTime::from_system_time(modified))
    {
        let _ = fs::remove_file(&destination);
        return Err(error.to_string());
    }

    match index_one(&conn, thumbnail_dir, &destination) {
        Ok(ScanOutcome::Indexed) => Ok(ImportOutcome::Imported),
        Ok(ScanOutcome::Duplicate) => {
            let _ = fs::remove_file(&destination);
            Ok(ImportOutcome::Duplicate)
        }
        Ok(ScanOutcome::Skipped) => Ok(ImportOutcome::Skipped),
        Err(error) => {
            let _ = fs::remove_file(&destination);
            Err(error)
        }
    }
}

pub fn import_monitored_event(
    db_path: &Path,
    thumbnail_dir: &Path,
    capture_dir: &Path,
    monitored_roots: &[PathBuf],
    event_path: &Path,
) -> Result<usize, String> {
    if event_path.is_dir() {
        let mut paths = Vec::new();
        walk_directory(event_path, &mut paths)?;
        let mut imported = 0;
        for path in paths {
            if import_monitored_file(db_path, thumbnail_dir, capture_dir, monitored_roots, &path)?
                == ImportOutcome::Imported
            {
                imported += 1;
            }
        }
        Ok(imported)
    } else {
        Ok(usize::from(
            import_monitored_file(
                db_path,
                thumbnail_dir,
                capture_dir,
                monitored_roots,
                event_path,
            )? == ImportOutcome::Imported,
        ))
    }
}

fn wait_for_import_stable(path: &Path) -> Result<(u64, SystemTime), String> {
    let mut last_error = "The file is still being written.".to_string();
    for _ in 0..12 {
        if !path.exists() {
            return Err("The screenshot file is no longer available.".to_string());
        }
        match wait_for_stable_file(path) {
            Ok(stable) => return Ok(stable),
            Err(error) => last_error = error,
        }
        std::thread::sleep(Duration::from_millis(400));
    }
    Err(last_error)
}

fn hash_file(path: &Path) -> Result<String, String> {
    let mut file = BufReader::new(File::open(path).map_err(|error| error.to_string())?);
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = file.read(&mut buffer).map_err(|error| error.to_string())?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

pub fn import_monitored_folders(
    db_path: &Path,
    thumbnail_dir: &Path,
    capture_dir: &Path,
    roots: &[PathBuf],
) -> ImportSummary {
    let mut summary = ImportSummary {
        imported: 0,
        duplicates: 0,
        failed: 0,
        monitored_folders: roots
            .iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect(),
    };
    for root in roots {
        let mut paths = Vec::new();
        if walk_directory(root, &mut paths).is_err() {
            summary.failed += 1;
            continue;
        }
        for path in paths {
            match import_monitored_file(db_path, thumbnail_dir, capture_dir, roots, &path) {
                Ok(ImportOutcome::Imported) => summary.imported += 1,
                Ok(ImportOutcome::Duplicate) => summary.duplicates += 1,
                Ok(ImportOutcome::Skipped) => {}
                Err(_) => summary.failed += 1,
            }
        }
    }
    summary
}

fn walk_directory(path: &Path, found: &mut Vec<PathBuf>) -> Result<(), String> {
    let entries =
        fs::read_dir(path).map_err(|error| format!("Cannot scan {}: {error}", path.display()))?;
    for entry in entries {
        let entry = entry.map_err(|error| error.to_string())?;
        let kind = entry.file_type().map_err(|error| error.to_string())?;
        let entry_path = entry.path();
        if kind.is_dir() {
            walk_directory(&entry_path, found)?;
        } else if kind.is_file() {
            let supported = entry_path
                .extension()
                .and_then(|value| value.to_str())
                .map(|value| IMAGE_EXTENSIONS.contains(&value.to_ascii_lowercase().as_str()))
                .unwrap_or(false);
            if supported {
                found.push(entry_path);
            }
        }
    }
    Ok(())
}

fn index_one(conn: &Connection, thumbnail_dir: &Path, path: &Path) -> Result<ScanOutcome, String> {
    let path = fs::canonicalize(path).map_err(|error| error.to_string())?;
    let (size, modified) = wait_for_stable_file(&path)?;
    let path_text = path.to_string_lossy().into_owned();
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("Screenshot")
        .to_string();
    let captured_at = modified
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;

    let existing: Option<(i64, u64, Option<String>, String)> = conn
        .query_row(
            "SELECT id, file_size, file_hash, status FROM screenshots WHERE path = ?1",
            [&path_text],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .optional()
        .map_err(|error| error.to_string())?;

    let hash = hash_file(&path)?;

    if let Some((_, old_size, Some(old_hash), status)) = &existing {
        if *old_size == size && old_hash == &hash && status == "indexed" {
            return Ok(ScanOutcome::Skipped);
        }
    }

    let duplicate: Option<i64> = conn
        .query_row(
            "SELECT id FROM screenshots WHERE file_hash = ?1 AND path != ?2 LIMIT 1",
            params![hash, path_text],
            |row| row.get(0),
        )
        .optional()
        .map_err(|error| error.to_string())?;
    if duplicate.is_some() {
        if let Some((id, _, _, _)) = existing {
            conn.execute("DELETE FROM screenshot_fts WHERE screenshot_id = ?1", [id])
                .map_err(|error| error.to_string())?;
            conn.execute("DELETE FROM screenshots WHERE id = ?1", [id])
                .map_err(|error| error.to_string())?;
        }
        return Ok(ScanOutcome::Duplicate);
    }

    let image =
        image::open(&path).map_err(|error| format!("Unsupported or damaged image: {error}"))?;
    let thumbnail_path = thumbnail_dir.join(format!("{hash}.jpg"));
    if !thumbnail_path.exists() {
        let thumbnail = image::DynamicImage::ImageRgb8(image.thumbnail(480, 320).to_rgb8());
        let mut output = Cursor::new(Vec::new());
        thumbnail
            .write_to(&mut output, ImageFormat::Jpeg)
            .map_err(|error| error.to_string())?;
        fs::write(&thumbnail_path, output.into_inner()).map_err(|error| error.to_string())?;
    }
    let thumb_text = thumbnail_path.to_string_lossy().into_owned();
    match existing {
        Some((id, _, _, _)) => {
            conn.execute(
                "UPDATE screenshots SET file_hash=?1, file_name=?2, captured_at=?3, file_size=?4,
                 status='pending', ocr_text='', last_error=NULL, thumbnail_path=?5 WHERE id=?6",
                params![hash, file_name, captured_at, size, thumb_text, id],
            )
            .map_err(|error| error.to_string())?;
            conn.execute("DELETE FROM screenshot_fts WHERE screenshot_id=?1", [id])
                .map_err(|error| error.to_string())?;
            conn.execute(
                "DELETE FROM screenshot_embeddings WHERE screenshot_id=?1",
                [id],
            )
            .map_err(|error| error.to_string())?;
        }
        None => {
            conn.execute(
                "INSERT INTO screenshots(path,file_hash,file_name,captured_at,file_size,status,thumbnail_path)
                 VALUES(?1,?2,?3,?4,?5,'pending',?6)",
                params![path_text, hash, file_name, captured_at, size, thumb_text],
            ).map_err(|error| error.to_string())?;
        }
    }
    Ok(ScanOutcome::Indexed)
}

fn wait_for_stable_file(path: &Path) -> Result<(u64, SystemTime), String> {
    let mut previous = fs::metadata(path).map_err(|error| error.to_string())?;
    if SystemTime::now()
        .duration_since(previous.modified().unwrap_or(UNIX_EPOCH))
        .unwrap_or_default()
        > Duration::from_secs(3)
    {
        if previous.len() == 0 {
            return Err("The screenshot file is empty.".to_string());
        }
        return Ok((previous.len(), previous.modified().unwrap_or(UNIX_EPOCH)));
    }
    let mut stable = false;
    for _ in 0..5 {
        std::thread::sleep(Duration::from_millis(250));
        let current = fs::metadata(path).map_err(|error| error.to_string())?;
        if current.len() == previous.len() && current.modified().ok() == previous.modified().ok() {
            stable = true;
            previous = current;
            break;
        }
        previous = current;
    }
    if !stable {
        return Err("The file is still being written. Run Index again to retry.".to_string());
    }
    if previous.len() == 0 {
        return Err("The screenshot file is empty.".to_string());
    }
    Ok((previous.len(), previous.modified().unwrap_or(UNIX_EPOCH)))
}

fn record_scan_error(conn: &Connection, path: &Path, error: &str) -> Result<(), String> {
    let path = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let name = path
        .file_name()
        .and_then(|part| part.to_str())
        .unwrap_or("Screenshot");
    let size = fs::metadata(&path)
        .map(|metadata| metadata.len())
        .unwrap_or(0);
    let modified = fs::metadata(&path)
        .and_then(|metadata| metadata.modified())
        .unwrap_or(UNIX_EPOCH);
    let captured_at = modified
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    conn.execute(
        "INSERT INTO screenshots(path,file_name,captured_at,file_size,status,last_error)
         VALUES(?1,?2,?3,?4,'error',?5)
         ON CONFLICT(path) DO UPDATE SET file_size=excluded.file_size,captured_at=excluded.captured_at,
         file_hash=NULL,status='error',ocr_text='',last_error=excluded.last_error,thumbnail_path=NULL",
        params![path.to_string_lossy(), name, captured_at, size, error.chars().take(500).collect::<String>()],
    ).map_err(|issue| issue.to_string())?;
    conn.execute(
        "DELETE FROM screenshot_fts WHERE screenshot_id IN (SELECT id FROM screenshots WHERE path = ?1)",
        [path.to_string_lossy().as_ref()],
    ).map_err(|issue| issue.to_string())?;
    conn.execute(
        "DELETE FROM screenshot_embeddings WHERE screenshot_id IN (SELECT id FROM screenshots WHERE path = ?1)",
        [path.to_string_lossy().as_ref()],
    ).map_err(|issue| issue.to_string())?;
    Ok(())
}

fn load_screenshots(
    db_path: &Path,
    _thumbnail_dir: &Path,
    fts_query: Option<String>,
) -> Result<Vec<Screenshot>, String> {
    let conn = connection(db_path)?;
    let sql = if fts_query.is_some() {
        "SELECT s.id,s.path,s.file_name,s.captured_at,s.file_size,s.status,s.ocr_text,s.last_error,s.thumbnail_path,
                s.category,s.subcategory,s.suggested_category,s.suggested_subcategory,s.category_is_manual
         FROM screenshot_fts f JOIN screenshots s ON s.id=CAST(f.screenshot_id AS INTEGER)
         WHERE screenshot_fts MATCH ?1 ORDER BY s.captured_at DESC"
    } else {
        "SELECT id,path,file_name,captured_at,file_size,status,ocr_text,last_error,thumbnail_path,
                category,subcategory,suggested_category,suggested_subcategory,category_is_manual
         FROM screenshots ORDER BY captured_at DESC"
    };
    let mut statement = conn.prepare(sql).map_err(|error| error.to_string())?;
    let mut rows = match fts_query {
        Some(query) => statement
            .query([query])
            .map_err(|error| error.to_string())?,
        None => statement.query([]).map_err(|error| error.to_string())?,
    };
    let mut screenshots = Vec::new();
    while let Some(row) = rows.next().map_err(|error| error.to_string())? {
        screenshots.push(screenshot_from_row(row).map_err(|error| error.to_string())?);
    }
    Ok(screenshots)
}

fn load_screenshots_by_ids(
    db_path: &Path,
    _thumbnail_dir: &Path,
    ids: &[i64],
) -> Result<Vec<Screenshot>, String> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let conn = connection(db_path)?;
    let placeholders = (1..=ids.len()).map(|_| "?").collect::<Vec<_>>().join(",");
    let sql = format!(
        "SELECT id,path,file_name,captured_at,file_size,status,ocr_text,last_error,thumbnail_path,
         category,subcategory,suggested_category,suggested_subcategory,category_is_manual
         FROM screenshots WHERE id IN ({placeholders})"
    );
    let mut statement = conn.prepare(&sql).map_err(|error| error.to_string())?;
    let mut rows = statement
        .query(rusqlite::params_from_iter(ids.iter()))
        .map_err(|error| error.to_string())?;
    let mut screenshots = Vec::new();
    while let Some(row) = rows.next().map_err(|error| error.to_string())? {
        screenshots.push(screenshot_from_row(row).map_err(|error| error.to_string())?);
    }
    Ok(screenshots)
}

fn screenshot_from_row(row: &rusqlite::Row<'_>) -> Result<Screenshot, rusqlite::Error> {
    let thumbnail_path: Option<String> = row.get(8)?;
    let thumbnail_data_url = thumbnail_path
        .and_then(|path| fs::read(path).ok())
        .map(|bytes| format!("data:image/jpeg;base64,{}", BASE64.encode(bytes)));
    let path: String = row.get(1)?;
    let stored_status: String = row.get(5)?;
    Ok(Screenshot {
        id: row.get(0)?,
        path: path.clone(),
        file_name: row.get(2)?,
        captured_at: row.get(3)?,
        file_size: row.get(4)?,
        status: if Path::new(&path).is_file() {
            stored_status
        } else {
            "missing".to_string()
        },
        ocr_text: row.get(6)?,
        last_error: row.get(7)?,
        thumbnail_data_url,
        category: row.get(9)?,
        subcategory: row.get(10)?,
        suggested_category: row.get(11)?,
        suggested_subcategory: row.get(12)?,
        category_is_manual: row.get::<_, i64>(13)? != 0,
        match_reason: None,
    })
}

#[cfg(test)]
mod category_tests {
    use super::{suggest_category, validate_category_selection};

    #[test]
    fn website_navigation_suggests_projects_website_subcategory() {
        let suggestion = suggest_category(
            "Not secure. Services About Contact. Firefox noticed something suspicious.",
        );
        assert_eq!(
            suggestion,
            (
                Some("Projects".to_string()),
                Some("Websites & landing pages".to_string())
            )
        );
    }

    #[test]
    fn valid_parent_and_subcategory_pair_is_accepted() {
        assert!(
            validate_category_selection(Some("Projects"), Some("Websites & landing pages")).is_ok()
        );
    }

    #[test]
    fn mismatched_or_missing_parent_is_rejected() {
        assert!(
            validate_category_selection(Some("Finance"), Some("Websites & landing pages")).is_err()
        );
        assert!(validate_category_selection(None, Some("Websites & landing pages")).is_err());
    }
}
