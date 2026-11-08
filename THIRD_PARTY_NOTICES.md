# Third-party notices

- Tesseract.js 7 and Tesseract.js Core 7 are distributed under Apache License 2.0. Their notices are copied to `public/ocr-notices/` before a dev run or build.
- The English traineddata is distributed by `@tesseract.js-data/eng` 1.0.0 under MIT, from the [naptha/tessdata project](https://github.com/naptha/tessdata).
- XCap 0.9.8 is distributed under Apache License 2.0.
- On Windows, `windows` 0.62 (Microsoft's Rust bindings, used for the built-in Windows OCR) is distributed under MIT or Apache License 2.0.
- On macOS, `objc2` 0.6 and `objc2-foundation` 0.3 are distributed under MIT, and `objc2-vision` 0.3 (bindings for Apple's Vision framework, used for OCR) under Zlib, Apache License 2.0, or MIT.
- `mouse_position` 0.1.4 is distributed under MIT.
- Tauri plugins `tauri-plugin-autostart` 2.7 and `tauri-plugin-global-shortcut` 2.4 are distributed under Apache License 2.0 or MIT.
- `notify` 8.2.0 is distributed under CC0-1.0; its `notify-types` dependency is distributed under MIT or Apache License 2.0.
- Transformers.js 3.8 and ONNX Runtime Web are distributed under Apache License 2.0 and MIT, respectively.
- `Xenova/all-MiniLM-L6-v2` is distributed under Apache License 2.0. Its quantized ONNX weights and tokenizer are bundled locally under `public/models/`.
- `Xenova/clip-vit-base-patch32`, an ONNX conversion of OpenAI's CLIP ViT-B/32, is distributed under MIT. Its quantized ONNX weights, tokenizer, and configuration are downloaded at build time from a pinned revision, verified by SHA-256, and bundled locally under `public/models/` for visual search.
