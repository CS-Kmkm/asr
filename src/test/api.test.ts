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

test("History IPC carries the search and cursor, defaulting to the first page", async () => {
  Object.defineProperty(window, "__TAURI_INTERNALS__", { value: {}, configurable: true });
  const api = await import("../api");
  vi.mocked(invoke).mockResolvedValue({ items: [], retainedIds: [] });
  const before = { createdAt: "2026-10-23T00:00:00Z", id: 123 };
  await api.listHistory("translate", { query: "日本語 %_", before });
  await api.listHistory();
  expect(vi.mocked(invoke).mock.calls).toEqual([
    ["list_history", { filter: "translate", limit: 100, query: "日本語 %_", before, knownIds: [] }],
    ["list_history", { filter: "all", limit: 100, query: "", before: null, knownIds: [] }],
  ]);
});

test("text-only History IPC passes the explicit saved row and action", async () => {
  Object.defineProperty(window, "__TAURI_INTERNALS__", { value: {}, configurable: true });
  const api = await import("../api");
  await api.reprocessHistoryText(12, { kind: "correct", profile: "category:email" });
  await api.reprocessHistoryText(12, { kind: "translate", target: "ja" });
  expect(vi.mocked(invoke).mock.calls).toEqual([
    ["reprocess_history_text", { id: 12, action: { kind: "correct", profile: "category:email" } }],
    ["reprocess_history_text", { id: 12, action: { kind: "translate", target: "ja" } }],
  ]);
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
