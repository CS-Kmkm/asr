Goal: Resolve #111 through separately owned changes for insertion, recording durability and native inference, with repeatable Phase 2 evidence.

Integration: `feat/issue-111-phase2`. Topic branches and worktrees derive from the integration branch. The root checkout carries existing reviewed work. Preserve that work and split new patch sets by intent. Branches may have the same committed base while the patches are under review. Commit, push and PR actions require separate authorization.

Scope and ownership:

- `feat/issue-111-insertion`: input delivery, target and clipboard guards, and insertion fixtures.
- `feat/issue-111-recording`: recording lifecycle, crash-time ownership, startup recovery, retention/cancel and device changes.
- `feat/issue-111-native-worker`: native worker transport, artifact validation, capability mapping and managed process lifetime.

Constraints: Preserve user settings, privacy choices, existing model/recording outputs, and the Phase 1 evidence. Automatic cloud fallback is prohibited. Keep unconfirmed delivery and product evaluation failures visible. Keep the Phase 1 evaluator compatible. See the clipboard, recording and interrupted-capture task records for the reviewed starting point.

Acceptance: Each topic independently builds and passes applicable checks. The integrated tree passes independent review. Use fixed trial denominators and independent observations for the final 1,000-trial, anomaly, soak and latency gates. Publish actual coverage and remaining blockers. Mark individual acceptance criteria only once verified.

Implementation pass: introduce a narrow, opt-in direct-insertion transport for verified native fields, reject stalled armed microphones, and add native worker ownership plus catalog-bound capability handling. Keep unsupported transports and models on their existing paths. Defer VM runs, reliability trials, soak, and competitor performance measurements until the remaining product implementation is complete. Use build checks and independent code review while implementing.

Native cache bootstrap: Online native model load may provision missing pinned assets through the bundled cache installer. The installer runs only on a cache miss, observes cancellation and the load deadline, and publishes using the existing no-clobber rules. Offline load performs only local verification. A complete cache loads directly without Python. Existing files that fail verification produce an error and are preserved.

Remaining implementation pass: add catalog-driven multilingual speech.cpp support and a verified preinstalled runtime path. Add a bounded clipboard-free transport for readable browser and Electron carets. For active microphone recovery, retain the original capture and its retention policy, bound reopen time, and carry a single stable time base into live audio and crash checkpoints. Keep product evaluation deferred.

## Worktree deliverables

