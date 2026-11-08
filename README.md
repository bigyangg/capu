# Screenshot Memory

Screenshot Memory is a local-first desktop app for capturing, organizing, and finding screenshots. It keeps a managed library on the device, indexes screenshot text and images locally, and lets you search by words you remember or by describing what a screenshot shows.

## How it works

- **Capture in the app:** The full-screen shortcut captures the active display (or all displays, depending on Capture settings). The region shortcut opens region selection. Both shortcuts can be changed in Capture settings. Captures are saved directly into the app's managed library.
- **Import automatically:** On Windows, the app watches the standard `Pictures/Screenshots` folder used by Snipping Tool. New image files are added to the managed library as they appear. If Snipping Tool is configured to save somewhere else, add that folder in the app's library settings.
- **Import another folder:** Choose **Import folder** to add a folder to the watch list. Existing supported images are added to the library, and new files are watched afterward. Source files stay where they are.
- **Find screenshots:** Local OCR extracts text in the background. Search combines full-text search, a locally generated text embedding for related wording, and a local CLIP image embedding so you can describe what a screenshot shows. The app can suggest categories and subcategories from OCR text; suggestions can be changed in the UI.

The app does not need an existing screenshot folder to start. You can use the managed capture library immediately and add import folders when you want.

## Privacy and storage

Screenshot files, OCR, search, category suggestions, and text and image embeddings are processed on the device. OCR and embedding model assets are bundled with the app; runtime model downloads and remote OCR services are disabled. Screenshot contents and recognized text are not uploaded.

The app stores its local database, managed captures, and thumbnail cache under the operating system's Tauri application data directory:

