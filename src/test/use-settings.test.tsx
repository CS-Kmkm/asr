import { act, renderHook, waitFor } from "@testing-library/react";
import { beforeEach, expect, test, vi } from "vitest";
import { useSettings } from "../hooks/useSettings";
import { defaultSettings, getSettings, updateSettings } from "../api";
import type { SettingsSnapshot } from "../types";
const subscriptions = vi.hoisted(() => new Set<(event: { payload: SettingsSnapshot }) => void>());
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn(async (_name, handler) => {
  subscriptions.add(handler);
  return () => subscriptions.delete(handler);
}) }));
vi.mock("../api", async (original) => ({ ...await original<typeof import("../api")>(), getSettings: vi.fn(), updateSettings: vi.fn() }));
function deferred<T>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((done) => { resolve = done; });
  return { promise, resolve };
}
beforeEach(() => { vi.clearAllMocks(); subscriptions.clear(); });
function fixture() {
  const callbacks = { onLanguageChange: vi.fn(), onSetupNeeded: vi.fn(), onNotReady: vi.fn(), onError: vi.fn() };
  return { ...renderHook(() => useSettings(callbacks)), callbacks };
}
test("saving before initial settings load is refused and retries the read", async () => {
  const first = deferred<SettingsSnapshot>();
  const retry = deferred<SettingsSnapshot>();
  vi.mocked(getSettings).mockReturnValueOnce(first.promise).mockReturnValueOnce(retry.promise);
  const { result, callbacks } = fixture();
  await act(async () => { expect(result.current.settingsReady()).toBe(false); });
  expect(getSettings).toHaveBeenCalledTimes(2);
  expect(updateSettings).not.toHaveBeenCalled();
  expect(callbacks.onNotReady).toHaveBeenCalledTimes(1);
  await act(async () => {
    retry.resolve({ settings: { ...defaultSettings, historyRetention: "forever", setupComplete: false }, revision: 2 });
    first.resolve({ settings: defaultSettings, revision: 1 });
  });
  expect(result.current.settings.historyRetention).toBe("forever");
  expect(result.current.settingsReady()).toBe(true);
  expect(callbacks.onSetupNeeded).toHaveBeenCalledTimes(1);
});
test("a settings event supersedes both pending initial and retry reads", async () => {
  const first = deferred<SettingsSnapshot>();
  const retry = deferred<SettingsSnapshot>();
  vi.mocked(getSettings).mockReturnValueOnce(first.promise).mockReturnValueOnce(retry.promise);
  const { result } = fixture();
  await act(async () => { result.current.settingsReady(); });
  await waitFor(() => expect(subscriptions.size).toBe(1));
  await act(async () => {
    for (const handler of subscriptions) handler({ payload: { settings: { ...defaultSettings, historyRetention: "one_week" }, revision: 3 } });
    retry.resolve({ settings: defaultSettings, revision: 2 });
    first.resolve({ settings: defaultSettings, revision: 1 });
  });
  expect(result.current.settings.historyRetention).toBe("one_week");
  expect(updateSettings).not.toHaveBeenCalled();
});
test("settings obtained after a failed first read route unfinished setup only once", async () => {
  vi.mocked(getSettings).mockRejectedValueOnce(new Error("not ready"));
  const { result, callbacks } = fixture();
  await waitFor(() => expect(callbacks.onError).toHaveBeenCalledTimes(1));
  vi.mocked(getSettings).mockResolvedValueOnce({ settings: { ...defaultSettings, setupComplete: false }, revision: 1 });
  await act(async () => { expect(result.current.settingsReady()).toBe(false); });
  await waitFor(() => expect(result.current.settingsLoaded).toBe(true));
  expect(callbacks.onSetupNeeded).toHaveBeenCalledTimes(1);
  await act(async () => {
    for (const handler of subscriptions) handler({ payload: { settings: { ...defaultSettings, setupComplete: false, theme: "dark" }, revision: 2 } });
  });
  expect(callbacks.onSetupNeeded).toHaveBeenCalledTimes(1);
  expect(result.current.settings.theme).toBe("dark");
});
