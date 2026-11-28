Goal: Restore the previous clipboard after a final terminal paste when the target's clipboard read is attributable.

Scope: Windows terminal paste dispatch, delayed rendering and ownership checks, and targeted VM verification.

Constraints: Dispatch one existing terminal chord. Keep result verification distinct from clipboard read attribution. Preserve newer clipboard contents and keep the transcript available when attribution is missing or ambiguous. Honor the clipboard restoration setting.

Acceptance:

- Restore only after a target-associated render request and a matching clipboard owner and sequence under the clipboard lock.
- An unrelated or windowless reader, a timeout, and an ownership change cannot authorize restoration. The delayed owner retains or materializes its payload when restoration is unavailable.
- Verify the platform path and regression tests on ASR-Eval. Independently review the ownership and message lifetime.

The terminal reports `paste_unverified` because the application observes the clipboard read. The VM fixture checks the resulting terminal input independently. A windowless read cannot identify the requester through Win32. [GetOpenClipboardWindow](https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-getopenclipboardwindow)

## ASR-Eval results

- mintty received the exact nonce-tagged transcript and the previous clipboard text was restored.
- PuTTY received the exact nonce-tagged transcript and the transcript remained available on the clipboard.
- Clippy passed all targets with warnings denied. Insertion tests passed with 114 passed and two interactive tests ignored. The regression cases cover an attributed read, an unconfirmed read, and a newer copy.
- Independent Claude Code review checked the Win32 owner lifetime and restoration guards. The final code allows the delayed owner to keep serving its payload when an early read advances the sequence.

Terminal evidence is in `.ai/evaluation/wave-a-checks-e828e8d3d3254d96832e94f6f709b859/terminals/`. Clippy and Rust test evidence is in `.ai/evaluation/wave-a-checks-465c23e3244643f5bac188c1d834a8ac/clippy/`.
