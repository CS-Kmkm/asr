Goal: Remove eight known limitations and Low follow-ups left after the dev integration of issues #7-#15, on `fix/known-limitations` (from `dev` 330feb4).

Scope / non-scope:
- Scope (user request 2026-10-04):
  1. Native Win32 Edit controls without a UI Automation text range cannot be safely replaced (clipboard-only).
  2. Intent-aware correction protects proper nouns only through the prompt.
  3. The local OpenAI-compatible ASR server rejects `stream=true`.
  4. Model downloads have no resume indication and no checksum verification.
  5. A Retry whose AI correction fails fails as a whole instead of keeping the transcript.
  6. Deleting all History keeps dictionary candidates.
  7. Ask search queries lose words because command words are stripped anywhere (for example 検索エンジン最適化).
  8. Speak to edit reports one generic failure for every cause.
- Non-scope: other Low follow-ups (Ask search Retry stores the raw instruction, Ask/Translate failure wording), Japanese/katakana named-entity recognition, RichEdit-specific message handling, merging into `dev`/`main`, pushing.

Constraints:
- User decisions (2026-10-04): native Edit replacement is allowed when the IME composition state is unknown only if the input monitor shows no external keyboard/pointer input since the operation checkpoint (recording start for voice modes, hotkey for selected-text translation); a known-closed IME is treated as inactive. Commit per intent on a new branch from `dev`; no push.
- Preserve existing focus, input, IME, exact-content, pending-clipboard and no-retry guards everywhere else. Never log transcripts, selected text, clipboard data or API keys.
- Rust checks use `-j 4`; do not rebuild a test executable while it runs.
- Conventional Commits in English with the Claude attribution trailer.

Reuse / creation plan:
- 1: extend `injection.rs` Windows backend with a Win32 Edit fallback (`injection/native_edit.rs`: WM_GETTEXT/EM_GETSEL/EM_SETSEL, IME open status via the default IME window) and a `SafetyPolicy` variant chosen in `batch.rs`; reuse batch verification. Opt-in native test in `native_tests.rs`.
- 2: add a proper-noun check beside URL/number/code/uncertainty in `correction.rs`, reusing occurrence roles for explicit repairs.
- 3: extend `asr_worker/api.py` with SSE streaming and an optional backend segment iterator.
- 4: extend `asr_worker/download.py` (resume bytes, pending-verification marker, `HfApi.verify_repo_checksums`, one repair re-download) and the existing progress event (`stage: verify`, `resumedBytes`).
- 5: `commands.rs` Retry keeps the transcript as `faithful_fallback` on correction failure.
- 6: `storage.rs` purges candidates/decisions on delete-all (production candidates carry no history id, so single deletes are not linked).
- 7: `ask.rs` strips command words, site names and connectors only at the query edges.
- 8: `commands.rs` maps `CorrectionError` to distinct localized Edit messages (`i18n.tsx`).

Acceptance criteria:
1. Native Edit: unit tests cover policy selection (unknown IME + native Edit + quiet monitor permits destructive edits; input since checkpoint, missing monitor, non-native target or active IME refuse) and CRLF/UTF-16 position mapping; an opt-in interactive test replaces a provisional draft in a real Edit control. Verify: `cargo test --lib`; ignored test run on the desktop when available, otherwise reported as unrun.
2. Proper nouns: intent-aware fixtures reject a dropped or invented Latin proper noun or prompted dictionary term, accept capitalization fixes, explicit repairs and merged duplicates; conservative mode unchanged. Verify: `cargo test --lib correction`.
3. Streaming: `stream=true` returns `transcript.text.delta` events per segment and one `transcript.text.done`; unsupported formats are rejected; non-stream behavior unchanged. Verify: pytest with the mock/fake backend.
4. Downloads: interrupted partial files are reported as resumed bytes; a downloaded snapshot is verified; a mismatched file is re-downloaded once, persistent mismatch fails with `model_checksum_mismatch`; unverifiable downloads stay pending and are verified on the next load; previously cached legacy snapshots load without network. Verify: pytest with a fake hub; frontend shows verify/resume labels (type check + build).
5. Retry: a failing or fact-check-rejected correction stores and returns the transcript with mode `faithful_fallback`; cancellation still cancels. Verify: Rust unit test of the fallback helper.
6. Delete-all removes pending candidates and decisions; confirmed dictionary entries stay; the confirmation names suggested spellings. Verify: storage test.
7. Ask: 「グーグルで検索エンジン最適化を検索」→「検索エンジン最適化」, "Search Google for tools for kids" keeps "for", site names inside queries survive; existing template tests pass. Verify: `cargo test --lib ask`.
8. Edit failures: missing key, output limit, invalid endpoint, unreachable provider, HTTP status, invalid response and unsupported provider produce distinct messages in English and Japanese. Verify: Rust unit test + `tsc`.
9. Whole tree: cargo fmt --check, clippy, cargo test --lib, tsc, component tests, pnpm build, pytest, git diff --check pass; one independent audit with no unresolved Blocker/High.

Open questions:
- Native Edit IME policy and commit handling: decided by the user (see Constraints).
- Proper-noun detection rule, verification marker location and repair behavior: may decide myself; decisions recorded in Status.

Context:
- `docs/tasks/dev-integration.md` (integrated state), `docs/tasks/correction-all-apps.md` (native Edit evidence: TextPattern absent, IME unknown), issue #9/#10/#12/#13/#15 closing comments (Low follow-ups).

Status:
- In progress. Next: implement 6, 5, 7, 8, 2, 3, 4, 1 in that order, committing each.
