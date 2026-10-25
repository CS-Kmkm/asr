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

test("overlay releases all event subscriptions when its window unmounts", async () => {
  const view = render(<I18nProvider language="en"><RecordingOverlay /></I18nProvider>);
  await waitFor(() => expect([...transport.handlers.keys()].sort()).toEqual(["app-state", "audio-level", "correction-preview", "insertion-notice", "overlay-hint", "overlay-message", "voice-mode"]));
  view.unmount();
  await waitFor(() => expect(transport.stopped).toHaveBeenCalledTimes(7));
  expect(transport.handlers.size).toBe(0);
});
