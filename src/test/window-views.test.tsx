import { act, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { beforeEach, expect, test, vi } from "vitest";
import { AskAnswerPanel } from "../windows/AskAnswerPanel";
import { RecordingOverlay } from "../windows/RecordingOverlay";
import { I18nProvider, translateAppMessage } from "../i18n";
import { copyToClipboard, dismissAskAnswer, getAskAnswer } from "../api";
import type { AppState } from "../types";

const transport = vi.hoisted(() => ({ handlers: new Map<string, (event: { payload: unknown }) => void>(), stopped: vi.fn() }));
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn(async (name, handler) => {
  transport.handlers.set(name, handler);
  return () => { if (transport.handlers.get(name) === handler) transport.handlers.delete(name); transport.stopped(name); };
}) }));
vi.mock("../api", () => ({ copyToClipboard: vi.fn(), dismissAskAnswer: vi.fn(), getAskAnswer: vi.fn(), cancelRecording: vi.fn(), cycleVoiceTranslationTarget: vi.fn() }));
beforeEach(() => {
  transport.handlers.clear(); transport.stopped.mockReset();
  vi.mocked(getAskAnswer).mockReset().mockResolvedValue(null);
  vi.mocked(copyToClipboard).mockReset().mockResolvedValue();
  vi.mocked(dismissAskAnswer).mockReset().mockResolvedValue(true);
});
async function emit(name: string, payload: unknown) {
  await waitFor(() => expect(transport.handlers.has(name)).toBe(true));
  await act(async () => { transport.handlers.get(name)!({ payload }); });
}
const processing: AppState = { phase: "processing", stage: "transcribing", operation: "recording", operationId: 1, message: "changed English wording", messageCode: null, lastResult: null, updatedAt: "now" };

test("quiet-input failure remains readable in Japanese with the main window hidden", async () => {
  render(<I18nProvider language="ja"><RecordingOverlay /></I18nProvider>);
  await emit("overlay-message", {
    severity: "failure", durationMs: 8000,
    message: "No speech was detected. Your audio may be too quiet. Check your microphone and Input gain in Input settings.",
  });
  expect(screen.getByText("音声を検出できませんでした。入力音量が小さすぎる可能性があります。入力設定でマイクと入力ゲインを確認してください。")).toBeTruthy();
});

test("generated Ask answers stay verbatim when displayed and copied; notices alone translate", async () => {
  render(<I18nProvider language="ja"><AskAnswerPanel /></I18nProvider>);
  await waitFor(() => expect(getAskAnswer).toHaveBeenCalledOnce());
  let operationId = 0;
  for (const payload of ["Dictation cancelled.", "Microphone test failed. Keep this generated answer unchanged.", "Ask needs clarification."]) {
    await emit("ask-answer", { operationId: ++operationId, payload, kind: "generated" });
    expect(screen.getByText(payload)).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "コピー" }));
    await waitFor(() => expect(copyToClipboard).toHaveBeenLastCalledWith(payload));
  }
  await emit("ask-answer", { operationId: ++operationId, payload: "Dictation cancelled.", kind: "notice" });
  expect(screen.getByText(translateAppMessage("ja", "Dictation cancelled.")!)).toBeTruthy();
  fireEvent.click(screen.getByRole("button", { name: "閉じる" }));
  await waitFor(() => expect(dismissAskAnswer).toHaveBeenCalledWith(operationId));
});

test("Ask ignores an older snapshot after a newer event and unsubscribes on unmount", async () => {
  let snapshot!: (value: { operationId: number; payload: string; kind: "generated" }) => void;
  vi.mocked(getAskAnswer).mockReturnValueOnce(new Promise((resolve) => { snapshot = resolve; }));
  const view = render(<I18nProvider language="en"><AskAnswerPanel /></I18nProvider>);
  await waitFor(() => expect(getAskAnswer).toHaveBeenCalledOnce());
  await emit("ask-answer", { operationId: 9, payload: "new event", kind: "generated" });
  await act(async () => { snapshot({ operationId: 8, payload: "old snapshot", kind: "generated" }); });
  expect(screen.getByText("new event")).toBeTruthy();
  expect(screen.queryByText("old snapshot")).toBeNull();
  view.unmount();
  await waitFor(() => expect(transport.stopped).toHaveBeenCalledWith("ask-answer"));
  expect(transport.stopped).toHaveBeenCalledWith("ask-answer-preview");
});

