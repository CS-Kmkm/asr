# Local Voice Input

Privacy-first Windows voice input built with Tauri, Rust, React, and a persistent
Python ASR worker (VibeVoice, faster-whisper, or an OpenAI-compatible API).

## Current workflow

`Ctrl+Shift+Space` captures the foreground target and starts recording. Press it
again to stop. Settingsの「押している間だけ録音」をモード別にオンにすると、約200ミリ秒待って開始し、キーを離すと停止します。
短い押下は無視します。開始・停止のタイミングはWindowsのキー通知に依存し、200ミリ秒の境界は厳密ではありません。
準備中に離した場合は録音を取り消します。既定は従来のtoggleです。待機録音・停止後の追加録音は行いません。
押下中は、ショートカットやhold設定の変更を保存できません。
The Windows capture backend converts input to a temporary 16 kHz (faster-whisper) or 24 kHz
mono PCM WAV, and sends it to the persistent JSONL worker. While you speak,
partial text appears only in the recording overlay. After transcription and any
AI correction, the final text is pasted once into the captured target with the
clipboard and Ctrl+V (Shift+Insert in mintty, PuTTY, and Emacs). When the target's text can be read through UI Automation,
the paste is verified and (with **Restore clipboard** on) the previous
clipboard is restored; otherwise one
guarded paste is sent unverified. With **Restore clipboard** on, the clipboard
is saved before it is first written; if it holds data that cannot be saved
(such as a very large image), nothing is pasted or copied and the result stays
in the app (and History, when on) instead. Whenever the result is left on the clipboard
(a copy instead of a paste, an unconfirmed paste, or a restore that failed),
the notice says the previous clipboard was not restored. With it off, the
clipboard is simply replaced. Clicking, scrolling, or typing while you
speak or while the text is being recognized does not prevent insertion: before
pasting, the app waits until input has been quiet for a moment (up to five
seconds) and the field that was focused when recording started is focused
again, then pastes at its current caret. In browsers and apps such as VS Code,
where every field shares one window, the field is identified through UI
Automation, so a click on another chat, panel, or the terminal never receives
the text. If you clicked, scrolled, or typed after recording started, the paste
also requires an empty selection and an IME confirmed to be inactive, so text
you selected or an unfinished IME conversion is never overwritten; fields that
cannot report either state get the clipboard instead. Without such input the
field is as you left it when recording started, so text selected beforehand is
replaced by the dictation as before. If a guard still fails (input does not
settle, the original field is not focused again, an IME conversion, or a
protected field), the text is copied to the clipboard instead. When History is
on, it stores the outcome of each Dictate, Translate, and Edit insertion, with
a fixed reason code for Dictate and Translate outcomes that were not confirmed.
The recording overlay shows non-confirmed outcomes for a few seconds. Typing partial text into the target
while speaking is an experimental opt-in (**Settings → Live text insertion**).

`Ctrl+Shift+Y` starts the independent voice **Translate** mode. Press it again
to stop, transcribe, translate into the configured target language, and insert
only the translated result. Settings stores an ordered target-language list
and the current target. The recording overlay shows that target and can cycle
the list for the current recording.
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

## 主な機能とショートカット

| 操作 | 既定キー |
|---|---|
| Dictate（音声入力） | `Ctrl+Shift+Space` |
| Translate（音声翻訳） | `Ctrl+Shift+Y` |
| 選択テキストの翻訳 | `Ctrl+Shift+T` |
| Edit（選択範囲を音声で編集） | `Ctrl+Shift+E` |
| Ask Anything（音声で質問・検索） | `Ctrl+Shift+A` |

Dictionary では用語・読みと適用先を管理できます。Personalization では
全体とアプリ別の style profile を設定し、補正に使う文章の調子を指定できます。
Personalizationがオンの場合はAsk・Editにも文章スタイルを適用し、入力先に合う辞書を綴りの参考に使います。
DictateではAI文章補正もオンにする必要があります。Ask・Editの実行内容や許可する操作はスタイル設定で変更しません。
メール・チャット・コード／技術文書・メモのプリセットを適用してから、各項目を編集できます。
最近使ったアプリやカテゴリから適用先を選べます。ターミナルも独立したカテゴリです。
アプリ候補は録音開始・選択テキスト翻訳時に取得した実行ファイル名の識別子を最大20件、メモリ内だけに保持します。
ウィンドウ名やパスは記録せず、「最近のアプリを消去」またはアプリ終了で消えます。選んで保存したスタイル設定は保持されます。
ショートカットは Settings で変更できます。

