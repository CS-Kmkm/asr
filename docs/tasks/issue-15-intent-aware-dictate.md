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
- UI explains that intent-aware mode may reorganize phrasing but never invent details; no automatic learning or context beyond the current transcript/profile is introduced.

Acceptance checks:
1. Legacy settings/default and round-trip tests preserve conservative mode.
2. Prompt fixtures cover out-of-order context, later global correction, duplicate merging, and paragraph/list organization; conservative prompt lacks intent-aware permissions.
3. Switch-precedence tests cover all existing correction switches in both modes.
4. Provider request tests keep the transcript separate and label it untrusted.
5. Protected-span extraction/validation tests cover names in prompt fixtures plus exact numeric, URL, code, and uncertainty preservation; unsafe output falls back to the original transcript.
6. Tests prove neither mode permits unspoken facts and custom guidance cannot override the prohibition.
7. Full Rust/frontend checks and `git diff --check` pass; real-provider Japanese/English quality remains a documented manual check.

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