| Branch | Worktree | Integrated change |
| --- | --- | --- |
| `feat/issue-111-insertion` | `.worktrees/issue-111-insertion` | Prepare and validate UTF-16 before clearing the clipboard. Disarm each transfer handle after Windows accepts ownership. `ASR_NATIVE_EDIT_INSERT=1` enables an undoable, clipboard-free caret insertion in eligible classic Unicode EDIT controls. The path requires exact readback and issues no second mutation after an ambiguous send. [Win32 handle ownership](https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-setclipboarddata) · [Direct EDIT insertion](https://learn.microsoft.com/en-us/windows/win32/controls/em-replacesel) |
| `feat/issue-111-recording` | `.worktrees/issue-111-recording` | For Dictate with History audio enabled, write bounded PCM checkpoints from a separate thread. On the next launch, import abandoned recordings as manual Retry entries. The expiry window is 24 hours, and each spool is capped at 32 MiB. History insertion and the replay-prevention token share one transaction. A privacy epoch invalidates checkpoints on Delete all or retention withdrawal. The normal stop and cancel paths clean up the checkpoint. |
| `feat/issue-111-native-worker` | `.worktrees/issue-111-native-worker` | `ASR_NATIVE_SPEECH_CPP=cpu` or `vulkan` opts Haas-ja-v1.0 and Parakeet Japanese Q8_0 into a direct Windows child. Model path, size and digest come from the shared catalog. A Windows Job owns the running child. The cache installer runs on an online miss, and verification, installation and readiness share a deadline. [speech.cpp 0.10.1 worker protocol](https://github.com/nyosegawa/speech.cpp/blob/v0.10.1/docs/worker.md) · [Windows Job Objects](https://learn.microsoft.com/en-us/windows/win32/procthread/job-objects) |

The topic worktrees and integration checkout share the prior reviewed changes as an uncommitted layer above commit `316be2d`. The integration checkout combines the new topic patches and preserves each topic's edits in its worktree.

The [insertion task record](issue-111-clipboard-ownership.md) contains the fixed normal-trial matrix and the pinned OpenWhispr, Handy and Amical insertion references. The [recording task record](issue-111-recording-recovery.md) tracks microphone and spool handling from those projects.

## Browser and Electron insertion

- `ASR_UNICODE_EDIT_INSERT=1` enables clipboard-free insertion for monitored, readable, writable Edit controls of the supported browser and Electron executables. The transport requires a collapsed caret, an inactive IME, and a stable text snapshot and field identity. It sends up to 256 UTF-16 units with no control characters and requires exact readback. A partially queued input sequence is never replayed via the clipboard. [Windows Unicode key packets](https://learn.microsoft.com/en-us/windows/win32/api/winuser/ns-winuser-keybdinput)

## Native runtime distribution

- Wave A caps native readiness at 60 seconds, with headroom over the measured nine-second Vulkan cold start, and caps the handshake at 10 seconds within the overall native load deadline. A dead idle child clears residency before reuse. Windows starts the worker suspended and resumes it after Job assignment.
- Verification hashes in cancellable blocks and shares results for an unchanged artifact identity. Installer receipts can satisfy the same identity check. The supervisor drains capped stdout during native audio chunk upload so a rejecting peer cannot deadlock the two pipes.

- The native path selects pinned batch Q8_0 models from the shared catalog. Qwen3-ASR 0.6B and 1.7B receive the selected language or automatic language detection, and a vocabulary prompt when provided. Japanese-only models keep the Japanese default and omit the prompt. [speech.cpp Qwen3-ASR options](https://github.com/nyosegawa/speech.cpp/blob/v0.10.1/docs/models/qwen3-asr.md)
- A release can supply a read-only runtime at `speech-cpp/runtime-0.10.1` under its resources. Stage the pinned official Windows x64 archive contents and upstream license notices in `.ai/speech-cpp-packaging/runtime-0.10.1/`, then use `pnpm exec tauri build --config src-tauri/tauri.speech-cpp.conf.json` from the checkout. The standard configuration still uses the managed cache. The optional bundle verifies both installed runtime files, provisions only missing weights if online, and uses the same pinned GGUF cache. [Official runtime release](https://github.com/nyosegawa/speech.cpp/releases/tag/v0.10.1) · [Tauri resource mapping](https://v2.tauri.app/develop/resources/)

The first checkpoint raises that profile's minimum database reader version to 9. Earlier builds cannot invalidate checkpoint epochs. To roll back after checkpointing, stop the app, preserve the current database and audio as a separate copy, and start the earlier build against its matching pre-upgrade profile backup. Keep the database and its audio together.

## Recording continuity

- A take with no complete input frames for 3 seconds enters microphone recovery. If reopening fails, the interrupted-capture path preserves the audio before the gap. A timely callback carrying digital silence still indicates a live stream. The Windows clock includes elapsed sleep and hibernation, so the first resumed callback can observe a long gap before appending any samples. [Windows uptime clock](https://learn.microsoft.com/en-us/windows/win32/sysinfo/windows-time)
- An automatic stop carries the originating operation ID through the lifecycle claim. Normal stop freezes callback admission before dispatching device teardown. The regression fixtures exercise a late callback, an empty callback between complete frames, a first-frame timeout at stop, and an automatic stop from a previous take. Independent review found no defect in the scoped delta.
- A stalled armed microphone is rejected before promotion. Its first-frame deadline starts after successful play and preserves any callback already received during startup. Existing teardown and one fresh reopen then handle the stale stream.
- An active stream failure starts one asynchronous reopen with a three-second adoption deadline. A saved device must reopen by name. A take on the system default can use the current default. Stop and cancel revoke admission for the candidate. A successful reopen retains the recorded prefix and converts the replacement segment to the original raw sample rate, keeping snapshot offsets and the crash checkpoint format stable. The elapsed gap carries no audio.
- Dictate, Translate, and Edit checkpoints use checksummed PCM frames. A translation checkpoint records the two-byte target language chosen at capture start. Recovery validates it against the existing language list and creates a manual History Retry entry in translation mode.
- Edit checkpoints preserve the captured selection and PCM under normal History audio retention. The selection is serialized only when the first PCM block is written, is limited to 3 MiB of UTF-8, and counts toward the full 32 MiB checkpoint limit. Recovery imports the selection and audio into the existing manual Edit Retry path.
- A stopped take keeps its checkpoint through processing. A retained History entry consumes the replay token in the same transaction as the insert. If staging the History WAV fails and a checkpoint exists, the checkpoint remains eligible for startup recovery. Cancellation and privacy actions invalidate the token or its epoch before attempting to unlink the spool.

## Terminal delivery

- Recognized terminals receive one per-app paste chord for final Dictate, Translate, and Ask caret delivery. Live hypotheses stay in the app. Selection replacement retains its existing verification path.
- The app restores the previous clipboard after an attributable terminal read and only while its clipboard write still owns the same sequence. If attribution is unavailable, the final text stays available. The app records `paste_unverified`, and the evaluation harness confirms delivery independently through its terminal receiver. [Terminal clipboard restore](issue-111-terminal-clipboard-restore.md)
- WASAPI classifies unplug, endpoint removal and some stream-format changes as device invalidation. A future replacement path needs to preserve the distinction between an explicit microphone selection and the system default. Format changes require a new stream and a separately tagged segment before resampling and joining. [WASAPI device invalidation](https://learn.microsoft.com/en-us/windows/win32/coreaudio/recovering-from-an-invalid-device-error)
- The integrated continuity revision passed 983 Rust library tests (11 ignored), 220 frontend tests, TypeScript checking, and Clippy with `-D warnings`. The recording worktree passed a standalone Clippy run. The VM reports below belong to the earlier release executable.

## Open gates

- The 1,000-trial contract still includes 200 automatic terminal insertions. Its terminal trials require independent receiver readback. The application reports terminal paste as unverified.
- Exercise process-death recovery in the desktop application, measure checkpoint overhead, and validate suspend/resume and confirmed device reconnection on hardware. A healthy active stream keeps its current endpoint until it reports a fault or stops delivering frames.
- Build and exercise the optional runtime bundle and online cache bootstrap on clean Windows. Measure native Qwen recognition, in-flight cancellation and crash/OOM behavior. Product evaluation remains deferred.
- Run the fixed 1,000-trial matrix, abnormal cases, soak, matched competitor measurements and full latency/accuracy/resource gates.

## Verified on the integration checkout

- The focused Edit import and epoch-invalidation test, Translate target round-trip test, and terminal final-copy/no-paste regression all passed. Clippy passed on the recording and insertion worktrees and the integration checkout. TypeScript checking and terminal message catalog parsing passed. Independent review rechecked the Edit size cap and selected-text Translate terminal status.
- The direct EDIT, armed microphone and native bootstrap changes passed Clippy with `-D warnings` in the integration checkout and each topic worktree. TypeScript and the Python installer syntax check passed. Independent code review rechecked the after-play timer and cumulative native deadline. These are static checks for the implementation pass.
- Rust library suite 981 passed, 11 ignored. This includes a Windows child-process termination fixture that writes a checkpoint, is forcibly stopped, and is recovered by the startup routine. Storage fixtures cover replay and privacy deletion, and simulate the reader requirement before and after claiming a spool.
- The opt-in native supervisor smoke passed separately on CPU and Vulkan using the same locked Japanese WAV. Each run verified an unloaded diagnostic, model load, two transcriptions, and shutdown through the application's Rust supervisor. It used the already installed and hash-checked runtime and model cache.
- Clippy with `-D warnings`, TypeScript checking, and 220 frontend tests passed. Independent review and rechecks covered clipboard handle transfer, privacy deletion and downgrade handling, native long-upload timeouts and cancellation, and typed OOM handling.
- All three topic worktrees passed standalone `cargo check --locked --offline --all-targets`. The integration checkout produced a release desktop executable with `tauri build --no-bundle`.
- The isolated VM smoke passed both stages on the archived release executable. Both mock insertions and the focus-away and cancel guards passed. The real-model Notepad insertion passed 1/1, with 10,249 ms from stop to independent readback. The app recorded 10,168 ms for ASR and 58 ms for paste. This single take does not measure warm percentiles. The local evidence is under `.ai/evaluation/phase2-worktrees-7f9930c9e916491c885ed419b0a47ee5/report/`.
- The expanded VM suite passed on the same executable. Independent receivers confirmed mock insertion in 6/6 trials and all five existing guards passed. The Windows Terminal fixture observed the expected text while the product correctly reported `paste_unverified`. The existing CPU recognizer passed 14/14 real-model insertions (Notepad 10/10, Chrome 2/2, VS Code 2/2). Warm Notepad stop-to-insertion p50/p95 were 10,102/10,596 ms (9 trials), above the 1,500/2,500 ms Phase 2 targets. The local evidence is under `.ai/evaluation/phase2-worktrees-7f9930c9e916491c885ed419b0a47ee5/report-full-1dad1a34e5494baeaf8dc2c96d604686/`.

## Wave A verification in ASR-Eval

- The integrated executable passed all 11 cases of the expanded insertion smoke on the dedicated `ASREvalReal` desktop. Six receiver cases matched exactly, including Windows Terminal. The app published `paste_unverified` for Terminal. The other cases covered focus movement, cancellation, selection changes, minimization, and worker crash recovery.
- The same executable passed the real-model Notepad case using the locked Japanese FLEURS audio. This is one fixed-utterance delivery check.
- Rust passed 1,007 library tests with 11 ignored. The separately enabled VB-CABLE probe passed two native input opens. The speech.cpp CPU smoke passed two recognitions through the Rust supervisor using the pinned Haas model. Vulkan startup could not complete because the guest exposed no `Vulkan0` device.
- During the CPU cancel smoke, the active worker did not acknowledge the cancel. The supervisor reaped the old process after the grace period, restored model residency in a new worker, and completed the next recognition. The trace is in `native-active-cancel.txt`.
- TypeScript checking passed, Vitest passed 220 tests, and pytest passed 520 tests with one skipped. The Rust test executable was built on the host and run in the guest. The run's `candidate.json` associates the transferred executable with the local source snapshot.
- Evidence is under `.ai/evaluation/wave-a-20261127094646-ciotiwno/`. The Windows Terminal receiver is part of the expanded run in `vm-insertion/result.json`. The real-model Notepad run is in `vm-real-notepad/result.json`.

## Review verification

The Windows host passed 1,010 Rust library tests with 12 ignored, Clippy with warnings denied, Rust formatting, TypeScript checking, 220 Vitest tests, and 520 pytest tests with one skipped. The Rust run includes the final checkpoint flush and Windows PID reuse regressions. The VM results above refer to the previously staged binaries.

Status: Wave A's insertion, recording, and native worker changes are integrated on `feat/issue-111-phase2`. The full 1,000-trial matrix, abnormal cases, soak, and performance gates remain open.
