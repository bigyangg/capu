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
/// MiniLM sentence vectors built from OCR text.
const TEXT_EMBEDDING_DIMENSIONS: usize = 384;
/// CLIP ViT-B/32 vectors; images and search text share this space.
const IMAGE_EMBEDDING_DIMENSIONS: usize = 512;
/// CLIP text-to-image similarity is low in absolute terms. On sample images, matches
/// scored about 0.24-0.31 and unrelated images 0.16-0.22.
const VISUAL_MATCH_FLOOR: f32 = 0.23;
/// Keeps only images close to the best visual match, so a weak query does not return
/// everything that clears the floor.
const VISUAL_MATCH_MARGIN: f32 = 0.05;

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
pub struct ImageEmbeddingJob {
    pub id: i64,
    pub image_data_url: String,
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
    pub removed: usize,
    pub moved: usize,
    pub monitored_folders: Vec<String>,
}

#[derive(Serialize)]
pub struct StorageUsage {
    /// Files kept in the app's own folder: captures made in the app, plus any earlier
    /// import copy whose original was not available to convert.
    library_files: u64,
    library_bytes: u64,
    /// Screenshots from watched folders, read from where they already are.
    referenced_files: u64,
    thumbnail_bytes: u64,
    database_bytes: u64,
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
         CREATE TABLE IF NOT EXISTS screenshot_image_embeddings (
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
        ("source_path", "TEXT"),
        ("file_modified_ms", "INTEGER"),
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
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS screenshots_source ON screenshots(source_path);",
    )
    .map_err(|error| error.to_string())?;

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

/// Indexes new files in the managed capture folder. Folders chosen by the user are never
/// indexed in place; they are imported through monitored folders instead.
#[tauri::command]
pub async fn index_capture_folder(state: State<'_, AppState>) -> Result<IndexReport, String> {
    let db_path = state.db_path.clone();
    let thumbnail_dir = state.thumbnail_dir.clone();
    let capture_dir = state.capture_dir.clone();
    tauri::async_runtime::spawn_blocking(move || {
        scan_folder(&db_path, &thumbnail_dir, &capture_dir)
    })
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
    image_query_embedding: Vec<f32>,
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
        let text_vector = is_valid_vector(&embedding, TEXT_EMBEDDING_DIMENSIONS);
        let image_vector = is_valid_vector(&image_query_embedding, IMAGE_EMBEDDING_DIMENSIONS);
        if search_terms.is_empty() && !text_vector && !image_vector {
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
        let conn = connection(&db_path)?;
        if text_vector {
            for (id, similarity) in similarities(&conn, VectorTable::Text, &embedding)? {
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
        if image_vector {
            for (id, score) in visual_matches(&conn, &image_query_embedding)? {
                scores
                    .entry(id)
                    .and_modify(|current| *current += score)
                    .or_insert(score);
                reasons
                    .entry(id)
                    .or_insert_with(|| "Visual match".to_string());
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
    let updated = transaction
        .execute(
            "UPDATE screenshots SET ocr_text=?1, status='indexed', last_error=NULL,
             suggested_category=?2,suggested_subcategory=?3,
             category=CASE WHEN category_is_manual=1 THEN category ELSE ?2 END,
             subcategory=CASE WHEN category_is_manual=1 THEN subcategory ELSE ?3 END WHERE id=?4",
            params![text, suggestion, suggested_subcategory, id],
        )
        .map_err(|error| error.to_string())?;
    // The screenshot may have been removed while OCR was running.
    if updated == 0 {
        return Ok(());
    }
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

#[derive(Clone, Copy)]
enum VectorTable {
    Text,
    Image,
}

impl VectorTable {
    fn dimensions(self) -> usize {
        match self {
            Self::Text => TEXT_EMBEDDING_DIMENSIONS,
            Self::Image => IMAGE_EMBEDDING_DIMENSIONS,
        }
    }

    fn insert_sql(self) -> &'static str {
        match self {
            Self::Text => {
                "INSERT INTO screenshot_embeddings(screenshot_id,vector)
                 SELECT ?1,?2 WHERE EXISTS(SELECT 1 FROM screenshots WHERE id=?1)
                 ON CONFLICT(screenshot_id) DO UPDATE SET vector=excluded.vector"
            }
            Self::Image => {
                "INSERT INTO screenshot_image_embeddings(screenshot_id,vector)
                 SELECT ?1,?2 WHERE EXISTS(SELECT 1 FROM screenshots WHERE id=?1)
                 ON CONFLICT(screenshot_id) DO UPDATE SET vector=excluded.vector"
            }
        }
    }

    fn select_sql(self) -> &'static str {
        match self {
            Self::Text => "SELECT screenshot_id,vector FROM screenshot_embeddings",
            Self::Image => "SELECT screenshot_id,vector FROM screenshot_image_embeddings",
        }
    }
}

fn is_valid_vector(vector: &[f32], dimensions: usize) -> bool {
    vector.len() == dimensions && vector.iter().all(|value| value.is_finite())
}

fn store_vector(
    conn: &Connection,
    table: VectorTable,
    id: i64,
    vector: &[f32],
) -> Result<(), String> {
    if !is_valid_vector(vector, table.dimensions()) {
        return Err("The local model returned an invalid vector.".to_string());
    }
    let bytes = vector
        .iter()
        .flat_map(|value| value.to_le_bytes())
        .collect::<Vec<_>>();
    conn.execute(table.insert_sql(), params![id, bytes])
        .map_err(|error| error.to_string())?;
    Ok(())
}

fn store_embedding(conn: &Connection, id: i64, vector: &[f32]) -> Result<(), String> {
    store_vector(conn, VectorTable::Text, id, vector)
}

/// Dot product against every stored vector. Stored and query vectors are normalized, so
/// this is cosine similarity.
fn similarities(
    conn: &Connection,
    table: VectorTable,
    query: &[f32],
) -> Result<Vec<(i64, f32)>, String> {
    let mut statement = conn
        .prepare(table.select_sql())
        .map_err(|error| error.to_string())?;
    let mut rows = statement.query([]).map_err(|error| error.to_string())?;
    let mut found = Vec::new();
    while let Some(row) = rows.next().map_err(|error| error.to_string())? {
        let id: i64 = row.get(0).map_err(|error| error.to_string())?;
        let bytes: Vec<u8> = row.get(1).map_err(|error| error.to_string())?;
        if bytes.len() != table.dimensions() * 4 {
            continue;
        }
        let similarity = bytes
            .chunks_exact(4)
            .zip(query)
            .map(|(bytes, query)| {
                f32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) * query
            })
            .sum::<f32>();
        found.push((id, similarity));
    }
    Ok(found)
}

/// Screenshots that look like the query, with a score that ranks alongside text matches.
fn visual_matches(conn: &Connection, query: &[f32]) -> Result<Vec<(i64, f32)>, String> {
    let visual = similarities(conn, VectorTable::Image, query)?;
    let best = visual
        .iter()
        .map(|(_, similarity)| *similarity)
        .fold(f32::MIN, f32::max);
    let cutoff = VISUAL_MATCH_FLOOR.max(best - VISUAL_MATCH_MARGIN);
    Ok(visual
        .into_iter()
        .filter(|(_, similarity)| *similarity >= cutoff)
        // Rescale CLIP's narrow 0.23-0.32 band to roughly 0.4-0.85 so strong visual
        // matches rank alongside related-meaning matches.
        .map(|(id, similarity)| (id, (similarity - 0.15) * 5.0))
        .collect())
}

#[tauri::command]
pub fn save_image_embedding(
    state: State<'_, AppState>,
    id: i64,
    embedding: Vec<f32>,
) -> Result<(), String> {
    let conn = connection(&state.db_path)?;
    store_vector(&conn, VectorTable::Image, id, &embedding)
}

/// Returns a small batch of screenshots that still need a visual search vector, with
/// their thumbnails. The vision model works on 224px input, so the thumbnail carries
/// enough detail and avoids sending full-size images to the web view.
#[tauri::command]
pub async fn get_unembedded_images(
    state: State<'_, AppState>,
    exclude: Vec<i64>,
) -> Result<Vec<ImageEmbeddingJob>, String> {
    let db_path = state.db_path.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let conn = connection(&db_path)?;
        let mut statement = conn
            .prepare(
                "SELECT id,thumbnail_path FROM screenshots s
                 WHERE thumbnail_path IS NOT NULL AND NOT EXISTS (
                     SELECT 1 FROM screenshot_image_embeddings e WHERE e.screenshot_id=s.id
                 ) ORDER BY captured_at DESC",
            )
            .map_err(|error| error.to_string())?;
        let rows = statement
            .query_map([], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(|error| error.to_string())?;
        let mut jobs = Vec::new();
        for row in rows {
            let (id, thumbnail) = row.map_err(|error| error.to_string())?;
            if exclude.contains(&id) {
                continue;
            }
            let Ok(bytes) = fs::read(&thumbnail) else {
                continue;
            };
            jobs.push(ImageEmbeddingJob {
                id,
                image_data_url: format!("data:image/jpeg;base64,{}", BASE64.encode(bytes)),
            });
            if jobs.len() == 16 {
                break;
            }
        }
        Ok(jobs)
    })
    .await
    .map_err(|error| format!("Visual search task failed: {error}"))?
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

fn scan_folder(db_path: &Path, thumbnail_dir: &Path, folder: &Path) -> Result<IndexReport, String> {
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

#[derive(PartialEq, Eq)]
enum ScanOutcome {
    Indexed,
    Skipped,
    Duplicate,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ImportOutcome {
    Imported,
    /// An existing entry now follows its renamed or moved original.
    Moved,
    Duplicate,
    Skipped,
}

fn is_supported_image(path: &Path) -> bool {
    path.extension()
        .and_then(|value| value.to_str())
        .map(|value| IMAGE_EXTENSIONS.contains(&value.to_ascii_lowercase().as_str()))
        .unwrap_or(false)
}

/// Adds a screenshot from a watched folder by reference: the library records where the
/// original is and keeps only its thumbnail, text, and vectors. The original is never moved,
/// renamed, copied, or edited.
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
    // Reading an online-only cloud file downloads it. Those are indexed once they are on
    // this device, so syncing a large cloud folder does not download all of it.
    if is_online_only(&source) {
        return Ok(ImportOutcome::Skipped);
    }

    let conn = connection(db_path)?;
    let source_text = source.to_string_lossy().into_owned();
    // A file already in the library with the same size and modified time has not changed,
    // so it is skipped without reading it again.
    let metadata = fs::metadata(&source).map_err(|error| error.to_string())?;
    let unchanged: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM screenshots WHERE path=?1 AND file_size=?2
             AND file_modified_ms=?3 AND file_hash IS NOT NULL)",
            params![source_text, metadata.len(), modified_ms(&metadata)],
            |row| row.get(0),
        )
        .map_err(|error| error.to_string())?;
    if unchanged {
        return Ok(ImportOutcome::Skipped);
    }

    let stable = wait_for_import_stable(&source)?;
    let hash = hash_file(&source)?;
    let same_picture: Option<(i64, String)> = conn
        .query_row(
            "SELECT id,path FROM screenshots WHERE file_hash=?1 LIMIT 1",
            [&hash],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(|error| error.to_string())?;
    if let Some((id, path)) = same_picture
        && path != source_text
    {
        // The library's file is gone and the same picture appeared here: the original was
        // renamed or moved, so the entry follows it and keeps its text and category.
        if source_is_gone(Path::new(&path)) {
            let file_name = source
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("Screenshot");
            conn.execute(
                "UPDATE screenshots SET path=?1,source_path=?1,file_name=?2 WHERE id=?3",
                params![source_text, file_name, id],
            )
            .map_err(|error| error.to_string())?;
            return Ok(ImportOutcome::Moved);
        }
        return Ok(ImportOutcome::Duplicate);
    }

    // An original edited in place keeps its entry: the same row is updated and re-read.
    let outcome = index_file(&conn, thumbnail_dir, &source, stable, hash)?;
    conn.execute(
        "UPDATE screenshots SET source_path=?1 WHERE path=?1",
        [&source_text],
    )
    .map_err(|error| error.to_string())?;
    Ok(match outcome {
        ScanOutcome::Indexed => ImportOutcome::Imported,
        ScanOutcome::Duplicate => ImportOutcome::Duplicate,
        ScanOutcome::Skipped => ImportOutcome::Skipped,
    })
}

/// Counts the screenshots added and followed after a rename for one watcher event.
#[derive(Default)]
pub struct EventChanges {
    pub imported: usize,
    pub moved: usize,
}

impl EventChanges {
    fn add(&mut self, outcome: ImportOutcome) {
        match outcome {
            ImportOutcome::Imported => self.imported += 1,
            ImportOutcome::Moved => self.moved += 1,
            ImportOutcome::Duplicate | ImportOutcome::Skipped => {}
        }
    }
}

pub fn import_monitored_event(
    db_path: &Path,
    thumbnail_dir: &Path,
    capture_dir: &Path,
    monitored_roots: &[PathBuf],
    event_path: &Path,
) -> Result<EventChanges, String> {
    let mut changes = EventChanges::default();
    if event_path.is_dir() {
        let mut paths = Vec::new();
        walk_directory(event_path, &mut paths)?;
        for path in paths {
            changes.add(import_monitored_file(
                db_path,
                thumbnail_dir,
                capture_dir,
                monitored_roots,
                &path,
            )?);
        }
    } else {
        changes.add(import_monitored_file(
            db_path,
            thumbnail_dir,
            capture_dir,
            monitored_roots,
            event_path,
        )?);
    }
    Ok(changes)
}

/// Earlier versions kept a hard link or copy of every imported screenshot in the app's
/// folder. This points those entries at their originals and removes the app's duplicate,
/// so each picture is stored once. Only exact matches are converted: if the original is
/// missing, changed, or online-only, the app's copy is kept. It also marks entries from the
/// retired in-place "library folder" flow as references, so deleting their originals is
/// handled like any other watched file. Returns the number of entries converted.
pub fn convert_imports_to_references(
    db_path: &Path,
    thumbnail_dir: &Path,
    capture_dir: &Path,
) -> Result<usize, String> {
    let conn = connection(db_path)?;
    let managed_root = fs::canonicalize(capture_dir).unwrap_or_else(|_| capture_dir.to_path_buf());
    let rows = conn
        .prepare(
            "SELECT id,path,source_path,file_hash,thumbnail_path FROM screenshots
             WHERE source_path IS NULL OR path != source_path",
        )
        .map_err(|error| error.to_string())?
        .query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, Option<String>>(4)?,
            ))
        })
        .map_err(|error| error.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())?;

