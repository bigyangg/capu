use crate::library;
use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, mpsc},
    time::{Duration, Instant},
};
use tauri::{AppHandle, Emitter};

pub struct FolderWatchService {
    watcher: RecommendedWatcher,
    roots: Arc<Mutex<Vec<PathBuf>>>,
}

impl FolderWatchService {
    pub fn new(
        app: AppHandle,
        db_path: PathBuf,
        thumbnail_dir: PathBuf,
        capture_dir: PathBuf,
        import_lock: Arc<Mutex<()>>,
    ) -> Result<Self, String> {
        let (sender, receiver) = mpsc::channel::<PathBuf>();
        let mut watcher = notify::recommended_watcher(move |event: notify::Result<Event>| {
            if let Ok(event) = event
                && (event.kind.is_create() || event.kind.is_modify())
            {
                for path in event.paths {
                    let _ = sender.send(path);
                }
            }
        })
        .map_err(|error| format!("Could not start folder monitoring: {error}"))?;

        let stored_roots = library::monitored_folders_from_db(&db_path)?
            .into_iter()
            .filter_map(|path| std::fs::canonicalize(path).ok())
            .filter(|path| path.is_dir())
            .collect::<Vec<_>>();
        let mut active_roots = Vec::new();
        for root in stored_roots {
            match watcher.watch(&root, RecursiveMode::Recursive) {
                Ok(()) => active_roots.push(root),
                Err(error) => eprintln!("Could not monitor {}: {error}", root.display()),
            }
        }
        let roots = Arc::new(Mutex::new(active_roots));
        let event_roots = Arc::clone(&roots);
        std::thread::Builder::new()
            .name("screenshot-folder-importer".into())
            .spawn(move || {
                let mut pending: HashMap<PathBuf, (Instant, u8)> = HashMap::new();
                loop {
                    match receiver.recv_timeout(Duration::from_millis(250)) {
                        Ok(path) => {
                            pending.insert(path, (Instant::now() + Duration::from_millis(500), 0));
                        }
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                    }
                    while let Ok(path) = receiver.try_recv() {
                        pending.insert(path, (Instant::now() + Duration::from_millis(500), 0));
                    }
                    let now = Instant::now();
                    let ready = pending
                        .iter()
                        .filter(|(_, (retry_at, _))| *retry_at <= now)
                        .map(|(path, _)| path.clone())
                        .collect::<Vec<_>>();
                    let mut imported = 0;
                    let mut first_failure = None;
                    for path in ready {
                        let attempts = pending
                            .remove(&path)
                            .map(|(_, attempts)| attempts)
                            .unwrap_or(0);
                        let monitored = event_roots
                            .lock()
                            .map(|roots| roots.clone())
                            .unwrap_or_default();
                        let Ok(_guard) = import_lock.lock() else {
                            continue;
                        };
                        match library::import_monitored_event(
                            &db_path,
                            &thumbnail_dir,
                            &capture_dir,
                            &monitored,
                            &path,
                        ) {
                            Ok(count) => imported += count,
                            Err(error) => {
                                if path.exists() && attempts < 4 {
                                    let delay = Duration::from_millis(500 * (1 << attempts));
                                    pending.insert(path, (Instant::now() + delay, attempts + 1));
                                } else {
                                    first_failure.get_or_insert_with(|| {
                                        format!("{}: {error}", path.display())
                                    });
                                }
                            }
                        }
                    }
                    if imported > 0 {
                        let _ = app.emit("library-changed", imported.to_string());
                    }
                    if let Some(error) = first_failure {
                        let _ = app.emit("folder-import-failed", error);
                    }
                }
            })
            .map_err(|error| format!("Could not start folder import queue: {error}"))?;

        Ok(Self { watcher, roots })
    }

    pub fn add(&mut self, folder: &Path) -> Result<PathBuf, String> {
        let canonical = std::fs::canonicalize(folder)
            .map_err(|error| format!("Cannot access that folder: {error}"))?;
        if !canonical.is_dir() {
            return Err("Choose an existing folder.".to_string());
        }
        let mut roots = self
            .roots
            .lock()
            .map_err(|_| "Folder monitor is unavailable.".to_string())?;
        if roots.iter().any(|root| root == &canonical) {
            return Ok(canonical);
        }
        self.watcher
            .watch(&canonical, RecursiveMode::Recursive)
            .map_err(|error| format!("Could not monitor that folder: {error}"))?;
        roots.push(canonical.clone());
        Ok(canonical)
    }

    pub fn remove(&mut self, folder: &Path) -> Result<(), String> {
        let mut roots = self
            .roots
            .lock()
            .map_err(|_| "Folder monitor is unavailable.".to_string())?;
        if roots.iter().any(|root| root == folder) {
            self.watcher
                .unwatch(folder)
                .map_err(|error| format!("Could not stop monitoring that folder: {error}"))?;
            roots.retain(|root| root != folder);
        }
        Ok(())
    }
}
