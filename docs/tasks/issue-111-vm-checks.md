Goal: Make the remaining Issue 111 terminal, physical microphone, and Clippy checks repeatable in the ASR-Eval guest.

Scope: Test fixtures, isolated guest provisioning, and test entry points. Use the integrated Wave A application and preserve prior runs.

Acceptance:

- One terminal invocation verifies exact receipt in real mintty and PuTTY windows under the dedicated guest account. Each receiver is disposable and can only accept test data locally.
- The physical microphone check verifies the same guest-visible device disappearing and returning after a physical disconnect, then captures speech again. A virtual or remote-audio endpoint cannot satisfy this check.
- Clippy runs against the integrated source inside ASR-Eval using the locked dependency set and reports warnings as errors.
- A short operator guide describes setup, one-command entry points, evidence, and the distinction between missing prerequisites and a test failure. An independent reviewer checks the new harness.

Status: The combined runner passed both terminal cases in ASR-Eval under ASREvalReal. Both receiver files matched the expected nonce-tagged text exactly. A mintty check run concurrently with a cold Clippy build produced a clipboard-only result. The supported combined runner sequences the two checks.

Clippy 0.1.97 passed all targets with warnings denied in the guest. The same run built the physical microphone test in the guest and checked that its exact test ID is present. The current guest preflight returned BLOCKED because it exposes only a virtual recording endpoint. The physical disconnect and reconnect remain unverified until a USB recording endpoint is redirected to the guest.

The current ASREvalReal desktop has an `ASR USB Microphone Test` shortcut to the staged guest test. Its target and preflight were verified under the ASREvalReal account. The preflight returned exit code 2 with no eligible recording endpoint. Guest evidence is in `%LOCALAPPDATA%\ASR-Evaluation\microphone-launcher-23d40a27e4644a7f8192a36613a2a539.json`.

Evidence: `.ai/evaluation/wave-a-checks-b220a9b56bda4491a99dc7a5d6970646/`. The terminal and Clippy results include the executable and source snapshot hashes. The physical launcher is staged under the guest source path in `clippy/result.json`. The independent review prompted fail-closed test selection, scratch terminal configuration and cleanup, and clearer snapshot identity reporting.