    let mut converted = 0;
    for (id, path, source, hash, thumbnail) in rows {
        let in_library = Path::new(&path).starts_with(&managed_root);
        let Some(source) = source else {
            if !in_library {
                conn.execute("UPDATE screenshots SET source_path=path WHERE id=?1", [id])
                    .map_err(|error| error.to_string())?;
            }
            continue;
        };
        let original = Path::new(&source);
        if !in_library || !original.is_file() || is_online_only(original) {
            continue;
        }
        if hash.is_none() || hash_file(original).ok() != hash {
            continue;
        }
        let already_referenced: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM screenshots WHERE path=?1)",
                [&source],
                |row| row.get(0),
            )
            .map_err(|error| error.to_string())?;
        if already_referenced {
            delete_library_item(
                &conn,
                thumbnail_dir,
                capture_dir,
                id,
                &path,
                thumbnail.as_deref(),
            )?;
        } else {
            let metadata = fs::metadata(original).map_err(|error| error.to_string())?;
            let file_name = original
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("Screenshot");
            conn.execute(
                "UPDATE screenshots SET path=?1,file_name=?2,file_modified_ms=?3 WHERE id=?4",
                params![source, file_name, modified_ms(&metadata), id],
            )
            .map_err(|error| error.to_string())?;
            let _ = fs::remove_file(&path);
        }
        converted += 1;
    }
    Ok(converted)
}

