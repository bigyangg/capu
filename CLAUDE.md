# Claude Code Instructions

## Project overview

Screenshot Memory is a Tauri 2 desktop application with a React/TypeScript frontend and Rust native services. It captures screenshots, imports images into a managed local library, performs local OCR, text embedding, and CLIP image embedding for visual search, and stores/searches metadata in SQLite with FTS5.

Read `README.md` for setup, user workflows, privacy behavior, and current platform status. Read `THIRD_PARTY_NOTICES.md` before changing dependencies or redistributing bundled assets.

## Architecture map

- `src/App.tsx`: Main library UI, Tauri command calls, search state, capture and import flows, category presentation, worker lifecycle.
- `src/App.css`: Frontend layout and visual styles.
- `src/ocr.worker.ts`: Tesseract OCR, MiniLM text embedding, and CLIP image/query embedding work. Keep CPU-heavy OCR/model work outside the UI thread.
- `src/modelWorkers.ts`: Starts model workers on demand and terminates them when idle (indexing after 15 s, search after 3 min), because WebAssembly memory is only returned when a worker ends.
- `src-tauri/src/lib.rs`: Tauri plugins and setup, app state, command registration, default Windows screenshot-folder setup, tray behavior, global shortcuts, autostart.
- `src-tauri/src/capture.rs`: Native display capture and region selection.
- `src-tauri/src/folder_watch.rs`: Monitored-folder events (create/modify/remove), debouncing/retries, and import/removal notifications.
- `src-tauri/src/library.rs`: SQLite schema and migrations, FTS5 search, indexing metadata, thumbnails, imports, duplicate detection, category rules, and persistence.
- `src-tauri/capabilities/`: Tauri window permissions. Keep capabilities narrow and aligned with actual command/plugin needs.
- `scripts/copy-ocr-assets.mjs`: Copies local OCR and ONNX assets into the frontend's public assets before dev/build.
- `scripts/fetch-vision-model.mjs`: Downloads the pinned CLIP model into `public/models/` at build time and verifies each file by SHA-256. The app never downloads models at runtime.

## Product requirements to preserve

1. **Local and private by default.** Screenshot bytes, OCR text, and embeddings stay on the device. Do not add remote OCR, analytics that include screenshot content, model CDN access, or cloud processing.
2. **Preserve originals.** Capture files belong in the managed library. Screenshots from watched folders are indexed by reference: the library records the original's path and keeps only its thumbnail, text, and vectors, so no second copy is stored. Imports detect duplicates using hashes and leave source files untouched. Online-only cloud placeholders are not read during import, so syncing a folder never downloads it. When an original is deleted from a watched folder, only its library entry and derived data are removed. Never move, rename, or delete user originals as part of import or indexing.
3. **Reliable automatic import.** Monitored folder events may arrive while a screenshot is still being written. Preserve the existing debounce/retry behavior and avoid indexing incomplete files. Notify the UI through the established Tauri events when imports or captures change the library.
4. **Keep slow work off the UI thread.** OCR and embedding run in the web worker; filesystem and SQLite work run in Rust commands/tasks. Avoid adding blocking work to React rendering or event handlers.
5. **Safe persistence and paths.** Use parameterized SQLite queries. Validate caller-supplied paths against user-selected monitored roots and the managed library. Keep Tauri permissions scoped to the minimum necessary.
6. **Keep category definitions aligned.** Category/subcategory names and validation exist in the Rust library and frontend presentation. Update both sides together when changing the taxonomy.
7. **Do not overstate platform support.** Windows is the current development target. macOS has not been built and end-to-end tested. Account for OS screen-recording permissions, shortcut behavior, filesystem differences, and packaging before claiming support.
8. Do not add billing, authentication, cloud sync, or AI categorization unless the user explicitly asks for them.

## Common commands

```sh
npm install
npm run tauri dev
npm run build
npm run tauri -- build
npm run tauri -- build --no-bundle
```

`npm run build` runs the prebuild hooks (CLIP model fetch/verify, which needs network once, and asset copy), TypeScript compiler, and Vite production build. `npm run tauri -- build` builds/packages the native app for the current host platform. A frontend build alone does not verify native Tauri behavior or another operating system.

Use the package manager and scripts already in `package.json`. Do not introduce a dependency when the existing Tauri, React, Rust, SQLite, Tesseract.js, Transformers.js, or Lucide facilities can do the work.

## Implementation guidance

- Inspect the current command/type/schema before changing interfaces. Keep Tauri command payloads and frontend TypeScript types synchronized.
- Register new native commands in `src-tauri/src/lib.rs` and add only the permissions required in the relevant capability files.
- Make schema changes additive and migration-safe for existing local databases.
- Preserve offline model paths and local asset staging. Do not silently fall back to network downloads.
- Keep the managed capture directory, database, and thumbnail cache within Tauri's app data directory.
- Keep UI controls honest: do not add buttons that have no implemented behavior.
- Prefer accessible names, keyboard focus styles, restrained motion, and responsive layouts in the UI.
- Keep notices for new bundled or redistributed third-party assets accurate in `THIRD_PARTY_NOTICES.md`.

## Verification and reporting

- Run the checks relevant to the change. For frontend/type changes, use `npm run build`; for native Rust changes, run the repository's available Cargo/Tauri check or build for the current host when practical.
- Do not describe a command as passing unless it was run in this working tree during the task.
- Distinguish frontend build validation from native app build and manual end-to-end validation.
- Report platform-specific gaps plainly. Do not claim Windows/macOS compatibility, store readiness, signing, or distribution unless each relevant step has actually been completed.
