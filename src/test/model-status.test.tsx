import { fireEvent, render, screen } from "@testing-library/react";
import { ModelProgressBar } from "../components/ui";
import { expect, test, vi } from "vitest";
import { defaultSettings } from "../api";
import { I18nProvider } from "../i18n";
import { SetupPage } from "../pages/SetupPage";
import { ModelsPage } from "../pages/ModelsPage";
import type { ModelStatus } from "../types";
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn(async () => () => {}) }));

test("Setup requires a ready model and renders preparation progress", () => {
  const props = { settings: defaultSettings, devices: [], gpu: null, gpuChecking: false, onSettingsChange: vi.fn(), onConfigureModel: vi.fn(), onDiagnoseGpu: vi.fn(), onFinish: vi.fn() };
  const view = render(<I18nProvider language="en"><SetupPage {...props} modelReady={false} /></I18nProvider>);
  expect((screen.getByRole("button", { name: "Finish setup" }) as HTMLButtonElement).disabled).toBe(true);
  view.rerender(<I18nProvider language="en"><SetupPage {...props} modelReady progress={{ stage: "setup", model: null, completedBytes: null, totalBytes: null }} /></I18nProvider>);
  expect((screen.getByRole("button", { name: "Finish setup" }) as HTMLButtonElement).disabled).toBe(false);
  expect(screen.getByRole("progressbar").textContent).toContain("Preparing worker environment");
  view.rerender(<I18nProvider language="en"><SetupPage {...props} modelReady modelLoading /></I18nProvider>);
  expect((screen.getByRole("button", { name: "Finish setup" }) as HTMLButtonElement).disabled).toBe(true);
});

test("Models distinguishes loading, ready, error and not loaded and offers Retry", () => {
  const props = { settings: defaultSettings, gpu: null, asrBackendOptions: [], modelLoading: false, onConfigureModel: vi.fn(), onSaveCustomModel: async () => true, onDiagnoseGpu: vi.fn() };
  const view = render(<I18nProvider language="en"><ModelsPage {...props} /></I18nProvider>);
  for (const [state, label] of [["not_loaded", "Model not loaded"], ["loading", "Model loading"], ["ready", "Ready"], ["error", "Model error"]] as const) {
    const model: ModelStatus = { modelId: "mock", quantization: "4bit", installed: state === "ready", state, detail: "" };
    view.rerender(<I18nProvider language="en"><ModelsPage {...props} model={model} modelLoading={state === "loading"} /></I18nProvider>);
    expect(screen.getByText(label)).toBeTruthy();
    if (state === "error") expect(screen.getByRole("button", { name: "Retry" })).toBeTruthy();
  }
});

test("model errors can open diagnostics and CPU preparation stays localized", async () => {
  const open = vi.fn();
  const props = { settings: defaultSettings, gpu: null, asrBackendOptions: [], modelLoading: false, onConfigureModel: vi.fn(), onSaveCustomModel: async () => true, onDiagnoseGpu: vi.fn(), onOpenLogs: open };
  const model: ModelStatus = { modelId: "mock", quantization: "4bit", installed: false, state: "error", detail: "" };
  const view = render(<I18nProvider language="ja"><ModelsPage {...props} model={model} /></I18nProvider>);
  fireEvent.click(screen.getByRole("button", { name: "診断ログを開く" }));
  expect(open).toHaveBeenCalledOnce();
  view.rerender(<I18nProvider language="ja"><ModelProgressBar progress={{ stage: "cpu_fallback", model: null, completedBytes: null, totalBytes: null }} /></I18nProvider>);
  expect(screen.getByRole("progressbar").textContent).toContain("CPUを準備中");
});
