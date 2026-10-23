import { act, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { beforeEach, expect, test, vi } from "vitest";
import App from "../App";
import { defaultSettings, getSettings, getShortcutWarning, updateSettings, listDictionaryCandidates,
  listHistory, getGpuDiagnostics, getModelStatus, getStartupHotkeyWarning, stopRecording } from "../api";
import type { DictionaryCandidate, HistoryItem, Settings, SettingsSnapshot } from "../types";

const events = vi.hoisted(() => new Map<string, Set<(event: { payload: unknown }) => void>>());
vi.mock("@tauri-apps/api/event", () => ({
  listen: vi.fn(async (name: string, handler: (event: { payload: unknown }) => void) => {
    const listeners = events.get(name) ?? new Set();
    events.set(name, listeners);
    listeners.add(handler);
    return () => { listeners.delete(handler); };
  }),
}));
vi.mock("@tauri-apps/api/webviewWindow", () => ({
  getCurrentWebviewWindow: () => { throw new Error("Browser entry must not call this API"); },
}));
vi.mock("../api", async (original) => {
  const actual = await original<typeof import("../api")>();
  return {
    ...actual,
    getUiPreferences: async () => ({ settings: { uiLanguage: "en", theme: "system" }, revision: 1 }),
    getSettings: vi.fn(),
    updateSettings: vi.fn(),
    getStartupHotkeyWarning: vi.fn(async () => []),
    getShortcutWarning: vi.fn(async () => []),
    getAppState: async () => ({ phase: "idle", stage: "idle", operation: null, operationId: null, message: null, messageCode: null, lastResult: null }),
    getModelStatus: vi.fn(async () => ({ modelId: null, quantization: "4bit", installed: false, state: "not_configured", detail: "" })),
    getGpuDiagnostics: vi.fn(async () => null),
    listAudioDevices: async () => [],
    listHistory: vi.fn(async () => []),
    listDictionary: async () => [],
    listDictionaryCandidates: vi.fn(async () => []),
    stopRecording: vi.fn(),
  };
});

let settings: Settings;
beforeEach(() => {
  vi.clearAllMocks();
  events.clear();
  settings = { ...structuredClone(defaultSettings), setupComplete: true, uiLanguage: "en" };
  vi.mocked(getSettings).mockResolvedValue({ settings, revision: 1 });
  vi.mocked(updateSettings).mockReset();
  vi.mocked(getShortcutWarning).mockResolvedValue([]);
  vi.mocked(getStartupHotkeyWarning).mockResolvedValue([]);
  vi.mocked(listDictionaryCandidates).mockResolvedValue([]);
  vi.mocked(listHistory).mockResolvedValue([]);
  vi.mocked(getGpuDiagnostics).mockResolvedValue({ status: "unavailable", adapterName: null, driverVersion: null, memoryTotalMb: null, recommendation: "" });
  vi.mocked(getModelStatus).mockResolvedValue({ modelId: null, quantization: "4bit", installed: false, state: "not_configured", detail: "" });
  vi.spyOn(window, "confirm").mockReturnValue(true);
});

async function openSettings() {
  render(<App />);
  fireEvent.click(await screen.findByRole("button", { name: "Settings" }));
  await screen.findByDisplayValue(settings.openaiCorrectionModel);
}

test("browser startup renders the real Settings page without a false shortcut alert", async () => {
  await openSettings();
  expect(screen.queryByRole("alert")).toBeNull();
});

test("Settings shows the inactive shortcut names when native registration failed", async () => {
  vi.mocked(getShortcutWarning).mockResolvedValue(["Voice Translate (Ctrl+Shift+Y)", "Ask Anything (Ctrl+Shift+A)"]);
  await openSettings();
  await waitFor(() => expect(screen.getByRole("alert").textContent).toContain("Inactive shortcuts: Voice Translate (Ctrl+Shift+Y), Ask Anything (Ctrl+Shift+A)"));
  expect(screen.getByRole("alert").textContent).toContain("Some saved shortcuts could not be activated at startup.");
});

test("settings saves stay ordered and an old acknowledgement cannot replace a newer edit", async () => {
  let acknowledge!: (value: SettingsSnapshot) => void;
  vi.mocked(updateSettings).mockImplementationOnce(() => new Promise((resolve) => { acknowledge = resolve; }));
  vi.mocked(updateSettings).mockImplementationOnce(async (patch) => ({ settings: { ...settings, ...patch }, revision: 3 }));
  await openSettings();
  const model = screen.getByDisplayValue(settings.openaiCorrectionModel);
  fireEvent.change(model, { target: { value: "first-model" } });
  await waitFor(() => expect(updateSettings).toHaveBeenCalledTimes(1));
  fireEvent.change(model, { target: { value: "newest-model" } });
  expect(updateSettings).toHaveBeenCalledTimes(1);
  await act(async () => { acknowledge({ settings: { ...settings, openaiCorrectionModel: "first-model" }, revision: 2 }); });
  await waitFor(() => expect(updateSettings).toHaveBeenCalledTimes(2));
  expect(vi.mocked(updateSettings).mock.calls[1][0]).toMatchObject({ openaiCorrectionModel: "newest-model" });
  expect(screen.getByDisplayValue("newest-model")).toBeTruthy();
});

test("a newer settings event wins over a late initial read", async () => {
  let finishRead!: (value: SettingsSnapshot) => void;
  vi.mocked(getSettings).mockImplementationOnce(() => new Promise((resolve) => { finishRead = resolve; }));
  render(<App />);
  await waitFor(() => expect(events.get("settings-changed")?.size).toBe(2));
  await act(async () => {
    for (const handler of events.get("settings-changed") ?? []) handler({ payload: { settings: { ...settings, openaiCorrectionModel: "event-model" }, revision: 2 } });
    finishRead({ settings, revision: 1 });
  });
  fireEvent.click(screen.getByRole("button", { name: "Settings" }));
  expect(await screen.findByDisplayValue("event-model")).toBeTruthy();
});

function deferred<T>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((done) => { resolve = done; });
  return { promise, resolve };
}
const candidate = (id: number): DictionaryCandidate => ({ id, originalSpan: "open ai", preferredSpan: `Suggested-${id}`, confidence: 1, historyId: null, createdAt: "2026-10-23" });
async function emit(name: string, payload: unknown) {
  await act(async () => {
    for (const handler of events.get(name) ?? []) handler({ payload });
  });
}
async function phase(value: string) {
  await emit("app-state", { phase: value, stage: value, operation: "dictate", operationId: 1, message: null, messageCode: null, lastResult: null });
}
async function navigate(name: string) {
  fireEvent.click(await screen.findByRole("button", { name }));
}
async function changeRetention(value: string) {
  await navigate("Privacy");
  fireEvent.change(screen.getByRole("combobox", { name: "History retention" }), { target: { value } });
  await waitFor(() => expect(updateSettings).toHaveBeenCalled());
}

