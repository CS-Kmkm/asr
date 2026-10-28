import { act, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { beforeEach, expect, test, vi } from "vitest";
import App from "../App";
import { listen } from "@tauri-apps/api/event";
import { defaultSettings, getSettings, getShortcutWarning, updateSettings, listDictionaryCandidates,
  listHistory, listHistoryPage, deleteAllHistory, reprocessHistoryText, copyToClipboard, getGpuDiagnostics, getModelStatus, getStartupHotkeyWarning, stopRecording, loadModel, previewHistoryRetentionPurge, getProviderCredentials } from "../api";
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
  const listHistory = vi.fn<typeof actual.listHistory>(async () => []);
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
    getProviderCredentials: vi.fn(async () => []),
    listAudioDevices: async () => [],
    listHistory,
    deleteAllHistory: vi.fn(async () => 0),
    reprocessHistoryText: vi.fn(),
    copyToClipboard: vi.fn(async () => {}),
    listHistoryPage: vi.fn<typeof actual.listHistoryPage>(),
    listDictionary: async () => [],
    listDictionaryCandidates: vi.fn(async () => []),
    stopRecording: vi.fn(),
    previewHistoryRetentionPurge: vi.fn(async () => ({ historyItems: 0, recordings: 0 })),
    loadModel: vi.fn(async () => ({ modelId: "mock", quantization: "4bit", installed: true, state: "ready", detail: "" })),
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
  vi.mocked(reprocessHistoryText).mockReset();
  vi.mocked(listHistoryPage).mockReset().mockImplementation(async (filter, options) => ({ items: await listHistory(filter, options), retainedIds: options?.knownIds ?? [] }));
  vi.mocked(getGpuDiagnostics).mockResolvedValue({ status: "unavailable", adapterName: null, driverVersion: null, memoryTotalMb: null, recommendation: "" });
  vi.mocked(getModelStatus).mockResolvedValue({ modelId: null, quantization: "4bit", installed: false, state: "not_configured", detail: "" });
  vi.mocked(getProviderCredentials).mockResolvedValue([]);
  vi.spyOn(window, "confirm").mockReturnValue(true);
});

async function openSettings() {
  render(<App />);
  fireEvent.click(await screen.findByRole("button", { name: "Settings" }));
  fireEvent.click(screen.getByRole("button", { name: "Writing and translation" }));
  await screen.findByDisplayValue(settings.openaiCorrectionModel);
}

test("browser startup renders the real Settings page without a false shortcut alert", async () => {
  await openSettings();
  expect(screen.queryByRole("alert")).toBeNull();
});