pub fn storage_usage(
    db_path: &Path,
    thumbnail_dir: &Path,
    capture_dir: &Path,
) -> Result<StorageUsage, String> {
    let folder_size = |folder: &Path| -> (u64, u64) {
        fs::read_dir(folder)
            .map(|entries| {
                entries
                    .filter_map(Result::ok)
                    .filter_map(|entry| entry.metadata().ok())
                    .filter(|metadata| metadata.is_file())
                    .fold((0, 0), |(count, bytes), metadata| {
                        (count + 1, bytes + metadata.len())
                    })
            })
            .unwrap_or((0, 0))
    };
    let (library_files, library_bytes) = folder_size(capture_dir);
    let (_, thumbnail_bytes) = folder_size(thumbnail_dir);
    let database_bytes = ["", "-wal", "-shm"]
        .iter()
        .filter_map(|suffix| {
            let mut name = db_path.as_os_str().to_owned();
            name.push(suffix);
            fs::metadata(PathBuf::from(name)).ok()
        })
        .map(|metadata| metadata.len())
        .sum();
    let managed_root = fs::canonicalize(capture_dir).unwrap_or_else(|_| capture_dir.to_path_buf());
    let conn = connection(db_path)?;
    let mut statement = conn
        .prepare("SELECT path FROM screenshots")
        .map_err(|error| error.to_string())?;
    let referenced_files = statement
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(|error| error.to_string())?
        .filter_map(Result::ok)
        .filter(|path| !Path::new(path).starts_with(&managed_root))
        .count() as u64;
    Ok(StorageUsage {
        library_files,
        library_bytes,
        referenced_files,
        thumbnail_bytes,
        database_bytes,
    })
}

