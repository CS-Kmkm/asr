Goal: Recover useful microphone audio from an interrupted take that fails the normal duration or voice gate and recover a failed armed input stream at recording start.

Scope: The CPAL capture lifecycle and existing History Retry path. A retained rejected capture offers explicit retry and does not start automatic ASR or delivery. Continue the previous recording recovery slice. Product evaluation and new test fixtures are outside the current request.

Constraints: Check the existing failed-take policy before creating a rejected capture artifact, and again when saving. Respect explicit cancellation, dictation-test isolation and mode restrictions. Keep raw capture artifacts cleanup-owned until the background job finishes. Reuse the saved device and fallback policy when reopening.

Acceptance criteria: A usable disconnected take continues its existing path. A short or quiet disconnected take with PCM16 signal can reach History Retry if policy and mode permit. Empty and digital-silent takes remain rejected. Recorder cancellation and setup audio do not produce recovery rows. An armed stream reporting an error is disposed before one fresh open. Compile, lint, and independently audit the paths.

The recorder distinguishes an interrupted capture that passes the normal gate from a rejected capture eligible for manual Retry. Eligibility at start follows the existing failed-recording policy. At stop, only interrupted captures with nonzero representable PCM can bypass the gate, and the retry artifact uses the same resampling and enhancement as ordinary speech. The stop pipeline reserves a History completion before sealing the operation and hands cleanup ownership to the deferred save, which rechecks current retention. Recognition and insertion are skipped. History notice text reflects the outcome of the actual save.

Before promoting a healthy armed capture, the recorder compares device, fallback and preroll settings and applies the current processing settings. A failed or incompatible armed stream is shut down off the async runtime, then reopened once using the requested configuration.

Remaining: crash-time persistence and startup recovery, in-take device/rate changes, sleep/wake recovery, and policy for short or quiet takes stopped normally. Phase 2 also retains the dispatch-to-receipt clipboard race, native worker integration, and product acceptance evaluations.

For an in-take replacement design, Windows Core Audio can report endpoint removal, state and default-device changes. Its notification callbacks must remain nonblocking and endpoint IDs are opaque. A recorder generation can associate queued notifications with the current take. Device selection, CPAL stream shutdown and segment finalization belong on the recorder owner outside that callback. [Windows audio device events](https://learn.microsoft.com/en-us/windows/win32/coreaudio/device-events) · [IMMNotificationClient](https://learn.microsoft.com/en-us/windows/win32/api/mmdeviceapi/nn-mmdeviceapi-immnotificationclient)

Verification: `cargo clippy --all-targets --locked --offline -- -D warnings` and `git diff --check` passed. Independent review found no behavior blocker and noted conflicting user guidance on the recovery path. The immediate message now describes the interruption, and the later save notice offers Retry after a successful History write.

Status: Implemented and reviewed for this slice. Device behavior and History recovery remain to be verified in the product evaluation.