test("startup shortcut notice does not prevent the main interface from rendering", async () => {
  vi.mocked(getStartupHotkeyWarning).mockResolvedValue(["Some saved hotkeys overlap or could not be registered."]);
  render(<App />);
  await navigate("Dictionary");
  expect(await screen.findByRole("button", { name: "Add entry" })).toBeTruthy();
});

test("dictionary entry and shortcut completion load current retained candidates", async () => {
  vi.mocked(listDictionaryCandidates).mockResolvedValue([candidate(1)]);
  render(<App />);
  await navigate("Dictionary");
  expect(await screen.findByText("Suggested-1")).toBeTruthy();
  await phase("recording");
  vi.mocked(listDictionaryCandidates).mockResolvedValue([candidate(2)]);
  await phase("completed");
  expect(await screen.findByText("Suggested-2")).toBeTruthy();
  expect(screen.queryByText("Suggested-1")).toBeNull();
  expect(stopRecording).not.toHaveBeenCalled();
});

test("a newer visible-page refresh wins over a delayed earlier dictionary response", async () => {
  render(<App />);
  await navigate("Settings");
  const old = deferred<DictionaryCandidate[]>();
  vi.mocked(listDictionaryCandidates).mockReturnValueOnce(old.promise);
  await navigate("Dictionary");
  await waitFor(() => expect(listDictionaryCandidates).toHaveBeenCalledTimes(1));
  vi.mocked(listDictionaryCandidates).mockResolvedValue([candidate(2)]);
  await phase("completed");
  expect(await screen.findByText("Suggested-2")).toBeTruthy();
  await act(async () => { old.resolve([candidate(1)]); });
  expect(screen.queryByText("Suggested-1")).toBeNull();
});

