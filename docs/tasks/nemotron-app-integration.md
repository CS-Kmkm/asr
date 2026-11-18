Goal: Verify the desktop application's Rust streaming owner against the cached real Nemotron worker, filling the gap between Python engine replay and native microphone/insertion acceptance.

Scope: Implement an explicit opt-in Rust integration test that uses JsonlTranscriber, streaming::Session and the AudioCapture/FinalTail interfaces with hash-pinned public PCM audio. Exercise start/push/stop-tail/flush, rejected reuse after settings changes and a fresh subsequent session. Run CPU then explicit CUDA with the existing prepared interpreter. Fix a production defect only if this flow reproduces it. UI selection and per-mode wiring already exist; do not duplicate them or add features without a demonstrated gap.

Constraints: Preserve all unrelated uncommitted work, normal settings, weights and results. No downloads, environment synchronization, installs, cloud requests, commits, pushes or Issue changes. Use the production managed model cache authorized by the user; force offline model access. Do not launch microphone capture or foreground input from the agent. An ignored integration test must fail clearly when explicitly requested without prerequisites, rather than silently skip/pass. Keep source fixtures/results in new paths; do not print public reference/recognized text into logs.

Reuse / creation plan: Extend the existing private streaming-module tests using the production JsonlTranscriber and fake capture implementations. Reuse the prepared one-clip WAV through an explicit path and check its fixed SHA256 outside the Rust test before execution. No new dependencies, duplicate inference adapters, shell wrappers or model downloads. Record test output and exact environment/source scope under a new .ai app-integration directory.

Acceptance criteria:
1. The normal suite contains deterministic application streaming lifecycle tests without model or microphone prerequisites; the real-model test is ignored by default and explicit prerequisite omissions fail with actionable names.
2. The real-model test drives the production recording Session through AudioCapture snapshots and sealed FinalTail, validates a nonempty final from Nemotron, refuses final reuse after a relevant settings change and allows a new session after cleanup. The unchanged full-audio fallback remains the pipeline's responsibility; do not claim it ran unless actually exercised.
3. Execute the opt-in test with the cached model on CPU and explicit CUDA, preserve both outcomes and verify worker shutdown. Preserve timing/quality/GPU context limits; this is no GUI, shortcut, physical microphone or insertion acceptance.
4. Relevant Rust checks and one independent code/evidence audit pass, with supported material findings resolved. Keep native application acceptance pending.

Open questions: May decide myself: use the same public 4.8-second clip as the engine smoke and an explicitly provided interpreter/device; use deterministic capture pacing sufficient to split pre-stop audio from the sealed tail. No need to edit runtime environments or application profiles.

Context: docs/tasks/nemotron-runtime-preparation.md; src-tauri/src/live_dictation/streaming.rs; src-tauri/src/asr.rs; src-tauri/src/audio.rs; src-tauri/src/pipeline/stop.rs. The earlier Python smoke does not prove this Rust owner path.

Status: AC1–AC4 verified on 2026-11-18 for the Rust session/worker boundary. The development checkout uses ASR_PYTHON or .venv in normal operation. This test uses the provided interpreter. Native application acceptance remains pending.

Reproduction: Set ASR_TEST_NEMOTRON_PYTHON to an absolute path to the prepared interpreter and ASR_TEST_NEMOTRON_WAV to an absolute path to a mono PCM16 speech WAV. Verify the WAV hash against the selected fixture manifest. Set ASR_TEST_NEMOTRON_DEVICE to cpu, run the command below, then use cuda for a separate invocation. The worker verifies the cached model pins and uses offline mode. The actual run set ASR_MANAGED_MODEL_HOME to the production LocalVoice/models path under LOCALAPPDATA.

```powershell
cargo test --offline --manifest-path src-tauri/Cargo.toml --lib cached_nemotron_application_owner -- --ignored --nocapture
```

Evidence:
- `.ai/nemotron-app-integration-20261118-980bb028` records the fixed clip SHA256 and CPU/CUDA test logs. Both trials passed with nonempty Nemotron finals before and after a session whose final reuse was rejected after an input-gain setting change.
- CPU reported ONNX Runtime 1.24.4 CPUExecutionProvider. CUDA reported CUDAExecutionProvider and NVIDIA GeForce RTX 3060 through worker health. Each test's shutdown call returned success. A separate post-run process check recorded zero matching sherpa-onnx Python processes. This evidence does not distinguish graceful worker exit from forced termination inside the shutdown implementation.
- The fixture feeds the first three quarters of the clip through the application's streaming session, then seals the remainder through FinalTail. It uses accelerated snapshots. A native capture and insertion trial, real-time latency measurement, and full-audio pipeline fallback remain outside this test.
- `cargo fmt --check`, `git diff --check`, `cargo clippy --offline --tests -- -D warnings` and the live_dictation suite passed. The suite reported 78 passed and 3 ignored. The opt-in test passed separately for each device.
- Automatic review denied sending repository source and test logs to the external Claude service. A read-only workspace reviewer found no blocking test-path defect and flagged incomplete provenance and shutdown wording. `post-run-verification.json` records the interpreter, model home, checkout revision and dirty status, hashes for selected source files observed after execution, and the separate process check. These hashes do not freeze the loaded code image, and the process check is an observation after both runs.
- The review also noted that the fake worker would accept a new session even without cleanup. The deterministic test now verifies that the rejected session is closed with the same session ID before the next start, with no flush for the rejected session. The recorded post-run hashes precede this fake-only assertion change. The real-model test behavior was unchanged by that change.
