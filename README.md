# Screenshot Memory

Screenshot Memory is a local-first desktop app for capturing, organizing, and finding screenshots. It keeps a managed library on the device, indexes screenshot text locally, and lets you search by words you remember.

## How it works

- **Capture in the app:** `Ctrl + Shift + S` captures the active display (or all displays, depending on Capture settings). `Ctrl + Shift + R` opens region selection. Captures are saved directly into the app's managed library.
- **Import automatically:** On Windows, the app watches the standard `Pictures/Screenshots` folder used by Snipping Tool. New image files are copied into the managed library as they appear. If Snipping Tool is configured to save somewhere else, add that folder in the app's library settings.
- **Import another folder:** Choose **Import folder** to add a folder to the watch list. Existing supported images are copied into the library, and new files are watched afterward. Source files stay where they are.
- **Find screenshots:** Local OCR extracts text in the background. Search combines full-text search with a locally generated text embedding to help find related wording. The app can suggest categories and subcategories from OCR text; suggestions can be changed in the UI.

The app does not need an existing screenshot folder to start. You can use the managed capture library immediately and add import folders when you want.

## Privacy and storage

Screenshot files, OCR, search, category suggestions, and semantic embeddings are processed on the device. OCR and embedding model assets are bundled with the app; runtime model downloads and remote OCR services are disabled. Screenshot contents and recognized text are not uploaded.

The app stores its local database, managed captures, and thumbnail cache under the operating system's Tauri application data directory:

```text
<Tauri application data directory>/
|-- screenshot-memory.sqlite3
|-- captures/
`-- thumbnails/
```

The exact parent directory is chosen by the operating system and application identifier. Imported screenshots are copied and deduplicated by file hash. Original files are not moved, renamed, or edited. Removing a watched folder stops future imports; it does not remove source files.

## Search and organization

Supported image types: PNG, JPG/JPEG, WebP, BMP, GIF (first frame), and TIFF.

OCR currently uses the bundled English language data. Categories include Projects, Finance, Meetings, Development, and Reference, with supported subcategories defined by the application. Category suggestions are lightweight rules based on recognized text, so they can miss content or suggest the wrong folder; users can correct them.

Search is based on recognized text and local text embeddings. It does not visually understand an image, recover words OCR missed, or perform image similarity search. A screenshot with little or no readable text may have limited search results.

## Keyboard shortcuts

| Shortcut | Action |
| --- | --- |
| `Ctrl + Shift + S` | Capture a display |
| `Ctrl + Shift + R` | Select and capture a region |
| `Escape` | Cancel region selection |

Global shortcuts operate while the app is running, including when its window is hidden to the system tray. Choose **Quit** from the tray menu to stop the background process. Launch at sign-in is configurable in Capture settings where supported.

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

The predev and prebuild steps run `scripts/copy-ocr-assets.mjs`, which stages the Tesseract worker, OCR engine files, English language data, and ONNX WebAssembly runtime for local use. The MiniLM model is kept under `public/models/` and is also used locally. Do not remove these assets or configure runtime CDN/model access without revisiting the offline privacy requirement.

## Platform status

Windows is the current development target. The project contains Windows screenshot-folder monitoring and Tauri packaging configuration. macOS has not yet been built and tested end to end; screen capture on macOS also requires the user to grant the relevant Screen Recording permission. Do not describe either platform as store-ready or claim platform compatibility beyond builds and workflows that have actually been verified.

App Store / Microsoft Store submission, signing, notarization, and release distribution are not configured by this repository's current Tauri settings.

## Project map

| Path | Purpose |
| --- | --- |
| `src/App.tsx` | Main React UI, library state, commands, OCR worker coordination |
| `src/App.css` | Application and gallery styling |
| `src/ocr.worker.ts` | Local OCR and text-embedding work outside the UI thread |
| `src-tauri/src/lib.rs` | Tauri setup, commands, shortcuts, tray, autostart, app state |
| `src-tauri/src/capture.rs` | Native full-display and region capture |
| `src-tauri/src/folder_watch.rs` | Monitored-folder events and automatic imports |
| `src-tauri/src/library.rs` | SQLite, file imports, thumbnails, OCR metadata, search and categories |
| `scripts/copy-ocr-assets.mjs` | Stage local OCR and ONNX assets for development/build |
| `THIRD_PARTY_NOTICES.md` | Third-party components and licenses |

See [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md) for dependency notices.
