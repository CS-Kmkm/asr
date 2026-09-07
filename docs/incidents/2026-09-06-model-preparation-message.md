# Startup always announced a possible model download

- Date: 2026-09-06
- Environment `[stated]`: Windows 11, faster-whisper backend (`large-v3-turbo`), Hugging Face cache at `%USERPROFILE%\.cache\huggingface\hub`.
- Symptom `[derived]`: Every autostart showed "Preparing the speech model. The first use may download model files." even though the model files were already cached and nothing was downloaded.
- Cause `[derived]`: `ensure_model_loaded` emitted that single hard-coded message on every load (`src-tauri/src/commands.rs`). The message merged two different states — reading a cached model into memory, and downloading it on first use — and no component knew which one was happening: `WhisperModel` resolves and downloads the model internally, so neither Rust nor the worker distinguished the two.
- Non-causes `[derived]`: The cache was intact (`models--mobiuslabsgmbh--faster-whisper-large-v3-turbo`, 1.6 GB) and no repeated download occurred; loading the model at every launch is expected because model state lives in the worker process, which is spawned per run.
- Fix `[decision]`: Resolve the model files in the worker before constructing `WhisperModel`. A cached snapshot loads directly, a missing one is downloaded through `huggingface_hub.snapshot_download` with a `tqdm_class` hook. The worker reports `stage: "download"` (with byte counts) and `stage: "load"` as progress notifications on the JSONL protocol (`docs/worker_protocol.md` §2.2.1); Rust forwards them as `model-progress` events, shows "Loading the speech model." by default and switches to the download message only when a download is actually reported.
- Reproduction `[derived]`: Run the worker with a warm cache (`python -m asr_worker --backend faster-whisper`, `load` request) and with an empty one (`HF_HOME` pointing at a fresh directory, `ASR_MODEL_ID=tiny`).
- Verification `[derived]`: Warm cache emitted only `{"stage":"load"}` before the `load` response; the empty cache emitted `stage: "download"` events rising to 78.2 MB against a reported total of 75.5 MB, then `{"stage":"load"}`. Byte counts can slightly exceed the announced total on Xet transfers, so the progress bar clamps the ratio at 100%.

## Follow-up: version skew broke the installed build

- Symptom `[derived]`: The first launch after the fix failed with `local operation failed: worker protocol failed: invalid type: null, expected struct WorkerError`.
- Cause `[derived]`: Windows autostart runs `src-tauri/target/release/local-voice-input.exe`, built before this change, while the worker it spawns is the *repository source* (ADR-0003). The old client read the new `event: "progress"` line as the response, found no `ok` and no `error`, and failed to deserialize `null` into `WorkerError`. The protocol has no version handshake (`docs/worker_protocol.md` §5.1).
- Fix `[decision]`: Make notifications opt-in. The worker emits them only when the client sets `ASR_WORKER_PROGRESS=1`, which `WorkerCommand::python` always does; without it the worker behaves exactly as v1. A desktop binary of either generation now works with a worker of either generation.
- Verification `[derived]`: A `load` against the real worker emitted no notification without the variable and `{"stage":"load"}` with it.
- Remaining action `[stated]`: The autostart binary must be rebuilt (`pnpm tauri build`) to get the new messages and the progress bar; the compatibility gate only keeps the old binary working.
