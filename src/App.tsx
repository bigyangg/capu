import { useCallback, useEffect, useMemo, useRef, useState, type KeyboardEvent as ReactKeyboardEvent } from "react";
import { convertFileSrc, invoke } from "@tauri-apps/api/core";
import { IdleWorker, type WorkerResult } from "./modelWorkers";
import { listen } from "@tauri-apps/api/event";
import { open, save as saveDialog } from "@tauri-apps/plugin-dialog";
import {
  AlertCircle,
  ArrowDownWideNarrow,
  ArrowUpRight,
  ChevronLeft,
  ChevronRight,
  Check,
  ChevronDown,
  Clock3,
  Copy,
  Download,
  FileImage,
  Folder,
  FolderOpen,
  Grid2X2,
  ImagePlus,
  LayoutGrid,
  List,
  Layers3,
  LoaderCircle,
  LockKeyhole,
  RotateCw,
  Search,
  Settings2,
  ShieldCheck,
  Sparkles,
  X,
} from "lucide-react";
import "./App.css";

type Screenshot = {
  id: number;
  path: string;
  file_name: string;
  captured_at: number;
  file_size: number;
  status: "pending" | "indexed" | "error" | "missing" | "unavailable" | string;
  ocr_text: string;
  last_error: string | null;
  /** Content hash of the thumbnail, served by the app's thumb:// protocol. */
  thumbnail: string | null;
  category: string | null;
  subcategory: string | null;
  suggested_category: string | null;
  suggested_subcategory: string | null;
  category_is_manual: boolean;
  match_reason: string | null;
};

type IndexReport = {
  discovered: number;
  indexed: number;
  duplicates: number;
  failed: number;
  screenshots: Screenshot[];
};

type FolderStatus = { path: string; available: boolean };

// "missing": the original's folder is there but the file is not.
// "unavailable": the folder cannot be reached, usually a disconnected drive; the entry
// returns to normal when it is reconnected.
function originalReachable(item: Screenshot) {
  return item.status !== "missing" && item.status !== "unavailable";
}

type StorageUsage = {
  library_files: number;
  library_bytes: number;
  referenced_files: number;
  thumbnail_bytes: number;
  database_bytes: number;
};

type ImportSummary = {
  imported: number;
  duplicates: number;
  failed: number;
  removed: number;
  monitored_folders: string[];
};

const SCREENSHOT_CATEGORIES = ["Projects", "Finance", "Meetings", "Development", "Reference"] as const;
const CATEGORY_SUBCATEGORIES: Record<string, string[]> = {
  Projects: ["Websites & landing pages", "Client projects", "Product planning", "Campaigns"],
  Finance: ["Budgets", "Invoices & receipts", "Payments & statements", "Taxes"],
  Meetings: ["Notes & agendas", "Decisions", "Follow-ups"],
  Development: ["Code & terminal", "Errors & debugging", "Hosting & domains", "Configuration"],
  Reference: ["Design inspiration", "Documentation", "Saved references"],
};
const CATEGORY_DESCRIPTIONS: Record<string, string> = {
  Projects: "Product work, client work and plans",
  Finance: "Budgets, invoices and statements",
  Meetings: "Agendas, decisions and follow-ups",
  Development: "Code, setup and troubleshooting",
  Reference: "Design, documentation and saved items",
  Unfiled: "Screenshots that need a folder",
};
const CATEGORY_TONES: Record<string, string> = {
  Projects: "blue",
  Finance: "sage",
  Meetings: "lavender",
  Development: "slate",
  Reference: "sand",
  Unfiled: "gray",
};

type CategoryChoice = { category: string | null; subcategory: string | null };

function CategoryPicker({ screenshot, onChange }: {
  screenshot: Screenshot;
  onChange: (screenshot: Screenshot, choice: CategoryChoice) => void;
}) {
  const value = screenshot.category
    ? screenshot.subcategory ? `${screenshot.category}::${screenshot.subcategory}` : screenshot.category
    : "";
  const suggested = screenshot.suggested_category
    ? `${screenshot.suggested_category}${screenshot.suggested_subcategory ? ` · ${screenshot.suggested_subcategory}` : ""}`
    : null;
  const label = screenshot.category
    ? screenshot.category_is_manual ? `${screenshot.category}${screenshot.subcategory ? ` · ${screenshot.subcategory}` : ""}` : `Suggested · ${suggested ?? screenshot.category}`
    : "Needs a category";
  return <label className={`category-picker ${screenshot.category_is_manual ? "manual" : "suggested"}`} onClick={(event) => event.stopPropagation()}>
    {!screenshot.category_is_manual && screenshot.category && <Sparkles size={12} />}
    <span>{label}</span><ChevronDown size={12} />
    <select
      value={screenshot.category_is_manual ? value : ""}
      aria-label={`Category for ${screenshot.file_name}`}
      onChange={(event) => {
        const [category, subcategory] = event.currentTarget.value.split("::");
        onChange(screenshot, { category: category || null, subcategory: subcategory || null });
      }}
    >
      <option value="">Use automatic suggestion</option>
      {SCREENSHOT_CATEGORIES.map((category) => <optgroup key={category} label={category}>
        <option value={category}>{category} (top level)</option>
        {CATEGORY_SUBCATEGORIES[category].map((subcategory) => <option key={subcategory} value={`${category}::${subcategory}`}>{subcategory}</option>)}
      </optgroup>)}
    </select>
  </label>;
}

function FolderTile({ name, description, count, tone, compact = false, onOpen }: {
  name: string;
  description: string;
  count: number;
  tone: string;
  compact?: boolean;
  onOpen: () => void;
}) {
  return <button className={`folder-tile ${compact ? "compact" : ""}`} onClick={onOpen} aria-label={`Open ${name} folder, ${count} screenshots`}>
    <div className="folder-tile-top">
      <span className={`folder-art ${tone}`} aria-hidden="true"><span className="folder-art-tab" /><span className="folder-art-face" /></span>
      <span className="folder-tile-count">{count}</span>
    </div>
    <strong>{name}</strong>
    <small>{description}</small>
  </button>;
}

type SemanticText = { id: number; text: string };

function thumbnailUrl(hash: string) {
  return convertFileSrc(hash, "thumb");
}

// Model workers are shut down when idle to give their memory back. Indexing finishes in
// bursts, so its worker stops soon after; search models stay a little longer so a few
// searches in a row stay instant.
const INDEX_WORKER_IDLE_MS = 15_000;
const SEARCH_WORKER_IDLE_MS = 3 * 60_000;

const dateFormat = new Intl.DateTimeFormat(undefined, {
  month: "short",
  day: "numeric",
  year: "numeric",
});

function RegionCapture() {
  const [image, setImage] = useState<string | null>(null);
  const [selection, setSelection] = useState<{ x: number; y: number; width: number; height: number } | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);
  const stageRef = useRef<HTMLDivElement>(null);
  const startRef = useRef<{ x: number; y: number } | null>(null);

  useEffect(() => {
    void invoke<string>("get_region_preview").then(setImage).catch((cause) => setError(String(cause)));
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.key === "Escape") void invoke("cancel_region_capture");
      if (event.key === "Enter" && selection && !saving) void saveSelection();
    };
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, [selection, saving]);

  const point = (event: React.PointerEvent) => {
    const bounds = stageRef.current!.getBoundingClientRect();
    return { x: Math.max(0, Math.min(1, (event.clientX - bounds.left) / bounds.width)), y: Math.max(0, Math.min(1, (event.clientY - bounds.top) / bounds.height)) };
  };
  const onPointerDown = (event: React.PointerEvent<HTMLDivElement>) => {
    event.currentTarget.setPointerCapture(event.pointerId);
    const start = point(event);
    startRef.current = start;
    setSelection({ ...start, width: 0, height: 0 });
  };
  const onPointerMove = (event: React.PointerEvent<HTMLDivElement>) => {
    if (!startRef.current) return;
    const end = point(event);
    const start = startRef.current;
    setSelection({ x: Math.min(start.x, end.x), y: Math.min(start.y, end.y), width: Math.abs(end.x - start.x), height: Math.abs(end.y - start.y) });
  };
  const stopDrag = () => { startRef.current = null; };
  const saveSelection = async () => {
    if (!selection || selection.width < 0.003 || selection.height < 0.003) return;
    setSaving(true);
    setError(null);
    try {
      await invoke("finish_region_capture", selection);
    } catch (cause) {
      setError(String(cause));
      setSaving(false);
    }
  };

  return <main className="region-stage" ref={stageRef} onPointerDown={onPointerDown} onPointerMove={onPointerMove} onPointerUp={stopDrag} onPointerCancel={stopDrag}>
    {image && <img className="region-image" src={image} draggable={false} alt="Desktop capture" />}
    <div className="region-shade" />
    {selection && <div className="region-selection" style={{ left: `${selection.x * 100}%`, top: `${selection.y * 100}%`, width: `${selection.width * 100}%`, height: `${selection.height * 100}%` }} />}
    <div className="region-toolbar" onPointerDown={(event) => event.stopPropagation()}><strong>Capture a region</strong><span>Drag to select · Esc to cancel</span><button onClick={() => void invoke("cancel_region_capture")}>Cancel</button><button className="region-save" disabled={!selection || selection.width < 0.003 || saving} onClick={() => void saveSelection()}>{saving ? "Saving…" : "Save capture"}</button></div>
    {error && <div className="region-error">{error}</div>}
  </main>;
}

