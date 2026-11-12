"""transformers (torch) adapter: natively supported transformers ASR models.

Qwen3-ASR is loaded from the official transformers conversion published by the
Qwen organisation (``Qwen/Qwen3-ASR-0.6B-hf``), pinned by revision and SHA256,
and only from the app-managed folder: ``from_pretrained`` never contacts the
network here. CUDA is required unless ``ASR_DEVICE=cpu`` explicitly asks for
the (slow) CPU path; ``auto`` never falls back silently.
"""
from __future__ import annotations

import contextlib
import gc
import os
from pathlib import Path
from typing import Any

from .backends import (
    BackendError,
    _cancel_stopping_criteria,
    _eos_token_ids,
    compute_max_new_tokens,
    generation_reached_limit,
)
from .runtime_adapter import ManagedModel, PinnedFile, RuntimeAdapter

QWEN3_ASR_0_6B = ManagedModel(
    id="qwen3-asr-0.6b",
    display_name="Qwen3-ASR 0.6B",
    repo="Qwen/Qwen3-ASR-0.6B-hf",
    revision="7f1569a48a89f3e3f4dc3a5c9d28bddd903bc76c",
    files=(
        PinnedFile("config.json", "9eecf6f1b383e343889c2e6010e632590fa57d4bc678e151c7d6a160a0dfb04a", 2398),
        PinnedFile("generation_config.json", "9939fc9388b79bd70757f938b87381e817173d6a6158f5af6506c0b73e775c3c", 165),
        PinnedFile("model.safetensors", "d3f212dd20abecd315d830bc54ae3865e56ebfc3276484e57b771288ba27fd35", 1564928088),
        PinnedFile("processor_config.json", "bc0b230081b44e629dd5b9045b78495615c1831b4b9f4cffe97bd37e82a6156a", 487),
        PinnedFile("chat_template.jinja", "f50e6b694fbf4a683206e37869990d68333fe95d285730f084c838a34b0d98c2", 1434),
        PinnedFile("tokenizer.json", "fe1fad59be22a41ee293363fcf95fdedbc7c93f3b49270b1d2e18bd1399a7a05", 11429653),
        PinnedFile("tokenizer_config.json", "945e980986de2ca7768f3326bfdbb4fbea3406f972b8ae0be233089f2b253c11", 998),
    ),
    license="Apache-2.0",
    japanese_only=False,
    kind="qwen3_asr",
)
MODELS = {model.id: model for model in (QWEN3_ASR_0_6B,)}
SAMPLE_RATE = 16_000


class TransformersAsrBackend(RuntimeAdapter):
    backend_name = "transformers-asr"
    display_label = "Qwen3-ASR (transformers)"
    install_extra = "transformers-asr"
    models = MODELS
    default_model = QWEN3_ASR_0_6B.id

    def __init__(self) -> None:
        super().__init__()
        self.processor: Any = None
        self.network: Any = None

    def _import_runtime(self) -> None:
        import torch  # noqa: F401
        from transformers import AutoProcessor, Qwen3ASRForConditionalGeneration  # noqa: F401

    def _open(self, model: ManagedModel, paths: dict[str, Path]) -> None:
        import torch
        import transformers
        from transformers import AutoProcessor, Qwen3ASRForConditionalGeneration

        folder = str(paths["config.json"].parent)
        if self.device_policy == "cpu":
            device, dtype, compute_type = "cpu", torch.float32, "float32"
        else:
            if not torch.cuda.is_available():
                raise BackendError("gpu_unsupported", "Qwen3-ASR requires a CUDA GPU; set the speech device to CPU to run it slowly on the CPU")
            device, dtype, compute_type = "cuda", torch.bfloat16, "bfloat16"
        self.device = device
        # local_files_only: the verified folder is the only source; nothing
        # is fetched implicitly, even when a file is unexpectedly missing.
        self.processor = AutoProcessor.from_pretrained(folder, local_files_only=True)
        self.network = Qwen3ASRForConditionalGeneration.from_pretrained(
            folder, local_files_only=True, dtype=dtype, device_map=device,
        )
        self.network.eval()
        actual = str(next(self.network.parameters()).device)
        if not actual.startswith(device):
            raise BackendError("gpu_unsupported", f"Qwen3-ASR was placed on {actual.split(':')[0]}, not {device}")
        self.report_progress("warmup")
        import numpy as np

        self._generate(np.zeros(SAMPLE_RATE, dtype=np.float32), "ja", None, max_new_tokens=8)
        self.compute_type = compute_type
        self.runtime_label = f"transformers {transformers.__version__} / torch {torch.__version__}"
        if device == "cuda":
            self.device_name = torch.cuda.get_device_name(self.network.device)

    def _generate(self, samples: Any, language: str | None, prompt: str | None,
                  max_new_tokens: int) -> str:
        import torch

        with torch.inference_mode():
            inputs = self.processor.apply_transcription_request(
                audio=samples, language=language, prompt=prompt or None,
                sampling_rate=SAMPLE_RATE,
            ).to(self.network.device, self.network.dtype)
            input_length = inputs["input_ids"].shape[1]
            options: dict[str, Any] = {"max_new_tokens": max_new_tokens, "do_sample": False}
            if self.cancel_requested is not None:
                options["stopping_criteria"] = _cancel_stopping_criteria(self)
            output = self.network.generate(**inputs, **options)
            del inputs
            self.raise_if_cancelled()
            generated = output[:, input_length:]
            if generation_reached_limit(generated, max_new_tokens, _eos_token_ids(self.network)) and max_new_tokens > 8:
                raise BackendError(
                    "transcript_truncated",
                    f"Qwen3-ASR reached its {max_new_tokens}-token output limit before the transcript ended. "
                    "Record a shorter clip or raise ASR_MAX_NEW_TOKENS.",
                )
            text = self.processor.decode(generated[0], return_format="transcription_only")
            del output, generated
        return str(text).strip()

    def _recognize(self, samples: Any, rate: int, language: str | None,
                   prompt: str | None) -> tuple[str, list[dict[str, Any]]]:
        if rate != SAMPLE_RATE:
            import librosa

            samples = librosa.resample(samples, orig_sr=rate, target_sr=SAMPLE_RATE)
        duration = len(samples) / SAMPLE_RATE
        max_new_tokens = compute_max_new_tokens(duration, os.environ.get("ASR_MAX_NEW_TOKENS"))
        text = self._generate(samples, language, prompt, max_new_tokens)
        return text, [{"start": 0.0, "end": round(duration, 3), "speaker": None, "text": text}]

    def _close(self) -> None:
        self.network = None
        self.processor = None
        gc.collect()
        with contextlib.suppress(ImportError, RuntimeError):
            import torch

            if torch.cuda.is_available():
                torch.cuda.empty_cache()
                if hasattr(torch.cuda, "ipc_collect"):
                    torch.cuda.ipc_collect()
