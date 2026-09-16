# Live dictation segmentation: agent reference

Date: 2026-09-16. Scope: provisional recognition while recording. Status: active.

- [decision] Keep the active utterance's context and advance audio/text together only at a silence endpoint. Reconcile all provisional text with full-recording ASR after stop. Reason: existing backends provide text without a common reliable alignment contract.
- [decision] Reject overlap text matching as a stitching policy: repetition can be intentional, so removing matching words is not an acceptable substitute for alignment. Reject hard duration cuts through speech; a long utterance may finish at a shorter breath instead.
- [stated] A warmed, single-run, 34.046-second synthetic Japanese comparison on RTX 3060 / faster-whisper large-v3-turbo measured first drafts at 2.305 s (cumulative), 2.076 s (650 ms silence), and 2.038 s (450 ms). Combined preparation/inference was 15.790 s, 18.359 s, and 18.080 s respectively. Smaller windows did not imply less compute.
- [stated] The 450 ms draft inserted punctuation inside a number/repetition phrase. Both utterance policies preserved the normalized reference at stop; the cumulative draft was four characters behind. This is a draft-latency observation, not evidence that final cumulative ASR is less accurate.
- [decision] Prefer 650 ms silence with adaptive update cadence on this evidence, preserving phrase context over a marginally earlier first draft. Defaults are task-specific; do not claim a universal optimum or bounded work for uninterrupted speech.
- [stated] Endpoint tests exposed loud-to-quiet truncation and an expired short blip that never retired. Energy hysteresis and conservative candidate retirement fixed those cases. Weak background energy can still keep an utterance open, and the full active window is prepared each poll.
- [decision] Future evaluation should report first display, last-draft completeness, punctuation, preprocessing and inference together. Numeric/punctuation normalization alone hides relevant errors.

Evidence: [task contract](../tasks/live-dictation.md), especially Final comparison and acceptance; local preserved reports `.diagnostics/live-segmentation-20260916-091236/report-1789517840228.json` and `.diagnostics/live-segmentation-20260916-092107-continuity/report-1789518196988.json`. Synthetic fixtures are not representative microphone accuracy evidence. Raw fixtures/reports are local and must not be overwritten or deleted.
