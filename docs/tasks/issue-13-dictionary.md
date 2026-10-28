Goal: Implement GitHub issue #13 as a scoped vocabulary workflow that users can safely edit, search, import, and grow from explicitly confirmed correction candidates.

Scope / non-scope:
- Add dictionary edit, search, source filters, transactional CSV import, source persistence, and conservative preferred-spelling candidates.
- Reuse Issue #11 app/category scope routing and validation.
- Use `reading`, `surface`, aliases, priority, and app scope in ASR/correction hints.
- Never silently add a candidate to the dictionary and never derive broad semantic pairs from arbitrary rewrites.

Fixed design:
- Add an idempotent `source` column (`manual` or `auto`, legacy rows become `manual`) and update/list/import commands. Keep current IDs stable on edit.
- Logical duplicate key is case-insensitive `(surface, normalized app_scope)`. Aliases must not collide case-insensitively with another entry's surface/alias in the same scope. Reading collisions are allowed; higher priority orders the hints.
- Add a local `dictionary_candidates` table with original span, preferred span, confidence, optional history ID, and timestamps. Confidence is a fixed detector marker, not a measurement, and the UI does not display it. Candidate confirmation creates a global `source=auto` entry only when no entry in any scope already has the preferred span as its surface or an alias; otherwise it resolves to that entry unchanged. Rejection deletes the candidate. Confirmed and rejected pairs are not proposed again within the History retention window. No candidate directly changes recognition output.
- Candidate detection is deliberately narrow: after successful correction, trim the longest identical prefix/suffix, expand the changed region over adjacent ASCII term characters only (letters, digits, `-`, `_`), and propose one bounded span when removing case/space/hyphen/underscore differences makes both spans equal. Reject empty, multiline, control-character, oversized, numeric-only, single-letter, and mixed-script/punctuated spans, sentence-start-only capitalization, and spans whose surrounding ASCII token is a URL or code. Suppress spans already represented by any entry's surface or alias in any scope. Do not persist candidates when History is disabled.
- ASR hints are filtered by app scope and ordered by priority. Prompt-based backends receive one bounded line per entry (`reading => surface (aliases: ...)` for manual entries, surface and aliases only for auto entries; 1200 characters). faster-whisper hotwords list every surface, then aliases, then manual readings, without case-insensitive duplicates (200 characters). Correction hints retain preferred-surface mappings.
- CSV is read as strict UTF-8 with a strict CP932 fallback and a required header (`reading,surface,category,aliases,priority,app_scope`); aliases are `|` separated and a blank priority is 0. Limit input to 1 MiB/1000 rows. Parse quoted CSV correctly, validate every row and collision before one SQLite transaction, and write nothing on any error. Imported rows are `manual`.
- Search is case-insensitive substring matching across reading/surface/category/aliases/scope. Filters are All/Auto-added/Manually-added. UI exposes edit, delete, search, filter, import result, and pending-candidate confirm/reject.

Acceptance checks:
1. CRUD tests cover edit with stable ID and source/scope/alias preservation.
2. Search/source-filter tests cover all fields and manual/auto.
3. CSV tests cover quoting, multiple rows, size/count/field errors, duplicate/collision rejection, and transaction rollback.
4. Hint tests prove reading, surface, aliases, priority, and app/category scope routing.
5. Candidate detector fixtures cover capitalization/spacing preferred spelling and reject semantic rewrites, URLs, numbers, code, multiline, and History-off persistence.
6. Confirmation/rejection tests prove only explicit confirmation affects the dictionary and source becomes `auto`.
7. Full Rust/frontend checks and `git diff --check` pass; manual UI/import/provider checks remain documented.

Status (2026-09-29):
- Implemented on `feat/issue-13-dictionary` (PR #23, base `feat/issue-11-personalization`, reviewed #11 fixes merged): source-aware dictionary CRUD/search, atomic quoted-CSV import, scoped priority hints, and explicit candidate confirmation/rejection. Candidates are derived only from successful AI correction output (not from the user's own edits), stored only while History is enabled, and never alter output automatically.
- PR #23 review fixes (F1-F8) are committed locally: ASCII-term candidate spans with Japanese mixed-script fixtures, sentence-start and single-letter rejection, per-span code/URL exclusion, suppression of candidates already represented in any scope, hotword order surfaces -> aliases -> readings, tests on the production `dictionary_asr_prompt_for` (the unused `dictionary_prompt_terms_for` was removed), an effective numeric-only fixture, no displayed confidence, blank CSV priority as 0, and a separate empty search/filter message (ja/en).
- Automated checks passed at the fix tip: `cargo test --lib -j 4` (155 passed, 4 ignored), `cargo fmt --check`, `cargo clippy -j 4` (exit 0; 19 library warnings, none in code this branch introduces), `pnpm exec tsc --noEmit`, `pnpm build`, and `git diff --check`. `asr_worker/` is unchanged.
- Not covered by automated tests: legacy-DB migration of the `source` column, edit-time and surface-vs-alias collisions, intra-CSV collisions, the CP932 decode path, and the frontend (no test runner).
- Manual limits: desktop UI interaction, native CSV file-picker flow, a real correction-provider response, and live ASR hint behavior (faster-whisper, VibeVoice, OpenAI-compatible) require a running Tauri application and configured provider; they were not exercised. `docs/windows_verification.md` still describes the removed `dictionary_prompt_terms()` path and is outside this branch's fix scope.
