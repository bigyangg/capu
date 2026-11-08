// Compares image-search models on a labelled set of your own screenshots.
//
//   node scripts/eval-vision-models.mjs <eval.json>
//
// eval.json:
//   {
//     "images":  ["C:/path/shot1.png", ...],               // indexes are used below
//     "queries": [{ "query": "photo of a man", "relevant": [1, 5], "kind": "visual" }, ...],
//     "models":  [
//       { "name": "OpenAI B/32 int8 (shipped)", "type": "transformers",
//         "modelRoot": "public/models", "id": "Xenova/clip-vit-base-patch32", "dtype": "q8" },
//       { "name": "LAION-2B B/32", "type": "openclip", "dir": "C:/models/ViT-B-32__laion2b-s34b-b79k",
//         "visual": "visual/model.onnx", "textual": "textual/model.onnx" }
//     ]
//   }
//
// "openclip" models use the Immich ONNX layout: visual/ and textual/ folders, config.json with
// text_cfg.context_length, and visual/preprocess_cfg.json. Every model sees the same
// 480x320 JPEG thumbnails the app stores, and runs on one CPU thread like the app's
// WebAssembly worker, so times are comparable to each other but not equal to the app's.
// Nothing leaves the machine; screenshots and results stay local.

import { mkdir, readFile, mkdtemp, writeFile, stat } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { performance } from "node:perf_hooks";
import ort from "onnxruntime-node";
import sharp from "sharp";
import {
  AutoProcessor,
  AutoTokenizer,
  CLIPTextModelWithProjection,
  CLIPVisionModelWithProjection,
  RawImage,
  env,
} from "@huggingface/transformers";

const config = JSON.parse(await readFile(process.argv[2], "utf8"));
if (config.dump) await mkdir(config.dump, { recursive: true });
env.allowRemoteModels = false;
const singleThread = { intraOpNumThreads: 1, interOpNumThreads: 1 };

function normalize(vector) {
  const length = Math.hypot(...vector) || 1;
  return Float32Array.from(vector, (value) => value / length);
}

function dot(a, b) {
  let sum = 0;
  for (let i = 0; i < a.length; i += 1) sum += a[i] * b[i];
  return sum;
}

// The app's thumbnail: fit inside 480x320, JPEG.
const workDir = await mkdtemp(join(tmpdir(), "vision-eval-"));
const thumbnails = [];
for (const [index, path] of config.images.entries()) {
  const file = join(workDir, `${index}.jpg`);
  await writeFile(file, await sharp(path).resize(480, 320, { fit: "inside" }).jpeg({ quality: 75 }).toBuffer());
  thumbnails.push(file);
}

async function openclipModel(spec) {
  const modelConfig = JSON.parse(await readFile(join(spec.dir, "config.json"), "utf8"));
  const pre = JSON.parse(await readFile(join(spec.dir, "visual/preprocess_cfg.json"), "utf8"));
  const visual = await ort.InferenceSession.create(join(spec.dir, spec.visual), singleThread);
  const textual = await ort.InferenceSession.create(join(spec.dir, spec.textual), singleThread);
  env.localModelPath = spec.dir;
  const tokenizer = await AutoTokenizer.from_pretrained("textual");
  const tokenizerConfig = JSON.parse(await readFile(join(spec.dir, "textual/tokenizer_config.json"), "utf8"));
  const padId = tokenizer.model.tokens_to_ids.get(tokenizerConfig.pad_token) ?? 0;
  const contextLength = modelConfig.text_cfg.context_length;
  const siglip = Boolean(modelConfig.text_cfg.hf_tokenizer_name);
  const [width, height] = pre.size;
  let textType = "int32";

  return {
    async image(file) {
      const { data } = await sharp(file)
        .removeAlpha()
        .resize(width, height, { fit: pre.resize_mode === "squash" ? "fill" : "cover", kernel: "cubic" })
        .raw()
        .toBuffer({ resolveWithObject: true });
      const pixels = new Float32Array(3 * width * height);
      for (let i = 0; i < width * height; i += 1) {
        for (let c = 0; c < 3; c += 1) {
          pixels[c * width * height + i] = (data[i * 3 + c] / 255 - pre.mean[c]) / pre.std[c];
        }
      }
      const output = await visual.run({ image: new ort.Tensor("float32", pixels, [1, 3, height, width]) });
      return normalize(output.embedding.data);
    },
    async text(query) {
      // SigLIP's tokenizer expects canonicalized text: lowercase, no punctuation.
      const input = siglip ? query.toLowerCase().replace(/[^\p{L}\p{N}\s]/gu, "").replace(/\s+/g, " ").trim() : query;
      const ids = Array.from(tokenizer(input).input_ids.data, Number).slice(0, contextLength);
      while (ids.length < contextLength) ids.push(padId);
      const run = (type) => runText(type, ids);
      try {
        return await run(textType);
      } catch {
        textType = textType === "int32" ? "int64" : "int32";
        return run(textType);
      }
    },
    files: [join(spec.dir, spec.visual), join(spec.dir, spec.textual)],
  };

  async function runText(type, ids) {
    const values = type === "int64" ? BigInt64Array.from(ids, BigInt) : Int32Array.from(ids);
    const output = await textual.run({ text: new ort.Tensor(type, values, [1, contextLength]) });
    return normalize(output.embedding.data);
  }
}

