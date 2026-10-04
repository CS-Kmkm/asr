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

`Ctrl+Shift+Y` starts the independent voice **Translate** mode. Press it again
to stop, transcribe, translate into the configured target language, and insert
only the translated result. Settings stores an ordered target-language list
and the current target. The recording overlay shows that target and can cycle
the list; clicking the overlay counts as user activity, so that recording uses
the clipboard fallback instead of modifying a target after interaction.
`Ctrl+Shift+T` remains the separate selected-text translation action.
If saved hotkeys overlap, for example when an older selected-text translation
hotkey is `Ctrl+Shift+Y`, the older action keeps the chord: recording, then
selected-text translation, then voice Translate. The overlapped action stays off
until it is assigned a different hotkey, and the main window names it.

`Ctrl+Shift+E` starts **Speak to edit** for the text selected at that moment.
Speak an instruction such as “make this concise” or “translate this to
Japanese,” then press the shortcut again. The provider receives the immutable
selected text and spoken instruction as separate untrusted fields and may only
return replacement text. The replacement keeps the selection's leading and
trailing spaces and line breaks. Deleting the selection is not an Edit result:
an empty provider response is rejected and the selection is left unchanged. If
focus, selection, user input, shortcut release, or IME safety checks fail, the
original text is left alone and the generated edit remains on the clipboard.

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
are not stored in application settings. The desktop app loads a local `.env`
file at startup and reads the environment variable named in the UI (by default
`OPENAI_API_KEY`). The `.env` file may be placed in the project directory during
development or next to the desktop executable. Existing process/system
environment variables take precedence over values from `.env`.

```powershell
# Put this in .env instead:
OPENAI_API_KEY=...

# Or set it in PowerShell for the current process:
$env:OPENAI_API_KEY = "..."
# Base URL: https://api.openai.com/v1
# Model ID: gpt-4o-mini-transcribe (or another available transcription model)
pnpm run tauri dev
```

For another compatible endpoint, set its `/v1` base URL. An unauthenticated
local endpoint does not require the configured key environment variable to
exist.

### AI transcript correction (OpenAI, Gemini, or local)

The optional correction stage runs after transcription and before text
insertion. Its dedicated **AI text correction** card in **Settings** has a
master switch that enables or disables all correction processing and external
API requests at once. Provider, model, and editing options remain editable
while the master switch is off, so correction can be configured before it is
enabled. The transcript text and preferred dictionary spellings are sent to
the selected provider; recorded audio is not. If correction fails, the
original transcript is inserted instead.

For fully local correction, select **Local (OpenAI-compatible)** and configure
the model ID and a numeric loopback base URL such as
`http://127.0.0.1:11434/v1`. The app calls the Chat Completions endpoint,
requires no API key, bypasses system proxies, rejects redirects, and refuses
non-loopback hosts so transcript text cannot be sent to an obvious remote
endpoint through this provider. The local model server itself is not bundled
or started by the app. The endpoint URL and model ID are saved when the field
loses focus or Enter is pressed (Escape reverts the draft), so they can be
edited from an empty draft.
The local output token limit defaults to 4096 and can be set from 128 to
32768; some servers count thinking tokens toward this limit. When a response
stops at this limit (`finish_reason: "length"`), the original transcript is
used and the notice says that the local output token limit was reached;
increase it, subject to the server's context and memory limits.
A leading `<think>...</think>` block in the response text is removed before it
reaches the preview or the inserted result; a response whose leading think
block never closes falls back to the original transcript.
Local requests use a 10-second connect timeout and fail when no data arrives
for 90 seconds, with no total deadline, so a long generation that keeps
streaming is not cut off.

The automatic editor has independent switches for:

- filler removal, while retaining hesitation or discourse markers that carry meaning;
- accidental repetition and false-start removal, while retaining intentional emphasis;
- resolving explicit spoken self-corrections to the speaker's final revision;
- formatting spoken lists, steps, action items, and key points;
- light clarity and grammar repair without changing meaning, tone, or formality.