#[tauri::command]
pub async fn get_storage_usage(state: State<'_, AppState>) -> Result<StorageUsage, String> {
    let db_path = state.db_path.clone();
    let thumbnail_dir = state.thumbnail_dir.clone();
    let capture_dir = state.capture_dir.clone();
    tauri::async_runtime::spawn_blocking(move || {
        storage_usage(&db_path, &thumbnail_dir, &capture_dir)
    })
    .await
    .map_err(|error| format!("Storage check failed: {error}"))?
}

/// Opens a library screenshot in the system's default image viewer. The path comes from
/// the library, not from the web view.
#[tauri::command]
pub async fn open_screenshot(state: State<'_, AppState>, id: i64) -> Result<(), String> {
    let db_path = state.db_path.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let path = screenshot_path_for_id(&db_path, id)?;
        tauri_plugin_opener::open_path(shell_path(&path), None::<&str>)
            .map_err(|error| format!("Could not open the screenshot: {error}"))
    })
    .await
    .map_err(|error| format!("Open task failed: {error}"))?
}

/// Windows canonical paths carry a `\\?\` prefix that some shell APIs reject.
fn shell_path(path: &Path) -> PathBuf {
    let text = path.to_string_lossy();
    match text.strip_prefix(r"\\?\") {
        Some(rest) if !rest.starts_with("UNC\\") => PathBuf::from(rest),
        _ => path.to_path_buf(),
    }
}