function prettyDate(timestamp: number) {
  return dateFormat.format(new Date(timestamp * 1000));
}

function formatBytes(bytes: number) {
  if (bytes < 1024 * 1024) return `${Math.max(1, Math.round(bytes / 1024))} KB`;
  return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
}

type ShortcutSettings = {
  full_screen: string;
  region: string;
  default_full_screen: string;
  default_region: string;
  platform: string;
};

// Shortcuts are stored as accelerator strings such as "Alt+Shift+S" and shown with the
// key names used on the current operating system.
function shortcutParts(value: string, platform: string) {
  const mac = platform === "macos";
  const names: Record<string, string> = {
    control: mac ? "Control" : "Ctrl",
    ctrl: mac ? "Control" : "Ctrl",
    alt: mac ? "Option" : "Alt",
    option: mac ? "Option" : "Alt",
    shift: "Shift",
    super: mac ? "Cmd" : "Win",
    cmd: mac ? "Cmd" : "Win",
    command: mac ? "Cmd" : "Win",
  };
  return value.split("+").map((part) => names[part.trim().toLowerCase()] ?? part.trim().toUpperCase());
}

function formatShortcut(value: string, platform: string) {
  return shortcutParts(value, platform).join(" + ");
}

function keyFromCode(code: string) {
  const letter = /^Key([A-Z])$/.exec(code);
  if (letter) return letter[1];
  const digit = /^Digit([0-9])$/.exec(code);
  if (digit) return digit[1];
  if (/^F([1-9]|1[0-9]|2[0-4])$/.test(code)) return code;
  return null;
}

function ShortcutRecorder({ label, value, platform, disabled, onChange }: {
  label: string;
  value: string;
  platform: string;
  disabled: boolean;
  onChange: (value: string) => void;
}) {
  const [recording, setRecording] = useState(false);
  const [hint, setHint] = useState<string | null>(null);
  const mac = platform === "macos";
  const onKeyDown = (event: ReactKeyboardEvent<HTMLButtonElement>) => {
    if (!recording) return;
    event.preventDefault();
    event.stopPropagation();
    if (event.key === "Escape") {
      setRecording(false);
      setHint(null);
      return;
    }
    const key = keyFromCode(event.code);
    if (!key) {
      if (!["Control", "Alt", "Shift", "Meta"].includes(event.key)) setHint("Use a letter, number or function key.");
      return;
    }
    if (!(event.ctrlKey || event.altKey || event.metaKey)) {
      setHint(mac ? "Hold Control, Option or Cmd with the key." : "Hold Ctrl or Alt with the key.");
      return;
    }
    const parts = [event.ctrlKey && "Control", event.altKey && "Alt", event.shiftKey && "Shift", event.metaKey && "Super", key].filter(Boolean);
    setRecording(false);
    setHint(null);
    onChange(parts.join("+"));
  };
  return <div className="shortcut-row">
    <span>{label}</span>
    <button
      type="button"
      className={recording ? "shortcut-recorder recording" : "shortcut-recorder"}
      disabled={disabled}
      aria-label={recording ? `Press the new ${label.toLowerCase()} shortcut, or Escape to cancel` : `${label} shortcut: ${formatShortcut(value, platform)}. Activate to change.`}
      onClick={() => { setRecording(true); setHint(null); }}
      onBlur={() => setRecording(false)}
      onKeyDown={onKeyDown}
    >
      {recording ? "Press keys…" : shortcutParts(value, platform).map((part) => <kbd key={part}>{part}</kbd>)}
    </button>
    {hint && <small>{hint}</small>}
  </div>;
}

