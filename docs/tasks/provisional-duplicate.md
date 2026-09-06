Goal: Diagnose and fix duplicate target text during provisional transcription correction.
Scope: Trace ASR, lifecycle, provider events, and Windows injection; change only demonstrated causes and regression coverage. Preserve pre-existing changes in commands.rs, injection.rs, and lib.rs.
Constraints: Preserve provisional input and correction behavior, user-input/focus guards, and clipboard policy. Do not log user text or alter experiment settings.
Reuse / creation plan: Extend injection tests and existing Windows backend; use disposable native-control probes if needed.
Acceptance criteria:
- Reproduce a duplication mechanism with an executable check that observes target text.
- Corrected text replaces the provisional text once; no duplicate from completion or input fallback.
- Existing injection and lifecycle/provider tests pass; validate Windows compilation.
- Record any application-specific verification limits explicitly.
Open questions: User's affected application and IME state (requested asynchronously).
Context: src-tauri/src/{commands,injection,input_monitor,correction,state}.rs and src/App.tsx.
Status: Complete. Native Windows EDIT reproduced `correcteddraft` from replacing `draft` with `corrected`. Missing EXTENDEDKEY on VK_LEFT caused synthetic Shift releases with NumLock on; setting that flag on both key edges alone fixed selection and replacement. The retained native test passes for ASCII, Japanese, LF, and CRLF with surrounding text preserved. All 92 ordinary Rust library tests pass; the one interactive test was run explicitly and passed. injection.rs rustfmt check and git diff --check pass. Independent advisor accepted the A/B evidence and minimal scope. No user text was logged; probe traces removed. Existing unrelated changes preserved.
Verification limits: User's actual application remains unspecified; native EDIT coverage does not establish universal terminal or IME behavior. See docs/incidents/2026-09-05-provisional-duplicate-numlock.md.
