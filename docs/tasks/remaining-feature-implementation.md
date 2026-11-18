Goal: Implement remaining application features identified after #82-#85, before product benchmarking.

Scope: confirmed style preferences from submitted History edits; process CPU/RAM/GPU measurement for benchmark tools; streaming recognition for Edit/Ask; an optional official Qwen streaming connector; explicit local llama.cpp process management. The user requested a vLLM/llama.cpp comparison; Windows-local text correction targets llama.cpp while existing OpenAI-compatible URLs continue to support separately operated vLLM. #108 model architecture changes/training remain deferred.

Constraints: Preserve all prior uncommitted changes and pre-existing benchmark edits. No commits/pushes, no training, no model-weight downloads, no cloud inference or paid provisioning. Local server startup requires explicit settings/action and uses a configured executable/model only, shell-free and loopback-bound. Do not claim native Windows vLLM support. Personalization keeps only fixed, reviewed style codes and scope/modes, never edited full text. Learning/History OFF prohibit new learning. Models must not receive new destinations implicitly.

Reuse / creation plan: Extend existing style profiles, History editing, settings patch/revision events, streaming session and benchmark harness. New helpers are justified for structured style detection, process resource sampling, an official-Qwen protocol adapter and owned local-server lifecycle. Do not replace existing insertion guards or rewrite previous task records.

Acceptance criteria:
1. Submitted edits can propose narrow deterministic style changes; only explicit approval persists fixed preferences with scope/modes; delete/OFF behavior and no-full-text persistence are tested.
2. Benchmark tools can optionally sample process-tree CPU/RSS and observed GPU memory with unavailable values represented as missing; production benchmarks are not run as part of implementation.
3. Edit/Ask can use the existing streaming recognizer while keeping their own captured selection/input guards and final-only action processing; failure falls back to complete audio.
4. Qwen streaming is explicitly connected via the official supported runtime/API, with cumulative hypotheses, bounded I/O/cancellation, correct capability labels and safe fallback. Platform/runtime prerequisites remain explicit.
5. Local llama.cpp lifecycle has explicit configuration/start/stop/status, loopback-only endpoint and owned-process cleanup; missing executable/model/readiness errors are recoverable. No installer/model download is performed.
6. Appropriate tests/build/format checks and independent audit pass; supported findings fixed. Actual inference, hardware performance, foreground Windows checks and research #108 remain separate.

Open questions: May decide myself: first text-server implementation uses llama-server; vLLM is a separately operated OpenAI-compatible server. Official Qwen streaming needs a Linux vLLM environment; choose a connector compatible with the official demo API rather than changing model architecture or installing unsupported Windows forks.

Status: Implementation complete (2026-11-17); all changes remain uncommitted. Product benchmarking and actual runtime/hardware verification were not performed.

Completion report:
- AC1: History edits propose deterministic fixed formal/casual and list/paragraph preferences. Explicit confirmation stores only preference/scope/modes; no edited body or candidate is persisted. Mode/scope resolution, more-specific manual profiles, History/learning OFF and deletion are covered by tests. Existing manual profiles and spelling learning are preserved.
- AC2: Optional --record-resources on batch/replay tools samples the root and observed descendants using optional psutil, and per-process NVIDIA memory when reported. Missing readings stay null; first CPU readings baseline lifetime usage. JSON declares sampled/shared-RSS/external-server limitations and replay startup measurement phase. Existing user benchmark changes are preserved; no product benchmark ran.
- AC3: Recognition-only Edit/Ask owners retain the mode's selection/input monitor and never insert during recording. Existing verified-final reuse and complete-audio fallback apply; Retry stays batch.
- AC4: Explicit Qwen official demo API connector uses numeric loopback HTTP, no API key, automatic language, cumulative hypotheses and bounded cleanup. Identity is qwen-demo:unverified and final reuse is prohibited. The complete recording is processed again at stop. Privacy live-preview opt-in is required for streaming uploads; otherwise only final batch runs. Separate vLLM operation/tunneling is required, not provisioned here.
- AC5: Configured existing llama-server/GGUF can be saved, started, stopped and queried. Startup is opt-in, shell-free, loopback-bound, cancelable and readiness-checked; shutdown only stops the owned process. No download or external-process attachment is performed. Main-window IPC permission manifest includes the new commands.
- AC6: Independent native read-only audit and scoped follow-ups completed; no outstanding supported finding. Fixed first-observed CPU inflation, server identity including auto_start, learned/manual scope precedence, Qwen model metadata and preview prerequisites. Main permission omission was found by the Node regression and fixed. External Claude source transmission was not used, consistent with the existing task policy.

Verification evidence:
- Rust full library regression: 955 passed, 8 ignored (.ai/remaining-rust-full-v2.log). Final Qwen preview-policy streaming tests: 5 passed (.ai/remaining-streaming-final.log).
- Frontend full regression: 218 passed (.ai/remaining-ui-new-v1.log); final related UI tests and production build are recorded in .ai/remaining-ui-final.log and .ai/remaining-build-final.log.
- Python integrated regression: 454 passed, 1 skipped, 233 subtests (.ai/remaining-python-full-v3.log). Offline cached pytest was used after dependency resolution stalled; no project dependency files were changed.
- Node integration/permission tests: 41 passed (.ai/remaining-node-v2.log). Clippy all targets with -D warnings (.ai/remaining-clippy-v2.log), cargo fmt --check and git diff --check passed. Final preview gating change is covered by scoped Rust tests and introduces no new lint-sensitive construct.

Remaining verification: Real llama-server readiness/inference and model memory needs, official Qwen service calls, real Nemotron initialization, foreground Windows selection/IME/clipboard checks, hardware resource sampling and #81 product benchmarks. Model quality/default adoption is not decided. #108 architecture/training work remains deferred. No commit, push or model-weight download occurred.
