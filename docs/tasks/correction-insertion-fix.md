Goal: Restore the user's clipboard after confirmed paste and replace a provisional draft with the AI-corrected text when UIA recreates the focused text element.

Scope / non-scope: May edit only the injection files named by the user (and commands.rs only if required). Do not touch the frontend, worker, manifests, dependencies, or git history.

Constraints: Preserve the no-retry/no-overwrite rule after paste input is queued; require exact text readback before reporting success; verify selected text before Select(); require affirmatively inactive IME state for destructive edits; abort on foreground/control/process/thread changes; keep corrected text recoverable when replacement is refused. Treat B1-B5 as established facts.

Reuse / creation plan: Extend TargetText comparison semantics and the existing batch mock suite. Add one ignored native clipboard round-trip test in clipboard.rs. Add no dependencies or production abstractions beyond the smallest comparison/capture changes.

Acceptance criteria:
- A mock batch regression changes only the UIA element identity after a visible draft paste, fails before the fix as PasteUnverified, and passes after the fix through confirmed corrected replacement.
- clipboard::write returns the sequence number observed after CloseClipboard; an ignored native round-trip test preserves/restores clipboard data on an interactive desktop.
- accessibility::select_recent cannot prematurely stop solely because CRLF consumes more provider character moves than normalized UTF-16 length, while every candidate range remains text-verified before Select().
- cargo fmt --manifest-path src-tauri/Cargo.toml --check exits 0.
- cargo check --manifest-path src-tauri/Cargo.toml --all-targets exits 0.
- cargo test --manifest-path src-tauri/Cargo.toml --lib exits 0.

Open questions: None. The user fixed the evidence, scope, safety invariants, and required checks.

Context: src-tauri/src/injection.rs defines TargetText and Backend contracts; batch.rs owns paste verification/provisional replacement and mock tests; accessibility.rs supplies UIA state and range selection; clipboard.rs owns Win32 clipboard sequencing and native tests. B1-B5 need not be re-derived.

Status:
- Done: Defect B regression failed before the fix with PasteUnverified and passes after content/identity comparison separation.
- Done: Defect A native ignored regression failed before the fix on the stale sequence and passes after post-close capture.
- Done: select_recent walks to the provider boundary, transfers only newly added units, and verifies the full selected range before Select().
- Done: formatting, all-target check, and library test suite pass.

Decisions:
- UIA RuntimeId remains diagnostic state but is excluded from edit-content equality. Target identity remains enforced by Backend::validate_target and TextControl::focused's process check.
- The native clipboard regression keeps a second snapshot so its expected pre-fix failure can restore the user's clipboard before panicking.
