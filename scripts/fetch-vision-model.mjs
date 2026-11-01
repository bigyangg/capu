// Stages the CLIP visual search model under public/models before dev/build.
// The weights are too large to keep in git, so they are downloaded once at build time from a
// pinned revision and verified by SHA-256. The app itself never downloads models at runtime.
import { createHash } from "node:crypto";
import { createReadStream } from "node:fs";
import { mkdir, rename, rm, stat, writeFile } from "node:fs/promises";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const model = "Xenova/clip-vit-base-patch32";
const revision = "6b7c523703ae26e39be4b03774966f8571c7f523";
const files = {
  "config.json": "493ef57ff783e42d1530c91b53469b7fdf8db8a9c1408e86998fcb7899a4f495",
  "preprocessor_config.json": "6f638fb9401a6d6296feff533ee7efe657b787c49f954f82f5906b36ef2a1b1f",
  "tokenizer.json": "f7f3b7af117d467b58374797691a6438d3e6b9e9cef800dfd5dced7f697a90cd",
  "tokenizer_config.json": "60ba2912bc6344c94bc16bbdec27fa1209409167b6f2fdf3cfe9e65462ea3967",
  "onnx/text_model_quantized.onnx": "73baab855d406190da9faa498cfedf65f15cf309f4cc7385b7b032e6d08e5c3a",
  "onnx/vision_model_quantized.onnx": "583fd1110a514667812fee7d684952aaf82a99b959760c8d7dca7e0ab9839299",
};
const target = join(root, "public", "models", ...model.split("/"));

function sha256(path) {
  return new Promise((resolveHash, reject) => {
    const hash = createHash("sha256");
    createReadStream(path).on("data", (chunk) => hash.update(chunk)).on("error", reject).on("end", () => resolveHash(hash.digest("hex")));
  });
}

async function isStaged(path, expected) {
  try {
    await stat(path);
  } catch {
    return false;
  }
  return (await sha256(path)) === expected;
}

let downloaded = 0;
for (const [file, expected] of Object.entries(files)) {
  const destination = join(target, ...file.split("/"));
  if (await isStaged(destination, expected)) continue;
  await mkdir(dirname(destination), { recursive: true });
  const url = `https://huggingface.co/${model}/resolve/${revision}/${file}`;
  console.log(`Downloading ${model}/${file}…`);
  const response = await fetch(url);
  if (!response.ok) throw new Error(`Could not download ${url}: HTTP ${response.status}`);
  const partial = `${destination}.part`;
  await writeFile(partial, Buffer.from(await response.arrayBuffer()));
  const actual = await sha256(partial);
  if (actual !== expected) {
    await rm(partial, { force: true });
    throw new Error(`${file} failed verification: expected ${expected}, got ${actual}`);
  }
  await rename(partial, destination);
  downloaded += 1;
}
console.log(downloaded ? `Prepared the visual search model (${downloaded} files downloaded).` : "Visual search model is ready.");
