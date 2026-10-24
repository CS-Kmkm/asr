import { act, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { beforeEach, expect, test, vi } from "vitest";
import { SetupTests } from "../components/SetupTests";
import { I18nProvider } from "../i18n";
import { cancelSetupTest, copyToClipboard, startDictationTest, stopDictationTest, testInsertion } from "../api";
import type { AppState } from "../types";

vi.mock("../api", () => ({ cancelSetupTest: vi.fn(), copyToClipboard: vi.fn(), startDictationTest: vi.fn(), stopDictationTest: vi.fn(), testInsertion: vi.fn() }));
const idle: AppState = { phase: "idle", stage: "idle", operation: null, operationId: null, message: null, messageCode: null, lastResult: null, updatedAt: "now" };
beforeEach(() => { vi.resetAllMocks(); vi.mocked(startDictationTest).mockResolvedValue(7); vi.mocked(testInsertion).mockResolvedValue(8); vi.mocked(cancelSetupTest).mockResolvedValue(); vi.mocked(copyToClipboard).mockResolvedValue(); });
function show(initial = idle, prepare = vi.fn(async () => true)) {
  const busy = vi.fn();
  const view = (state: AppState) => <I18nProvider language="en"><SetupTests state={state} modelReady disabled={false} onPrepare={prepare} onBusyChange={busy} /></I18nProvider>;
  const rendered = render(view(initial));
  return { ...rendered, update: (state: AppState) => rendered.rerender(view(state)), prepare, busy };
}

test("saves setup choices before claiming a test and blocks a second claim before its state event", async () => {
  let saved!: (ok: boolean) => void;
  const prepare = vi.fn(() => new Promise<boolean>((resolve) => { saved = resolve; }));
  show(idle, prepare);
  fireEvent.click(screen.getByText("Test dictation"));
  expect(startDictationTest).not.toHaveBeenCalled();
  await act(async () => { saved(true); });
  expect(startDictationTest).toHaveBeenCalledOnce();
  expect(screen.getByText("Test insertion")).toHaveProperty("disabled", true);
  expect(screen.getByText("Cancel test")).toHaveProperty("disabled", false);
});

test("stop retains cancellation while recognition is pending; only owned results are displayed", async () => {
  vi.mocked(stopDictationTest).mockReturnValue(new Promise(() => {}));
  const app = show();
  fireEvent.click(screen.getByText("Test dictation"));
  await waitFor(() => expect(startDictationTest).toHaveBeenCalledOnce());
  app.update({ ...idle, phase: "recording", stage: "recording", operation: "dictation_test", operationId: 7 });
  fireEvent.click(screen.getByText("Stop test and transcribe"));
  await waitFor(() => expect(stopDictationTest).toHaveBeenCalledWith(7));
  fireEvent.click(screen.getByText("Cancel test"));
  await waitFor(() => expect(cancelSetupTest).toHaveBeenCalledWith(7));
  app.update({ ...idle, phase: "completed", operationId: 99, lastResult: "unrelated" });
  expect(screen.queryByText("unrelated")).toBeNull();
});

test("automatic completion displays raw text and copies it without requiring a stop response", async () => {
  const app = show();
  fireEvent.click(screen.getByText("Test dictation"));
  await waitFor(() => expect(startDictationTest).toHaveBeenCalledOnce());
  app.update({ ...idle, phase: "completed", stage: "completed", operationId: 7, lastResult: "recognized sample" });
  expect(await screen.findByText("recognized sample")).toBeTruthy();
  fireEvent.click(screen.getByText("Copy result"));
  await waitFor(() => expect(copyToClipboard).toHaveBeenCalledWith("recognized sample"));
});

test("a delayed stop response cannot replace a later state result", async () => {
  let stopped!: (result: { text: string; insertion: string; durationMs: number; latencyMs: number }) => void;
  vi.mocked(stopDictationTest).mockReturnValueOnce(new Promise((resolve) => { stopped = resolve; }));
  const app = show();
  fireEvent.click(screen.getByText("Test dictation"));
  await waitFor(() => expect(startDictationTest).toHaveBeenCalledOnce());
  app.update({ ...idle, phase: "recording", stage: "recording", operation: "dictation_test", operationId: 7 });
  fireEvent.click(screen.getByText("Stop test and transcribe"));
  await waitFor(() => expect(stopDictationTest).toHaveBeenCalledWith(7));
  app.update({ ...idle, phase: "completed", stage: "completed", operationId: 7, lastResult: "canonical state result" });
  expect(await screen.findByText("canonical state result")).toBeTruthy();
  app.update({ ...idle, phase: "processing", operationId: 99, operation: "recording" });
  await act(async () => { stopped({ text: "stale IPC result", insertion: "test_only", durationMs: 0, latencyMs: 0 }); });
  expect(screen.queryByText("stale IPC result")).toBeNull();
  expect(screen.getByText("canonical state result")).toBeTruthy();
});

test("leaving during a delayed start cancels the returned identity", async () => {
  let started!: (id: number) => void;
  vi.mocked(startDictationTest).mockReturnValueOnce(new Promise((resolve) => { started = resolve; }));
  const app = show();
  fireEvent.click(screen.getByText("Test dictation"));
  await waitFor(() => expect(startDictationTest).toHaveBeenCalledOnce());
  app.unmount();
  await act(async () => { started(42); });
  expect(cancelSetupTest).toHaveBeenCalledWith(42);
});

test("insertion does not require a model and unmount cancels only its own identity", async () => {
  const busy = vi.fn();
  const app = render(<I18nProvider language="en"><SetupTests state={idle} modelReady={false} disabled={false} onPrepare={async () => true} onBusyChange={busy} /></I18nProvider>);
  expect(screen.getByText("Test dictation")).toHaveProperty("disabled", true);
  fireEvent.click(screen.getByText("Test insertion"));
  await waitFor(() => expect(testInsertion).toHaveBeenCalledOnce());
  app.unmount();
  expect(cancelSetupTest).toHaveBeenCalledWith(8);
});

test("a rejected settings save prevents the recording claim", async () => {
  show(idle, vi.fn(async () => false));
  fireEvent.click(screen.getByText("Test dictation"));
  await waitFor(() => expect(screen.getByText("Test dictation")).toHaveProperty("disabled", false));
  expect(startDictationTest).not.toHaveBeenCalled();
});
