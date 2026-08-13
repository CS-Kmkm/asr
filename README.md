# Local Voice Input

Privacy-first Windows voice input built with Tauri, Rust, React, and a persistent
Python ASR worker (VibeVoice, faster-whisper, or an OpenAI-compatible API).

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
pnpm install
py -3.10 -m venv .venv
.\.venv\Scripts\Activate.ps1
pip install -e .
$env:ASR_PYTHON = "$PWD\.venv\Scripts\python.exe"
pnpm run tauri dev
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
pnpm run tauri dev
```

See [docs/adr/0004-default-asr-backend.md](docs/adr/0004-default-asr-backend.md)
for the rationale behind the default backend choice.

## Backends

Three ASR backends are supported. The backend is selected via the `--backend`
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

### OpenAI-compatible API

This backend calls `POST /v1/audio/transcriptions`, so the same desktop workflow
can use OpenAI or a compatible local server. In **Models**, select
**OpenAI-compatible API**, then configure the base URL and model ID. API secrets
are not stored in application settings: the worker reads the environment
variable named in the UI (by default `OPENAI_API_KEY`). Restart the desktop app
after setting the variable so it inherits the value.

```powershell
$env:OPENAI_API_KEY = "..."
# Base URL: https://api.openai.com/v1
# Model ID: gpt-4o-mini-transcribe (or another available transcription model)
pnpm run tauri dev
```

For another compatible endpoint, set its `/v1` base URL. An unauthenticated
local endpoint does not require the configured key environment variable to
exist.

### AI transcript correction (OpenAI or Gemini)

The optional correction stage runs after transcription and before text
insertion. Its dedicated **AI text correction** card in **Settings** has a
master switch that enables or disables all correction processing and external
API requests at once. Provider, model, and editing options remain editable
while the master switch is off, so correction can be configured before it is
enabled. The transcript text and preferred dictionary spellings are sent to
the selected provider; recorded audio is not. If correction fails, the
original transcript is inserted instead.

The automatic editor has independent switches for:

- filler removal, while retaining hesitation or discourse markers that carry meaning;
- accidental repetition and false-start removal, while retaining intentional emphasis;
- resolving explicit spoken self-corrections to the speaker's final revision;
- formatting spoken lists, steps, action items, and key points;
- light clarity and grammar repair without changing meaning, tone, or formality.

The provider prompt treats the transcript as untrusted data. Questions and
commands spoken into the transcript are edited as text rather than answered or
executed. Additional style and tone guidance can be configured separately from
the safety and fidelity rules.

To keep API cost and latency low, the editor uses a compact instruction, sends
only dictionary aliases actually found in the transcript, caps optional style
guidance at 500 characters, requests minimal/no reasoning on supported models,
and sets an output-token limit based on the transcript length.

Correction responses are consumed as server-sent events from both the OpenAI
Responses API and Gemini Interactions API. As soon as ASR finishes, the
raw transcript is inserted into the captured target as provisional text. The
first correction delta replaces that draft and later deltas are appended while
the API is still generating; the always-on-top status overlay mirrors the same
progress. A short-lived helper process observes keyboard and pointer activity
during this replacement session. It reports only activity counters (never key
values or typed text), ignores this application's tagged input, and never
suppresses an event. If the user types, clicks, changes focus, starts IME
composition, or the monitor becomes unavailable, live replacement stops and
the completed result is left on the clipboard instead of modifying the target
again.

All fixed prompt text and prompt-size limits are centralized in
`src-tauri/src/correction_prompt.rs` under the `Prompt tuning` block. Edit that
block to tune correction behavior without changing provider/API request code.

API secrets are never stored in application settings. Set the environment
variable shown in Settings, then restart the desktop app so it inherits the
value:

```powershell
# OpenAI Responses API
$env:OPENAI_API_KEY = "..."

# Google Gemini Interactions API
$env:GEMINI_API_KEY = "..."
```

Correction is disabled by default. The default model IDs can be changed in the
UI without rebuilding the application.

## Serve a local model through the OpenAI API shape

Install the serving extra and expose either local backend over HTTP:

```powershell
uv sync --extra serve
$env:ASR_MODEL_ID = "large-v3-turbo"
uv run --extra serve python -m asr_worker --backend faster-whisper --serve `
  --host 127.0.0.1 --port 8000 --served-model local-asr
```

To serve VibeVoice instead, sync both extras with
`uv sync --extra serve --extra vibevoice` and select `--backend vibevoice`.

The server exposes `POST /v1/audio/transcriptions`, `GET /v1/models`, and
`GET /health`. Supported response formats are `json`, `text`, `verbose_json`,
`srt`, and `vtt`. Optional bearer authentication can be enabled with the
`ASR_SERVE_API_KEY` environment variable.

It can be called with the OpenAI Python client by changing only `base_url`:

```python
from openai import OpenAI

client = OpenAI(base_url="http://127.0.0.1:8000/v1", api_key="local")
with open("sample.wav", "rb") as audio:
    result = client.audio.transcriptions.create(model="local-asr", file=audio)
print(result.text)
```

The local server currently provides non-streaming transcription and does not
provide diarization or token log probabilities.

## Checks

```bash
pnpm run build
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

- Local processing remains the default. Cloud/API processing is used only when
  the OpenAI-compatible backend is explicitly selected.
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
