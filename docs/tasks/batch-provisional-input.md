Goal: Insert a provisional transcript in one batch, display API deltas only in the app preview, then replace the provisional range once with completed correction.
Scope: Generic Windows input and correction pipeline; no VS Code extension, per-application APIs, or application-name routing. Preserve unrelated autostart changes and previous NumLock fix.
Constraints: No per-character text injection fallback. Preserve clipboard restoration/history preferences, focus/input-monitor guards, secure-target restrictions, cancellation, and API failure behavior. Never log target/user text.
Reuse / creation plan: Reuse existing injection backend, clipboard transactions, input monitor, and correction preview events. Extend clipboard preservation and target-range verification only as needed for batch pasting.
Acceptance criteria:
- Initial text uses one paste; clipboard contents cannot silently select Unicode typing.
- API deltas never mutate the external target; completion replaces exactly once, and identical final text requires no second insertion.
- A focus/user-input/IME safety failure never triggers blind replacement or duplicate retry; existing explicit clipboard-only outcome remains available.
- Clipboard restoration cannot cause already-queued paste to consume the old value; preserve supported existing clipboard data.
- Meaningful injection regression tests and Rust library checks pass; report interactive test limits honestly.
Open questions: May decide implementation details locally. Generic automation cannot guarantee every application's paste/selection semantics; unsupported safety checks must report the existing clipboard-only result rather than claim success.
Context: src-tauri/src/injection.rs, commands.rs, input_monitor.rs, correction.rs, types.rs; previous diagnosis docs distinguish evidence from unconfirmed IME hypotheses.
Design: Use one clipboard paste and generic UI Automation text/selection snapshots to verify edits. Keep clipboard payload in place when a submitted paste is unconfirmed, do not retry or replace that payload, and expose an explicit unconfirmed outcome. Restore clipboard only after observed target success and a matching clipboard sequence. Select the verified provisional text through UIA; never simulate character deletions or force IME commit with Enter. Unknown composition/unsupported accessibility must not authorize destructive replacement. Clipboard data is eagerly duplicated by format, not kept as a lazy IDataObject proxy. Existing clipboard-only fallback applies before any paste is queued.
Status: Implementing. Clipboard materialization is a disjoint helper; main owns injection orchestration/UIA and command wiring.