## Windows prerequisites

- Windows 11 and WebView2
- Rust stable with the MSVC target and Visual Studio 2022 Build Tools
- Node.js 22 (CI uses 22)
- uv（開発環境は `.python-version` の Python 3.12 を使用）
- NVIDIA CUDA-capable GPU with 12 GB VRAM or more — **required only for the
  optional VibeVoice backend**; the default faster-whisper backend runs on CPU
  and needs no GPU (GPU optional for speed)

VibeVoice 4-bit and 8-bit modes require `bitsandbytes`. CPU fallback is
intentionally disabled for VibeVoice. A current NVIDIA driver with
`nvidia-smi` available is required when using VibeVoice.

## 配布版のセットアップ

配布版にはワーカーのソースと依存ロックを同梱します。uv を事前にインストールし、
PATH に追加するか `ASR_UV` へ実行ファイルを指定してください。Rust、Node、Python の
開発環境は配布版の実行には不要です。Setup でモデルを読み込むと、uv が Python 3.12 と
必要なパッケージをユーザーデータ内へ構築します。初回はネットワークと空き容量が必要です。
GPU 用の VibeVoice は追加依存を取得します。構築に失敗した場合は表示された案内に従い、
再度モデルを読み込んでください。

Setup では、モデル読み込み後に「音声認識をテスト」で録音し、停止して認識結果を
確認できます。このテストでは AI 補正・文字挿入・履歴保存は行いません。一時録音は
処理後に削除し、削除に失敗した場合は通知します。
クラウド音声認識を選択した場合は、通常の録音と同じくプロバイダーへの同意が必要です。
「文字挿入をテスト」はモデルなしでも使えます。3秒以内にメモ帳などの入力欄へ
切り替えると、固定のテスト文を挿入します。貼り付けの確認が取れなかった場合や
クリップボードへのコピーだけで終わった場合は、画面にその結果を表示します。
テストはキャンセルでき、Setup を離れたときもそのテストだけをキャンセルします。

インストーラーの新規ユーザー環境での受け入れ確認は進行中です。開発用 release exe は
チェックアウト内でも実行できます。配布リソースがある場合はそちらが優先されます。

## Setup

開発用の手順です。uv が管理する `.venv` は自動検出されます。別の Python を
使うときだけ `ASR_PYTHON` を指定してください。配布方式は
[ADR-0003](docs/adr/0003-python-sidecar-distribution.md) を参照してください。

```powershell
pnpm install
uv sync
pnpm run tauri dev
```

`uv sync` installs the default faster-whisper stack (CPU-capable, no
GPU required). On first setup, select **Download and load speech model**, or use
**Load Model** on the Models page. Startup does not download a model before
setup is complete. Later dictations prepare the selected model before recording. The worker
downloads `large-v3-turbo` from Hugging Face on first load and uses the normal
Hugging Face cache. The model is not bundled with the app.

To use the optional VibeVoice backend (GPU required, long-form/high-accuracy),
install its extra dependencies and select it on the Models page:

```powershell
uv sync --extra vibevoice
# then: Models → VibeVoice (GPU)
```

For protocol-only development without a GPU:

```powershell
$env:ASR_WORKER_BACKEND = "mock"
$env:ASR_WORKER_MOCK_TEXT = "test transcript"
pnpm run tauri dev
```

See [docs/adr/0004-default-asr-backend.md](docs/adr/0004-default-asr-backend.md)
for the rationale behind the default backend choice.

### Development data