fn modified_ms(metadata: &fs::Metadata) -> i64 {
    metadata
        .modified()
        .unwrap_or(UNIX_EPOCH)
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
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

fn source_is_gone(path: &Path) -> bool {
    matches!(fs::symlink_metadata(path), Err(error) if error.kind() == std::io::ErrorKind::NotFound)
}

/// True for a cloud placeholder whose contents are not on this device (OneDrive and other
/// Windows cloud-sync clients). Reading such a file downloads it.
#[cfg(windows)]
fn is_online_only(path: &Path) -> bool {
    use std::os::windows::fs::MetadataExt;
    const NOT_LOCAL: u32 = 0x1000 // FILE_ATTRIBUTE_OFFLINE
        | 0x40000 // FILE_ATTRIBUTE_RECALL_ON_OPEN
        | 0x400000; // FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS
    fs::metadata(path)
        .map(|metadata| metadata.file_attributes() & NOT_LOCAL != 0)
        .unwrap_or(false)
}

#[cfg(not(windows))]
fn is_online_only(_path: &Path) -> bool {
    false
}

/// Removes a screenshot and everything derived from it: search text, embedding,
/// thumbnail, and the managed library file. Files outside the managed library are never
/// deleted.
fn delete_library_item(
    conn: &Connection,
    thumbnail_dir: &Path,
    capture_dir: &Path,
    id: i64,
    path: &str,
    thumbnail_path: Option<&str>,
) -> Result<(), String> {
    let transaction = conn
        .unchecked_transaction()
        .map_err(|error| error.to_string())?;
    transaction
        .execute("DELETE FROM screenshot_fts WHERE screenshot_id=?1", [id])
        .map_err(|error| error.to_string())?;
    transaction
        .execute(
            "DELETE FROM screenshot_embeddings WHERE screenshot_id=?1",
            [id],
        )
        .map_err(|error| error.to_string())?;
    transaction
        .execute(
            "DELETE FROM screenshot_image_embeddings WHERE screenshot_id=?1",
            [id],
        )
        .map_err(|error| error.to_string())?;
    transaction
        .execute("DELETE FROM screenshots WHERE id=?1", [id])
        .map_err(|error| error.to_string())?;
    transaction.commit().map_err(|error| error.to_string())?;

    let managed_root = fs::canonicalize(capture_dir).unwrap_or_else(|_| capture_dir.to_path_buf());
    let library_file = Path::new(path);
    if library_file.starts_with(&managed_root) && library_file.is_file() {
        let _ = fs::remove_file(library_file);
    }
    if let Some(thumbnail) = thumbnail_path {
        let still_used: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM screenshots WHERE thumbnail_path=?1)",
                [thumbnail],
                |row| row.get(0),
            )
            .map_err(|error| error.to_string())?;
        if !still_used && Path::new(thumbnail).starts_with(thumbnail_dir) {
            let _ = fs::remove_file(thumbnail);
        }
    }
    Ok(())
}

fn remove_items_with_missing_sources(
    db_path: &Path,
    thumbnail_dir: &Path,
    capture_dir: &Path,
    belongs: impl Fn(&Path) -> bool,
) -> Result<usize, String> {
    let conn = connection(db_path)?;
    let mut statement = conn
        .prepare(
            "SELECT id,path,source_path,thumbnail_path FROM screenshots WHERE source_path IS NOT NULL",
        )
        .map_err(|error| error.to_string())?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
            ))
        })
        .map_err(|error| error.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())?;
    drop(statement);
    let mut removed = 0;
    for (id, path, source, thumbnail) in rows {
        let source = Path::new(&source);
        if belongs(source) && source_is_gone(source) {
            delete_library_item(
                &conn,
                thumbnail_dir,
                capture_dir,
                id,
                &path,
                thumbnail.as_deref(),
            )?;
            removed += 1;
        }
    }
    Ok(removed)
}

/// Handles a monitored path that no longer exists. A deleted file or folder removes the
/// screenshots imported from it. Nothing is removed while the monitored root itself is
/// unavailable, because that usually means an unplugged drive rather than a deletion.
pub fn remove_deleted_source(
    db_path: &Path,
    thumbnail_dir: &Path,
    capture_dir: &Path,
    monitored_roots: &[PathBuf],
    removed: &Path,
) -> Result<usize, String> {
    // The removed path cannot be canonicalized, so resolve its parent to match the
    // canonical form stored for sources.
    let removed = match (
        removed
            .parent()
            .and_then(|parent| fs::canonicalize(parent).ok()),
        removed.file_name(),
    ) {
        (Some(parent), Some(name)) => parent.join(name),
        _ => removed.to_path_buf(),
    };
    let available_root = monitored_roots
        .iter()
        .any(|root| removed.starts_with(root) && removed != *root && root.is_dir());
    if !available_root || !source_is_gone(&removed) {
        return Ok(0);
    }
    remove_items_with_missing_sources(db_path, thumbnail_dir, capture_dir, |source| {
        source.starts_with(&removed)
    })
}

