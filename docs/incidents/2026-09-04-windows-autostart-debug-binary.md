# Windows autostart registered a development binary

- Date: 2026-09-04
- Environment `[stated]`: Windows 11, Tauri 2.5, `tauri-plugin-autostart` 2.5.1.
- Symptom `[derived]`: Windows autostart launched `src-tauri/target/debug/local-voice-input.exe` while no Vite server was listening on `localhost:1420`, so the desktop process could not load its web UI.
- Cause `[derived]`: The autostart plugin registers `current_exe()`. Startup synchronization ran in development builds, so `tauri dev` replaced the Windows Run entry with the development executable.
- Non-causes `[derived]`: The Run entry was enabled, its target existed, the executable remained alive when started from `C:\Windows\System32`, and no application crash was present in the Windows Application event log.
- Fix `[decision]`: Synchronize the saved autostart setting at application startup only in release builds. Reject a new enable request from a development build, while allowing disable requests and preserving an existing release registration during development. This prevents an executable that requires the Vite server from becoming the login target.
- Reproduction `[derived]`: Set `autoStart` to true from a development build, confirm the Run entry points to `target/debug/local-voice-input.exe`, stop Vite, and invoke the registered command with `C:\Windows\System32` as its working directory.
- Verification `[derived]`: A production build changed the Run entry to `target/release/local-voice-input.exe`; launching the rebuilt development binary afterward left that entry unchanged; invoking the final registered command from `C:\Windows\System32` produced a live application window.
