import { copyFile, mkdir, readdir } from "node:fs/promises";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const publicDir = join(root, "public");
const tesseractDir = join(publicDir, "tesseract");
const coreDir = join(publicDir, "tesseract-core");
const tessdataDir = join(publicDir, "tessdata");
const noticesDir = join(publicDir, "ocr-notices");
const onnxDir = join(publicDir, "onnxruntime");
await Promise.all([mkdir(tesseractDir, { recursive: true }), mkdir(coreDir, { recursive: true }), mkdir(tessdataDir, { recursive: true }), mkdir(noticesDir, { recursive: true }), mkdir(onnxDir, { recursive: true })]);

const packages = join(root, "node_modules");
await copyFile(join(packages, "tesseract.js", "dist", "worker.min.js"), join(tesseractDir, "worker.min.js"));
await copyFile(join(packages, "tesseract.js", "dist", "worker.min.js.LICENSE.txt"), join(noticesDir, "tesseract-worker-notice.txt"));
const coreSource = join(packages, "tesseract.js-core");
const coreFiles = (await readdir(coreSource)).filter((name) => name.startsWith("tesseract-core") && (name.endsWith(".wasm.js") || name.endsWith(".wasm")));
await Promise.all(coreFiles.map((name) => copyFile(join(coreSource, name), join(coreDir, name))));
await copyFile(join(coreSource, "LICENSE"), join(noticesDir, "tesseract-core-LICENSE"));

const modelSource = join(packages, "@tesseract.js-data", "eng", "4.0.0", "eng.traineddata.gz");
await copyFile(modelSource, join(tessdataDir, "eng.traineddata.gz"));
const onnxWasmSource = join(packages, "onnxruntime-web", "dist", "ort-wasm-simd-threaded.wasm");
const onnxJsepWasmSource = join(packages, "onnxruntime-web", "dist", "ort-wasm-simd-threaded.jsep.wasm");
const onnxJsepModuleSource = join(packages, "onnxruntime-web", "dist", "ort-wasm-simd-threaded.jsep.mjs");
await Promise.all([
  copyFile(onnxWasmSource, join(onnxDir, "ort-wasm-simd-threaded.wasm")),
  copyFile(onnxJsepWasmSource, join(onnxDir, "ort-wasm-simd-threaded.jsep.wasm")),
  copyFile(onnxJsepModuleSource, join(onnxDir, "ort-wasm-simd-threaded.jsep.mjs")),
]);
console.log(`Prepared local OCR assets and the offline ONNX runtime.`);