/// Removes screenshots whose originals were deleted while the app was not running.
fn reconcile_deleted_sources(
    db_path: &Path,
    thumbnail_dir: &Path,
    capture_dir: &Path,
    roots: &[PathBuf],
) -> Result<usize, String> {
    let available = roots
        .iter()
        .filter(|root| root.is_dir())
        .collect::<Vec<_>>();
    if available.is_empty() {
        return Ok(0);
    }
    remove_items_with_missing_sources(db_path, thumbnail_dir, capture_dir, |source| {
        available.iter().any(|root| source.starts_with(root))
    })
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
        removed: 0,
        moved: 0,
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
                Ok(ImportOutcome::Moved) => summary.moved += 1,
                Ok(ImportOutcome::Duplicate) => summary.duplicates += 1,
                Ok(ImportOutcome::Skipped) => {}
                Err(_) => summary.failed += 1,
            }
        }
    }
    // Reconcile after importing so renamed originals are re-pointed before missing
    // sources are treated as deletions.
    match reconcile_deleted_sources(db_path, thumbnail_dir, capture_dir, roots) {
        Ok(removed) => summary.removed = removed,
        Err(_) => summary.failed += 1,
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
    let stable = wait_for_stable_file(&path)?;
    let hash = hash_file(&path)?;
    index_file(conn, thumbnail_dir, &path, stable, hash)
}

