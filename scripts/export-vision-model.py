"""Exports an OpenCLIP ViT-B/32 checkpoint to the ONNX layout the app loads with Transformers.js.

Development tool, not run by the build. Produces, under <output>/onnx:
  vision_model_quantized.onnx  pixel_values -> image_embeds
  text_model_quantized.onnx    input_ids    -> text_embeds
with dynamic int8 weights (see the quantization comment below). The tokenizer and
preprocessing files are copied from the OpenAI ViT-B/32 folder, because LAION's ViT-B/32 uses
the same tokenizer, input size and normalization.

    python scripts/export-vision-model.py laion2b_s34b_b79k <output> <public/models/Xenova/clip-vit-base-patch32>

Needs: torch, open_clip_torch, onnx, onnxruntime (CPU wheels are enough).
"""
import shutil
import sys
import tempfile
from pathlib import Path

import open_clip
import torch
from onnxruntime.quantization import QuantType, quantize_dynamic

pretrained, output, template = sys.argv[1], Path(sys.argv[2]), Path(sys.argv[3])

model, _, _ = open_clip.create_model_and_transforms("ViT-B-32", pretrained=pretrained)
model.eval().requires_grad_(False)


class Vision(torch.nn.Module):
    def __init__(self):
        super().__init__()
        self.model = model

    def forward(self, pixel_values):
        return self.model.encode_image(pixel_values)


class Text(torch.nn.Module):
    def __init__(self):
        super().__init__()
        self.model = model

    def forward(self, input_ids):
        # Transformers.js pads with the end-of-text token; open_clip takes the first
        # highest token id (the end-of-text position), so padding does not change the result.
        return self.model.encode_text(input_ids)


(output / "onnx").mkdir(parents=True, exist_ok=True)
with tempfile.TemporaryDirectory() as work, torch.no_grad():
    work = Path(work)
    torch.onnx.export(Vision(), torch.randn(1, 3, 224, 224), work / "vision.onnx",
                      input_names=["pixel_values"], output_names=["image_embeds"],
                      dynamic_axes={"pixel_values": {0: "batch"}, "image_embeds": {0: "batch"}},
                      opset_version=17, dynamo=False)
    torch.onnx.export(Text(), torch.zeros(1, 77, dtype=torch.int64), work / "text.onnx",
                      input_names=["input_ids"], output_names=["text_embeds"],
                      dynamic_axes={"input_ids": {0: "batch"}, "text_embeds": {0: "batch"}},
                      opset_version=17, dynamo=False)
    # Image model: int8 MatMul/Gemm only (the patch convolution stays fp32). Text model:
    # per-channel int8. On the screenshot benchmark these kept rankings closest to fp32.
    quantize_dynamic(work / "vision.onnx", output / "onnx" / "vision_model_quantized.onnx",
                     weight_type=QuantType.QUInt8, op_types_to_quantize=["MatMul", "Gemm"])
    quantize_dynamic(work / "text.onnx", output / "onnx" / "text_model_quantized.onnx",
                     weight_type=QuantType.QUInt8, per_channel=True)

for name in ["config.json", "preprocessor_config.json", "tokenizer.json", "tokenizer_config.json"]:
    shutil.copy(template / name, output / name)
for path in sorted(output.rglob("*")):
    if path.is_file():
        print(path.relative_to(output), f"{path.stat().st_size / 1e6:.1f} MB")
