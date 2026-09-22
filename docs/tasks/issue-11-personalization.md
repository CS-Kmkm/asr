Goal: Implement GitHub issue #11's first complete personalization stage: privacy-safe foreground app context, manually configured abstract style profiles, scoped dictionary routing, and correction routing.

Scope / non-scope:
- Capture a normalized foreground application key and a coarse category from the already-captured target process.
- Persist only the coarse category in History; never persist an executable path, window title, surrounding text, or raw dictation as profile data.
- Add manual global and app/category style profiles with formal/casual, concise/detailed, and bounded abstract guidance.
- Route dictionary entries by global, `app:<normalized-key>`, or `category:<category>` scope and expose that scope in the existing Dictionary UI.
- Add a Personalization ON/OFF setting and a simple report of configured profiles/scopes.
- Do not implement automatic learning, edit watching, content-derived preferences, or new providers.

Fixed design:
- Add an `AppContext { app_key, category }` contract. On Windows, derive a lowercase executable stem from `TargetWindow.process_id` using a read-only process query; map known application stems into stable coarse categories (`browser`, `email`, `messaging`, `development`, `document`, `other`). On lookup failure use no app key and category `other`; do not fail dictation.
- Never store full executable paths or window titles. Validate manual profile fields and scope strings with bounded length and no control characters.
- Store profiles in the existing Settings JSON as a global `StyleProfile` plus an ordered list of scoped profiles. A scoped profile key is exactly `app:<key>` or `category:<category>`; exact app wins over category, which wins over global. Disabled personalization supplies no style profile.
- Keep abstract profile settings structured (`formality`, `detail`, optional bounded guidance). The report is the Settings UI representation of those values; no transcript examples are displayed or retained.
- Resolve app context once from the target captured at recording start. Use the same context for ASR dictionary terms, correction hints/profile, and History even if foreground focus later changes.
- Dictionary scope routing is independent of the personalization toggle: include entries with no scope/`global`, then matching app/category scopes. Unknown context includes global entries only.
- Extend the correction prompt through a separate trusted style-guidance argument. The transcript remains untrusted data in its existing field. Profile text must not be concatenated with transcript content.

Acceptance checks:
1. Unit tests cover executable-stem normalization/category mapping and lookup failure fallback without exposing a path.
2. History round-trip stores the captured coarse `app_category`; existing history-off and retention behavior remain unchanged.
3. Dictionary ASR terms and correction hints include global plus matching app/category entries and exclude nonmatching scopes; legacy null scopes behave as global.
4. Profile resolution order is app > category > global; disabling personalization yields no profile.
5. Prompt fixtures prove style guidance and transcript occupy separate fields and that profile validation rejects controls/oversize data.
6. Rust full library tests, `cargo fmt --check`, Clippy, all-target check, frontend TypeScript/build, and `git diff --check` pass.
7. Manual Windows checks remain documented for process lookup across normal/elevated targets, app-category History display, scoped dictionary behavior, and visibly different correction styles.

Status (2026-09-22):
- Implemented in this dedicated worktree. Automated Rust/frontend gates are covered by the implementation report.
- Manual Windows verification remains outstanding: process lookup across normal/elevated targets, app-category History display, scoped dictionary behavior, and visibly different correction styles.
