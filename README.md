# Local Voice Input

Privacy-first Windows voice input built with Tauri, Rust, React, and a persistent
Python ASR worker (VibeVoice or faster-whisper).

## Current workflow

`Ctrl+Shift+Space` captures the foreground target and starts recording. Press it
again to stop. The Windows capture backend converts input to a temporary 24 kHz
mono PCM WAV, sends it to the persistent JSONL worker, and inserts the transcript
using clipboard paste, Unicode input, or the available UI Automation path. If
automatic insertion fails after the clipboard is populated, the transcript
remains on the clipboard.

Audio is deleted after processing by default. Transcript history is optional and
stored in the application SQLite database. Logs and status events must never
contain transcript, audio, window-title, or clipboard content.

## Windows prerequisites

- Windows 11 and WebView2
- Rust stable with the MSVC target and Visual Studio 2022 Build Tools
- Node.js 20 or newer
- Python 3.10 through 3.13
- NVIDIA CUDA-capable GPU with 12 GB VRAM or more — **required only for the
  optional VibeVoice backend**; the default faster-whisper backend runs on CPU
  and needs no GPU (GPU optional for speed)

VibeVoice 4-bit and 8-bit modes require `bitsandbytes`. CPU fallback is
intentionally disabled for VibeVoice. A current NVIDIA driver with
`nvidia-smi` available is required when using VibeVoice.

## Setup

```powershell
npm install
py -3.10 -m venv .venv
.\.venv\Scripts\Activate.ps1
pip install -e .
$env:ASR_PYTHON = "$PWD\.venv\Scripts\python.exe"
npm run tauri dev
```

`pip install -e .` installs the default faster-whisper stack (CPU-capable, no
GPU required). Select **Load ASR model** before the first dictation. The worker
downloads `large-v3-turbo` from Hugging Face on first load and uses the normal
Hugging Face cache. The model is not bundled with the app.

To use the optional VibeVoice backend (GPU required, long-form/high-accuracy),
install its extra dependencies and select it in Settings:

```powershell
pip install -e ".[vibevoice]"
# then: Settings → ASR backend → VibeVoice (GPU)
```

For protocol-only development without a GPU:

```powershell
$env:ASR_WORKER_BACKEND = "mock"
$env:ASR_WORKER_MOCK_TEXT = "test transcript"
npm run tauri dev
```

See [docs/adr/0004-default-asr-backend.md](docs/adr/0004-default-asr-backend.md)
for the rationale behind the default backend choice.

## Backends

Two ASR backends are supported. The backend is selected via the `--backend`
argument to the worker or the `ASR_WORKER_BACKEND` environment variable.

### faster-whisper (default, CPU-capable)

The default backend. Runs on CPU (int8) or CUDA (int8/fp16). No NVIDIA GPU
required. Installed by `pip install -e .`.

The Whisper model is configured via `ASR_FASTER_WHISPER_MODEL` (default:
`large-v3-turbo`). The model is downloaded from Hugging Face on first load.

### VibeVoice (optional GPU backend, long-form/high-accuracy)

Uses `microsoft/VibeVoice-ASR-HF` via Transformers. Requires a CUDA GPU.
CPU fallback is intentionally disabled. Install via the optional extra and
select in Settings (`ASR_WORKER_BACKEND=vibevoice`).

The generation token limit for VibeVoice can be overridden via
`ASR_MAX_NEW_TOKENS` (integer; overrides the length-based estimate when set).

## Checks

```bash
npm run build
python3 -m unittest discover -s asr_worker/tests -v
cd src-tauri && cargo test
```

Linux can build the frontend and run Python protocol tests. The microphone and
injection native APIs are Windows-only. A Linux Tauri Rust build still requires
the standard WebKitGTK/libsoup development packages.

## Benchmarks

A benchmark harness is included at `scripts/bench_asr.py`. Run a quick offline
smoke test (no GPU or audio files required):

```bash
python scripts/bench_asr.py --backend mock --smoke
```

Full usage and Phase 0 gate criteria are documented in
[docs/benchmarks.md](docs/benchmarks.md).

## Documentation

- [docs/local_ai_voice_input_implementation_plan.md](docs/local_ai_voice_input_implementation_plan.md) — implementation plan
- [docs/adr/](docs/adr/) — architecture decision records
- [docs/benchmarks.md](docs/benchmarks.md) — benchmark protocol and Phase 0 gate

## Privacy and limitations

- Cloud processing is off and no cloud provider is implemented.
- History can be disabled; when disabled, transcript rows are not written.
- Temporary WAV deletion defaults to enabled.
- The captured target must still be foreground and non-secure at insertion time.
- UI Automation security inspection exists and is used to avoid secure targets.
  Direct UI Automation insertion is available where supported, but some controls
  may still fall back to clipboard paste, Unicode input, or clipboard-only
  behavior.
- Elevated applications, password controls, and secure controls are not forced.
- Model download progress/resume UI and checksum verification remain future model
  management work; Transformers manages the current cache download.
- The default shortcut is registered at startup, and persisted custom shortcuts
  are re-registered on launch.