async function transformersModel(spec) {
  env.localModelPath = spec.modelRoot;
  const options = { dtype: spec.dtype, session_options: singleThread };
  const processor = await AutoProcessor.from_pretrained(spec.id);
  const vision = await CLIPVisionModelWithProjection.from_pretrained(spec.id, options);
  const tokenizer = await AutoTokenizer.from_pretrained(spec.id);
  const text = await CLIPTextModelWithProjection.from_pretrained(spec.id, options);
  const suffix = spec.dtype === "q8" ? "_quantized" : "";
  return {
    async image(file) {
      const { image_embeds } = await vision(await processor(await RawImage.read(file)));
      return normalize(image_embeds.data);
    },
    async text(query) {
      const { text_embeds } = await text(tokenizer([query], { padding: "max_length", truncation: true }));
      return normalize(text_embeds.data);
    },
    files: [
      join(spec.modelRoot, spec.id, `onnx/vision_model${suffix}.onnx`),
      join(spec.modelRoot, spec.id, `onnx/text_model${suffix}.onnx`),
    ],
  };
}

function summarize(scores) {
  const total = (list, pick) => list.reduce((sum, item) => sum + pick(item), 0) / Math.max(1, list.length);
  return {
    hit1: total(scores, (s) => (s.firstRank === 1 ? 1 : 0)),
    hit5: total(scores, (s) => (s.firstRank <= 5 ? 1 : 0)),
    recall5: total(scores, (s) => s.recall5),
    mrr: total(scores, (s) => 1 / s.firstRank),
  };
}

const percent = (value) => `${(value * 100).toFixed(0)}%`;
const rows = [];
for (const spec of config.models) {
  const model = spec.type === "openclip" ? await openclipModel(spec) : await transformersModel(spec);
  await model.image(thumbnails[0]); // warm-up, not timed
  let started = performance.now();
  const images = [];
  for (const file of thumbnails) images.push(await model.image(file));
  const imageMs = (performance.now() - started) / thumbnails.length;
  await model.text("warm up");
  started = performance.now();
  const scores = [];
  const relevantScores = [];
  const otherScores = [];
  const matrix = [];
  for (const item of config.queries) {
    const query = await model.text(item.query);
    matrix.push(images.map((vector) => dot(vector, query)));
    const ranked = images.map((vector, index) => ({ index, score: dot(vector, query) })).sort((a, b) => b.score - a.score);
    const ranks = item.relevant.map((index) => ranked.findIndex((entry) => entry.index === index) + 1);
    scores.push({ kind: item.kind, firstRank: Math.min(...ranks), recall5: ranks.filter((rank) => rank <= 5).length / ranks.length });
    relevantScores.push(ranked.find((entry) => item.relevant.includes(entry.index)).score);
    otherScores.push(ranked.find((entry) => !item.relevant.includes(entry.index)).score);
  }
  const queryMs = (performance.now() - started) / config.queries.length;
  let bytes = 0;
  for (const file of model.files) bytes += (await stat(file)).size;
  const all = summarize(scores);
  const visual = summarize(scores.filter((s) => s.kind === "visual"));
  const textual = summarize(scores.filter((s) => s.kind === "text"));
  const mean = (list) => (list.reduce((a, b) => a + b, 0) / list.length).toFixed(3);
  rows.push({
    model: spec.name,
    "hit@1": percent(all.hit1),
    "hit@5": percent(all.hit5),
    "recall@5": percent(all.recall5),
    MRR: all.mrr.toFixed(2),
    "visual hit@5": percent(visual.hit5),
    "text hit@5": percent(textual.hit5),
    "ms/image": imageMs.toFixed(0),
    "ms/query": queryMs.toFixed(0),
    "MB on disk": (bytes / 1e6).toFixed(0),
    "score: best right / best wrong": `${mean(relevantScores)} / ${mean(otherScores)}`,
  });
  // With "dump" set in eval.json, per-query scores are saved for significance tests and for
  // comparing an int8 model's rankings with its fp32 original.
  if (config.dump) {
    await writeFile(join(config.dump, `${spec.name.replace(/[^\w.-]+/g, "_")}.json`),
      JSON.stringify({ name: spec.name, firstRanks: scores.map((s) => s.firstRank), matrix }));
  }
  console.error(`done: ${spec.name}`);
}
console.log(`${config.images.length} screenshots, ${config.queries.length} queries ` +
  `(${config.queries.filter((q) => q.kind === "visual").length} visual, ${config.queries.filter((q) => q.kind === "text").length} text)`);
console.table(rows);
