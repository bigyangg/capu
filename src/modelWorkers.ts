// Runs local OCR and embedding models in a web worker that is shut down when idle.
//
// WebAssembly memory can grow but never shrink, so a worker that has loaded Tesseract or a
// CLIP model keeps that memory until it is terminated. Each IdleWorker starts its worker on
// the first request and terminates it after `idleMs` with nothing to do; the next request
// starts a fresh one and the models load again from the bundled files.

type WorkerMessage =
  | { type: "progress"; id: number; progress: number; status: string }
  | { type: "result"; id: number; text: string; embedding?: number[]; semanticError?: string }
  | { type: "embedding"; id: number; embedding: number[] }
  | { type: "ready"; id: number }
  | { type: "error"; id: number; error: string };

export type WorkerResult = { text?: string; embedding?: number[]; semanticError?: string; error?: string };

export type WorkerRequest =
  | { type: "ocr"; imageDataUrl: string }
  | { type: "embed"; text: string }
  | { type: "image-embed"; imageDataUrl: string }
  | { type: "visual-query"; text: string }
  | { type: "warm-search" };

type Callbacks = {
  onProgress?: (progress: number) => void;
  onCrash?: (message: string) => void;
};

export class IdleWorker {
  private worker: Worker | null = null;
  private active = 0;
  private idleTimer: number | undefined;
  private nextId = 1;
  private readonly pending = new Map<number, (result: WorkerResult) => void>();

  constructor(
    private readonly label: string,
    private readonly idleMs: number,
    private readonly callbacks: Callbacks = {},
  ) {}

  run(request: WorkerRequest): Promise<WorkerResult> {
    const worker = this.start();
    window.clearTimeout(this.idleTimer);
    this.active += 1;
    const id = this.nextId++;
    return new Promise<WorkerResult>((resolve) => {
      this.pending.set(id, resolve);
      worker.postMessage({ ...request, id });
    }).finally(() => {
      this.active -= 1;
      if (this.active === 0) this.scheduleStop();
    });
  }

  /** Resolves with the vector, or rejects with the worker's error. */
  async vector(request: WorkerRequest): Promise<number[]> {
    const result = await this.run(request);
    if (result.error) throw new Error(result.error);
    if (!result.embedding) throw new Error("The local model returned no vector.");
    return result.embedding;
  }

  /** Terminates the worker now and fails anything still waiting on it. */
  stop(reason = `${this.label} stopped.`) {
    window.clearTimeout(this.idleTimer);
    this.worker?.terminate();
    this.worker = null;
    const waiting = [...this.pending.values()];
    this.pending.clear();
    for (const resolve of waiting) resolve({ error: reason });
  }

  private scheduleStop() {
    window.clearTimeout(this.idleTimer);
    this.idleTimer = window.setTimeout(() => {
      if (this.active === 0) this.stop();
    }, this.idleMs);
  }

  private start() {
    if (this.worker) return this.worker;
    const worker = new Worker(new URL("./ocr.worker.ts", import.meta.url), { type: "module" });
    worker.onmessage = (event: MessageEvent<WorkerMessage>) => {
      const message = event.data;
      if (message.type === "progress") {
        this.callbacks.onProgress?.(message.progress);
        return;
      }
      const resolve = this.pending.get(message.id);
      this.pending.delete(message.id);
      if (message.type === "result") resolve?.({ text: message.text, embedding: message.embedding, semanticError: message.semanticError });
      else if (message.type === "embedding") resolve?.({ embedding: message.embedding });
      else if (message.type === "ready") resolve?.({});
      else resolve?.({ error: message.error });
    };
    worker.onerror = (event) => {
      event.preventDefault();
      const reason = `${this.label} failed: ${event.message}`;
      this.callbacks.onCrash?.(reason);
      // A crashed worker is replaced on the next request.
      this.stop(reason);
    };
    this.worker = worker;
    return worker;
  }
}
