"""Persistent, local Clef decision inference in the existing JSONL worker."""
from __future__ import annotations

import contextlib
import gc
import importlib.util
import ssl
import sys
from pathlib import Path
from typing import Any

from .backends import BackendError
from .download import ProgressCallback, byte_progress_tqdm

# Only these reviewed releases may supply executable model code.
RELEASES = {
    "Cloudflare/clef-flash": "17f0b0ad64efb65d273590632833508766b2aae6",
    "Cloudflare/clef": "2f3de3dd85f379784083b0814d997ab627200f0c",
}
ALLOW_PATTERNS = ["*.safetensors", "*.json", "*.jinja", "joint_schema_model.py", "LICENSE"]


class DecisionModel:
    def __init__(self) -> None:
        self.identity: tuple[str, str] | None = None
        self.model: Any = None
        self.processor: Any = None
        self.systemone: Any = None

    def load(self, model_id: str, quantization: str, progress: ProgressCallback | None = None) -> None:
        if model_id not in RELEASES or quantization not in {"4bit", "8bit", "bf16"}:
            raise BackendError("invalid_decision_model", "Unsupported local decision model or quantization")
        if self.identity == (model_id, quantization):
            return
        try:
            import torch
            import httpx
            import truststore
            from huggingface_hub import snapshot_download, set_client_factory
            from transformers import BitsAndBytesConfig
        except ImportError:
            raise BackendError("decision_dependencies_missing", "Install local decision dependencies with uv sync --extra decision") from None
        if quantization != "bf16" and not torch.cuda.is_available():
            raise BackendError("decision_cuda_required", "4-bit and 8-bit decision models require CUDA; select BF16 for CPU")
        self.unload()
        # Preserve certificate verification while using the OS trust store.
        set_client_factory(lambda: httpx.Client(verify=truststore.SSLContext(ssl.PROTOCOL_TLS_CLIENT), follow_redirects=True))
        with contextlib.redirect_stdout(sys.stderr):
            kwargs = {"revision": RELEASES[model_id], "allow_patterns": ALLOW_PATTERNS}
            try:
                snapshot = snapshot_download(model_id, local_files_only=True, **kwargs)
            except Exception:
                if progress:
                    progress({"stage": "download", "model": model_id})
                snapshot = snapshot_download(model_id, tqdm_class=byte_progress_tqdm(progress) if progress else None, **kwargs)
            if progress:
                progress({"stage": "load", "model": model_id})
            spec = importlib.util.spec_from_file_location("asr_clef_release", Path(snapshot) / "joint_schema_model.py")
            if spec is None or spec.loader is None:
                raise BackendError("decision_release_invalid", "Local decision release code is unavailable")
            release = importlib.util.module_from_spec(spec)
            sys.modules[spec.name] = release
            spec.loader.exec_module(release)
            device = "cuda" if torch.cuda.is_available() else "cpu"
            loader_options: dict[str, Any] = {}
            if quantization == "4bit":
                loader_options["quantization_config"] = BitsAndBytesConfig(load_in_4bit=True, bnb_4bit_quant_type="nf4", bnb_4bit_use_double_quant=True, bnb_4bit_compute_dtype=torch.bfloat16)
            elif quantization == "8bit":
                loader_options["quantization_config"] = BitsAndBytesConfig(load_in_8bit=True)
            self.model, self.processor = release.load_release_model(snapshot, device=device, dtype=torch.bfloat16, **loader_options)
            self.systemone = release.systemone
            self.identity = (model_id, quantization)

    def classify(self, request: Any) -> dict[str, Any]:
        if self.identity is None:
            raise BackendError("decision_not_loaded", "Load the local decision model first")
        if not isinstance(request, dict) or request.get("model") != self.identity[0]:
            raise BackendError("invalid_decision_input", "Decision model does not match the loaded release")
        state = request.get("state")
        if not isinstance(state, dict) or set(state) != {"spoken", "has_selection"}:
            raise BackendError("invalid_decision_input", "Decision state must contain speech and selection availability only")
        if not isinstance(state["spoken"], str) or not state["spoken"].strip() or len(state["spoken"]) > 8192 or not isinstance(state["has_selection"], bool):
            raise BackendError("invalid_decision_input", "Decision speech is empty or too long")
        with contextlib.redirect_stdout(sys.stderr):
            return self.systemone(self.model, self.processor, request, max_length=12288)

    def unload(self) -> None:
        self.identity = None
        self.model = self.processor = self.systemone = None
        gc.collect()
        if "torch" in sys.modules:
            torch = sys.modules["torch"]
            if torch.cuda.is_available():
                torch.cuda.empty_cache()