Correction mode defaults to **Conservative**, which keeps the spoken order. The
optional **Intent-aware** mode may organize the current transcript across its
utterance order, apply a later explicit correction consistently, merge duplicate
information, and select paragraph or list structure. The provider is instructed
not to add details; the mode does not learn automatically or use context beyond
the current transcript and selected profile. Only intent-aware results are
checked after correction: a result that drops or adds URLs, digit-bearing
tokens, backtick code, or listed explicit uncertainty markers is discarded and
the original transcript is used. This check is syntactic, so it cannot detect
changed names, spelled-out numbers, unquoted code, domains without a scheme, or
other invented details; review important text before relying on it.
Conservative results are inserted without this additional check, exactly as
before the mode switch existed.

For OpenAI correction, reasoning effort can be set to `none`, `low`, `medium`,
`high`, `xhigh`, or `max`. Higher values can improve difficult corrections at
the cost of additional latency and token usage; support depends on the selected
model.

The provider prompt treats the transcript as untrusted data. Questions and
commands spoken into the transcript are edited as text rather than answered or
executed. Additional style and tone guidance can be configured separately from
the safety and fidelity rules.

To keep API cost and latency low, the editor uses a compact instruction, sends
only dictionary aliases actually found in the transcript, caps optional style
guidance at 500 characters, requests minimal/no reasoning on supported models,
and sets an output-token limit based on the transcript length.

Correction responses are consumed as server-sent events from the OpenAI
Responses API, Gemini Interactions API, and compatible local Chat Completions
servers; non-streaming local responses are also accepted. In Dictate mode, as
soon as ASR finishes, the raw transcript is inserted into the captured target
as provisional text. The first correction delta replaces that draft and later deltas are appended while
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

API secrets are never stored in application settings. Put the environment
variables shown in Settings in `.env`, then restart the desktop app:

```powershell
# OpenAI Responses API
OPENAI_API_KEY=...

# Google Gemini Interactions API
GEMINI_API_KEY=...
```

Correction is disabled by default. The default model IDs can be changed in the
UI without rebuilding the application.

Manual provider quality check: with an explicitly configured OpenAI or Gemini
credential, test conservative and intent-aware correction on Japanese and
English dictation containing a name, URL, number, code span, and uncertainty.
Confirm that intent-aware organization improves only the current transcript and
never adds a concrete fact. This check is intentionally opt-in because it sends
the sample text to the selected provider and may incur charges.

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

With `stream=true` (and the `json` or `text` response format) the server sends
OpenAI-style server-sent events: one `transcript.text.delta` per decoded
segment, then `transcript.text.done` with the whole text. faster-whisper
streams segments as they are decoded; the other backends send their transcript
as a single delta.

```python
stream = client.audio.transcriptions.create(model="local-asr", file=audio, stream=True)
for event in stream:
    if event.type == "transcript.text.delta":
        print(event.delta, end="", flush=True)
```

The local server does not provide diarization or token log probabilities.

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
- Temporary WAV deletion defaults to enabled. When History is on and "Delete
  audio after processing" is off, History keeps a copy of each new recording
  for playback, download, and Retry. Turning it on stops keeping new recordings
  but does not delete recordings already in History; delete them there or let
  History retention remove them.
- The captured target must still be foreground and non-secure at insertion time.
- UI Automation security inspection exists and is used to avoid secure targets.
  Direct UI Automation insertion is available where supported, but some controls
  may still fall back to clipboard paste, Unicode input, or clipboard-only
  behavior.
- Native Win32 Edit controls (including Windows Forms text boxes) that expose
  no UI Automation text range are read and selected with window messages.
  Their IME composition cannot be observed from another process, so their
  text is replaced or deleted only when the IME is closed, or when no keyboard
  or pointer input occurred since the operation began (recording start for
  voice modes). Otherwise the result stays on the clipboard.
- Elevated applications, password controls, and secure controls are not forced.
- Model downloads use the Hugging Face cache. An interrupted download resumes
  from its partial files on the next load, and the progress notice says so.
  Newly downloaded files are checked against the Hub's SHA-256/git checksums;
  a mismatched file is downloaded again once, and a persistent mismatch fails
  the load. If the Hub cannot be reached for that check, the files stay
  unverified and are checked on the next load. Models cached before this check
  existed load offline without it. faster-whisper reports byte progress;
  VibeVoice (downloaded by Transformers) is verified after its first load.
- The default shortcut is registered at startup, and persisted custom shortcuts
  are re-registered on launch. If another app holds a saved hotkey, the app
  names the action that stays unavailable; saving other settings still works,
  while choosing a new hotkey that cannot be registered is rejected.