Debug builds (`pnpm run tauri dev`, `cargo build` without `--release`) keep
their SQLite database and `history-audio` folder in a `dev` subfolder of the
app data directory (`%APPDATA%\org.localvoice.input\dev\` on Windows), so
development and destructive testing never touch the History, settings, or
recordings of an installed release build. Release builds keep using the app
data directory itself. Release data is never copied or migrated into `dev`;
a fresh debug build starts with empty data. Worker logs stay in the shared
app log directory.

Debug builds also leave other release state alone. They never register or
remove startup at Windows sign-in: toggling **Start with Windows** in a debug
build only saves the setting and shows a notice. They also skip the startup
cleanup of stale capture WAVs in the shared temporary folder, because those
files cannot be told apart from ones a release instance keeps; delete old
`local-ai-voice-*.wav` files there by hand if a debug build leaves any.

To inspect release data from a debug build, opt in explicitly:

```powershell
$env:LVI_USE_RELEASE_DATA = "1"
pnpm run tauri dev
```

Only the exact value `1` opts in. While it is set, the debug build opens and
migrates the release database and recordings directly, so destructive tests
and unreleased migrations then affect your real data. Quit any running
release app first: the single-instance guard exists only in release builds,
so nothing stops both from opening the same database and `history-audio`
folder at once. Remove the variable (`Remove-Item Env:LVI_USE_RELEASE_DATA`)
when you are done.

## Backends

Three ASR backends are supported. The backend is selected via the `--backend`
argument to the worker or the `ASR_WORKER_BACKEND` environment variable.

### faster-whisper (default, CPU-capable)

The default backend. Runs on CPU (int8) or CUDA (int8/fp16). No NVIDIA GPU
required. Installed by `uv sync`.

デバイス選択は `ASR_DEVICE=auto|cpu|cuda`（既定は `auto`）です。`auto` は、CUDAの
ロード・ウォームアップ失敗に加え、文字起こし開始後のCUDA例外も、内部のデコーダーが
まだセグメントを返していなければCPU/int8で1回再試行します。キャンセル済みや、
内部でセグメントを取得済みの要求は再試行しません。アプリには全セグメントを集約して
から認識結果を返します。切り替え後のデバイスは Diagnostics の worker health で確認できます。
`cuda` を明示した場合と、プロセス自体が異常終了した場合はCPU再試行の対象外です。

Models の「音声認識デバイス」で自動・CPUのみ・CUDAのみを選び、「モデルを読み込む」で
反映できます。「環境設定」は既存の `ASR_DEVICE` を引き継ぎ、未指定なら自動選択です。
この選択はfaster-whisperに適用され、VibeVoiceとAPIバックエンドには影響しません。

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
are not stored in application settings. Settingsの「プロバイダーのAPIキー」で保存すると、Windows資格情報マネージャーを優先して使います。
未保存・利用不可の場合は、UIで指定した環境変数名（既定 `OPENAI_API_KEY`）の値を使います。
The desktop app loads a local `.env` file at startup. Only the first existing `.env` is loaded (files are not
merged), checked in this order:

1. next to the desktop executable;
2. the project directory: the nearest ancestor of the executable that
   contains `asr_worker/__main__.py` (this is how autostart finds it);
3. debug builds (`pnpm run tauri dev`) only: the working directory, then the
   workspace above `src-tauri`.

A `.env` at a drive root is never read. The Python worker is located the same
way (working directory in debug builds only) and runs with the project
directory (or, if none is found, the executable's folder) as its working
directory, so relative paths passed to the worker, such as `PYTHONPATH`
entries or local model paths, resolve against that directory rather than the
directory the app was started from. The chosen project directory and `.env`
path are printed to stderr at startup; they are visible only when the app is
started from a console (for example `pnpm run tauri dev`), not from release
builds started from a shortcut or autostart. Existing process/system
environment variables take precedence over values from `.env`. The file is read
into application configuration rather than exported into the process environment.
Child processes receive an allowlist of runtime variables; OpenAI/Gemini keys are
not inherited by Python, input-monitor helpers or GPU probes. Only the selected
speech API key is explicitly passed to an OpenAI-compatible ASR worker.
Hugging Face credentials, when configured, are passed only to the ASR worker for
private/gated models; they are not passed to input-monitor helpers or GPU probes.

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
master switch that enables or disables Dictate correction. Translate、Edit、Askはこのスイッチとは別に、起動時に選択プロバイダーを使います。
必要なAIモードでは、録音前にキー・モデル・ローカルendpointの設定を検査します。この検査は通信しないため、キーの有効性や接続先の稼働は保証しません。
Provider, model, and editing options remain editable
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
the current transcript and selected profile.

補正の入力は `{"transcript": ...}` で区切ります。Conservativeでは、文字列の重なり・長さ・主要な文字種を
使う簡易検査に通らない結果を破棄し、元の文字起こしを使います。Intent-awareではURL・数値・コード・
不確実さを示す表現・検出できる固有名詞も検査します。名前の比較はNFKCで全角・半角の差を吸収し、
辞書に読みがあり実際に発話された場合は「ズーム」から「Zoom」などの表記修正を許可します。
却下の種類は内容を含まないmetric codeとして保存します。これらは構文的な検査であり、意味の保持を
保証するものではありません。重要な文章は挿入結果を確認してください。

For OpenAI correction, reasoning effort can be set to `none`, `low`, `medium`,
`high`, `xhigh`, or `max`. Higher values can improve difficult corrections at
the cost of additional latency and token usage; support depends on the selected
model.

質問や命令も本文として編集するようプロンプトで指示し、簡易検査に通らない回答や翻訳は元文に戻します。
検査を通る誤りもあるため、回答・命令追従を完全に防ぐものではありません。
文体の追加指示は、意味保持の指示とは別に設定できます。

クラウド通信は接続10秒・受信の無通信90秒を上限とし、継続して届く長い回答を90秒で打ち切りません。
出力トークン予算に応じて3〜30分の総上限も設けます。接続・受信エラーや429/5xxは、文字列をまだ
送出していない場合に限り1回再試行します。2秒以下のRetry-Afterには短いjitterを加えて従い、
それより長い待機指定では元のエラーを返します。送出開始後は重複を避けるため再試行しません。

Askの計画は、[OpenAIのStructured Outputs](https://developers.openai.com/api/docs/guides/structured-outputs)・
[Gemini Interactionsの構造化出力](https://ai.google.dev/gemini-api/docs/structured-output)・対応するローカルAPIの
JSON Schemaを要求します。OpenAIの古いモデルや対応を確認できないIDでは、従来どおりプロンプトでJSONを要求します。
受信後はどちらも厳密に解析・権限判定し、形式の修復や別の操作への置換はしません。

To keep API cost and latency low, the editor uses a compact instruction, sends
only dictionary aliases actually found in the transcript, caps optional style
guidance at 500 characters, requests minimal/no reasoning on supported models,
and sets an output-token limit based on the transcript length.

Correction responses are consumed as server-sent events from the OpenAI
Responses API, Gemini Interactions API, and compatible local Chat Completions
servers; non-streaming local responses are also accepted. Correction deltas
stream into the always-on-top recording overlay, and the corrected text is
inserted once when correction completes. With the experimental **Live text
insertion** setting on, the raw transcript is first inserted as provisional
text and the corrected result replaces it. A short-lived helper process observes
keyboard and pointer activity from the start of recording until insertion. It
reports only activity counters (never key values or typed text), ignores this
application's tagged input, and never suppresses an event. The default final
paste only waits for that activity to settle. With live text insertion on, any
typing, click, focus change, or IME composition after recording starts, or an
unavailable monitor, leaves the result on the clipboard instead of modifying
the target.

All fixed prompt text and prompt-size limits are centralized in
`src-tauri/src/correction_prompt.rs` under the `Prompt tuning` block. Edit that
block to tune correction behavior without changing provider/API request code.

Settingsの「プロバイダーのAPIキー」ではマスク入力から保存・削除でき、変更は再起動なしで反映します。
保存先はWindows資格情報マネージャーで、設定・履歴のDBには保存しません。開発版と配布版は別の保存領域を使い、
キーは同じWindowsアカウントのローカル保存とします。[keyringのWindows用アダプター](https://github.com/open-source-cooperative/windows-native-keyring-store)
を使い、同じエントリの操作は1つの専用スレッドで順序付けます。
キーは設定済みの環境変数名に紐づきます。音声とOpenAIで同じ名前を使う場合は同じキーを共有します。
保存済みキーを削除しても、環境変数・`.env`のキーは削除しません。

環境変数・`.env`も引き続き代替として利用できます。`.env`を編集した場合はアプリを再起動します:

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

- [docs/worker_protocol.md](docs/worker_protocol.md) — ワーカープロトコル
- [docs/tasks/README.md](docs/tasks/README.md) — 現在の作業と過去の契約
- [docs/incidents/](docs/incidents/) — 障害調査と根拠

## Privacy and limitations

- Speech recognition is local by default. The OpenAI-compatible ASR backend
  sends recordings to its configured endpoint. Dictate AI correction, Translate,
  Edit and Ask use the configured AI provider: OpenAI/Gemini send transcripts
  and, where relevant, selected text to that provider; the local provider stays
  on loopback. Translate/Edit/Ask use AI even with Dictate AI correction off.
  Privacy requires explicit consent per text provider and speech endpoint before
  a request can be sent. Changing the speech backend or endpoint removes old
  speech consents, so returning to a previous endpoint requires consent again.
- Live transcription preview may decode the same audio repeatedly. It is on by
  default for local backends (including VibeVoice) and off for OpenAI-compatible speech; enabling it
  with cloud speech causes multiple audio uploads. Privacy controls local and
  cloud preview separately; a local opt-in never enables cloud uploads.
- History can be disabled; when disabled, transcript rows are not written
  (failed recordings kept for Retry are the exception described below).
- 履歴は本文検索と追加読み込みに対応しています。録音を削除した後も「原文をコピー」「再補正」「指定言語へ翻訳」を使えます。
  再補正では現在のスタイル設定、または保存済みのスタイルを選択できます。再処理は原文を使い、
  入力先への挿入は行いません。結果は履歴の保持設定に従って別の行に保存され、元の行は変更しません。
  結果は検索・絞り込みにかかわらず表示され、コピーできます。クラウドAIへの送信にはPrivacyでの同意も必要です。
  処理中に削除・期限切れになった元履歴は再保存しません。
- Usage metrics (event type, mode, coarse app category, provider, duration,
  success, and error code; no text) follow History: they are not recorded while History is off, are
  deleted with the History retention window, and are removed by Delete all.
  Diagnostics groups new performance and outcome records by mode, category,
  event and provider; older records without context keep an unknown group.
- New SQLite deletions overwrite freed record content using secure-delete.
  History deletions request a WAL checkpoint/truncation; an active external reader or
  database error can defer this until the next policy pass. Previously freed
  pages from older versions are not retroactively erased. This is not forensic
  erasure of filesystem backups, SSD snapshots or copied databases/audio files.
- Temporary WAV deletion defaults to enabled. When History is on and "Delete
  audio after processing" is off, History keeps a copy of each new recording
  for playback, download, and Retry. Turning it on stops keeping new recordings
  but does not delete recordings already in History; delete them there or let
  History retention remove them.
- "Keep failed recordings for 24 hours" (Privacy, on by default) is the one
  exception to these settings. When transcription fails (Dictate, Translate,
  or Edit), the recording is kept with its audio as a History item for Retry,
  even if History is set to Never or "Delete audio after processing" is on;
  for Edit, the selected text is kept with it, because Retry needs it. Unless
  History with audio keeps it (including when you turn that on later), it is
  deleted, recording and selected text alike, 24 hours after the failure (checked at startup and whenever History is
  listed or written), or as soon as a Retry succeeds; the Retry result follows
  the normal History and audio settings. Turning the setting off deletes such
  recordings; failed recordings are then kept only when History keeps audio.
- The captured target must still be foreground and non-secure at insertion time.
- UI Automation security inspection exists and is used to avoid secure targets.
  Text is entered by clipboard paste only; UI Automation reads the target to
  verify the paste. Controls that cannot be read back (for example some browser
  and code-editor fields) receive one unverified paste, and live text insertion
  in them may keep only the first fragment.
- Elevated applications, password controls, and secure controls are not forced.
- Model download progress is displayed during preparation. Resuming interrupted
  downloads and checksum verification remain future model management work;
  Transformers manages the current cache download.
- The default shortcut is registered at startup, and persisted custom shortcuts
  are re-registered on launch. If another app holds a saved hotkey, the app
  names the action that stays unavailable; saving other settings still works,
  while choosing a new hotkey that cannot be registered is rejected.

## License

本プロジェクトの独自コードは [MIT License](LICENSE) で公開します。
同梱の VibeVoice 参考サンプルには[上流の著作権表示とMITライセンス](docs/reference/vibevoice-samples/LICENSE)を保持しています。
外部の依存パッケージ、ダウンロードするモデル・音声データには、それぞれのライセンスが適用されます。
