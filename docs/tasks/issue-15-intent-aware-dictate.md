Goal: Implement GitHub issue #15 as an explicit intent-aware Dictate correction mode while preserving the existing conservative mode and hard factual-safety boundary.

Scope / non-scope:
- Add a persisted `conservative` / `intent_aware` mode switch and Settings UI/report.
- In intent-aware mode, permit reordering later context, applying later corrections globally, merging duplicate information, and choosing paragraph/list structure.
- Preserve names, numbers, URLs, code, and uncertainty; never add unspoken concrete facts.
- Do not add fact completion, creative completion, web retrieval, or implicit mode switching.

Fixed design:
- Legacy/missing settings default to `conservative`; its existing prompt and behavior remain byte-for-byte equivalent except for necessary function signatures.
- Intent-aware mode adds a trusted prompt section that allows organization across utterance order and whole-document self-correction, but still forbids summarization, translation, answers, actions, and new facts.
- Precedence: hard safety/fact preservation and dictionary spelling always win; correction mode controls cross-sentence reorganization; existing independent switches then enable/disable filler removal, repetition removal, self-correction, formatting, and clarity. A disabled switch must be described explicitly in either mode.
- The user `correction_instruction` remains subordinate to the fixed safety rules and cannot enable fact/creative completion.
- Add a deterministic postcondition for intent-aware output: every source URL, digit-bearing token, backtick code span, and explicit English/Japanese uncertainty marker must remain present. Missing protected spans reject the provider result through the existing safe original-transcript fallback. Names/proper nouns are enforced in the trusted prompt and request fixtures without speculative NER.
- Provider inputs retain the existing system-instruction/transcript separation for OpenAI, Gemini, and loopback-local providers.
- UI explains that intent-aware mode may reorganize phrasing and is instructed not to add details, and names what the post-check cannot detect; no automatic learning or context beyond the current transcript/profile is introduced.

Acceptance checks:
1. Legacy settings/default and round-trip tests preserve conservative mode.
2. Prompt fixtures cover out-of-order context, later global correction, duplicate merging, and paragraph/list organization; conservative prompt lacks intent-aware permissions.
3. Switch-precedence tests cover all existing correction switches in both modes.
4. Provider request tests keep the transcript separate and label it untrusted.
5. Protected-span extraction/validation tests cover names in prompt fixtures plus exact numeric, URL, code, and uncertainty preservation; unsafe output falls back to the original transcript.
6. Tests prove neither mode permits unspoken facts and custom guidance cannot override the prohibition.
7. Full Rust/frontend checks and `git diff --check` pass; real-provider Japanese/English quality remains a documented manual check.

Status (2026-09-29):
- PR #24 review findings F1-F7 are fixed locally on
  `feat/issue-15-intent-aware-dictate`; not pushed.
- Verified: `cargo fmt --check`; `cargo clippy -j 4` (exit 0, 19 warnings, none
  in code added by this branch); `cargo test --lib -j 4` (154 passed, 4 ignored);
  `pnpm exec tsc --noEmit`; `pnpm build`; `git diff --check`.
- Real-provider quality and rejection rates remain an opt-in manual check. No
  stub-server end-to-end test was added: the providers use fixed endpoints, so
  the validator is covered through `accept_provider_correction`, the seam
  `correct_transcript` calls after the provider request.

