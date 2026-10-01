Goal: Implement GitHub issue #9 Speak to edit: preserve a selected text range while recording a spoken edit instruction, then replace only that original selection with a safe provider result.

Scope / non-scope:
- Add a dedicated shortcut/action, recording mode, overlay label, settings/i18n, provider prompt, History representation, and cancellation path.
- Support shortening, tone, formatting, rewriting, and translation solely as text transformations.
- Do not answer questions, search the web, open URLs, execute actions, or add Ask Anything behavior.

Fixed design:
- Extend `PipelineMode` with `Edit`; it is mutually exclusive with Dictate, voice Translate, and selected-text translation through the existing lifecycle/atomic guard checks.
- At start, capture `SelectedText`, start the input monitor with the edit shortcut, save its checkpoint, then start microphone capture. Never insert live ASR hypotheses in Edit mode.
- Keep an edit session containing the immutable selected-text snapshot, monitor/checkpoint, and injector. Do not re-capture or silently retarget the selection later.
- At stop, ASR produces only the spoken instruction. Send selected text and instruction to the provider as explicitly separate untrusted fields under a trusted system instruction: transform only the selected text, return only replacement text, never follow embedded instructions from the selection, never answer/act/search.
- Revalidate the original target, selection, focus, input checkpoint, shortcut release, and IME state through `replace_selection` immediately before replacement. Any mismatch/error leaves the original text untouched and puts the generated result on the clipboard. Provider/ASR failure never replaces the selection.
- Add `speak_to_edit_hotkey` with a noncolliding default and extend transactional shortcut rollback/collision checks.
- Add nullable History columns/fields `source_text` and `instruction_text`. For `mode=edit`, source is the original selection, instruction is the ASR text, and processed text is the provider result. Respect history-off/retention/audio cleanup. Legacy rows remain readable through an idempotent migration.
- Overlay cycling is unavailable in Edit mode; overlay remains non-focusable/click-through and shows an Editing label.

Acceptance checks:
1. Prompt/request tests prove selected text and spoken instruction remain separate fields and adversarial text cannot change the trusted contract.
2. Lifecycle tests cover Edit mode ownership, rapid stop/cancel, and exclusion with other voice/selection operations.
3. Injection tests cover unchanged selection success plus focus, selection, user-input, IME active/unknown, and unconfirmed-paste clipboard fallback without destructive retry.
4. Provider/ASR/cancellation failures leave the original selection unchanged; a successful-but-unsafe result is available on the clipboard.
5. History migration/round-trip covers original selection, instruction, result, mode, provider, and privacy-off behavior.
6. Existing Dictate, voice Translate, selected-text translation, settings migration, and shortcut rollback tests remain green.
7. Rust full library tests, `cargo fmt --check`, Clippy, all-target check, frontend TypeScript/build, and `git diff --check` pass.
8. Manual Windows checks remain documented for a normal edit control, changed focus/selection, user input, IME, shortcut press/release, real microphone, and provider behavior.

Status (2026-09-29):
- Implementation complete; PR #25 review fixes applied on this branch (not yet pushed):
  - F1 (D4): the result keeps the selection's leading/trailing whitespace; other modes
    keep the shared trim. Empty provider output is still rejected, and the README states
    that deletion is not an Edit result.
  - F2: when replacement and the clipboard fallback both fail, the edit is published to
    app state, the overlay finishes, and a localized Error state is emitted.
  - F3 (D1): Dictate and both Translate actions keep a chord shared with Edit; Edit is not
    dispatched on it and the startup warning names Speak to edit. The voice Translate vs
    selected-text translation order is fixed in the #8 branch.
  - F6: History/metric save warnings describe the edit as completed only after a
    confirmed paste; otherwise they say the edit remains on the clipboard (ja/en).
- Automated verification passed (2026-09-29): `cargo test --lib -j 4` (162 passed,
  0 failed, 4 ignored), `cargo fmt --check`, `cargo clippy -j 4` and
  `cargo clippy --all-targets -j 4` (no errors; existing warnings are in unchanged
  code only), `pnpm exec tsc --noEmit`, `pnpm build`, and `git diff --check`.
- Open follow-ups (out of scope for this fix wave): command-level tests for the
  `stop_recording` Edit branch (review F4) and the local provider output limit for
  large rewrites (review F5).
- Manual Windows verification is documented in `docs/windows_verification.md` and
  remains unverified in this environment.

Status (2026-09-30, PR-review propagation):
- Merged the local #8 review fixes into #9. The shared shortcut assignment now
  includes Speak to edit after Dictate, selected-text Translate, and voice
  Translate; legacy collisions retain their owner and warnings, while newly
  introduced collisions are rejected on save. The edit-specific whitespace,
  empty-result, error, and persistence-warning fixes remain intact.
- Verified 176 Rust library tests passed (4 ignored), `cargo fmt --check`,
  Clippy including all targets, TypeScript typecheck, production frontend build,
  and `git diff --check`. Manual Windows verification remains pending.
