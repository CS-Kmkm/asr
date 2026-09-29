Goal: Implement GitHub issue #7 by adding a configurable local-LLM correction provider that preserves the existing correction contract without sending transcripts to an external service.

Scope / non-scope:
- In scope: persisted settings and migration defaults; Settings UI and Japanese/English copy; a local OpenAI-compatible correction transport; dictionary hints; streaming preview; cancellation; raw-transcript fallback; focused regression tests; README configuration instructions.
- Out of scope: ASR backend changes; bundling or installing a model server; Translate/Ask Anything/Speak to edit UI; intent-aware correction from issue #15; unrelated cleanup.

Constraints:
- Work only on branch `feat/issue-7-local-llm-correction` in `C:/Users/Koshi/asr/.worktrees/issue-7-local-llm-correction`.
- Extend the existing Settings and correction pipeline; do not introduce a second correction workflow.
- Existing OpenAI and Gemini behavior must remain compatible.
- Local mode must permit an unauthenticated endpoint and must reject endpoints that can resolve directly to an obvious non-local host.
- API secrets remain environment-variable references and are never persisted as secret values.
- Preserve the existing provider-agnostic caller behavior for preview, cancellation, history metrics, and fallback.

Reuse / creation plan:
- Extend `src-tauri/src/types.rs`, `src/types.ts`, and `src/api.ts` for settings.
- Extend `src-tauri/src/correction.rs` for provider transport and response decoding.
- Extend `src/components/AiCorrectionSettings.tsx` and `src/i18n.tsx` for configuration UI.
- Extend validation in `src-tauri/src/commands.rs` and existing unit tests near each contract.
- Update `README.md`; add no new runtime dependency unless the existing HTTP/URL stack cannot express the endpoint contract safely.

Acceptance criteria:
1. Settings offers a Local (OpenAI-compatible) correction provider with endpoint and model configuration; verify with `pnpm build` and code review.
2. Local correction uses only an accepted local endpoint and can run without an API key; verify with Rust unit tests for URL validation/request construction.
3. Local responses support non-streaming and streaming text, using the existing preview callback and cancellation path; verify with focused Rust unit tests at the provider parsing/stream-event seam.
4. Dictionary hints and all independent correction switches continue through the shared prompt builder; verify existing prompt tests and the Rust test suite.
5. Local provider failures retain the existing raw-transcript fallback; verify the unchanged provider-agnostic command path by review plus its existing tests.
6. Existing OpenAI/Gemini request and parsing tests remain green; verify the Rust test suite.
7. Legacy settings deserialize with safe local-provider defaults; verify a migration/default test.
8. README documents a compatible local server contract and privacy boundary; verify documentation review.

Open questions:
- Resolved: use OpenAI Chat Completions as the local compatibility surface because Ollama, llama.cpp, LM Studio, and similar local servers commonly expose it without requiring the cloud Responses semantics.
- Resolved: accept only numeric IPv4/IPv6 loopback base URLs and append `/chat/completions`; reject DNS names, credentials, query/fragment data, proxies, and redirects to keep the local-provider privacy claim enforceable without DNS-resolution races.

Context:
- GitHub issue #7 is the authoritative feature request; roadmap dependencies are #17 and #8-#10.
- Relevant existing paths: `src-tauri/src/correction.rs`, `src-tauri/src/correction_prompt.rs`, `src-tauri/src/types.rs`, `src-tauri/src/commands.rs`, `src/components/AiCorrectionSettings.tsx`, `src/types.ts`, `src/api.ts`, `src/i18n.tsx`, `README.md`.
- Shared memory `C:/Users/Koshi/agent-memory/tasks/asr--live-dictation.md` describes prior streaming-insertion work; do not alter its artifacts or resurrect its paused hands-on verification scope.

Status (2026-09-29):
- Backend settings, validation, unauthenticated local transport, non-streaming parsing, streaming deltas/completion, error classification, defaults, and focused tests are implemented.
- Frontend settings types/defaults, provider selector, local endpoint/model controls, Japanese/English copy, and README privacy/server-contract documentation are implemented.
- Independent completion review found three response-safety gaps: non-text/truncated finish reasons were accepted, response-body reads were not cancellation-aware, and terminal SSE events did not end collection immediately. All three were fixed and the reviewer confirmed no remaining blocker. Verification on 2026-09-22 passed (full Rust library suite 128 passed, 4 ignored).
- PR #20 review fixes (2026-09-29, `docs/tasks/pr-review-fixes.md`):
  - F1: a leading `<think>...</think>` block is withheld from the streaming preview and removed from streaming and non-streaming results, including tags split across deltas; a leading block that never closes falls back to the raw transcript.
  - F2: `finish_reason: "length"` is a distinct `OutputLimit` error with Japanese/English status and completion text; local requests use a 10 s connect timeout and a 90 s read (idle) timeout with no total deadline, while OpenAI/Gemini keep the 90 s total timeout.
  - F3: the local model ID field is a draft committed on blur or Enter; Escape reverts it.
  - F4: empty or suppressed local content deltas are not forwarded to the preview.
- Verification passed at `a6313fb`: `cargo fmt --check`; `cargo clippy -j 4` (no errors; warnings are pre-existing); `cargo test --lib -j 4` (138 passed, 4 ignored); `pnpm exec tsc --noEmit`; `pnpm build`; `git diff --check`. Intermediate fix commits were also tested (135 and 137 passed, 4 ignored).
- Remaining: F5 test backfill (in-flight cancellation through the `select!` branch, non-streaming local responses through `correct_transcript`, fallback and error-kind mapping) is outside this fix wave. Live local servers (Ollama / LM Studio / llama.cpp) and the UI behavior remain manual checks.
