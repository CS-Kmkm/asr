Goal: Implement GitHub issue #13 as a scoped vocabulary workflow that users can safely edit, search, import, and grow from explicitly confirmed correction candidates.

Scope / non-scope:
- Add dictionary edit, search, source filters, transactional CSV import, source persistence, and conservative preferred-spelling candidates.
- Reuse Issue #11 app/category scope routing and validation.
- Use `reading`, `surface`, aliases, priority, and app scope in ASR/correction hints.
- Never silently add a candidate to the dictionary and never derive broad semantic pairs from arbitrary rewrites.

Fixed design:
- Add an idempotent `source` column (`manual` or `auto`, legacy rows become `manual`) and update/list/import commands. Keep current IDs stable on edit.
- Logical duplicate key is case-insensitive `(surface, normalized app_scope)`. Aliases must not collide case-insensitively with another entry's surface/alias in the same scope. Reading collisions are allowed; higher priority orders the hints.
- Add a local `dictionary_candidates` table with original span, preferred span, confidence, optional history ID, and timestamps. Candidate confirmation creates/updates a dictionary entry with `source=auto`; rejection deletes the candidate. No candidate directly changes recognition output.
- Candidate detection is deliberately narrow: after successful correction, trim the longest identical prefix/suffix and propose only one bounded changed span when removing case/space/hyphen/underscore differences makes both spans equal. Reject empty, multiline, control-character, oversized, numeric-only, URL, and code spans. Do not persist candidates when History is disabled.
- ASR prompt entries include a bounded `reading => surface` relationship plus surface/aliases, ordered by priority and filtered by app scope. Correction hints retain preferred-surface mappings.
- CSV is UTF-8 with a required header (`reading,surface,category,aliases,priority,app_scope`); aliases are `|` separated. Limit input to 1 MiB/1000 rows. Parse quoted CSV correctly, validate every row and collision before one SQLite transaction, and write nothing on any error. Imported rows are `manual`.
- Search is case-insensitive substring matching across reading/surface/category/aliases/scope. Filters are All/Auto-added/Manually-added. UI exposes edit, delete, search, filter, import result, and pending-candidate confirm/reject.

Acceptance checks:
1. CRUD tests cover edit with stable ID and source/scope/alias preservation.
2. Search/source-filter tests cover all fields and manual/auto.
3. CSV tests cover quoting, multiple rows, size/count/field errors, duplicate/collision rejection, and transaction rollback.
4. Hint tests prove reading, surface, aliases, priority, and app/category scope routing.
5. Candidate detector fixtures cover capitalization/spacing preferred spelling and reject semantic rewrites, URLs, numbers, code, multiline, and History-off persistence.
6. Confirmation/rejection tests prove only explicit confirmation affects the dictionary and source becomes `auto`.
7. Full Rust/frontend checks and `git diff --check` pass; manual UI/import/provider checks remain documented.

Status (2026-09-23):
- Complete in the Issue #11-based worktree; independently reviewed and awaiting local commits.
- Implemented source-aware dictionary CRUD/search, atomic quoted-CSV import, scoped priority hints, and explicit candidate confirmation/rejection. Candidates are stored only after successful correction while History is enabled and never alter output automatically.
- Review follow-up made confirmation fully transactional, refreshed candidates after dictation, treated candidate learning as best-effort, tightened URL/path/code rejection, completed CSV/privacy/priority coverage, and localized the new UI. Independent re-review found no remaining issue.
- Automated checks passed: Rust library tests (139 passed, 4 ignored), `cargo fmt --check`, TypeScript no-emit, frontend production build, and `git diff --check`. Strict Clippy currently fails only on the same 19 library / 18 test-target warnings present on the Issue #11 base under the current Rust toolchain; no new warning is introduced by this branch.
- Manual limits: desktop UI interaction, native CSV file-picker flow, a real correction-provider response, and live ASR prompt behavior require a running Tauri application and configured provider; they were not exercised in this non-interactive verification.