```text
<Tauri application data directory>/
|-- screenshot-memory.sqlite3
|-- captures/
`-- thumbnails/
```

The exact parent directory is chosen by the operating system and application identifier. `captures/` holds only screenshots taken in the app. Screenshots from watched folders are not copied: the library records where each original is and keeps a small thumbnail (about 15 KB) plus its recognized text and search vectors, so each picture is stored once. Imported screenshots are deduplicated by file hash, and unchanged files are skipped on later syncs without being read again. Files that are online-only in OneDrive or another cloud client are not downloaded during import; they are added once they are on the device. Original files are not moved, renamed, or edited. Settings shows how much space the library uses.

Earlier versions kept a hard link or copy of each imported screenshot in `captures/`. At startup, each one whose original is still present and identical is switched to point at the original, and the app's duplicate is deleted. If an original is deleted from a watched folder, its library entry, thumbnail, search text, and embedding are removed too. This happens immediately while the app runs, or at the next launch. Renamed or moved originals keep their library entry, and an original edited in place is re-read under the same entry, keeping any category chosen by hand. Nothing is removed while a watched folder itself is unavailable, for example on an unplugged drive or an offline network share: its screenshots stay searchable, are marked as not connected, and Settings shows the folder as not connected. The app checks every 15 seconds and resumes watching and syncing once the folder is back. A subfolder that cannot be read is skipped without removing anything. A screenshot whose original was deleted while its folder is still reachable can be removed from the detail panel.

Folder syncs skip files whose size and change time are unchanged. On macOS that change time cannot be set back by programs, so every edit is noticed. On Windows, an edit made while the app is closed that keeps both the file size and the modified time is not noticed until **Re-check all files** in Settings, which reads every watched file again. Edits made while the app is running are always read in full. Removing a watched folder stops future imports; it does not remove source files or imported screenshots.

## Search and organization

Supported image types: PNG, JPG/JPEG, WebP, BMP, GIF (first frame), and TIFF.

OCR uses the operating system's built-in, on-device engine: Windows OCR (`Windows.Media.Ocr`) for the languages in Windows language settings, and Apple's Vision framework on macOS (automatic language detection on macOS 13 and later). On Windows it reads a typical screenshot in about 0.2 seconds. If no system engine is available, for example on Windows without an OCR-capable language installed, the bundled Tesseract with English language data is used instead. Native OCR on macOS is compiled in CI but has not yet been tested on a Mac. Categories include Projects, Finance, Meetings, Development, and Reference, with supported subcategories defined by the application. Category suggestions are lightweight rules based on recognized text, so they can miss content or suggest the wrong folder; users can correct them.

Search combines three local signals: exact words from recognized text, related wording from a text embedding (MiniLM), and visual similarity from a CLIP image embedding. With the visual signal, a description such as "night moon" can find a photo of the moon even when the image has no readable text. Visual matches are labeled "Visual match". CLIP is a general-purpose model, so visual search works best for clear subjects and scenes and is less reliable for fine details. It does not write captions or recover words OCR missed. Each new screenshot is indexed for visual search in the background, in a separate worker from OCR.

## Keyboard shortcuts

| Shortcut | Action |
| --- | --- |
| `Alt + Shift + S` on Windows, `Control + Shift + S` on macOS | Capture a display |
| `Alt + Shift + R` on Windows, `Control + Shift + R` on macOS | Select and capture a region |
| `Escape` | Cancel region selection |

Global shortcuts operate while the app is running, including when its window is hidden to the system tray. Choose **Quit** from the tray menu to stop the background process. These are the defaults; change them in Capture settings. Windows defaults avoid `Ctrl + Shift + S` and `Ctrl + Shift + R`, which many apps use for Save As and reload. A shortcut held by another app cannot be registered, and the previous shortcut stays active.

Launch at sign-in is turned on once, at the first launch of a release build, and can be switched off in Capture settings; the app does not turn it back on.

## Development

### Requirements

- Node.js and npm
- Rust toolchain
- Tauri 2 prerequisites for the target operating system (including its platform SDK and native build tools)

Install dependencies and run the desktop app in development mode:

```sh
npm install
npm run tauri dev
```

To run a development build beside an installed copy without sharing its library, give it its own data folder and WebView2 profile. In PowerShell:

```powershell
$env:SCREENSHOT_MEMORY_DEV_DATA = "$env:TEMP\screenshot-memory-dev"
$env:WEBVIEW2_USER_DATA_FOLDER = "$env:TEMP\screenshot-memory-dev\webview"
npm run tauri dev
```

With `SCREENSHOT_MEMORY_DEV_DATA` set, the default Snipping Tool folder is not watched; put test images in its `captures` folder. Release builds ignore the variable.

Build the frontend and TypeScript:

```sh
npm run build
```

Build native Tauri installers/bundles for the current platform:

```sh
npm run tauri -- build
```

Build the native executable without packaging it:

```sh
npm run tauri -- build --no-bundle
```

The predev and prebuild steps run `scripts/copy-ocr-assets.mjs`, which stages the Tesseract worker, OCR engine files, English language data, and ONNX WebAssembly runtime for local use. The MiniLM model is kept under `public/models/` and is also used locally. They first run `scripts/fetch-vision-model.mjs`, which downloads the CLIP visual search model (about 150 MB) from a pinned Hugging Face revision into `public/models/` and verifies each file's SHA-256 hash. This needs network access once; later builds reuse the verified files. The app itself never downloads models. Do not remove these assets or configure runtime CDN/model access without revisiting the offline privacy requirement.

## Platform status

Windows is the current development target. The project contains Windows screenshot-folder monitoring and Tauri packaging configuration. macOS has not yet been built and tested end to end; screen capture on macOS also requires the user to grant the relevant Screen Recording permission. Do not describe either platform as store-ready or claim platform compatibility beyond builds and workflows that have actually been verified.

App Store / Microsoft Store submission, signing, notarization, and release distribution are not configured by this repository's current Tauri settings.

## Project map

| Path | Purpose |
| --- | --- |
| `src/App.tsx` | Main React UI, library state, commands, OCR worker coordination |
| `src/App.css` | Application and gallery styling |
| `src/ocr.worker.ts` | Local OCR, text-embedding, and CLIP visual-embedding work outside the UI thread |
| `src-tauri/src/lib.rs` | Tauri setup, commands, shortcuts, tray, autostart, app state |
| `src-tauri/src/capture.rs` | Native full-display and region capture |
| `src-tauri/src/folder_watch.rs` | Monitored-folder events and automatic imports |
| `src-tauri/src/library.rs` | SQLite, file imports, deleted-original cleanup, thumbnails, OCR metadata, text/visual search and categories |
| `scripts/copy-ocr-assets.mjs` | Stage local OCR and ONNX assets for development/build |
| `scripts/fetch-vision-model.mjs` | Download and SHA-256-verify the pinned CLIP model at build time |
| `THIRD_PARTY_NOTICES.md` | Third-party components and licenses |

See [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md) for dependency notices.
