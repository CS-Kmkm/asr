import { afterEach, beforeEach, expect, test, vi } from "vitest";
import { invoke } from "@tauri-apps/api/core";

vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn() }));
beforeEach(() => { vi.resetModules(); vi.mocked(invoke).mockReset(); Reflect.deleteProperty(window, "__TAURI_INTERNALS__"); });
afterEach(() => { Reflect.deleteProperty(window, "__TAURI_INTERNALS__"); });

test("browser shortcut warnings are empty lists", async () => {
  const api = await import("../api");
  expect(await api.getShortcutWarning()).toEqual([]);
  expect(await api.getStartupHotkeyWarning()).toEqual([]);
  expect(invoke).not.toHaveBeenCalled();
});

test("diagnostic folder uses a fixed native command without a client path", async () => {
  Object.defineProperty(window, "__TAURI_INTERNALS__", { value: {}, configurable: true });
  const api = await import("../api");
  await api.openDiagnosticsFolder();
  expect(invoke).toHaveBeenCalledWith("open_diagnostics_folder");
});

test("credential IPC reads presence and passes explicit save/delete without Settings persistence", async () => {
  Object.defineProperty(window, "__TAURI_INTERNALS__", { value: {}, configurable: true });
  const api = await import("../api");
  await api.getProviderCredentials();
  await api.updateProviderCredential("openai", "OPENAI_API_KEY", "synthetic-key");
  await api.updateProviderCredential("openai", "OPENAI_API_KEY", null);
  expect(vi.mocked(invoke).mock.calls).toEqual([
    ["get_provider_credentials"],
    ["update_provider_credential", { provider: "openai", expectedAlias: "OPENAI_API_KEY", secret: "synthetic-key" }],
    ["update_provider_credential", { provider: "openai", expectedAlias: "OPENAI_API_KEY", secret: null }],
  ]);
});

test("native shortcut warnings keep the IPC list type", async () => {
  Object.defineProperty(window, "__TAURI_INTERNALS__", { value: {}, configurable: true });
  vi.mocked(invoke).mockResolvedValue(["Ask Anything (Ctrl+Shift+A)"]);
  const api = await import("../api");
  expect(await api.getShortcutWarning()).toEqual(["Ask Anything (Ctrl+Shift+A)"]);
  expect(invoke).toHaveBeenCalledWith("get_shortcut_warning");
});

test.each([null, "Some saved hotkeys overlap or could not be registered."])("startup warning %s becomes a list without losing its text", async (warning) => {
  Object.defineProperty(window, "__TAURI_INTERNALS__", { value: {}, configurable: true });
  vi.mocked(invoke).mockResolvedValue(warning);
  const api = await import("../api");
  expect(await api.getStartupHotkeyWarning()).toEqual(warning ? [warning] : []);
  expect(invoke).toHaveBeenCalledWith("get_startup_hotkey_warning");
});