Review remediation (2026-09-29, PR #24 F1-F7):
- F1 (D2): conservative mode performs no post-correction fact validation, as
  before the mode switch; only `intent_aware` output is validated. Conservative
  fixtures (numbered lists, 3000 -> 3,000, full-width digits, 一緒) are accepted.
- F2: source occurrences are classified by position as kept, superseded by a
  nearby explicit repair, or replaceable by the final value of a later repair of
  the same fact. A later correction may therefore replace every earlier mention,
  unrelated later mentions must survive, and duplicate mentions may merge when
  the repetition switch is on. A value may appear in the output only as often as
  in the source plus its replaceable mentions.
- F3: repair cues include ではなく, ではなくて, じゃなく, じゃなくて, 違う, I meant,
  and I mean (English cues case-insensitive on word boundaries), and fillers such
  as えーと, えっと, あの, um, and uh between a cue and its replacement are skipped.
- F4: equivalent counters (人/名, 回/度, つ/個) share one unit, only kanji,
  katakana, and つ count as a unit, and a lone kanji numeral is a number only
  before a counter or next to other numerals (一緒, 一番, 一旦, 十分 are words).
- F5: README and Settings no longer claim the mode never invents details, and
  しかも no longer counts as the uncertainty marker かも.
- F6: the conservative personalized prompt keeps #11's guidance position and
  prefix; the intent-aware section is one additive line, tested for every
  editing-switch combination with and without a profile.
- F7: validator rejects return `ProtectedContentChanged`, reported as
  `protected_content_changed` in the status message and correction metric.
- Remaining limits: the check is syntactic. It cannot detect changed names,
  spelled-out or differently written numbers (三 vs 3, ３ vs 3, 3000 vs 3,000 are
  different values in intent-aware mode), unquoted code, scheme-less domains, or
  a superseded value reintroduced elsewhere when duplicates may merge; merging
  also accepts dropping a repeated same-value fact that was not a duplicate.

Status (2026-09-23):
- Complete in the Issue #11-based worktree; independently reviewed and awaiting local commits.
- Added persisted `correction_mode`, defaulting legacy and new settings to
  `conservative`, with Settings UI and README guidance.
- Intent-aware prompting permits only transcript organization and retains the
  existing conservative prompt path unchanged. Existing editing switches remain
  explicit and independent in both modes.
- Intent-aware provider results deterministically require every source URL,
  digit-bearing token, backtick code span, and listed English/Japanese
  uncertainty marker. A rejected result follows the existing correction failure
  path, which restores the original transcript.
- Review follow-up made protected-span validation boundary-exact and Unicode-safe,
  protects embedded case-insensitive URLs, conditions duplicate merging on its
  switch, keeps the conservative personalized prompt compatible, and localized
  all new settings labels. Independent re-review found no remaining issue.
- Automated verification: `cargo fmt --all -- --check`; `cargo test --lib`
  (144 passed, 4 ignored); `pnpm exec tsc --noEmit`; `pnpm run build`; and
  `git diff --check`.
  `cargo clippy --all-targets -- -D warnings` remains blocked by pre-existing
  warning categories on the Issue #11 base (18 for both the library and test
  targets on this branch); the Issue #15 additions introduce no strict-Clippy
  finding.
- This branch base contains the OpenAI/Gemini providers but not Issue #7's local
  provider; the provider-separation contract is verified here for the providers
  present, and local-provider integration remains part of later branch integration.
- Manual provider quality remains an opt-in Japanese/English check documented in
  `README.md`; it was not run because it requires credentials and may incur cost.

Review remediation (2026-09-27):
- Protected numbers are extracted as individual numeric/ASCII identifier spans,
  including common kanji numerals, instead of whitespace-delimited Japanese sentences.
  Sentence punctuation after URLs is excluded from URL identity.
- Intent-aware output may omit an earlier numeric/URL/code value only when a nearby
  explicit repair cue is followed by another source value of the same kind and the
  self-correction switch is enabled. Both modes reject newly introduced numeric,
  URL, and backtick-code values; intent-aware mode still requires all other values.
  (Superseded on 2026-09-29: conservative mode no longer validates output.)
- Input/output fixtures exercise Japanese numeric repairs, reordered context,
  duplicate merging, URL punctuation, polite uncertainty, and invented values.
- Signed numbers retain their leading `+` or `-`. A later repair may cross up to
  two short sentence boundaries only when its cue begins a new sentence and a
  same-kind replacement occurs in the cue's first clause; unrelated later
  values do not license dropping the original value.
- This validator checks observable syntax, not semantic truth. It cannot reliably
  identify unspoken names, unquoted code, changed relationships using existing
  numbers, implicit corrections, or all paraphrases of uncertainty. Some legitimate
  edits may still fall back to the raw transcript; provider quality/rejection rates
  require opt-in real-provider evaluation.
- The #7 local provider is absent from this branch. On an explicit integration
  branch, verify local request separation and intent-aware validation/fallback
  with a stub local endpoint after resolving the overlapping provider files.