function LibraryApp() {
  const [folder, setFolder] = useState<string | null>(null);
  const [managedCaptureFolder, setManagedCaptureFolder] = useState<string | null>(null);
  const [items, setItems] = useState<Screenshot[]>([]);
  const [query, setQuery] = useState("");
  const [selected, setSelected] = useState<Screenshot | null>(null);
  const [detailImage, setDetailImage] = useState<string | null>(null);
  const [detailImageBusy, setDetailImageBusy] = useState(false);
  const [detailImageError, setDetailImageError] = useState<string | null>(null);
  const [shareAction, setShareAction] = useState<"copy" | "save" | null>(null);
  const [shareFeedback, setShareFeedback] = useState<"copy" | "save" | null>(null);
  const [loading, setLoading] = useState(true);
  const [scanning, setScanning] = useState(false);
  const [ocrBusy, setOcrBusy] = useState(false);
  const [ocrProgress, setOcrProgress] = useState(0);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [settingsOpen, setSettingsOpen] = useState(false);
  const [storage, setStorage] = useState<StorageUsage | null>(null);
  const [folderStatus, setFolderStatus] = useState<Record<string, boolean>>({});
  const [recheckBusy, setRecheckBusy] = useState(false);
  const [captureMode, setCaptureMode] = useState<"active" | "all">("active");
  const [autostart, setAutostart] = useState(false);
  const [captureStatus, setCaptureStatus] = useState<string | null>(null);
  const [shortcuts, setShortcuts] = useState<ShortcutSettings | null>(null);
  const [shortcutBusy, setShortcutBusy] = useState(false);
  const [filter, setFilter] = useState<"all" | "recent" | "attention">("all");
  const [categoryFilter, setCategoryFilter] = useState<string | null>(null);
  const [subcategoryFilter, setSubcategoryFilter] = useState<string | null>(null);
  const [viewMode, setViewMode] = useState<"grid" | "list">("grid");
  const [sortOrder, setSortOrder] = useState<"newest" | "oldest" | "name">("newest");
  const [monitoredFolders, setMonitoredFolders] = useState<string[]>([]);
  const [monitorBusy, setMonitorBusy] = useState(false);
  const [recentImportCount, setRecentImportCount] = useState(0);
  const searchInputRef = useRef<HTMLInputElement>(null);
  // OCR and image vectors for new screenshots; text vectors for typed searches.
  const indexWorker = useRef<IdleWorker | null>(null);
  const searchWorker = useRef<IdleWorker | null>(null);
  const runningJobs = useRef(new Set<number>());
  const semanticBackfillRunning = useRef(false);
  const visualBackfillRunning = useRef(false);
  const visualBackfillRequested = useRef(false);
  const visualBackfillFailed = useRef(new Set<number>());

  const refresh = useCallback(async (search = "") => {
    try {
      let response: Screenshot[];
      if (search.trim()) {
        const worker = searchWorker.current;
        const [meaning, visual] = worker
          ? await Promise.allSettled([
            worker.vector({ type: "embed", text: search }),
            worker.vector({ type: "visual-query", text: search }),
          ])
          : [{ status: "rejected" as const, reason: null }, { status: "rejected" as const, reason: null }];
        const embedding = meaning.status === "fulfilled" ? meaning.value : [];
        const imageQueryEmbedding = visual.status === "fulfilled" ? visual.value : [];
        if (meaning.status === "rejected") setNotice("Showing text matches while local meaning search prepares.");
        response = await invoke<Screenshot[]>("search_screenshots", { query: search, embedding, imageQueryEmbedding });
      } else {
        response = await invoke<Screenshot[]>("list_screenshots");
      }
      setItems(response);
      setSelected((current) => current ? response.find((item) => item.id === current.id) ?? null : null);
      return response;
    } catch (cause) {
      setError(String(cause));
      return [];
    }
  }, []);

  useEffect(() => {
    const index = new IdleWorker("Local indexing", INDEX_WORKER_IDLE_MS, { onProgress: setOcrProgress, onCrash: setError });
    const search = new IdleWorker("Local search", SEARCH_WORKER_IDLE_MS, { onCrash: setError });
    indexWorker.current = index;
    searchWorker.current = search;

    let cancelled = false;
    void (async () => {
      setLoading(true);
      try {
        const [captureFolder, savedMode, autostartEnabled, shortcutStatus, watchedFolders, captureShortcuts] = await Promise.all([
          invoke<string>("get_capture_folder"),
          invoke<"active" | "all">("get_capture_display_mode"),
          invoke<boolean>("get_autostart_enabled"),
          invoke<string | null>("get_capture_status"),
          invoke<string[]>("get_monitored_folders"),
          invoke<ShortcutSettings>("get_capture_shortcuts"),
        ]);
        if (cancelled) return;
        setFolder(captureFolder);
        setShortcuts(captureShortcuts);
        setManagedCaptureFolder(captureFolder);
        setCaptureMode(savedMode);
        setAutostart(autostartEnabled);
        setCaptureStatus(shortcutStatus);
        setMonitoredFolders(watchedFolders);
        const existing = await invoke<IndexReport>("index_capture_folder");
        if (!cancelled) setItems(existing.screenshots);
        const sync = await invoke<ImportSummary>("sync_monitored_folders");
        if (!cancelled) {
          const current = await invoke<Screenshot[]>("list_screenshots");
          setItems(current);
          if (sync.imported > 0) setNotice(`${sync.imported} new screenshot${sync.imported === 1 ? "" : "s"} imported from monitored folders.`);
          else if (sync.removed > 0) setNotice(`${sync.removed} screenshot${sync.removed === 1 ? "" : "s"} removed because the original was deleted.`);
          if (sync.failed > 0) setError(`${sync.failed} screenshot${sync.failed === 1 ? " needs" : "s need"} attention during automatic import.`);
        }
      } catch (cause) {
        if (!cancelled) setError(`Could not open the local library: ${String(cause)}`);
      } finally {
        if (!cancelled) setLoading(false);
      }
    })();

    return () => {
      cancelled = true;
      index.stop();
      search.stop();
      indexWorker.current = null;
      searchWorker.current = null;
    };
  }, []);

  const runOcr = useCallback(async (screenshots: Screenshot[]) => {
    const worker = indexWorker.current;
    if (!worker) return;
    const queue = screenshots.filter((item) => item.status === "pending" && !runningJobs.current.has(item.id));
    if (!queue.length) return;
    queue.forEach((item) => runningJobs.current.add(item.id));
    setOcrBusy(true);
    setNotice(`Preparing to read ${queue.length} screenshot${queue.length === 1 ? "" : "s"} on this device...`);
    let complete = 0;
    for (const screenshot of queue) {
      setNotice(`Reading text ${complete + 1} of ${queue.length} - ${screenshot.file_name}`);
      setOcrProgress(queue.length ? complete / queue.length : 0);
      try {
        // The system's own OCR (Windows or macOS) is used when available; it runs in Rust and
        // frees its memory after each image. Tesseract in the worker is the fallback.
        const nativeText = await invoke<string | null>("recognize_text", { id: screenshot.id }).catch(() => null);
        let result: WorkerResult;
        if (nativeText !== null) {
          result = { text: nativeText };
          if (nativeText.trim()) {
            try {
              result.embedding = await worker.vector({ type: "embed", text: nativeText });
            } catch (cause) {
              result.semanticError = String(cause);
            }
          }
        } else {
          const imageDataUrl = await invoke<string>("get_ocr_image", { id: screenshot.id });
          result = await worker.run({ type: "ocr", imageDataUrl });
        }
        if (result.error) throw new Error(result.error);
        const text = result.text ?? "";
        await invoke("save_ocr_text", { id: screenshot.id, text, embedding: result.embedding ?? null });
        if (result.semanticError) setNotice("Text indexed. Local meaning search will retry when its model is available.");
        setItems((current) => current.map((item) => item.id === screenshot.id ? { ...item, status: "indexed", ocr_text: text, last_error: null } : item));
        setSelected((current) => current?.id === screenshot.id ? { ...current, status: "indexed", ocr_text: text, last_error: null } : current);
      } catch (cause) {
        const message = String(cause);
        await invoke("mark_ocr_failed", { id: screenshot.id, error: message }).catch(() => undefined);
        setItems((current) => current.map((item) => item.id === screenshot.id ? { ...item, status: "error", last_error: message } : item));
        setSelected((current) => current?.id === screenshot.id ? { ...current, status: "error", last_error: message } : current);
      }
      runningJobs.current.delete(screenshot.id);
      complete += 1;
      setOcrProgress(complete / queue.length);
    }
    setOcrBusy(false);
    setOcrProgress(0);
    setNotice(null);
    await refresh(query);
  }, [query, refresh]);

  const runSemanticBackfill = useCallback(async () => {
    if (semanticBackfillRunning.current) return;
    semanticBackfillRunning.current = true;
    try {
      const queue = await invoke<SemanticText[]>("get_unembedded_texts");
      if (!queue.length) return;
      let complete = 0;
      setNotice(`Preparing local meaning search for ${queue.length} screenshots...`);
      for (const item of queue) {
        try {
          const worker = indexWorker.current;
          if (!worker) break;
          const embedding = await worker.vector({ type: "embed", text: item.text });
          await invoke("save_semantic_embedding", { id: item.id, embedding });
        } catch (cause) {
          setError(`Local meaning search could not index ${item.id}: ${String(cause)}`);
          break;
        }
        complete += 1;
        if (complete % 8 === 0 || complete === queue.length) {
          setNotice(`Adding local meaning search · ${complete} of ${queue.length}`);
          setOcrProgress(complete / queue.length);
        }
      }
      setOcrProgress(0);
      setNotice(null);
      await refresh(query);
    } catch (cause) {
      setError(`Could not prepare local meaning search: ${String(cause)}`);
    } finally {
      semanticBackfillRunning.current = false;
    }
  }, [query, refresh]);

  useEffect(() => {
    if (loading) return;
    const unfinished = items.filter((item) => item.status === "pending");
    if (unfinished.length) void runOcr(unfinished);
  }, [items, loading, runOcr]);

  useEffect(() => {
    if (!loading) void runSemanticBackfill();
  }, [loading, runSemanticBackfill]);

  const runVisualBackfill = useCallback(async () => {
    if (visualBackfillRunning.current) {
      visualBackfillRequested.current = true;
      return;
    }
    visualBackfillRunning.current = true;
    let complete = 0;
    try {
      do {
        visualBackfillRequested.current = false;
        for (;;) {
          const batch = await invoke<{ id: number; image_data_url: string }[]>("get_unembedded_images", {
            exclude: [...visualBackfillFailed.current],
          });
          if (!batch.length) break;
          for (const item of batch) {
            try {
              const worker = indexWorker.current;
              if (!worker) return;
              const embedding = await worker.vector({ type: "image-embed", imageDataUrl: item.image_data_url });
              await invoke("save_image_embedding", { id: item.id, embedding });
              complete += 1;
            } catch {
              // Skip this image for the rest of the session so one bad file cannot stall the queue.
              visualBackfillFailed.current.add(item.id);
            }
          }
          if (complete >= 16) setNotice(`Preparing visual search · ${complete} images`);
        }
      } while (visualBackfillRequested.current);
      if (complete >= 16) setNotice(null);
    } catch (cause) {
      setError(`Could not prepare visual search: ${String(cause)}`);
    } finally {
      visualBackfillRunning.current = false;
    }
  }, []);

  useEffect(() => {
    if (!loading) void runVisualBackfill();
  }, [items.length, loading, runVisualBackfill]);

  useEffect(() => {
    let unlistenCreated: (() => void) | undefined;
    let unlistenFailed: (() => void) | undefined;
    let unlistenImported: (() => void) | undefined;
    let unlistenImportFailed: (() => void) | undefined;
    let unlistenRemoved: (() => void) | undefined;
    void listen<string>("capture-created", async () => {
      try {
        const report = await invoke<IndexReport>("index_capture_folder");
        setItems(report.screenshots);
        setNotice(`${report.indexed} new capture${report.indexed === 1 ? "" : "s"} saved and indexing locally.`);
        setError(null);
        await runOcr(report.screenshots);
        await refresh(query);
      } catch (cause) {
        setError(`The screenshot was saved but could not be indexed: ${String(cause)}`);
      }
    }).then((dispose) => { unlistenCreated = dispose; });
    void listen<string>("capture-failed", (event) => setError(`Capture failed: ${event.payload}`)).then((dispose) => { unlistenFailed = dispose; });
    void listen<string>("library-changed", async (event) => {
      try {
        const current = await invoke<Screenshot[]>("list_screenshots");
        setItems(current);
        const importedCount = Number.parseInt(event.payload, 10) || 0;
        if (importedCount > 0) {
          setRecentImportCount(importedCount);
          setNotice(`${importedCount} new screenshot${importedCount === 1 ? "" : "s"} added to your library.`);
          setError(null);
        }
        await runOcr(current);
      } catch (cause) {
        setError(`Could not refresh the imported screenshots: ${String(cause)}`);
      }
    }).then((dispose) => { unlistenImported = dispose; });
    void listen<string>("folder-import-failed", (event) => setError(`Automatic import needs attention: ${event.payload}`)).then((dispose) => { unlistenImportFailed = dispose; });
    void listen<string>("library-items-removed", async (event) => {
      const removedCount = Number.parseInt(event.payload, 10) || 1;
      await refresh(query);
      setNotice(`${removedCount} screenshot${removedCount === 1 ? "" : "s"} removed because the original was deleted.`);
    }).then((dispose) => { unlistenRemoved = dispose; });
    return () => { unlistenCreated?.(); unlistenFailed?.(); unlistenImported?.(); unlistenImportFailed?.(); unlistenRemoved?.(); };
  }, [query, refresh, runOcr]);

  const selectLibrary = async () => {
    await addMonitoredFolder();
  };

  const addMonitoredFolder = async () => {
    setError(null);
    const selectedFolder = await open({ directory: true, multiple: false, title: "Choose a folder to monitor" });
    if (typeof selectedFolder !== "string") return;
    setMonitorBusy(true);
    try {
      const report = await invoke<ImportSummary>("add_monitored_folder", { folder: selectedFolder });
      setMonitoredFolders(report.monitored_folders);
      const current = await invoke<Screenshot[]>("list_screenshots");
      setItems(current);
      if (report.imported > 0) setRecentImportCount(report.imported);
      setNotice(`${report.imported} imported · ${report.duplicates} duplicates skipped${report.failed ? ` · ${report.failed} need attention` : ""}`);
      setMonitorBusy(false);
      void runOcr(current);
    } catch (cause) {
      setError(`Could not monitor that folder: ${String(cause)}`);
    } finally {
      setMonitorBusy(false);
    }
  };

  const removeMonitoredFolder = async (path: string) => {
    try {
      const folders = await invoke<string[]>("remove_monitored_folder", { folder: path });
      setMonitoredFolders(folders);
      setNotice("Folder monitoring stopped. Imported screenshots remain in your managed library.");
    } catch (cause) {
      setError(`Could not stop monitoring that folder: ${String(cause)}`);
    }
  };

  useEffect(() => {
    if (!settingsOpen) return;
    let cancelled = false;
    let unlisten: (() => void) | undefined;
    const loadFolderStatus = () => void invoke<FolderStatus[]>("get_monitored_folder_status")
      .then((statuses) => { if (!cancelled) setFolderStatus(Object.fromEntries(statuses.map((item) => [item.path, item.available]))); })
      .catch(() => undefined);
    void invoke<StorageUsage>("get_storage_usage")
      .then((usage) => { if (!cancelled) setStorage(usage); })
      .catch((cause) => { if (!cancelled) setError(`Could not check storage: ${String(cause)}`); });
    loadFolderStatus();
    void listen("monitored-folders-changed", loadFolderStatus).then((dispose) => {
      if (cancelled) dispose();
      else unlisten = dispose;
    });
    return () => { cancelled = true; unlisten?.(); };
  }, [settingsOpen]);

  const recheckAllFiles = async () => {
    setRecheckBusy(true);
    setError(null);
    try {
      const report = await invoke<ImportSummary>("sync_monitored_folders", { full: true });
      const current = await invoke<Screenshot[]>("list_screenshots");
      setItems(current);
      setNotice(report.imported > 0
        ? `${report.imported} new or changed screenshot${report.imported === 1 ? "" : "s"} found.`
        : "All watched screenshots are up to date.");
      setStorage(await invoke<StorageUsage>("get_storage_usage"));
      void runOcr(current);
    } catch (cause) {
      setError(`Could not re-check files: ${String(cause)}`);
    } finally {
      setRecheckBusy(false);
    }
  };

  const removeMissingScreenshot = async (item: Screenshot) => {
    try {
      await invoke("remove_missing_screenshot", { id: item.id });
      setItems((current) => current.filter((entry) => entry.id !== item.id));
      setSelected((current) => current?.id === item.id ? null : current);
      setNotice("Removed from your library.");
    } catch (cause) {
      setError(String(cause));
    }
  };

  const updateShortcuts = async (fullScreen: string, region: string) => {
    setShortcutBusy(true);
    try {
      setShortcuts(await invoke<ShortcutSettings>("set_capture_shortcuts", { fullScreen, region }));
      setCaptureStatus(null);
      setError(null);
    } catch (cause) {
      setError(`Could not change the shortcut: ${String(cause)}`);
    } finally {
      setShortcutBusy(false);
    }
  };

  const updateCategory = async (screenshot: Screenshot, choice: CategoryChoice) => {
    try {
      await invoke("set_screenshot_category", { id: screenshot.id, ...choice });
      const updated = {
        ...screenshot,
        category: choice.category ?? screenshot.suggested_category,
        subcategory: choice.category ? choice.subcategory : screenshot.suggested_subcategory,
        category_is_manual: choice.category !== null,
      };
      setItems((current) => current.map((item) => item.id === screenshot.id ? updated : item));
      setSelected((current) => current?.id === screenshot.id ? updated : current);
    } catch (cause) {
      setError(`Could not update the category: ${String(cause)}`);
    }
  };

  useEffect(() => {
    const onShortcut = (event: KeyboardEvent) => {
      if (!(event.metaKey || event.ctrlKey)) return;
      if (event.key.toLowerCase() === "k") {
        event.preventDefault();
        searchInputRef.current?.focus();
      } else if (event.key.toLowerCase() === "o") {
        event.preventDefault();
        void selectLibrary();
      }
    };
    window.addEventListener("keydown", onShortcut);
    return () => window.removeEventListener("keydown", onShortcut);
  }, []);

  const scanFolder = async () => {
    if (!folder) return;
    setScanning(true);
    setError(null);
    setNotice("Checking the selected folder and building your local library…");
    try {
      const report = await invoke<IndexReport>("index_capture_folder");
      setItems(report.screenshots);
      const details = [
        `${report.indexed} added`,
        report.duplicates ? `${report.duplicates} duplicate${report.duplicates === 1 ? "" : "s"} skipped` : "",
        report.failed ? `${report.failed} need attention` : "",
      ].filter(Boolean).join(" · ");
      setNotice(report.discovered ? details : "No screenshots found in this folder.");
      await runOcr(report.screenshots);
    } catch (cause) {
      setError(String(cause));
    } finally {
      setScanning(false);
    }
  };

  useEffect(() => {
    if (!folder || loading) return;
    const timer = window.setTimeout(() => void refresh(query), 180);
    return () => window.clearTimeout(timer);
  }, [folder, loading, query, refresh]);

  const visibleItems = useMemo(() => items.filter((item) => {
    const needsAttentionFilter = filter !== "attention" || item.status === "error" || item.status === "missing";
    const matchesCategory = !categoryFilter || (categoryFilter === "Unfiled" ? !item.category : item.category === categoryFilter);
    const matchesSubcategory = !subcategoryFilter || subcategoryFilter === "__all__" || item.subcategory === subcategoryFilter;
    return needsAttentionFilter && matchesCategory && matchesSubcategory;
  }).slice(0, !query && filter === "recent" ? 20 : undefined).sort((left, right) => {
    if (sortOrder === "name") return left.file_name.localeCompare(right.file_name);
    return sortOrder === "newest" ? right.captured_at - left.captured_at : left.captured_at - right.captured_at;
  }), [categoryFilter, filter, items, query, sortOrder, subcategoryFilter]);

  useEffect(() => {
    if (!selected) {
      setDetailImage(null);
      setDetailImageError(null);
      setDetailImageBusy(false);
      return;
    }
    let current = true;
    setDetailImage(null);
    setDetailImageError(null);
    setDetailImageBusy(true);
    void invoke<string>("get_screenshot_preview", { id: selected.id })
      .then((image) => { if (current) setDetailImage(image); })
      .catch((cause) => {
        if (!current) return;
        setDetailImage(selected.thumbnail ? thumbnailUrl(selected.thumbnail) : null);
        setDetailImageError(`Showing a smaller preview. ${String(cause)}`);
      })
      .finally(() => { if (current) setDetailImageBusy(false); });
    return () => { current = false; };
  }, [selected?.id]);

  useEffect(() => {
    if (!selected) return;
    const onPreviewKeyDown = (event: KeyboardEvent) => {
      if (event.key === "Escape") {
        event.preventDefault();
        setSelected(null);
        return;
      }
      if (event.key !== "ArrowLeft" && event.key !== "ArrowRight") return;
      const index = visibleItems.findIndex((item) => item.id === selected.id);
      if (index < 0 || visibleItems.length < 2) return;
      event.preventDefault();
      const offset = event.key === "ArrowRight" ? 1 : -1;
      setSelected(visibleItems[(index + offset + visibleItems.length) % visibleItems.length]);
    };
    window.addEventListener("keydown", onPreviewKeyDown);
    return () => window.removeEventListener("keydown", onPreviewKeyDown);
  }, [selected, visibleItems]);

  useEffect(() => {
    setShareFeedback(null);
  }, [selected?.id]);

  useEffect(() => {
    if (!shareFeedback) return;
    const timer = window.setTimeout(() => setShareFeedback(null), 1800);
    return () => window.clearTimeout(timer);
  }, [shareFeedback]);

  const retryIndexing = async (_item: Screenshot) => {
    await scanFolder();
  };

  const openScreenshot = async (item: Screenshot) => {
    setError(null);
    try {
      await invoke("open_screenshot", { id: item.id });
    } catch (cause) {
      setError(`Could not open the original screenshot: ${String(cause)}`);
    }
  };

  const copyScreenshotImage = async (item: Screenshot) => {
    setShareAction("copy");
    setError(null);
    try {
      await invoke("copy_screenshot_image_to_clipboard", { id: item.id });
      setShareFeedback("copy");
      setNotice("Image copied. It is ready to paste.");
    } catch (cause) {
      setError(String(cause));
    } finally {
      setShareAction(null);
    }
  };

  const saveScreenshotCopy = async (item: Screenshot) => {
    const extension = item.file_name.split(".").pop()?.toLowerCase() || "png";
    setShareAction("save");
    setError(null);
    try {
      const destination = await saveDialog({
        title: "Save a copy of this screenshot",
        defaultPath: item.file_name,
        filters: [{ name: "Screenshot image", extensions: [extension] }],
      });
      if (!destination) return;
      await invoke("save_screenshot_copy", { id: item.id, destination });
      setShareFeedback("save");
      setNotice("Screenshot copy saved.");
    } catch (cause) {
      setError(String(cause));
    } finally {
      setShareAction(null);
    }
  };

  const needsAttention = items.filter((item) => item.status === "error" || item.status === "missing").length;
  const categoryCounts = useMemo(() => Object.fromEntries([
    ...SCREENSHOT_CATEGORIES.map((category) => [category, items.filter((item) => item.category === category).length] as const),
    ["Unfiled", items.filter((item) => !item.category).length] as const,
  ]), [items]);
  const subcategoryCounts = useMemo(() => Object.fromEntries(
    (categoryFilter ? CATEGORY_SUBCATEGORIES[categoryFilter] ?? [] : []).map((subcategory) => [subcategory, items.filter((item) => item.category === categoryFilter && item.subcategory === subcategory).length]),
  ), [categoryFilter, items]);
  const currentLocationTitle = query ? "Search results" : filter === "attention" ? "Needs attention" : filter === "recent" ? "Recent screenshots" : subcategoryFilter === "__all__" ? `All ${categoryFilter}` : subcategoryFilter ?? categoryFilter ?? "All Screenshots";
  const currentLocationCount = query || filter !== "all" || subcategoryFilter ? visibleItems.length : categoryFilter ? categoryCounts[categoryFilter] ?? 0 : items.length;
  const platform = shortcuts?.platform ?? (navigator.userAgent.includes("Mac") ? "macos" : "windows");
  const fullScreenLabel = shortcuts ? formatShortcut(shortcuts.full_screen, platform) : null;
  const showScreenshotBrowser = !!query || filter !== "all" || !!categoryFilter;

  return (
    <div className="app-shell">
      <aside className="sidebar">
        <div className="brand-lockup">
          <div className="brand-mark"><Layers3 size={18} strokeWidth={2.1} /></div>
          <div className="brand-copy"><strong>Screenshot Memory</strong></div>
        </div>

        <button className="new-capture" onClick={() => void invoke("capture_screens_now").catch((cause) => setError(String(cause)))}>
          <ImagePlus size={16} />
          <span>Capture</span>
          {shortcuts && <span className="new-capture-shortcut">{shortcutParts(shortcuts.full_screen, platform).join("+")}</span>}
        </button>

        <div className="nav-label">LIBRARY</div>
        <nav className="side-nav" aria-label="Screenshot library">
          <button className={filter === "all" && !categoryFilter && !query ? "nav-item active" : "nav-item"} onClick={() => { setFilter("all"); setQuery(""); setCategoryFilter(null); setSubcategoryFilter(null); }}>
            <Grid2X2 size={17} /><span>All screenshots</span>
          </button>
          <button className={filter === "recent" ? "nav-item active" : "nav-item"} onClick={() => { setQuery(""); setCategoryFilter(null); setSubcategoryFilter(null); setFilter("recent"); }}>
            <Clock3 size={17} /><span>Recent screenshots</span><span className="nav-count">{Math.min(items.length, 20)}</span>
          </button>
          <button className={filter === "attention" ? "nav-item active" : "nav-item"} onClick={() => { setCategoryFilter(null); setSubcategoryFilter(null); setFilter("attention"); }}>
            <AlertCircle size={17} /><span>Needs attention</span>{needsAttention > 0 && <span className="nav-count attention-count">{needsAttention}</span>}
          </button>
        </nav>

        <div className="nav-label topic-nav-label">FOLDERS</div>
        <nav className="side-nav topic-nav" aria-label="Topic folders">
          {SCREENSHOT_CATEGORIES.map((category) => <button key={category} className={categoryFilter === category ? "nav-item active" : "nav-item"} onClick={() => { setFilter("all"); setQuery(""); setCategoryFilter(category); setSubcategoryFilter(null); }}>
            <Folder size={16} /><span>{category}</span><span className="nav-count">{categoryCounts[category] ?? 0}</span>
          </button>)}
          <button className={categoryFilter === "Unfiled" ? "nav-item active" : "nav-item"} onClick={() => { setFilter("all"); setQuery(""); setCategoryFilter("Unfiled"); setSubcategoryFilter("__all__"); }}>
            <Folder size={16} /><span>Unfiled</span><span className="nav-count">{categoryCounts.Unfiled ?? 0}</span>
          </button>
        </nav>

        <div className="nav-label folder-label"><span>{folder && folder === managedCaptureFolder ? "MANAGED LIBRARY" : "IMPORT SOURCE"}</span><button className="tiny-add" onClick={() => void selectLibrary()} aria-label="Import and watch a screenshot folder" title="Import and watch a screenshot folder"><span>+</span></button></div>
        {folder ? (
          <div className="source-folder-card">
            <button className="folder-entry" title={folder} onClick={() => { setFilter("all"); setCategoryFilter(null); setSubcategoryFilter(null); setQuery(""); }}>
              <span className="source-folder-icon"><Folder size={17} /></span><span className="source-folder-copy"><strong>{folder === managedCaptureFolder ? "Screenshot Memory" : folder.split(/[\\/]/).filter(Boolean).slice(-1)[0] ?? "Selected folder"}</strong><small>{folder === managedCaptureFolder ? "Managed automatically on this device" : "Imported and watched locally"}</small></span>
            </button>
          </div>
        ) : <div className="folder-empty">Choose a folder to add screenshots to your library.</div>}

        <div className="sidebar-spacer" />
        <div className="privacy-card">
          <div className="privacy-icon"><ShieldCheck size={16} /></div>
          <div><strong>Stored on this device</strong><span>Screenshots and text stay local.</span></div>
        </div>
        <button className="nav-item settings-nav" onClick={() => setSettingsOpen(true)}><Settings2 size={17} /><span>Capture &amp; shortcuts</span></button>
      </aside>

      <main className="main-area">
        <header className="topbar">
          <div className="breadcrumb">
            <button className="breadcrumb-button" onClick={() => { setFilter("all"); setCategoryFilter(null); setSubcategoryFilter(null); setQuery(""); }}>All screenshots</button>
            {categoryFilter && <><span className="crumb-slash">/</span><button className={subcategoryFilter ? "breadcrumb-button" : "breadcrumb-current"} onClick={() => setSubcategoryFilter(null)}>{categoryFilter}</button></>}
            {subcategoryFilter && <><span className="crumb-slash">/</span><strong>{currentLocationTitle}</strong></>}
            {!categoryFilter && <><span className="crumb-slash">/</span><strong>{currentLocationTitle}</strong></>}
          </div>
          <div className="topbar-actions">
            <div className="local-indicator"><span className="online-dot" /> Local index</div>
            <button className="icon-button topbar-icon" aria-label="Privacy information" title="Screenshots and OCR text are stored locally"><LockKeyhole size={16} /></button>
          </div>
        </header>

        <div className="content-scroll">
          <section className="library-heading">
            <div className="library-heading-copy">
              <h1>{currentLocationTitle}</h1>
              <span>{currentLocationCount.toLocaleString()} {currentLocationCount === 1 ? "screenshot" : "screenshots"}</span>
            </div>
            <div className="heading-actions">
              <button className="button primary-button" onClick={() => void invoke("capture_screens_now").catch((cause) => setError(String(cause)))} title="Save a new screenshot to your local library"><ImagePlus size={15} /> Capture screenshot</button>
              <button className="button secondary-button" onClick={() => void selectLibrary()} title="Import an existing folder and watch it for new screenshots"><FolderOpen size={15} /> Import folder</button>
            </div>
          </section>

          <section className="search-panel" aria-label="Search screenshots">
            <Search size={19} className="search-icon" />
            <input ref={searchInputRef} onFocus={() => void searchWorker.current?.run({ type: "warm-search" })} value={query} onChange={(event) => setQuery(event.target.value)} placeholder={categoryFilter ? `Search in ${subcategoryFilter && subcategoryFilter !== "__all__" ? subcategoryFilter : categoryFilter}…` : "Search all screenshots…"} aria-label={categoryFilter ? `Search screenshots in ${subcategoryFilter && subcategoryFilter !== "__all__" ? subcategoryFilter : categoryFilter}` : "Search all screenshots by text or meaning"} />
            {query ? <button className="clear-search" onClick={() => setQuery("")} aria-label="Clear search"><X size={15} /></button> : <kbd>{platform === "macos" ? "Cmd K" : "Ctrl K"}</kbd>}
            <span className="search-divider" />
            <span className="search-scope">{categoryFilter ? "This folder" : "Entire library"}</span>
          </section>

          {!query && filter === "all" && !categoryFilter && <section className="directory-section root-directory" aria-label="Screenshot folders">
            <div className="directory-section-heading"><div><h2>Topic folders</h2><p>Your library, organized by subject</p></div><span>{SCREENSHOT_CATEGORIES.length + 1} folders</span></div>
            <div className="folder-grid">
              {SCREENSHOT_CATEGORIES.map((category) => <FolderTile key={category} name={category} description={CATEGORY_DESCRIPTIONS[category]} count={categoryCounts[category] ?? 0} tone={CATEGORY_TONES[category]} onOpen={() => { setCategoryFilter(category); setSubcategoryFilter(null); }} />)}
              <FolderTile name="Unfiled" description={CATEGORY_DESCRIPTIONS.Unfiled} count={categoryCounts.Unfiled ?? 0} tone={CATEGORY_TONES.Unfiled} onOpen={() => { setCategoryFilter("Unfiled"); setSubcategoryFilter("__all__"); }} />
            </div>
            {items.length === 0 && <div className="first-capture-card">
              <div className="first-capture-icon"><ImagePlus size={20} /></div>
              <div className="first-capture-copy"><span>READY WHEN YOU ARE</span><h3>Your private library is ready</h3><p>Capture with {shortcuts ? shortcutParts(shortcuts.full_screen, platform).map((part, index) => <span key={part}>{index > 0 && " "}<kbd>{part}</kbd></span>) : "the Capture button"}. Screenshots are stored on this device, read locally, and organized into topic folders for you.</p></div>
              <button className="button primary-button" onClick={() => void invoke("capture_screens_now").catch((cause) => setError(String(cause)))}><ImagePlus size={15} /> Capture screenshot</button>
            </div>}
          </section>}

          {!query && filter === "all" && categoryFilter && !subcategoryFilter && <section className="directory-section subdirectory" aria-label={`${categoryFilter} subfolders`}>
            <div className="directory-section-heading"><div><h2>Folders in {categoryFilter}</h2><p>Open a subfolder to narrow the screenshot list</p></div><span>{(CATEGORY_SUBCATEGORIES[categoryFilter] ?? []).length + 1} folders</span></div>
            <div className="folder-grid subfolder-grid">
              <FolderTile name={`All ${categoryFilter}`} description="Every screenshot in this topic" count={categoryCounts[categoryFilter] ?? 0} tone={CATEGORY_TONES[categoryFilter] ?? "teal"} compact onOpen={() => setSubcategoryFilter("__all__")} />
              {(CATEGORY_SUBCATEGORIES[categoryFilter] ?? []).map((subcategory, index) => <FolderTile key={subcategory} name={subcategory} description={`Inside ${categoryFilter}`} count={subcategoryCounts[subcategory] ?? 0} tone={["blue", "sage", "lavender", "sand"][index % 4]} compact onOpen={() => setSubcategoryFilter(subcategory)} />)}
            </div>
          </section>}

          {showScreenshotBrowser && <div className="screenshot-toolbar">
            <div className="screenshot-toolbar-title"><h2>{query ? "Search results" : filter === "attention" ? "Needs attention" : filter === "recent" ? "Recent captures" : "Screenshots"}</h2><span>{visibleItems.length}{filter === "recent" && items.length > 20 ? "+" : ""} {visibleItems.length === 1 ? "item" : "items"}</span></div>
            <div className="screenshot-toolbar-actions">
              <label className="sort-control"><ArrowDownWideNarrow size={15} /><select value={sortOrder} onChange={(event) => setSortOrder(event.target.value as typeof sortOrder)} aria-label="Sort screenshots">
                <option value="newest">Newest first</option><option value="oldest">Oldest first</option><option value="name">Name A–Z</option>
              </select></label>
              <div className="view-switch" role="group" aria-label="Screenshot view">
                <button className={viewMode === "grid" ? "active" : ""} onClick={() => setViewMode("grid")} aria-label="Grid view" aria-pressed={viewMode === "grid"} title="Grid view"><LayoutGrid size={16} /></button>
                <button className={viewMode === "list" ? "active" : ""} onClick={() => setViewMode("list")} aria-label="List view" aria-pressed={viewMode === "list"} title="List view"><List size={16} /></button>
              </div>
            </div>
          </div>}

          {(error || notice || recentImportCount > 0) && <div className={error ? "status-banner error-banner" : "status-banner"} role={error ? "alert" : "status"}>
            {error ? <AlertCircle size={16} /> : scanning || ocrBusy ? <LoaderCircle className="spin" size={16} /> : <Check size={16} />}
            <span>{error ?? notice ?? `${recentImportCount} new screenshot${recentImportCount === 1 ? "" : "s"} added to your library.`}{ocrBusy && <span className="progress-inline"><span style={{ width: `${Math.round(ocrProgress * 100)}%` }} /></span>}</span>
            {!error && recentImportCount > 0 && <button className="view-imported-button" onClick={() => { setFilter("recent"); setCategoryFilter(null); setSubcategoryFilter(null); setQuery(""); setSortOrder("newest"); setError(null); setNotice(null); setRecentImportCount(0); }}>View recent</button>}
            {error && <button onClick={() => setError(null)} aria-label="Dismiss error"><X size={15} /></button>}
            {!error && !scanning && !ocrBusy && <button onClick={() => { setNotice(null); setRecentImportCount(0); }} aria-label="Dismiss notice"><X size={15} /></button>}
          </div>}

          {loading ? <div className="loading-state"><LoaderCircle className="spin" size={25} /><span>Opening your local library…</span></div> : !folder ? (
            <div className="welcome-state">
              <div className="welcome-art"><div className="art-sheet sheet-back" /><div className="art-sheet sheet-mid" /><div className="art-sheet sheet-front"><FileImage size={30} /><span /><span /><span /></div><div className="art-spark spark-one"><Sparkles size={16} /></div><div className="art-spark spark-two"><Folder size={18} /></div></div>
              <h3>Start with a screenshot folder</h3><p>Choose a folder and Screenshot Memory will find image files, make local previews, and index their text for search.</p>
              <button className="button primary-button" onClick={() => void selectLibrary()}><FolderOpen size={15} /> Choose a folder</button>
              <div className="welcome-privacy"><ShieldCheck size={14} /> Images and recognized text stay on this device.</div>
            </div>
          ) : !query && filter === "all" && !categoryFilter ? null : visibleItems.length === 0 ? (
            <div className="empty-state"><div className="empty-icon">{query ? <Search size={22} /> : <ImagePlus size={23} />}</div><h3>{query ? "No matching screenshots" : filter === "attention" ? "You’re all caught up" : "No screenshots found yet"}</h3><p>{query ? "Try another word or clear the search to browse your library." : filter === "attention" ? "Missing files and indexing errors will show up here." : "Try another folder, or index this folder again after adding screenshots."}</p>{query && <button className="text-button" onClick={() => setQuery("")}>Clear search</button>}</div>
          ) : viewMode === "grid" ? (
            <div className="capture-grid" aria-label="Screenshot thumbnails">
              {visibleItems.map((item) => <article className={`capture-card ${selected?.id === item.id ? "selected-card" : ""}`} key={item.id}>
                <button className="capture-thumb" onClick={() => setSelected(item)} aria-label={`Preview ${item.file_name}`} title={item.file_name}>
                  {item.thumbnail ? <img src={thumbnailUrl(item.thumbnail)} alt="" loading="lazy" decoding="async" /> : <span className="capture-thumb-fallback"><FileImage size={28} /></span>}
                  {item.status !== "indexed" && <span className={`capture-status ${item.status}`} role="status">{item.status === "pending" ? <><LoaderCircle size={11} className="spin" /> Indexing</> : item.status === "missing" ? <><AlertCircle size={11} /> Missing</> : item.status === "unavailable" ? <><AlertCircle size={11} /> Folder offline</> : <><AlertCircle size={11} /> Needs attention</>}</span>}
                  <span className="capture-hover-hint">Quick Look</span>
                </button>
                <div className="capture-card-body">
                  <button className="capture-file-name" onClick={() => setSelected(item)} title={item.file_name}>{item.file_name}</button>
                  <div className="capture-file-meta"><span>{prettyDate(item.captured_at)}</span><i /><span>{formatBytes(item.file_size)}</span></div>
                  {(item.subcategory ?? item.category) && <span className="capture-category">{item.subcategory ?? item.category}</span>}
                  <div className="capture-card-actions"><button className="capture-open-original" onClick={() => void openScreenshot(item)} disabled={!originalReachable(item)} title="Open original screenshot" aria-label={`Open original ${item.file_name}`}><ArrowUpRight size={15} /></button></div>
                </div>
              </article>)}
            </div>
          ) : (
            <div className="file-list" role="list" aria-label="Screenshots">
              {visibleItems.map((item) => <article className={`file-row ${selected?.id === item.id ? "selected-card" : ""}`} key={item.id} role="listitem">
                <button className="file-open" onClick={() => setSelected(item)} aria-label={`Open details for ${item.file_name}`}>
                  <span className="file-row-thumb">{item.thumbnail ? <img src={thumbnailUrl(item.thumbnail)} alt="" loading="lazy" decoding="async" /> : <FileImage size={19} />}<span className={`status-indicator ${item.status}`} title={item.status} /></span>
                  <span className="file-main"><strong title={item.file_name}>{item.file_name}</strong><small>{prettyDate(item.captured_at)} <i /> {formatBytes(item.file_size)} <i /> {item.subcategory ?? item.category ?? "Unfiled"}</small>
                    {item.match_reason && <em><Sparkles size={11} /> {item.match_reason}</em>}
                    {item.ocr_text && <span className="ocr-snippet">{item.ocr_text.slice(0, 125).replace(/\s+/g, " ")}</span>}
                    {item.status === "error" && <em className="file-warning"><AlertCircle size={12} /> Text indexing failed</em>}
                    {item.status === "missing" && <em className="file-warning"><AlertCircle size={12} /> Original file is missing</em>}
                    {item.status === "unavailable" && <em className="file-warning"><AlertCircle size={12} /> Original's folder isn't connected</em>}
                    {item.status === "pending" && <em><LoaderCircle size={12} className="spin" /> Waiting for on-device text recognition</em>}
                  </span>
                </button>
                <div className="file-row-actions">
                  <button className="file-action" onClick={() => void openScreenshot(item)} disabled={!originalReachable(item)}><ArrowUpRight size={14} /> Open original</button>
                  {item.status === "error" && <button className="file-action retry-action" onClick={() => void retryIndexing(item)}><RotateCw size={13} /> Retry scan</button>}
                </div>
              </article>)}
            </div>
          )}

          {items.length > 0 && showScreenshotBrowser && <div className="library-footnote"><LockKeyhole size={13} /><span>Indexed locally on this device</span><span className="footnote-dot">·</span><span>{items.length} screenshot{items.length === 1 ? "" : "s"}</span></div>}
        </div>
      </main>

      {selected && <div className="detail-backdrop" onClick={() => setSelected(null)}>
        <aside className="detail-panel" onClick={(event) => event.stopPropagation()}>
          <div className="detail-top"><div><span className="detail-overline">QUICK LOOK</span><h2 title={selected.file_name}>{selected.file_name}</h2></div><button className="icon-button" onClick={() => setSelected(null)} aria-label="Close preview" title="Close preview (Esc)"><X size={17} /></button></div>
          <div className="detail-image">{detailImage ? <img src={detailImage} alt={`Preview of ${selected.file_name}`} /> : detailImageBusy ? <LoaderCircle size={25} className="spin" aria-label="Loading full resolution preview" /> : <FileImage size={34} />}{detailImageError && <span className="detail-image-error">{detailImageError}</span>}</div>
          <div className="detail-toolbar">
            <div className="detail-image-actions">
            <button className={`button secondary-button ${shareFeedback === "copy" ? "action-success" : ""}`} onClick={() => void copyScreenshotImage(selected)} disabled={shareAction !== null || !originalReachable(selected)} title="Copy the full-resolution image to the clipboard">
              {shareAction === "copy" ? <LoaderCircle size={14} className="spin" /> : shareFeedback === "copy" ? <Check size={14} /> : <Copy size={14} />}{shareAction === "copy" ? "Copying..." : shareFeedback === "copy" ? "Copied" : "Copy image"}
            </button>
            <button className={`button secondary-button ${shareFeedback === "save" ? "action-success" : ""}`} onClick={() => void saveScreenshotCopy(selected)} disabled={shareAction !== null || !originalReachable(selected)} title="Save a copy of the original image">
              {shareAction === "save" ? <LoaderCircle size={14} className="spin" /> : shareFeedback === "save" ? <Check size={14} /> : <Download size={14} />}{shareAction === "save" ? "Saving..." : shareFeedback === "save" ? "Saved" : "Save a copy"}
            </button>
            </div>
            <div className="detail-preview-nav"><button className="text-button" disabled={visibleItems.length < 2} onClick={() => { const index = visibleItems.findIndex((item) => item.id === selected.id); setSelected(visibleItems[(index - 1 + visibleItems.length) % visibleItems.length]); }} aria-label="Previous screenshot"><ChevronLeft size={15} /></button><span>{Math.max(visibleItems.findIndex((item) => item.id === selected.id) + 1, 1)} <i>/</i> {visibleItems.length}</span><button className="text-button" disabled={visibleItems.length < 2} onClick={() => { const index = visibleItems.findIndex((item) => item.id === selected.id); setSelected(visibleItems[(index + 1) % visibleItems.length]); }} aria-label="Next screenshot"><ChevronRight size={15} /></button></div>
          </div>
          <div className="detail-meta"><div><span>Captured</span><strong>{prettyDate(selected.captured_at)}</strong></div><div><span>File size</span><strong>{formatBytes(selected.file_size)}</strong></div><div><span>Index status</span><strong className={`detail-status ${selected.status}`}>{selected.status === "indexed" ? "Text searchable" : selected.status === "pending" ? "Waiting for OCR" : selected.status === "missing" ? "Original missing" : selected.status === "unavailable" ? "Folder not connected" : "Needs retry"}</strong></div></div>
          <div className="detail-category"><span>CATEGORY</span><CategoryPicker screenshot={selected} onChange={(screenshot, category) => void updateCategory(screenshot, category)} /></div>
          {selected.ocr_text && <details className="detail-ocr"><summary><span>Recognized text</span><ChevronDown size={15} /></summary><div className="detail-ocr-content"><p>{selected.ocr_text}</p><button className="text-button" onClick={() => void navigator.clipboard.writeText(selected.ocr_text)}>Copy text</button></div></details>}
          {selected.last_error && <div className="detail-error"><AlertCircle size={15} /><span>{selected.last_error}</span></div>}
          <div className="detail-location"><Folder size={15} /><span>{selected.path}</span></div>
          <div className="detail-actions"><button className="button primary-button" onClick={() => void openScreenshot(selected)} disabled={!originalReachable(selected)}><ArrowUpRight size={15} /> Open original</button>{selected.status === "missing" && <button className="button secondary-button" onClick={() => void removeMissingScreenshot(selected)} title="The original was deleted or moved. Remove its preview and search text."><X size={14} /> Remove from library</button>}{selected.status === "error" && <button className="button secondary-button" onClick={() => void retryIndexing(selected)}><RotateCw size={14} /> Retry indexing</button>}</div>
          <div className="detail-privacy"><ShieldCheck size={14} /> Original image is never changed by indexing.</div>
        </aside>
      </div>}
      {settingsOpen && <div className="settings-backdrop" onClick={() => setSettingsOpen(false)}><section className="settings-dialog" onClick={(event) => event.stopPropagation()}>
        <div className="settings-heading"><div><span className="detail-overline">PREFERENCES</span><h2>Library settings</h2></div><button className="icon-button" onClick={() => setSettingsOpen(false)} aria-label="Close settings"><X size={17} /></button></div>
        <div className="settings-section monitor-settings">
          <h3>Automatic importing</h3>
          <p>{platform === "windows"
            ? "Screenshot Memory manages its private library automatically. Windows Snipping Tool captures are watched by default; add another folder only if you want screenshots from a different location. Originals stay in place."
            : "Screenshot Memory manages its private library automatically. Add a folder to import the screenshots saved there and any new ones. Originals stay in place."}{" "}Screenshots from watched folders are not copied: the library reads them where they are.</p>
          <button className="button secondary-button monitor-add" onClick={() => void addMonitoredFolder()} disabled={monitorBusy}>
            {monitorBusy ? <LoaderCircle size={14} className="spin" /> : <FolderOpen size={14} />}
            {monitorBusy ? "Adding folder..." : "Add folder to watch"}
          </button>
          {monitoredFolders.length > 0 ? <div className="monitored-folder-list">
            {monitoredFolders.map((path) => <div className="monitored-folder" key={path} title={path}>
              <Folder size={15} /><span>{path.split(/[\\/]/).filter(Boolean).slice(-1)[0] ?? path}</span>
              {folderStatus[path] === false && <small className="folder-offline" title="Not reachable now. Nothing is removed; it resumes when the folder is back.">Not connected</small>}
              <button className="icon-button" onClick={() => void removeMonitoredFolder(path)} aria-label={`Stop monitoring ${path}`} title="Stop monitoring"><X size={14} /></button>
            </div>)}
          </div> : <div className="monitor-empty">No monitored folders yet.</div>}
        </div>
        <div className="settings-section"><h3>Storage</h3><p>Each screenshot from a watched folder adds only a small preview and its search data. Deleting the original removes it from the library.</p>
          {storage ? <dl className="storage-list">
            <div><dt>Captured in Screenshot Memory</dt><dd>{storage.library_files ? `${storage.library_files} file${storage.library_files === 1 ? "" : "s"} · ${formatBytes(storage.library_bytes)}` : "None yet"}</dd></div>
            <div><dt>From watched folders</dt><dd>{storage.referenced_files} file{storage.referenced_files === 1 ? "" : "s"} · no extra copy</dd></div>
            <div><dt>Previews and search data</dt><dd>{formatBytes(storage.thumbnail_bytes + storage.database_bytes)}</dd></div>
          </dl> : <div className="monitor-empty">Checking storage…</div>}
          <button className="text-button storage-recheck" onClick={() => void recheckAllFiles()} disabled={recheckBusy} title="Read every watched file again. Finds edits that kept the same size and date.">{recheckBusy ? "Re-checking…" : "Re-check all files"}</button>
        </div>
        <div className="settings-section"><h3>Full screen capture</h3><p>Choose which display {fullScreenLabel ?? "the full-screen shortcut"} captures.</p>
          <label className="setting-choice"><input type="radio" name="capture-mode" checked={captureMode === "active"} onChange={() => { setCaptureMode("active"); void invoke("set_capture_display_mode", { mode: "active" }).catch((cause) => setError(String(cause))); }} /><span><strong>Active display</strong><small>Capture the display under your pointer.</small></span></label>
          <label className="setting-choice"><input type="radio" name="capture-mode" checked={captureMode === "all"} onChange={() => { setCaptureMode("all"); void invoke("set_capture_display_mode", { mode: "all" }).catch((cause) => setError(String(cause))); }} /><span><strong>All displays</strong><small>Save one screenshot for each connected display.</small></span></label>
        </div>
        <div className="settings-section"><h3>Run in background</h3><p>Keep capture shortcuts active after the window closes and when you sign in.</p>
          <label className="setting-toggle"><span><strong>Start at sign in</strong><small>Screenshot Memory stays available in the tray.</small></span><input type="checkbox" checked={autostart} onChange={(event) => { const value = event.currentTarget.checked; setAutostart(value); void invoke("set_autostart_enabled", { enabled: value }).catch((cause) => { setAutostart(!value); setError(String(cause)); }); }} /></label>
        </div>
        <div className="settings-section"><h3>Global shortcuts</h3><p>These work in any app while Screenshot Memory is running. Select a shortcut, then press the new keys.</p>
          {shortcuts && <>
            <ShortcutRecorder label="Full screen" value={shortcuts.full_screen} platform={platform} disabled={shortcutBusy} onChange={(value) => void updateShortcuts(value, shortcuts.region)} />
            <ShortcutRecorder label="Select region" value={shortcuts.region} platform={platform} disabled={shortcutBusy} onChange={(value) => void updateShortcuts(shortcuts.full_screen, value)} />
            {(shortcuts.full_screen !== shortcuts.default_full_screen || shortcuts.region !== shortcuts.default_region) && <button className="text-button shortcut-reset" disabled={shortcutBusy} onClick={() => void updateShortcuts(shortcuts.default_full_screen, shortcuts.default_region)}>Restore defaults</button>}
          </>}
          {captureStatus && <small className="shortcut-status">{captureStatus}</small>}
        </div>
      </section></div>}
    </div>
  );
}

function App() {
  return new URLSearchParams(window.location.search).get("capture") === "region" ? <RegionCapture /> : <LibraryApp />;
}

export default App;