/// Adds or refreshes the library row for a canonical, stable file whose hash is known.
fn index_file(
    conn: &Connection,
    thumbnail_dir: &Path,
    path: &Path,
    (size, modified): (u64, SystemTime),
    hash: String,
) -> Result<ScanOutcome, String> {
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
    let modified_ms = modified
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64;

    let existing: Option<(i64, u64, Option<String>, String, Option<String>)> = conn
        .query_row(
            "SELECT id, file_size, file_hash, status, thumbnail_path FROM screenshots WHERE path = ?1",
            [&path_text],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
        )
        .optional()
        .map_err(|error| error.to_string())?;

    if let Some((id, old_size, Some(old_hash), status, _)) = &existing
        && *old_size == size
        && old_hash == &hash
        && status != "error"
    {
        conn.execute(
            "UPDATE screenshots SET file_modified_ms=?1 WHERE id=?2",
            params![modified_ms, id],
        )
        .map_err(|error| error.to_string())?;
        return Ok(ScanOutcome::Skipped);
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
        if let Some((id, _, _, _, _)) = existing {
            conn.execute("DELETE FROM screenshot_fts WHERE screenshot_id = ?1", [id])
                .map_err(|error| error.to_string())?;
            conn.execute(
                "DELETE FROM screenshot_embeddings WHERE screenshot_id = ?1",
                [id],
            )
            .map_err(|error| error.to_string())?;
            conn.execute(
                "DELETE FROM screenshot_image_embeddings WHERE screenshot_id = ?1",
                [id],
            )
            .map_err(|error| error.to_string())?;
            conn.execute("DELETE FROM screenshots WHERE id = ?1", [id])
                .map_err(|error| error.to_string())?;
        }
        return Ok(ScanOutcome::Duplicate);
    }

    let image =
        image::open(path).map_err(|error| format!("Unsupported or damaged image: {error}"))?;
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
        Some((id, _, _, _, old_thumbnail)) => {
            conn.execute(
                "UPDATE screenshots SET file_hash=?1, file_name=?2, captured_at=?3, file_size=?4,
                 status='pending', ocr_text='', last_error=NULL, thumbnail_path=?5,
                 file_modified_ms=?6 WHERE id=?7",
                params![
                    hash,
                    file_name,
                    captured_at,
                    size,
                    thumb_text,
                    modified_ms,
                    id
                ],
            )
            .map_err(|error| error.to_string())?;
            // The file changed, so the previous version's thumbnail is no longer needed.
            if let Some(old) = old_thumbnail.filter(|old| *old != thumb_text) {
                let still_used: bool = conn
                    .query_row(
                        "SELECT EXISTS(SELECT 1 FROM screenshots WHERE thumbnail_path=?1)",
                        [&old],
                        |row| row.get(0),
                    )
                    .map_err(|error| error.to_string())?;
                if !still_used && Path::new(&old).starts_with(thumbnail_dir) {
                    let _ = fs::remove_file(&old);
                }
            }
            conn.execute("DELETE FROM screenshot_fts WHERE screenshot_id=?1", [id])
                .map_err(|error| error.to_string())?;
            conn.execute(
                "DELETE FROM screenshot_embeddings WHERE screenshot_id=?1",
                [id],
            )
            .map_err(|error| error.to_string())?;
            conn.execute(
                "DELETE FROM screenshot_image_embeddings WHERE screenshot_id=?1",
                [id],
            )
            .map_err(|error| error.to_string())?;
        }
        None => {
            conn.execute(
                "INSERT INTO screenshots(path,file_hash,file_name,captured_at,file_size,status,thumbnail_path,file_modified_ms)
                 VALUES(?1,?2,?3,?4,?5,'pending',?6,?7)",
                params![path_text, hash, file_name, captured_at, size, thumb_text, modified_ms],
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
    conn.execute(
        "DELETE FROM screenshot_image_embeddings WHERE screenshot_id IN (SELECT id FROM screenshots WHERE path = ?1)",
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
mod import_tests {
    use super::*;

    struct Fixture {
        root: PathBuf,
        watched: PathBuf,
        captures: PathBuf,
        thumbnails: PathBuf,
        db: PathBuf,
    }

    impl Fixture {
        fn new(name: &str) -> Self {
            let root = std::env::temp_dir().join(format!(
                "screenshot-memory-{name}-{}",
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            let (watched, captures, thumbnails) = (
                root.join("watched"),
                root.join("captures"),
                root.join("thumbnails"),
            );
            for dir in [&watched, &captures, &thumbnails] {
                fs::create_dir_all(dir).unwrap();
            }
            let db = root.join("library.sqlite3");
            initialize_database(&db).unwrap();
            Self {
                watched: fs::canonicalize(&watched).unwrap(),
                captures: fs::canonicalize(&captures).unwrap(),
                thumbnails,
                db,
                root,
            }
        }

        fn image(&self, name: &str, shade: u8) -> PathBuf {
            let path = self.watched.join(name);
            image::RgbImage::from_pixel(8, 8, image::Rgb([shade, 0, 0]))
                .save(&path)
                .unwrap();
            path
        }

        fn import(&self, source: &Path) -> ImportOutcome {
            import_monitored_file(
                &self.db,
                &self.thumbnails,
                &self.captures,
                std::slice::from_ref(&self.watched),
                source,
            )
            .unwrap()
        }

        fn remove(&self, source: &Path) -> usize {
            remove_deleted_source(
                &self.db,
                &self.thumbnails,
                &self.captures,
                std::slice::from_ref(&self.watched),
                source,
            )
            .unwrap()
        }

        fn rows(&self) -> Vec<(String, Option<String>)> {
            let conn = connection(&self.db).unwrap();
            let mut statement = conn
                .prepare("SELECT path,source_path FROM screenshots")
                .unwrap();
            statement
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap()
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn source_text(path: &Path) -> String {
        fs::canonicalize(path)
            .unwrap()
            .to_string_lossy()
            .into_owned()
    }

    #[test]
    fn imports_reference_the_original_without_copying_it() {
        let fixture = Fixture::new("reference");
        let source = fixture.image("shot.png", 10);
        assert!(fixture.import(&source) == ImportOutcome::Imported);
        let rows = fixture.rows();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0, source_text(&source));
        assert_eq!(rows[0].1.as_deref(), Some(source_text(&source).as_str()));
        assert_eq!(fs::read_dir(&fixture.captures).unwrap().count(), 0);
        assert!(source.is_file());
    }

    #[test]
    fn unchanged_files_are_skipped_on_the_next_sync() {
        let fixture = Fixture::new("unchanged");
        let source = fixture.image("shot.png", 15);
        assert!(fixture.import(&source) == ImportOutcome::Imported);
        assert!(fixture.import(&source) == ImportOutcome::Skipped);
    }

    #[test]
    fn deleting_an_original_removes_its_library_entry() {
        let fixture = Fixture::new("delete");
        let source = fixture.image("shot.png", 10);
        assert!(fixture.import(&source) == ImportOutcome::Imported);
        fs::remove_file(&source).unwrap();
        assert_eq!(fixture.remove(&source), 1);
        assert!(fixture.rows().is_empty());
        assert_eq!(fs::read_dir(&fixture.thumbnails).unwrap().count(), 0);
    }

    #[test]
    fn renaming_an_original_keeps_its_library_entry() {
        let fixture = Fixture::new("rename");
        let source = fixture.image("before.png", 20);
        assert!(fixture.import(&source) == ImportOutcome::Imported);
        let renamed = fixture.watched.join("after.png");
        fs::rename(&source, &renamed).unwrap();

        assert!(fixture.import(&renamed) == ImportOutcome::Moved);
        assert_eq!(fixture.remove(&source), 0);
        let rows = fixture.rows();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0, source_text(&renamed));
        assert_eq!(rows[0].1.as_deref(), Some(source_text(&renamed).as_str()));
    }

    #[test]
    fn edited_original_updates_the_same_entry() {
        let fixture = Fixture::new("edit");
        let source = fixture.image("shot.png", 30);
        assert!(fixture.import(&source) == ImportOutcome::Imported);
        std::thread::sleep(Duration::from_millis(20));
        let source = fixture.image("shot.png", 40);
        assert!(fixture.import(&source) == ImportOutcome::Imported);
        let rows = fixture.rows();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0, source_text(&source));
        assert_eq!(
            fs::read_dir(&fixture.thumbnails).unwrap().count(),
            1,
            "the earlier version's thumbnail is removed"
        );
    }

    #[test]
    fn earlier_import_copies_are_converted_to_references() {
        let fixture = Fixture::new("convert");
        let source = fixture.image("shot.png", 50);
        let copy = fixture.captures.join("Imported-0123456789ab-shot.png");
        fs::copy(&source, &copy).unwrap();
        let conn = connection(&fixture.db).unwrap();
        assert!(index_one(&conn, &fixture.thumbnails, &copy).unwrap() == ScanOutcome::Indexed);
        conn.execute(
            "UPDATE screenshots SET source_path=?1",
            [source_text(&source)],
        )
        .unwrap();

        let converted =
            convert_imports_to_references(&fixture.db, &fixture.thumbnails, &fixture.captures)
                .unwrap();
        assert_eq!(converted, 1);
        let rows = fixture.rows();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0, source_text(&source));
        assert!(!copy.exists(), "the app's duplicate is removed");
        assert!(source.is_file(), "the original is untouched");
    }

    #[test]
    fn a_copy_whose_original_changed_is_kept() {
        let fixture = Fixture::new("keep-copy");
        let source = fixture.image("shot.png", 60);
        let copy = fixture.captures.join("Imported-0123456789ab-shot.png");
        fs::copy(&source, &copy).unwrap();
        let conn = connection(&fixture.db).unwrap();
        index_one(&conn, &fixture.thumbnails, &copy).unwrap();
        conn.execute(
            "UPDATE screenshots SET source_path=?1",
            [source_text(&source)],
        )
        .unwrap();
        fixture.image("shot.png", 70);

        let converted =
            convert_imports_to_references(&fixture.db, &fixture.thumbnails, &fixture.captures)
                .unwrap();
        assert_eq!(converted, 0);
        assert!(copy.is_file());
    }
}

#[cfg(test)]
mod visual_search_tests {
    use super::*;

    /// A unit vector whose first two components are `x` and `y`, padded with zeros.
    fn vector(x: f32, y: f32) -> Vec<f32> {
        let length = (x * x + y * y).sqrt();
        let mut values = vec![0.0; IMAGE_EMBEDDING_DIMENSIONS];
        values[0] = x / length;
        values[1] = y / length;
        values
    }

    fn library_with_images(vectors: &[Vec<f32>]) -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE screenshots (id INTEGER PRIMARY KEY);
             CREATE TABLE screenshot_image_embeddings (
                 screenshot_id INTEGER PRIMARY KEY, vector BLOB NOT NULL
             );",
        )
        .unwrap();
        for (index, values) in vectors.iter().enumerate() {
            let id = index as i64 + 1;
            conn.execute("INSERT INTO screenshots(id) VALUES(?1)", [id])
                .unwrap();
            store_vector(&conn, VectorTable::Image, id, values).unwrap();
        }
        conn
    }

    #[test]
    fn keeps_only_images_close_to_the_best_match() {
        // Similarities to the query (1, 0) are the x components: 0.30, 0.27, 0.21, 0.24.
        let conn = library_with_images(&[
            vector(0.30, (1.0f32 - 0.09).sqrt()),
            vector(0.27, (1.0f32 - 0.0729).sqrt()),
            vector(0.21, (1.0f32 - 0.0441).sqrt()),
            vector(0.24, (1.0f32 - 0.0576).sqrt()),
        ]);
        let mut ids = visual_matches(&conn, &vector(1.0, 0.0))
            .unwrap()
            .into_iter()
            .map(|(id, _)| id)
            .collect::<Vec<_>>();
        ids.sort();
        // 0.24 clears the floor but is more than the margin below the best match.
        assert_eq!(ids, vec![1, 2]);
    }

    #[test]
    fn returns_nothing_when_no_image_clears_the_floor() {
        let conn = library_with_images(&[vector(0.20, (1.0f32 - 0.04).sqrt())]);
        assert!(visual_matches(&conn, &vector(1.0, 0.0)).unwrap().is_empty());
    }

    #[test]
    fn rejects_vectors_of_the_wrong_size() {
        let conn = library_with_images(&[]);
        conn.execute("INSERT INTO screenshots(id) VALUES(1)", [])
            .unwrap();
        assert!(
            store_vector(
                &conn,
                VectorTable::Image,
                1,
                &[0.5; TEXT_EMBEDDING_DIMENSIONS]
            )
            .is_err()
        );
    }
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