test("Ask streams a marked, silent preview that the validated answer replaces", async () => {
  const view = render(<I18nProvider language="en"><AskAnswerPanel /></I18nProvider>);
  await emit("ask-answer", { operationId: 1, payload: "earlier answer", kind: "generated" });
  await emit("ask-answer-preview", { operationId: 2, text: "The answ", active: true });
  const preview = screen.getByText("The answ");
  expect(preview.getAttribute("aria-live")).toBe("off");
  expect(preview.classList.contains("ask-answer-preview")).toBe(true);
  expect(screen.getByText("Generating answer…")).toBeTruthy();
  expect(view.container.querySelector("main")!.getAttribute("aria-busy")).toBe("true");
  // Partial text cannot be copied or dismissed as if it were final.
  expect((screen.getByRole("button", { name: "Copy" }) as HTMLButtonElement).disabled).toBe(true);
  expect((screen.getByRole("button", { name: "Dismiss" }) as HTMLButtonElement).disabled).toBe(true);
  // An older operation's late partial is stale.
  await emit("ask-answer-preview", { operationId: 1, text: "stale partial", active: true });
  expect(screen.queryByText("stale partial")).toBeNull();
  await emit("ask-answer-preview", { operationId: 2, text: "The answer is", active: true });
  expect(screen.getByText("The answer is")).toBeTruthy();
  await emit("ask-answer", { operationId: 2, payload: "The answer is 42.", kind: "generated" });
  const final = screen.getByText("The answer is 42.");
  expect(final.getAttribute("aria-live")).toBe("polite");
  expect(screen.queryByText("The answer is")).toBeNull();
  expect(screen.queryByText("Generating answer…")).toBeNull();
  expect(view.container.querySelector("main")!.getAttribute("aria-busy")).toBe("false");
  // A partial arriving after its final answer never covers it.
  await emit("ask-answer-preview", { operationId: 2, text: "The answer is 4", active: true });
  expect(screen.queryByText("The answer is 4")).toBeNull();
  fireEvent.click(screen.getByRole("button", { name: "Copy" }));
  await waitFor(() => expect(copyToClipboard).toHaveBeenLastCalledWith("The answer is 42."));
});

test("a failed or cancelled Ask withdraws its preview and keeps the earlier answer", async () => {
  render(<I18nProvider language="en"><AskAnswerPanel /></I18nProvider>);
  await emit("ask-answer", { operationId: 4, payload: "earlier answer", kind: "generated" });
  await emit("ask-answer-preview", { operationId: 5, text: "half an ans", active: true });
  expect(screen.getByText("half an ans")).toBeTruthy();
  // Withdrawing an older operation leaves the newer preview alone.
  await emit("ask-answer-preview", { operationId: 3, text: "", active: false });
  expect(screen.getByText("half an ans")).toBeTruthy();
  await emit("ask-answer-preview", { operationId: 5, text: "", active: false });
  expect(screen.queryByText("half an ans")).toBeNull();
  expect(screen.queryByText("Generating answer…")).toBeNull();
  expect(screen.getByText("earlier answer")).toBeTruthy();
  // A late partial of the withdrawn operation stays hidden.
  await emit("ask-answer-preview", { operationId: 5, text: "half an answer", active: true });
  expect(screen.queryByText("half an answer")).toBeNull();
  expect((screen.getByRole("button", { name: "Copy" }) as HTMLButtonElement).disabled).toBe(false);
});

test("overlay labels follow typed stages independently of English message wording", async () => {
  render(<I18nProvider language="ja"><RecordingOverlay /></I18nProvider>);
  for (const [stage, label] of [["preparing_model", "音声認識モデルを準備中"], ["finishing_audio", "音声処理を完了中"], ["transcribing", "文字起こし中"]] as const) {
    await emit("app-state", { ...processing, stage });
    expect(screen.getByText(label)).toBeTruthy();
  }
});

test("voice translation keeps its transcript preview and selected-text translation clears it", async () => {
  render(<I18nProvider language="en"><RecordingOverlay /></I18nProvider>);
  await emit("voice-mode", { mode: "translate", targetLanguage: "en" });
  await emit("correction-preview", { stage: "draft", text: "spoken transcript" });
  await emit("app-state", { ...processing, stage: "translating" });
  expect(screen.getByText("spoken transcript")).toBeTruthy();
  await emit("app-state", { ...processing, stage: "translating", operation: "selected_text" });
  expect(screen.queryByText("spoken transcript")).toBeNull();
});

const recording: AppState = { ...processing, phase: "recording", stage: "recording", message: null, lastResult: "previous take" };