test("Never hides candidates and invalidates an older pending response", async () => {
  vi.mocked(listDictionaryCandidates).mockResolvedValue([candidate(1)]);
  vi.mocked(updateSettings).mockImplementation(async (patch) => ({ settings: { ...settings, ...patch }, revision: 2 }));
  render(<App />);
  await navigate("Dictionary");
  expect(await screen.findByText("Suggested-1")).toBeTruthy();
  const old = deferred<DictionaryCandidate[]>();
  vi.mocked(listDictionaryCandidates).mockReturnValueOnce(old.promise);
  await phase("completed");
  vi.mocked(listDictionaryCandidates).mockResolvedValue([]);
  await changeRetention("never");
  await navigate("Dictionary");
  await act(async () => { old.resolve([candidate(1)]); });
  expect(screen.queryByText("Suggested-1")).toBeNull();
});

test("shorter retention reloads current candidates instead of restoring the cache", async () => {
  settings.historyRetention = "forever";
  vi.mocked(listDictionaryCandidates).mockResolvedValue([candidate(1), candidate(2)]);
  vi.mocked(updateSettings).mockImplementation(async (patch) => ({ settings: { ...settings, ...patch }, revision: 2 }));
  render(<App />);
  await navigate("Dictionary");
  expect(await screen.findByText("Suggested-1")).toBeTruthy();
  vi.mocked(listDictionaryCandidates).mockResolvedValue([candidate(2)]);
  await changeRetention("24_hours");
  await navigate("Dictionary");
  expect(await screen.findByText("Suggested-2")).toBeTruthy();
  expect(screen.queryByText("Suggested-1")).toBeNull();
});

test("post-save refresh replaces a candidate read that arrived before the purge", async () => {
  settings.historyRetention = "forever";
  const save = deferred<SettingsSnapshot>();
  vi.mocked(updateSettings).mockReturnValueOnce(save.promise);
  vi.mocked(listDictionaryCandidates).mockResolvedValue([candidate(1), candidate(2)]);
  render(<App />);
  await changeRetention("24_hours");
  await navigate("Dictionary");
  expect(await screen.findByText("Suggested-1")).toBeTruthy();
  vi.mocked(listDictionaryCandidates).mockResolvedValue([candidate(2)]);
  await act(async () => { save.resolve({ settings: { ...settings, historyRetention: "24_hours" }, revision: 2 }); });
  await waitFor(() => expect(screen.queryByText("Suggested-1")).toBeNull());
  expect(screen.getByText("Suggested-2")).toBeTruthy();
});

function history(id: number): HistoryItem {
  return { id, transcriptText: `Transcript-${id}`, processedText: null, sourceText: null,
    instructionText: null, actionKind: null, searchSite: null, mode: "dictate", asrProvider: "mock",
    llmProvider: null, targetLanguage: null, appCategory: null, durationMs: 100, latencyMs: 50,
    createdAt: "2026-10-23T00:00:00Z", hasAudio: false, retryOfId: null, insertionResult: null,
    insertionDetail: null, expiresAt: null };
}
test("History entry and shortcut completion refresh retained rows", async () => {
  vi.mocked(listHistory).mockResolvedValue([history(1)]);
  render(<App />);
  await navigate("History");
  expect(await screen.findByText("Transcript-1")).toBeTruthy();
  await phase("recording");
  vi.mocked(listHistory).mockResolvedValue([history(2)]);
  await phase("completed");
  expect(await screen.findByText("Transcript-2")).toBeTruthy();
  expect(screen.queryByText("Transcript-1")).toBeNull();
});

