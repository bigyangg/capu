import { env, pipeline, type FeatureExtractionPipelineType } from "@huggingface/transformers";
import { createWorker } from "tesseract.js";

type OcrRequest = { type: "ocr"; id: number; imageDataUrl: string };
type EmbeddingRequest = { type: "embed"; id: number; text: string };
type WorkerRequest = OcrRequest | EmbeddingRequest;
type WorkerScope = {
  location: Location;
  postMessage: (message: unknown) => void;
  onmessage: ((event: MessageEvent<WorkerRequest>) => void) | null;
};

const scope = self as unknown as WorkerScope;
let ocrWorker: Awaited<ReturnType<typeof createWorker>> | null = null;
let semanticModel: FeatureExtractionPipelineType | null = null;
let activeId = 0;

function localAsset(path: string) {
  return new URL(`../${path}`, scope.location.href).toString();
}

async function getWorker() {
  if (!ocrWorker) {
    ocrWorker = await createWorker("eng", 1, {
      workerPath: localAsset("tesseract/worker.min.js"),
      corePath: localAsset("tesseract-core/"),
      langPath: localAsset("tessdata/"),
      logger: (progress) => {
        if (activeId > 0 && typeof progress.progress === "number") {
          scope.postMessage({ type: "progress", id: activeId, progress: progress.progress, status: progress.status });
        }
      },
    });
  }
  return ocrWorker;
}

async function getSemanticModel() {
  if (!semanticModel) {
    // All weights and WebAssembly files ship with the app; no model or runtime CDN is allowed.
    env.allowRemoteModels = false;
    env.allowLocalModels = true;
    env.localModelPath = localAsset("models/");
    const onnxConfig = env.backends.onnx as unknown as { wasm?: Record<string, unknown> };
    onnxConfig.wasm ??= {};
    onnxConfig.wasm.wasmPaths = localAsset("onnxruntime/");
    onnxConfig.wasm.numThreads = 1;
    const createFeaturePipeline = pipeline as unknown as (
      task: "feature-extraction",
      model: string,
      options: { device: "wasm"; dtype: "q8" },
    ) => Promise<FeatureExtractionPipelineType>;
    semanticModel = await createFeaturePipeline("feature-extraction", "Xenova/all-MiniLM-L6-v2", { device: "wasm", dtype: "q8" });
  }
  return semanticModel;
}

async function embedText(text: string) {
  const model = await getSemanticModel();
  const result = await model(text.slice(0, 6000), { pooling: "mean", normalize: true });
  return Array.from(result.data as Float32Array);
}

async function imageTiles(imageDataUrl: string) {
  const response = await fetch(imageDataUrl);
  const bitmap = await createImageBitmap(await response.blob());
  const width = bitmap.width;
  const height = bitmap.height;
  const tileWidth = Math.min(1500, width);
  const tileHeight = Math.min(1100, height);
  const overlap = 180;
  const stepX = Math.max(1, tileWidth - overlap);
  const stepY = Math.max(1, tileHeight - overlap);
  const tiles: string[] = [];
  for (let y = 0; y < height; y += stepY) {
    for (let x = 0; x < width; x += stepX) {
      const cropWidth = Math.min(tileWidth, width - x);
      const cropHeight = Math.min(tileHeight, height - y);
      const canvas = new OffscreenCanvas(cropWidth, cropHeight);
      const context = canvas.getContext("2d");
      if (!context) continue;
      context.drawImage(bitmap, x, y, cropWidth, cropHeight, 0, 0, cropWidth, cropHeight);
      const blob = await canvas.convertToBlob({ type: "image/jpeg", quality: 0.92 });
      tiles.push(URL.createObjectURL(blob));
      if (x + cropWidth >= width) break;
    }
    if (y + tileHeight >= height) break;
  }
  bitmap.close();
  return tiles.length ? tiles : [imageDataUrl];
}

function mergeOcr(parts: string[]) {
  const lines = new Set<string>();
  for (const part of parts) {
    for (const line of part.split(/\r?\n/).map((value) => value.trim()).filter(Boolean)) {
      const key = line.toLowerCase().replace(/\s+/g, " ");
      if (!lines.has(key)) lines.add(key);
    }
  }
  return [...lines].join("\n");
}

async function processRequest(message: WorkerRequest) {
  const { id } = message;
  activeId = id;
  try {
    if (message.type === "embed") {
      const embedding = await embedText(message.text);
      scope.postMessage({ type: "embedding", id, embedding });
      return;
    }
    const worker = await getWorker();
    const fullPage = await worker.recognize(message.imageDataUrl);
    const parts = fullPage.data.text.trim() ? [fullPage.data.text] : [];
    const tiles = fullPage.data.text.trim().length < 220 || fullPage.data.confidence < 58
      ? await imageTiles(message.imageDataUrl)
      : [];
    try {
      for (const tile of tiles.filter((item) => item.startsWith("blob:"))) {
        const result = await worker.recognize(tile);
        if (result.data.text.trim()) parts.push(result.data.text);
      }
    } finally {
      tiles.forEach((tile) => { if (tile.startsWith("blob:")) URL.revokeObjectURL(tile); });
    }
    const text = mergeOcr(parts);
    let embedding: number[] | undefined;
    let semanticError: string | undefined;
    if (text.trim()) {
      try {
        embedding = await embedText(text);
      } catch (cause) {
        semanticError = String(cause);
      }
    }
    scope.postMessage({ type: "result", id, text, embedding, semanticError });
  } catch (cause) {
    scope.postMessage({ type: "error", id, error: String(cause) });
  } finally {
    activeId = 0;
  }
}

let requestQueue = Promise.resolve();
scope.onmessage = (event) => {
  requestQueue = requestQueue.then(() => processRequest(event.data)).catch((cause) => {
    scope.postMessage({ type: "error", id: event.data.id, error: String(cause) });
  });
};