test("Settings shows the inactive shortcut names when native registration failed", async () => {
  vi.mocked(getShortcutWarning).mockResolvedValue(["Voice Translate (Ctrl+Shift+Y)", "Ask Anything (Ctrl+Shift+A)"]);
  await openSettings();
  fireEvent.click(screen.getByRole("button", { name: "Shortcuts" }));
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
  expect(updateSettings).not.toHaveBeenCalled();
  fireEvent.blur(model);
  await waitFor(() => expect(updateSettings).toHaveBeenCalledTimes(1));
  fireEvent.change(model, { target: { value: "newest-model" } });
  fireEvent.blur(model);
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
  let reject!: (reason: unknown) => void;
  const promise = new Promise<T>((done, fail) => { resolve = done; reject = fail; });
  return { promise, resolve, reject };
}
const candidate = (id: number): DictionaryCandidate => ({ id, originalSpan: "open ai", preferredSpan: `Suggested-${id}`, confidence: 1, historyId: null, createdAt: "2026-10-23" });
async function emit(name: string, payload: unknown) {
  if (name === "app-state") payload = { message: null, messageCode: null, lastResult: null, ...(payload as Record<string, unknown>) };
  await act(async () => {
    for (const handler of events.get(name) ?? []) handler({ payload });
  });
}
async function phase(value: string) {
  await emit("app-state", { phase: value, stage: value, operation: "dictate", operationId: 1, message: null, messageCode: null, lastResult: null });
}
async function navigate(name: string) {
  if (["Privacy", "Models", "Setup", "Diagnostics"].includes(name) && !screen.queryByRole("button", { name })) {
    fireEvent.click(await screen.findByRole("button", { name: "Settings" }));
  }
  fireEvent.click(await screen.findByRole("button", { name: name === "Status" ? "Home" : name }));
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

test("home offers direct copy and task navigation without secondary settings links", async () => {
  render(<App />);
  const nav = within(await screen.findByRole("navigation", { name: "Main navigation" }));
  expect(nav.queryByRole("button", { name: "Models" })).toBeNull();
  await emit("app-state", { phase: "completed", stage: "completed", lastResult: "A useful result" });
  fireEvent.click(within(screen.getByRole("article", { name: "Latest result" })).getByRole("button", { name: "Copy" }));
  await waitFor(() => expect(copyToClipboard).toHaveBeenCalledWith("A useful result"));
  fireEvent.click(screen.getByRole("button", { name: "View history" }));
  expect(screen.getByRole("searchbox", { name: "Search history" })).toBeTruthy();
});

test("settings categories preserve shortcut drafts and expose only the active controls", async () => {
  render(<App />);
  fireEvent.click(await screen.findByRole("button", { name: "Settings" }));
  expect(screen.getByRole("button", { name: "Settings" }).hasAttribute("aria-expanded")).toBe(false);
  expect(screen.getByRole("combobox", { name: "Microphone" })).toBeTruthy();
  expect(screen.queryByRole("combobox", { name: "Language" })).toBeNull();
  fireEvent.click(screen.getByRole("button", { name: "Shortcuts" }));
  fireEvent.change(screen.getByRole("textbox", { name: "Dictation shortcuts 1" }), { target: { value: "Ctrl+Alt+D" } });
  document.documentElement.scrollTop = 600;
  document.body.scrollTop = 600;
  fireEvent.click(screen.getByRole("button", { name: "General" }));
  expect(document.documentElement.scrollTop).toBe(0);
  expect(document.body.scrollTop).toBe(0);
  expect(screen.getByRole("combobox", { name: "Language" })).toBeTruthy();
  expect(screen.queryByRole("textbox", { name: "Dictation shortcuts 1" })).toBeNull();
  fireEvent.click(screen.getByRole("button", { name: "Models" }));
  fireEvent.click(screen.getByRole("button", { name: "Shortcuts" }));
  expect(screen.getByRole("textbox", { name: "Dictation shortcuts 1" })).toHaveProperty("value", "Ctrl+Alt+D");
  expect(updateSettings).not.toHaveBeenCalled();
  fireEvent.click(screen.getByRole("button", { name: "Save shortcuts" }));
  await waitFor(() => expect(updateSettings).toHaveBeenCalledWith(expect.objectContaining({ shortcuts: expect.objectContaining({ dictate: ["Ctrl+Alt+D"] }) })));
});

test("each history entry keeps its own reprocessing choices", async () => {
  settings.translationTargetLanguages = ["en", "ja"];
  settings.translationTargetLanguage = "en";
  vi.mocked(listHistory).mockResolvedValue([history(1), history(2)]);
  vi.mocked(reprocessHistoryText).mockResolvedValue({ text: "translated", insertion: "history_only", durationMs: 0, latencyMs: 1 });
  render(<App />);
  await navigate("History");
  await screen.findByText("Transcript-1");
  const rows = document.querySelectorAll<HTMLElement>(".history-list > article");
  for (const row of rows) fireEvent.click(within(row).getByText("Reprocess text"));
  expect(within(rows[0]).getByRole("option", { name: "Japanese" })).toHaveProperty("value", "ja");
  fireEvent.change(within(rows[0]).getByRole("combobox", { name: "Translation target" }), { target: { value: "ja" } });
  expect(within(rows[1]).getByRole("combobox", { name: "Translation target" })).toHaveProperty("value", "en");
  fireEvent.click(within(rows[0]).getByRole("button", { name: "Translate to…" }));
  await waitFor(() => expect(reprocessHistoryText).toHaveBeenCalledWith(1, { kind: "translate", target: "ja" }));
});

test("Home shows the recording status once while other pages keep the header status", async () => {
  render(<App />);
  await screen.findByRole("heading", { name: "Latest result" });
  expect(document.querySelector(".status-pill")).toBeNull();
  await navigate("History");
  expect(document.querySelector(".status-pill")).not.toBeNull();
});

test("Setup offers a return from Models until the user navigates elsewhere", async () => {
  render(<App />);
  await navigate("Setup");
  fireEvent.click(screen.getByRole("button", { name: "Configure model" }));
  fireEvent.click(screen.getByRole("button", { name: /Back to setup/ }));
  expect(screen.getByRole("button", { name: "Finish setup" })).toBeTruthy();
  fireEvent.click(screen.getByRole("button", { name: "Configure model" }));
  fireEvent.click(screen.getByRole("button", { name: "General" }));
  fireEvent.click(screen.getByRole("button", { name: "Models" }));
  expect(screen.queryByRole("button", { name: /Back to setup/ })).toBeNull();
});

test("model configuration and unsaved API key drafts survive settings category changes", async () => {
  vi.mocked(getProviderCredentials).mockResolvedValue([
    { provider: "openai", alias: settings.openaiApiKeyEnvVar, source: "missing", storeAvailable: true },
  ]);
  render(<App />);
  await navigate("Models");
  const key = screen.getByLabelText("OpenAI API key");
  await waitFor(() => expect(key).toHaveProperty("disabled", false));
  fireEvent.change(key, { target: { value: "test-only-unsaved-key" } });
  fireEvent.change(screen.getByRole("combobox", { name: "ASR backend" }), { target: { value: "builtin:vibevoice" } });
  await navigate("Privacy");
  expect(key.closest("[hidden]")).not.toBeNull();
  await navigate("Models");
  expect(screen.getByLabelText("OpenAI API key")).toHaveProperty("value", "test-only-unsaved-key");
  expect(screen.getByRole("combobox", { name: "ASR backend" })).toHaveProperty("value", "builtin:vibevoice");
  expect(updateSettings).not.toHaveBeenCalled();
});
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

test("History loads beyond the first hundred and ignores a stale page after a search", async () => {
  const nextPage = deferred<HistoryItem[]>();
  vi.mocked(listHistory).mockImplementation(async (_filter, options) => {
    if (options?.query === "new search") return [history(900)];
    if (options?.before?.id === 100) return nextPage.promise;
    return Array.from({ length: 100 }, (_, index) => history(index + 1));
  });
  render(<App />);
  await navigate("History");
  const more = await screen.findByRole("button", { name: "Load more" });
  await waitFor(() => expect((more as HTMLButtonElement).disabled).toBe(false));
  fireEvent.click(more);
  await waitFor(() => expect(listHistory).toHaveBeenCalledWith("all", expect.objectContaining({ query: "", before: { createdAt: "2026-10-23T00:00:00Z", id: 100 } })));
  expect((more as HTMLButtonElement).disabled).toBe(true);
  fireEvent.change(screen.getByRole("searchbox", { name: "Search history" }), { target: { value: "new search" } });
  fireEvent.click(screen.getByRole("button", { name: "Search" }));
  expect(await screen.findByText("Transcript-900")).toBeTruthy();
  await act(async () => { nextPage.resolve([history(101)]); });
  expect(screen.queryByText("Transcript-101")).toBeNull();
  expect(screen.queryByText("Transcript-1")).toBeNull();
  expect(screen.queryByRole("button", { name: "Load more" })).toBeNull();
});

test("History appends the next page and changing filter keeps the submitted query", async () => {
  vi.mocked(listHistory).mockImplementation(async (filter, options) => {
    if (filter === "translate") return [history(700)];
    if (options?.before?.id === 100) return [history(101)];
    return Array.from({ length: 100 }, (_, index) => history(index + 1));
  });
  render(<App />);
  await navigate("History");
  const more = await screen.findByRole("button", { name: "Load more" });
  await waitFor(() => expect((more as HTMLButtonElement).disabled).toBe(false));
  fireEvent.click(more);
  expect(await screen.findByText("Transcript-101")).toBeTruthy();
  expect(screen.getByText("Transcript-1")).toBeTruthy();
  expect(screen.queryByRole("button", { name: "Load more" })).toBeNull();
  fireEvent.change(screen.getByRole("searchbox", { name: "Search history" }), { target: { value: "  日本語  " } });
  fireEvent.click(screen.getByRole("button", { name: "Search" }));
  await waitFor(() => expect(listHistory).toHaveBeenCalledWith("all", { query: "日本語", before: null, knownIds: [] }));
  fireEvent.click(within(screen.getByRole("group", { name: "History filter" })).getByRole("button", { name: "Translate" }));
  expect(await screen.findByText("Transcript-700")).toBeTruthy();
  expect(listHistory).toHaveBeenLastCalledWith("translate", { query: "日本語", before: null, knownIds: [] });
  expect(screen.queryByText("Transcript-1")).toBeNull();
});

test("History drops previously displayed rows purged before the next page", async () => {
  vi.mocked(listHistory).mockResolvedValue(Array.from({ length: 100 }, (_, index) => history(index + 1)));
  render(<App />);
  await navigate("History");
  const more = await screen.findByRole("button", { name: "Load more" });
  await waitFor(() => expect((more as HTMLButtonElement).disabled).toBe(false));
  vi.mocked(listHistoryPage).mockResolvedValueOnce({ items: [history(101)], retainedIds: Array.from({ length: 99 }, (_, index) => index + 2) });
  fireEvent.click(more);
  expect(await screen.findByText("Transcript-101")).toBeTruthy();
  expect(screen.queryByText("Transcript-1")).toBeNull();
  expect(screen.getByText("Transcript-2")).toBeTruthy();
});

test("History search failure offers reload without asserting no matches", async () => {
  render(<App />);
  await navigate("History");
  const search = deferred<Awaited<ReturnType<typeof listHistoryPage>>>();
  vi.mocked(listHistoryPage).mockReturnValueOnce(search.promise);
  fireEvent.change(screen.getByRole("searchbox", { name: "Search history" }), { target: { value: "a query" } });
  fireEvent.click(screen.getByRole("button", { name: "Search" }));
  expect(screen.getByRole("status").textContent).toContain("Loading...");
  expect(screen.queryByText("No matching history")).toBeNull();
  await act(async () => { search.reject(new Error("query failed")); });
  expect(await screen.findByRole("button", { name: "Reload history" })).toBeTruthy();
  expect(screen.queryByText("No matching history")).toBeNull();
  fireEvent.click(screen.getByRole("button", { name: "Reload history" }));
  expect(await screen.findByText("No matching history")).toBeTruthy();
});

test("a successful History deletion clears an earlier load failure", async () => {
  render(<App />);
  await navigate("History");
  vi.mocked(listHistoryPage).mockRejectedValueOnce(new Error("query failed"));
  fireEvent.click(screen.getByRole("button", { name: "Search" }));
  await screen.findByRole("button", { name: "Reload history" });
  fireEvent.click(screen.getByRole("button", { name: "Delete all" }));
  await waitFor(() => expect(deleteAllHistory).toHaveBeenCalled());
  await waitFor(() => expect(screen.queryByRole("button", { name: "Reload history" })).toBeNull());
  expect(screen.getByText("No dictations yet")).toBeTruthy();
});

test("History without audio supports raw copy, explicit profile correction and target translation", async () => {
  settings.translationTargetLanguages = ["en", "ja"];
  settings.globalStyleProfile = { formality: "formal", detail: "concise", guidance: "email style" };
  vi.mocked(listHistory).mockResolvedValue([{ ...history(12), processedText: "old processed text" }]);
  const correction = deferred<Awaited<ReturnType<typeof reprocessHistoryText>>>();
  vi.mocked(reprocessHistoryText).mockReturnValueOnce(correction.promise).mockResolvedValueOnce({ text: "翻訳", insertion: "history_only", durationMs: 0, latencyMs: 1 });
  render(<App />);
  await navigate("History");
  const correct = await screen.findByRole("button", { name: "Re-correct" });
  expect((screen.getByRole("button", { name: /^Retry:/ }) as HTMLButtonElement).disabled).toBe(true);
  expect((correct as HTMLButtonElement).disabled).toBe(false);
  fireEvent.click(screen.getByRole("button", { name: /^Copy raw:/ }));
  expect(screen.getByRole("button", { name: /^Copy raw:/ }).getAttribute("aria-label")).toContain("old processed text");
  await waitFor(() => expect(copyToClipboard).toHaveBeenCalledWith("Transcript-12"));
  fireEvent.change(screen.getByRole("combobox", { name: "Correction profile" }), { target: { value: "global" } });
  fireEvent.click(correct);
  expect(reprocessHistoryText).toHaveBeenCalledWith(12, { kind: "correct", profile: "global" });
  expect((screen.getByRole("button", { name: "Translate to…" }) as HTMLButtonElement).disabled).toBe(true);
  fireEvent.click(correct);
  expect(reprocessHistoryText).toHaveBeenCalledTimes(1);
  await act(async () => { correction.resolve({ text: "corrected", insertion: "history_only", durationMs: 0, latencyMs: 1 }); });
  await waitFor(() => expect((screen.getByRole("button", { name: "Translate to…" }) as HTMLButtonElement).disabled).toBe(false));
  fireEvent.change(screen.getByRole("combobox", { name: "Translation target" }), { target: { value: "ja" } });
  fireEvent.click(screen.getByRole("button", { name: "Translate to…" }));
  await waitFor(() => expect(reprocessHistoryText).toHaveBeenLastCalledWith(12, { kind: "translate", target: "ja" }));
});

test("History exposes returned text for copying even under a filter and source-only search", async () => {
  vi.mocked(listHistory).mockResolvedValue([{ ...history(12), mode: "ask", sourceText: "selected operand" }]);
  vi.mocked(reprocessHistoryText).mockResolvedValueOnce({ text: "new corrected output", insertion: "history_only", durationMs: 0, latencyMs: 1 });
  render(<App />);
  await navigate("History");
  fireEvent.click(within(screen.getByRole("group", { name: "History filter" })).getByRole("button", { name: "Ask" }));
  fireEvent.change(screen.getByRole("searchbox", { name: "Search history" }), { target: { value: "selected operand" } });
  fireEvent.click(screen.getByRole("button", { name: "Search" }));
  fireEvent.click(await screen.findByRole("button", { name: "Re-correct" }));
  const result = await screen.findByRole("region", { name: "Reprocessed result" });
  expect(result.textContent).toContain("new corrected output");
  expect(within(screen.getByRole("group", { name: "History filter" })).getByRole("button", { name: "Ask" }).getAttribute("aria-pressed")).toBe("true");
  expect((screen.getByRole("searchbox", { name: "Search history" }) as HTMLInputElement).value).toBe("selected operand");
  fireEvent.click(screen.getByRole("button", { name: "Copy result" }));
  await waitFor(() => expect(copyToClipboard).toHaveBeenCalledWith("new corrected output"));
  fireEvent.click(screen.getByRole("button", { name: "Dismiss result" }));
  expect(screen.queryByRole("region", { name: "Reprocessed result" })).toBeNull();
});

test("Delete all suppresses a late text-only reprocessing response", async () => {
  vi.mocked(listHistory).mockResolvedValue([history(12)]);
  const pending = deferred<Awaited<ReturnType<typeof reprocessHistoryText>>>();
  vi.mocked(reprocessHistoryText).mockReturnValueOnce(pending.promise);
  render(<App />);
  await navigate("History");
  fireEvent.click(await screen.findByRole("button", { name: "Re-correct" }));
  fireEvent.click(screen.getByRole("button", { name: "Delete all" }));
  await screen.findByText("History deleted.");
  await act(async () => { pending.resolve({ text: "late output", insertion: "history_only", durationMs: 0, latencyMs: 1 }); });
  expect(screen.queryByRole("region", { name: "Reprocessed result" })).toBeNull();
  expect(screen.queryByText("late output")).toBeNull();
});

test.each([false, true])("Delete all suppresses a stale post-reprocess refresh notice (failure=%s)", async (failure) => {
  vi.mocked(listHistory).mockResolvedValue([history(12)]);
  vi.mocked(reprocessHistoryText).mockResolvedValueOnce({ text: "new output", insertion: "history_only", durationMs: 0, latencyMs: 1 });
  render(<App />);
  await navigate("History");
  await screen.findByRole("button", { name: "Re-correct" });
  const refresh = deferred<Awaited<ReturnType<typeof listHistoryPage>>>();
  vi.mocked(listHistoryPage).mockReturnValueOnce(refresh.promise);
  fireEvent.click(screen.getByRole("button", { name: "Re-correct" }));
  await screen.findByRole("region", { name: "Reprocessed result" });
  fireEvent.click(screen.getByRole("button", { name: "Delete all" }));
  await screen.findByText("History deleted.");
  await act(async () => {
    if (failure) refresh.reject(new Error("stale refresh failure"));
    else refresh.resolve({ items: [history(12)], retainedIds: [] });
  });
  expect(screen.queryByText("History retry completed.")).toBeNull();
  expect(screen.queryByText("stale refresh failure")).toBeNull();
  expect(screen.queryByRole("region", { name: "Reprocessed result" })).toBeNull();
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

test("an AI failure survives completion warnings until the next operation", async () => {
  await openSettings();
  await emit("app-state", { phase: "processing", stage: "correcting", operation: "recording", operationId: 10, message: "busy", messageCode: null });
  await emit("status", { kind: "text_correction_failed", code: "ai_invalid_api_key", outcome: "using_transcript", message: "specific failure cause" });
  const shown = document.querySelector(".notice-message")!.textContent;
  await emit("status", { kind: "success", message: "generic completion" });
  for (const kind of ["history_save_failed", "metric_save_failed", "artifact_cleanup_failed"]) await emit("status", { kind, message: "later operational warning" });
  await emit("app-state", { phase: "completed", stage: "completed", operation: null, operationId: 10 });
  expect(document.querySelector(".notice-message")!.textContent).toBe(shown);
  await emit("app-state", { phase: "recording", stage: "recording", operation: "recording", operationId: 11 });
  await emit("status", { kind: "success", message: "new completion" });
  expect(document.querySelector(".notice-message")!.textContent).toBe("new completion");
});

test("History Retry retains cancellation across processing and insertion", async () => {
  render(<App />);
  await navigate("History");
  for (const phase of ["processing", "injecting"]) {
    await emit("app-state", { phase, stage: "transcribing", operation: "retry", operationId: 1, message: "different wording" });
    expect(screen.getByRole("button", { name: "Cancel" })).toBeTruthy();
  }
  await emit("app-state", { phase: "completed", stage: "completed", operation: null, operationId: 1 });
  expect(screen.queryByRole("button", { name: "Cancel" })).toBeNull();
});

test("model configuration waits for preceding settings saves before loading", async () => {
  const first = deferred<SettingsSnapshot>(), second = deferred<SettingsSnapshot>();
  vi.mocked(updateSettings).mockReturnValueOnce(first.promise).mockReturnValueOnce(second.promise);
  await openSettings();
  fireEvent.click(screen.getByRole("button", { name: "Input settings" }));
  fireEvent.click(screen.getByRole("switch", { name: "Interaction sounds" }));
  await waitFor(() => expect(updateSettings).toHaveBeenCalledTimes(1));
  await navigate("Models");
  fireEvent.change(screen.getByRole("combobox", { name: "ASR backend" }), { target: { value: "builtin:vibevoice" } });
  fireEvent.click(screen.getByRole("button", { name: "Load Model" }));
  expect(updateSettings).toHaveBeenCalledTimes(1);
  expect(loadModel).not.toHaveBeenCalled();
  await act(async () => first.resolve({ settings: { ...settings, interactionSounds: true }, revision: 2 }));
  await waitFor(() => expect(updateSettings).toHaveBeenCalledTimes(2));
  const patch = vi.mocked(updateSettings).mock.calls[1][0];
  expect(patch).toMatchObject({ asrBackend: "vibevoice" });
  expect(loadModel).not.toHaveBeenCalled();
  await act(async () => second.resolve({ settings: { ...settings, interactionSounds: true, ...patch }, revision: 3 }));
  await waitFor(() => expect(loadModel).toHaveBeenCalledWith(null, "4bit"));
});

test("custom model creation waits for queued settings and preserves their values", async () => {
  const first = deferred<SettingsSnapshot>(), second = deferred<SettingsSnapshot>();
  vi.mocked(updateSettings).mockReturnValueOnce(first.promise).mockReturnValueOnce(second.promise);
  await openSettings();
  fireEvent.click(screen.getByRole("button", { name: "Input settings" }));
  fireEvent.click(screen.getByRole("switch", { name: "Interaction sounds" }));
  await waitFor(() => expect(updateSettings).toHaveBeenCalledTimes(1));
  await navigate("Models");
  fireEvent.change(screen.getByRole("combobox", { name: "ASR backend" }), { target: { value: "__additional_model__" } });
  fireEvent.change(screen.getByRole("textbox", { name: "Model ID" }), { target: { value: "org/custom" } });
  fireEvent.click(screen.getByRole("button", { name: "Add Model" }));
  expect(updateSettings).toHaveBeenCalledTimes(1);
  await act(async () => first.resolve({ settings: { ...settings, interactionSounds: true }, revision: 2 }));
  await waitFor(() => expect(updateSettings).toHaveBeenCalledTimes(2));
  const patch = vi.mocked(updateSettings).mock.calls[1][0];
  expect(patch).toMatchObject({ modelId: "org/custom", customModels: [{ asrBackend: "faster-whisper", modelId: "org/custom" }] });
  await act(async () => second.resolve({ settings: { ...settings, interactionSounds: true, ...patch }, revision: 3 }));
  await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
  expect(screen.getByRole("option", { name: /org\/custom/ })).toBeTruthy();
});

test("failed Finish retains unsaved microphone selection for the next attempt", async () => {
  settings.setupComplete = false;
  settings.microphoneId = "previous-microphone";
  vi.mocked(getModelStatus).mockResolvedValue({ modelId: "mock", quantization: "4bit", installed: true, state: "ready", detail: "" });
  vi.mocked(updateSettings).mockRejectedValueOnce(new Error("save failed"));
  render(<App />);
  const finish = await screen.findByRole("button", { name: "Finish setup" });
  fireEvent.change(screen.getByRole("combobox", { name: "Microphone" }), { target: { value: "" } });
  fireEvent.click(finish);
  await waitFor(() => expect(updateSettings).toHaveBeenCalledTimes(1));
  expect(vi.mocked(updateSettings).mock.calls[0][0]).toMatchObject({ microphoneId: null, setupComplete: true });
  await waitFor(() => expect(screen.getByRole("alert")).toBeTruthy());
  expect((screen.getByRole("combobox", { name: "Microphone" }) as HTMLSelectElement).value).toBe("");
  vi.mocked(updateSettings).mockResolvedValueOnce({ settings: { ...settings, microphoneId: null, setupComplete: true }, revision: 2 });
  fireEvent.click(screen.getByRole("button", { name: "Finish setup" }));
  await waitFor(() => expect(updateSettings).toHaveBeenCalledTimes(2));
  expect(vi.mocked(updateSettings).mock.calls[1][0]).toMatchObject({ microphoneId: null, setupComplete: true });
});

test("a failed confirmed retention save restores the displayed original value", async () => {
  settings.historyRetention = "forever";
  vi.mocked(updateSettings).mockRejectedValueOnce(new Error("save failed"));
  render(<App />);
  await changeRetention("one_week");
  await waitFor(() => expect(screen.getByRole("alert").textContent).toContain("save failed"));
  await waitFor(() => expect((screen.getByRole("combobox", { name: "History retention" }) as HTMLSelectElement).value).toBe("forever"));
});

test("a successful confirmed retention save shows the new value", async () => {
  settings.historyRetention = "forever";
  vi.mocked(updateSettings).mockImplementation(async (patch) => ({ settings: { ...settings, ...patch }, revision: 2 }));
  render(<App />);
  await changeRetention("one_week");
  await waitFor(() => expect((screen.getByRole("combobox", { name: "History retention" }) as HTMLSelectElement).value).toBe("one_week"));
});

test("confirming a delayed purge count preserves settings changed while waiting", async () => {
  settings.historyRetention = "forever";
  const count = deferred<{ historyItems: number; recordings: number }>();
  vi.mocked(previewHistoryRetentionPurge).mockReturnValueOnce(count.promise);
  let stored = settings, revision = 1;
  vi.mocked(updateSettings).mockImplementation(async (patch) => { stored = { ...stored, ...patch }; return { settings: stored, revision: ++revision }; });
  render(<App />);
  await navigate("Privacy");
  fireEvent.change(screen.getByRole("combobox", { name: "History retention" }), { target: { value: "one_week" } });
  await waitFor(() => expect(previewHistoryRetentionPurge).toHaveBeenCalled());
  await navigate("Settings");
  fireEvent.click(screen.getByRole("button", { name: "Writing and translation" }));
  const model = screen.getByRole("textbox", { name: "OpenAI model" });
  fireEvent.change(model, { target: { value: "changed-while-counting" } }); fireEvent.blur(model);
  await waitFor(() => expect(updateSettings).toHaveBeenCalledTimes(1));
  await act(async () => count.resolve({ historyItems: 1, recordings: 0 }));
  await waitFor(() => expect(updateSettings).toHaveBeenCalledTimes(2));
  expect(stored.openaiCorrectionModel).toBe("changed-while-counting");
  expect(stored.historyRetention).toBe("one_week");
});

test("late preparation events cannot reopen progress after the model is ready", async () => {
  render(<App />);
  await navigate("Models");
  await emit("model-status", { modelId: "mock", quantization: "4bit", installed: false, state: "loading", detail: "" });
  await emit("model-status", { modelId: "mock", quantization: "4bit", installed: true, state: "ready", detail: "" });
  await emit("model-progress", { stage: "warmup", model: null, completedBytes: null, totalBytes: null });
  await emit("status", { kind: "model_downloading", message: "Downloading the speech model files. This runs once; later starts use the local cache." });
  expect(screen.queryByRole("progressbar")).toBeNull();
  expect(document.querySelector(".notice-message")).toBeNull();
});

test("a ready event wins over a delayed loading snapshot", async () => {
  const snapshot = deferred<Awaited<ReturnType<typeof getModelStatus>>>();
  vi.mocked(getModelStatus).mockReturnValue(snapshot.promise);
  render(<App />);
  await waitFor(() => expect(events.get("model-status")?.size).toBeGreaterThan(0));
  await emit("model-status", { modelId: "mock", quantization: "4bit", installed: true, state: "ready", detail: "" });
  await act(async () => snapshot.resolve({ modelId: "mock", quantization: "4bit", installed: false, state: "loading", detail: "" }));
  await navigate("Models");
  expect(screen.getByText("Ready")).toBeTruthy();
  expect(screen.queryByRole("progressbar")).toBeNull();
  expect(document.querySelector(".notice-message")).toBeNull();
});

test("the initial model snapshot is requested only after the status listener exists", async () => {
  vi.mocked(getModelStatus).mockImplementation(async () => {
    expect(events.get("model-status")?.size).toBeGreaterThan(0);
    return { modelId: "mock", quantization: "4bit", installed: true, state: "ready", detail: "" };
  });
  render(<App />);
  await navigate("Models");
  await waitFor(() => expect(screen.getByText("Ready")).toBeTruthy());
});

test.each(["model", "unrelated"])("a failed %s subscription still allows the model snapshot", async (failure) => {
  const implementation = vi.mocked(listen).getMockImplementation()! as typeof listen;
  const failingEvent = failure === "model" ? "model-status" : "app-state";
  const failingListen: typeof listen = (event, handler, options) =>
    event === failingEvent
      ? Promise.reject(new Error("test subscription failed"))
      : implementation(event, handler, options);
  vi.mocked(listen).mockImplementation(failingListen);
  vi.mocked(getModelStatus).mockResolvedValue({ modelId: "mock", quantization: "4bit", installed: true, state: "ready", detail: "" });
  try {
    const view = render(<App />);
    await navigate("Models");
    await waitFor(() => expect(screen.getByText("Ready")).toBeTruthy());
    view.unmount();
    await act(async () => {});
    expect([...events.values()].every((handlers) => handlers.size === 0)).toBe(true);
  } finally {
    vi.mocked(listen).mockImplementation(implementation);
  }
});

test("native notice-before-status ordering still displays legitimate model loading", async () => {
  render(<App />);
  await navigate("Models");
  await emit("status", { kind: "model_loading", message: "Loading the speech model." });
  await emit("model-status", { modelId: "mock", quantization: "4bit", installed: false, state: "loading", detail: "" });
  expect(document.querySelector(".notice-message")?.textContent).toBe("Loading the speech model.");
});

test("dismissing a preparation notice preserves the Models progress display", async () => {
  render(<App />);
  await navigate("Models");
  await emit("model-status", { modelId: "mock", quantization: "4bit", installed: false, state: "loading", detail: "" });
  await emit("model-progress", { stage: "setup", model: null, completedBytes: null, totalBytes: null });
  expect(screen.getAllByRole("progressbar")).toHaveLength(2);
  fireEvent.click(screen.getByRole("button", { name: "Dismiss notification" }));
  expect(screen.getAllByRole("progressbar")).toHaveLength(1);
  expect(screen.queryByRole("button", { name: "Dismiss notification" })).toBeNull();
});

test("a startup hotkey warning never hides page progress after dismissing the loading notice", async () => {
  vi.mocked(getStartupHotkeyWarning).mockResolvedValue(["Some saved hotkeys overlap or could not be registered."]);
  render(<App />);
  await navigate("Models");
  await emit("model-status", { modelId: "mock", quantization: "4bit", installed: false, state: "loading", detail: "" });
  await emit("status", { kind: "model_loading", message: "Loading the speech model." });
  await emit("model-progress", { stage: "setup", model: null, completedBytes: null, totalBytes: null });
  expect(screen.getAllByRole("progressbar")).toHaveLength(2);
  fireEvent.click(screen.getByRole("button", { name: "Dismiss notification" }));
  expect(screen.getAllByRole("progressbar")).toHaveLength(1);
  expect(document.querySelector(".notice-message")!.textContent).toContain("Some saved hotkeys overlap");
});