test("retention changes stay disabled until stored settings load", async () => {
  const read = deferred<SettingsSnapshot>();
  vi.mocked(getSettings).mockReturnValueOnce(read.promise);
  render(<App />);
  await navigate("Privacy");
  expect((screen.getByRole("combobox", { name: "History retention" }) as HTMLSelectElement).disabled).toBe(true);
  expect(updateSettings).not.toHaveBeenCalled();
  await act(async () => { read.resolve({ settings: { ...settings, historyRetention: "forever" }, revision: 1 }); });
  expect((screen.getByRole("combobox", { name: "History retention" }) as HTMLSelectElement).disabled).toBe(false);
});

test("a failed optional startup call still allows settings to load and save", async () => {
  vi.mocked(getGpuDiagnostics).mockRejectedValueOnce(new Error("GPU query failed"));
  vi.mocked(updateSettings).mockImplementation(async (patch) => ({ settings: { ...settings, ...patch }, revision: 2 }));
  render(<App />);
  await changeRetention("forever");
  expect(vi.mocked(updateSettings).mock.calls[0][0]).toMatchObject({ historyRetention: "forever" });
});

test("the first settings event routes unfinished setup once", async () => {
  vi.mocked(getSettings).mockReturnValueOnce(new Promise(() => {}));
  render(<App />);
  await navigate("Privacy");
  await emit("settings-changed", { settings: { ...settings, setupComplete: false }, revision: 2 });
  expect(await screen.findByRole("button", { name: "Finish setup" })).toBeTruthy();
  await navigate("Status");
  await emit("settings-changed", { settings: { ...settings, setupComplete: false, theme: "dark" }, revision: 3 });
  expect(screen.queryByRole("button", { name: "Finish setup" })).toBeNull();
});

test("Finish setup navigates only after a successful persisted save", async () => {
  settings.setupComplete = false;
  vi.mocked(getModelStatus).mockResolvedValue({ modelId: "mock", quantization: settings.modelQuantization, installed: true, state: "ready", detail: "" });
  const save = deferred<SettingsSnapshot>();
  vi.mocked(updateSettings).mockRejectedValueOnce(new Error("save failed")).mockReturnValueOnce(save.promise);
  render(<App />);
  fireEvent.click(await screen.findByRole("button", { name: "Finish setup" }));
  await waitFor(() => expect(updateSettings).toHaveBeenCalledTimes(1));
  expect(vi.mocked(updateSettings).mock.calls[0][0]).toMatchObject({ setupComplete: true });
  expect(screen.getByRole("button", { name: "Finish setup" })).toBeTruthy();
  fireEvent.click(screen.getByRole("button", { name: "Finish setup" }));
  await waitFor(() => expect(updateSettings).toHaveBeenCalledTimes(2));
  expect(vi.mocked(updateSettings).mock.calls[1][0]).toMatchObject({ setupComplete: true });
  expect(screen.getByRole("button", { name: "Finish setup" })).toBeTruthy();
  await act(async () => { save.resolve({ settings: { ...settings, setupComplete: true }, revision: 2 }); });
  await waitFor(() => expect(screen.queryByRole("button", { name: "Finish setup" })).toBeNull());
});

test("Finish setup cannot save before stored settings are known", async () => {
  vi.mocked(getSettings).mockReturnValue(new Promise(() => {}));
  vi.mocked(getModelStatus).mockResolvedValue({ modelId: "mock", quantization: defaultSettings.modelQuantization, installed: true, state: "ready", detail: "" });
  render(<App />);
  await navigate("Setup");
  const finish = await screen.findByRole("button", { name: "Finish setup" });
  await waitFor(() => expect((finish as HTMLButtonElement).disabled).toBe(false));
  fireEvent.click(finish);
  expect(updateSettings).not.toHaveBeenCalled();
  expect(screen.getByRole("button", { name: "Finish setup" })).toBeTruthy();
});
