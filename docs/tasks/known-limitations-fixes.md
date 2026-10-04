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
- 1: extend `injection.rs` Windows backend with a Win32 Edit fallback (`injection/native_edit.rs`: WM_GETTEXT/EM_GETSEL/EM_SETSEL reads and selection, WM_PASTE/WM_CLEAR edits) and a `SafetyPolicy` variant chosen in `batch.rs`; reuse batch verification. Native tests in `native_tests.rs`.
- 2: add a proper-noun check beside URL/number/code/uncertainty in `correction.rs`, reusing occurrence roles for explicit repairs.
- 3: extend `asr_worker/api.py` with SSE streaming and an optional backend segment iterator.
- 4: extend `asr_worker/download.py` (resume bytes, snapshot completeness, pending-verification marker, `HfApi.verify_repo_checksums`, one repair re-download) and the existing progress event (`stage: verify`, `resumedBytes`) and load notices.
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

Status (2026-10-04):
- Criteria 1-9 done. All work is committed on `fix/known-limitations`; nothing is pushed or merged.
- 1: unit tests cover policy selection, quiet/non-quiet replacement, cancellation, selection replacement, the checkpoint requirement and UTF-16/CRLF/surrogate mapping; a normal test drives WM_GETTEXT/EM_GETSEL/EM_SETSEL/WM_CLEAR on a real hidden Edit control. The opt-in end-to-end test (`native_edit_replaces_provisional_draft_with_quiet_input`) is unrun: the probe window could not take the foreground in this session (foreground lock), and the test stopped at its focus assertion before any input. WM_PASTE and real IME composition behavior are unverified.
- 2-8: verified by the tests named in the criteria. Real provider output, a real interrupted Hub download and a real repair are unverified; a read-only `verify_repo_checksums` call on the cached faster-whisper snapshot returned no mismatches (2.4 s).
- 9: cargo fmt --check, clippy (only pre-existing warnings in touched files), cargo test --lib 324 passed / 5 ignored, pytest 74 passed, tsc, component tests (11), pnpm build and git diff --check pass.
- Audit: Codex `gpt-6-sol` is unavailable for this ChatGPT account (shared knowledge), so one independent Opus agent audited `330feb4..cc65a66`: no Blocker; H1 (an interrupted download left a snapshot folder that looked cached, so resume was never reported and pending verification passed with files missing) fixed in 880694a; M1 (injected keys could reach a composition left open before the operation) mitigated in 1761721 by WM_PASTE/WM_CLEAR; M2 (false proper-noun rejections) fixed in 7c25994; L1/L2/L3/L4/L6 fixed (880694a, README, 1761721, 1028ab2, d71f75f); L5 is this update. The same auditor's bounded recheck confirmed H1 and M2 resolved and found no defect in the M1 change; its new Medium (a stale `.incomplete` blob made a complete model download and re-verify on every load, failing offline) is fixed by deciding completeness from required files and the pending mark only. Remaining Low name-check gaps: kana readings of three or more characters can still match inside other words, and a prompted surface directly followed by letters or digits (Vue3) is not recognized as that term.
- Decisions (may decide myself): proper nouns are Latin tokens with an inner capital or capitalized away from a sentence start (excluding single letters, "I'" contractions and OK, splitting possessive 's), plus prompted dictionary terms (non-ASCII readings of at least three characters); katakana and unregistered Japanese names are not detected. The pending-verification marker lives in `HF_HOME/local-voice-input/pending-verification/`; a mismatched file is force-downloaded once (a corrupted public cache file, not user data); an unreachable Hub fails a pending load. The user-chosen "known-closed IME counts as inactive" rule was dropped after the audit showed it had no effect under the quiet-input requirement; behavior stays at or below the approved risk.
- Next: (1) on a real desktop, run the ignored native Edit and Chromium tests and verify Dictate/Edit in a classic Edit control with and without an IME composition; (2) merge `fix/known-limitations` into `dev` when the user asks.
