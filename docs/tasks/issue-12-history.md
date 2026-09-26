Goal: Implement GitHub issue #12 as a privacy-safe History manager with typed filtering, deterministic retention, DB-owned audio artifacts, and output-only retry.

Scope / non-scope:
- Add All/Dictate/Translate/Edit/Ask filters, visible copy controls, delete-one/delete-all, the required retention choices, retained-audio playback/download, and retry.
- Reuse the existing `mode` column as the canonical operation filter. Do not add a duplicate operation-kind column.
- Do not add feedback collection, arbitrary filesystem paths, shell/open-directory actions, target reinsertion, search launch during retry, or provider/settings/credential snapshots.

Fixed design:
- Replace the legacy boolean/day pair with one canonical retention enum: `never`, `24_hours`, `1_week`, `1_month`, `1_year`, `forever`. `never` is History-off and immediately removes text rows and associated audio. Finite options use UTC 1/7/30/365-day cutoffs; `forever` skips age cleanup.
- Migrate raw legacy settings before normal deserialization: disabled becomes `never`; enabled day values map to the largest finite preset not exceeding the old limit, never to `forever`. Persist normalized V2 settings without discarding unrelated fields.
- Extend history with nullable `audio_filename` and `retry_of_id`; retry links use `ON DELETE SET NULL`. Persist only random relative `.wav` filenames under the app-data `history-audio` directory, never absolute paths.
- Retention is enforced at startup, settings changes, before filtered listing, and after inserts. Delete-one, delete-all, retention, and History-off remove owned audio. A DB-backed pending-deletion queue records locked-file failures and is retried at startup.
- Temporary recording audio remains cleanup-owned until a history commit succeeds. Stage a copy inside `history-audio`, atomically rename it, commit the row/file association, and remove finalized files on DB rollback. Startup reconciliation deletes unreferenced files and clears missing associations without exposing paths.
- Each retained history row owns one audio file, including retry rows. No reference counting or shared artifact ownership.
- Playback/download accept only a history ID and return a bounded WAV payload plus a fixed sanitized filename. The frontend creates and revokes Blob URLs for `<audio>` and download; no command accepts a path.
- Retry exclusively claims the pipeline lifecycle and uses its cancellation token. It copies the source audio to guarded temporary storage before processing so row deletion cannot race the read.
- Retry uses current ASR/correction/dictionary/profile settings while preserving the original semantic operands: mode, selected source, target language, Ask action/site, and instruction role. It never inserts into the old target or opens a browser. Success creates a new linked row; cancellation or failure creates neither a row nor artifact.
- A retried Ask search produces the normalized query/result for History only and never launches the site. History and audio settings at retry completion decide whether the new row/audio is retained.

Acceptance checks:
1. Idempotent schema/settings migrations cover every legacy retention value class and preserve unrelated settings/history fields.
2. SQL filtering happens before the limit and covers all five filter values; delete-one and delete-all report missing rows safely.
3. Retention tests cover all presets, exact cutoff boundaries, History-off, text-only retention, audio retention, and startup/settings/insert enforcement.
4. Artifact tests cover relative-name/path-traversal rejection, commit rollback, missing/orphan reconciliation, row/audio deletion, and durable retry of locked-file deletion.
5. Retry tests cover lifecycle exclusion/cancellation, current settings plus original semantics for Dictate/Translate/Edit/Ask, no injection/search side effects, new `retry_of_id`, and no row/artifact on failure.
6. Playback/download tests cover ID-only access, missing/deleted audio, bounded bytes, WAV metadata, and no absolute-path exposure.
7. History UI exposes filters, explicit Copy/Retry/Play/Download/Delete controls, confirmed Delete all, and localized English/Japanese copy.
8. Full Rust/frontend checks and `git diff --check` pass. Manual Windows checks cover real audio playback/download, locked-file recovery, provider retry quality, and cancellation timing.

Context:
- Base is Issue #10 commit `6411024`, so History already contains typed Dictate/Translate/Edit/Ask fields.
- Relevant seams are `src-tauri/src/{commands,lib,lifecycle,storage,types,audio}.rs`, `src/{App,api,types,i18n}.tsx`, and `src/pages/HistoryPage.tsx`.
- `delete_audio_after_processing` remains the explicit audio-retention privacy switch; retention `never` always overrides it.

Status (2026-09-27): Implemented and independently audited. Settings persistence and retention deletion share one SQLite transaction, recording every audio deletion intent in the durable pending queue before rows are removed; post-commit filesystem cleanup is best-effort and cannot falsely roll settings/shortcuts back. Schema adds cryptographically random relative owned audio filenames, a non-null uniqueness invariant, and retry links; raw legacy retention settings normalize to V2 while retaining unrelated JSON fields. Staged audio uses RAII rollback until DB commit, retention collects/deletes rows under one UTC cutoff transaction, and reconciliation clears invalid references before safely removing contained orphan entries. The final History-off/audio-retention decision is serialized with the insert, and audio staged during a settings change remains rollback-owned unless its filename is committed. Retry publishes processing before copy, uses the existing lifecycle plus an atomic successful-commit finish gate, current settings plus a strict raw typed-plan fixed-site Ask-search replan without launching a browser, produces output-only linked history, and never reinjects. Retry cleanup and post-commit metric failures cannot turn completed work into false command failures. UI guards stale filter responses, cancels Retry, and clears revoked audio blobs when their row disappears.

Verification: `cargo fmt --check`, `cargo check --all-targets --quiet`, `cargo test --lib --quiet` (164 passed, 4 ignored), `pnpm.cmd run build`, and `git diff --check` passed. Strict Clippy still reports pre-existing baseline warnings/errors outside this issue (audio/injection/test-layout); no new #12 diagnostic was retained. Manual Windows checks remain: physical playback/download, locked-file recovery after restart, provider retry quality, and cancellation timing.