test("live text is a recording subtitle that follows retractions and stays until the draft replaces it", async () => {
  const view = render(<I18nProvider language="en"><RecordingOverlay /></I18nProvider>);
  await emit("app-state", recording);
  // The previous take's result is never shown as live text.
  expect(screen.queryByText("previous take")).toBeNull();
  await emit("live-subtitle", { text: "" });
  expect(view.container.querySelector(".recording-overlay.with-subtitle .live-subtitle")).toBeTruthy();
  await emit("live-subtitle", { text: "first words" });
  expect(screen.getByText("first words")).toBeTruthy();
  await emit("live-subtitle", { text: "first words and more" });
  expect(screen.getByText("first words and more")).toBeTruthy();
  // An utterance that ended without text withdraws its partial.
  await emit("live-subtitle", { text: "first words" });
  expect(screen.getByText("first words")).toBeTruthy();
  expect(screen.queryByText("first words and more")).toBeNull();
  await emit("live-subtitle", { text: `${"x".repeat(200)}tail` });
  expect(screen.getByText(`…${"x".repeat(136)}tail`)).toBeTruthy();
  await emit("live-subtitle", { text: "final live words" });
  for (const stage of ["finishing_audio", "transcribing"] as const) {
    await emit("app-state", { ...processing, stage });
    expect(screen.getByText("final live words")).toBeTruthy();
  }
  // A late live event after stop changes nothing.
  await emit("live-subtitle", { text: "late window" });
  expect(screen.queryByText("late window")).toBeNull();
  await emit("correction-preview", { stage: "draft", text: "Final transcript." });
  expect(screen.getByText("Final transcript.")).toBeTruthy();
  expect(screen.queryByText("final live words")).toBeNull();
});

test("live subtitles reset when a recording starts and disappear when the take is cancelled", async () => {
  const view = render(<I18nProvider language="en"><RecordingOverlay /></I18nProvider>);
  await emit("app-state", recording);
  await emit("live-subtitle", { text: "cancelled words" });
  await emit("app-state", { ...processing, stage: "finishing_audio" });
  expect(screen.getByText("cancelled words")).toBeTruthy();
  await emit("app-state", { ...processing, phase: "idle", stage: "idle", message: "Dictation cancelled." });
  await emit("app-state", { ...processing, stage: "transcribing", operation: "retry" });
  expect(screen.queryByText("cancelled words")).toBeNull();
  // A take without live recognition keeps the compact recording view.
  await emit("app-state", recording);
  expect(view.container.querySelector(".live-subtitle")).toBeNull();
  expect(view.container.querySelector(".with-subtitle")).toBeNull();
  await emit("live-subtitle", { text: "next take" });
  await emit("app-state", { ...recording, phase: "idle", stage: "idle" });
  await emit("app-state", recording);
  expect(screen.queryByText("next take")).toBeNull();
  expect(view.container.querySelector(".live-subtitle")).toBeNull();
});

test("a final paste waiting for its target shows the hint and the next recording drops it", async () => {
  render(<I18nProvider language="ja"><RecordingOverlay /></I18nProvider>);
  const hint = "To insert, return to the original field or confirm the IME conversion.";
  const translated = translateAppMessage("ja", hint);
  expect(translated).not.toBe(hint);
  await emit("app-state", { ...processing, phase: "injecting", stage: "inserting" });
  await emit("correction-preview", { stage: "final", text: "Corrected text." });
  await emit("overlay-hint", { message: hint, durationMs: 6000 });
  expect(screen.getByRole("alert").textContent).toBe(translated);
  expect(screen.queryByText("Corrected text.")).toBeNull();
  await emit("app-state", recording);
  expect(screen.queryByText(translated!)).toBeNull();
});

test("a recording hint ends with the recording instead of covering the last live text", async () => {
  render(<I18nProvider language="en"><RecordingOverlay /></I18nProvider>);
  await emit("app-state", recording);
  await emit("overlay-hint", { message: "Recording from the default microphone.", durationMs: 6000 });
  await emit("live-subtitle", { text: "spoken words" });
  await emit("app-state", { ...processing, stage: "finishing_audio" });
  expect(screen.queryByRole("alert")).toBeNull();
  expect(screen.getByText("spoken words")).toBeTruthy();
});

test("overlay releases all event subscriptions when its window unmounts", async () => {
  const view = render(<I18nProvider language="en"><RecordingOverlay /></I18nProvider>);
  await waitFor(() => expect([...transport.handlers.keys()].sort()).toEqual(["app-state", "audio-level", "correction-preview", "insertion-notice", "live-subtitle", "overlay-hint", "overlay-message", "voice-mode"]));
  view.unmount();
  await waitFor(() => expect(transport.stopped).toHaveBeenCalledTimes(8));
  expect(transport.handlers.size).toBe(0);
});
