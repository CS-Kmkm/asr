import { fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { beforeEach, expect, test, vi } from "vitest";
import { DiagnosticsPage } from "../pages/DiagnosticsPage";
import { I18nProvider } from "../i18n";
import { copyToClipboard, getStorageDiagnostics, getWorkerHealth } from "../api";
import type { AppState, StorageDiagnostics } from "../types";

vi.mock("../api", () => ({ copyToClipboard: vi.fn(), getStorageDiagnostics: vi.fn(), getWorkerHealth: vi.fn() }));
const report: StorageDiagnostics = {
  databaseOk: true, schemaVersion: 3,
  summaries: [{ eventType: "asr", provider: "faster-whisper", mode: "dictate", appCategory: "email", count: 22, successes: 20, timedCount: 20, p50Ms: 10, p95Ms: 19 }],
  outcomes: [{ eventType: "asr", provider: "faster-whisper", mode: "dictate", appCategory: "email", success: false, code: "asr_failed", count: 2 }],
};
const state: AppState = { phase: "idle", stage: "idle", operation: null, operationId: null, message: null, messageCode: null, lastResult: "private transcript sentinel", updatedAt: new Date().toISOString() };
function show() {
  return render(<I18nProvider language="ja"><DiagnosticsPage state={state} gpu={null} statusLabel="待機中" onDiagnoseGpu={vi.fn()} /></I18nProvider>);
}
beforeEach(() => { vi.mocked(getStorageDiagnostics).mockReset(); vi.mocked(copyToClipboard).mockReset(); vi.mocked(getWorkerHealth).mockReset(); });

test("real diagnostics display aggregates and copy excludes dictated text", async () => {
  vi.mocked(getStorageDiagnostics).mockResolvedValue(report);
  show();
  const table = await screen.findByRole("table", { name: "モード・アプリカテゴリ・プロバイダー別の性能" });
  expect(within(table).getByText("19")).toBeTruthy();
  expect(screen.getByText("スキーマのバージョン: 3")).toBeTruthy();
  expect(within(table).getByText("dictate")).toBeTruthy();
  expect(within(table).getByText("email")).toBeTruthy();
  fireEvent.click(screen.getByRole("button", { name: "診断結果をコピー" }));
  await screen.findByText("診断結果をコピーしました");
  expect(copyToClipboard).toHaveBeenCalledWith(JSON.stringify({ storage: report, gpu: null, worker: null }, null, 2));
  expect(vi.mocked(copyToClipboard).mock.calls[0][0]).not.toContain(state.lastResult);
});

test("worker probe is explicit and copied metadata excludes the interpreter path", async () => {
  vi.mocked(getStorageDiagnostics).mockResolvedValue(report);
  vi.mocked(getWorkerHealth).mockResolvedValue({ pythonPath: "C:/Users/private-sentinel/python.exe", pythonVersion: "3.12", fasterWhisperVersion: "1.2", ctranslate2Version: "4.6", backend: "faster-whisper", loaded: true, device: "cpu", computeType: "int8" });
  show();
  await screen.findByRole("table", { name: "モード・アプリカテゴリ・プロバイダー別の性能" });
  expect(getWorkerHealth).not.toHaveBeenCalled();
  fireEvent.click(screen.getByRole("button", { name: "ワーカー環境を検査" }));
  await screen.findByText("int8");
  expect(screen.getByText("C:/Users/private-sentinel/python.exe")).toBeTruthy();
  fireEvent.click(screen.getByRole("button", { name: "診断結果をコピー" }));
  await screen.findByText("診断結果をコピーしました");
  const text = vi.mocked(copyToClipboard).mock.calls[0][0];
  expect(text).not.toContain("private-sentinel");
  expect(text).not.toContain(state.lastResult);
  expect(JSON.parse(text).worker).toMatchObject({ device: "cpu", computeType: "int8" });
});

test("failed refresh removes stale data and disables copy", async () => {
  vi.mocked(getStorageDiagnostics).mockResolvedValueOnce(report).mockRejectedValueOnce(new Error("private error"));
  show();
  await screen.findByRole("table", { name: "モード・アプリカテゴリ・プロバイダー別の性能" });
  fireEvent.click(screen.getByRole("button", { name: "データベースを検査" }));
  await screen.findByText("データベースの検査に失敗");
  expect(screen.queryByRole("table")).toBeNull();
  expect((screen.getByRole("button", { name: "診断結果をコピー" }) as HTMLButtonElement).disabled).toBe(true);
  expect(screen.queryByText("private error")).toBeNull();
});

test("browser diagnostics do not invent a connected database", async () => {
  vi.mocked(getStorageDiagnostics).mockResolvedValue(null);
  show();
  await waitFor(() => expect(getStorageDiagnostics).toHaveBeenCalledOnce());
  expect(screen.getAllByText("未確認").length).toBeGreaterThan(0);
  expect((screen.getByRole("button", { name: "診断結果をコピー" }) as HTMLButtonElement).disabled).toBe(true);
});
