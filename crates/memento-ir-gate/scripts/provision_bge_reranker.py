#!/usr/bin/env python3
"""Provision the int8-quantized bge-reranker-v2-m3 ONNX model used by the A1
cross-encoder rerank gate.

Downloads the stock FP32 ONNX (external-data layout) + tokenizer files from
the Hugging Face repo `rozgo/bge-reranker-v2-m3` (the same source fastembed's
`RerankerModel::BGERerankerV2M3` points at) and quantizes the graph with
dynamic QInt8 — the same recipe that produced the shipped
`models/int8/multilingual-e5-base-int8/` (obs 2696).

Notes:
  * The model uses `com.microsoft` contrib ops (Attention/FastGelu), so the
    quantizer needs `extra_options={"DefaultTensorType": ...}` to get past
    shape inference.
  * The FP32 graph is ~2.2 GB (external data) and quantization needs ~4-5 GB
    RAM peak; run on a host with headroom.

Usage:
    python crates/memento-ir-gate/scripts/provision_bge_reranker.py [OUTPUT_DIR]

Defaults OUTPUT_DIR to `models/int8/bge-reranker-v2-m3-int8` (repo root
relative to the workspace). Idempotent: skips when `model.onnx` already
exists.

Requirements: `pip install onnxruntime huggingface_hub`
"""

import os
import shutil
import sys
import time
from pathlib import Path

from huggingface_hub import hf_hub_download
from onnxruntime.quantization import quantize_dynamic, QuantType
import onnx

REPO = "rozgo/bge-reranker-v2-m3"
ONNX_FILE = "model.onnx"
TOKENIZER_FILES = [
    "tokenizer.json",
    "config.json",
    "special_tokens_map.json",
    "tokenizer_config.json",
]


def mb(path: Path) -> float:
    return round(path.stat().st_size / (1024 * 1024), 2)


def main() -> int:
    default_out = Path(__file__).resolve().parents[3] / "models" / "int8" / "bge-reranker-v2-m3-int8"
    out_dir = Path(sys.argv[1]) if len(sys.argv) > 1 else default_out
    out_dir.mkdir(parents=True, exist_ok=True)

    target = out_dir / "model.onnx"
    if target.exists():
        print(f"int8 reranker already present at {target} ({mb(target)} MB); skipping")
        return 0

    print(f"downloading FP32 ONNX ({ONNX_FILE}) + external data from {REPO} ...")
    fp32_path = hf_hub_download(repo_id=REPO, filename=ONNX_FILE)
    hf_hub_download(repo_id=REPO, filename="model.onnx.data")
    print(f"  fp32 onnx: {mb(Path(fp32_path))} MB")

    print(f"quantizing to int8 -> {target}")
    t0 = time.time()
    quantize_dynamic(
        fp32_path,
        str(target),
        weight_type=QuantType.QInt8,
        extra_options={"DefaultTensorType": int(onnx.TensorProto.FLOAT)},
    )
    print(f"  quantized: {mb(target)} MB in {time.time() - t0:.1f}s")

    for remote in TOKENIZER_FILES:
        local = out_dir / Path(remote).name
        if not local.exists():
            downloaded = hf_hub_download(repo_id=REPO, filename=remote)
            shutil.copy2(downloaded, local)
        print(f"  tokenizer file: {local.name}")

    print(f"provisioned int8 reranker at {out_dir}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
