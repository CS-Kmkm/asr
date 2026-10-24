import { render } from "@testing-library/react";
import { expect, test, vi } from "vitest";
import { defaultSettings } from "../api";
import { I18nProvider } from "../i18n";
import { SetupPage } from "../pages/SetupPage";
import { ModelsPage } from "../pages/ModelsPage";
import catalogue from "../backend-messages.json";
import type { GpuDiagnostics } from "../types";

test("Setup and Models render every fixed GPU recommendation in Japanese", () => {
  for (const [en, entry] of Object.entries(catalogue).filter(([, entry]) => entry.code.startsWith("gpu_"))) {
    const gpu: GpuDiagnostics = { status: entry.code === "gpu_suitable_for_vibevoice" ? "available" : "unavailable",
      adapterName: null, driverVersion: null, memoryTotalMb: null, recommendation: en };
    for (const page of [
      <SetupPage settings={defaultSettings} devices={[]} gpu={gpu} gpuChecking={false} onSettingsChange={vi.fn()}
        onConfigureModel={vi.fn()} onDiagnoseGpu={vi.fn()} onFinish={vi.fn()} />,
      <ModelsPage settings={defaultSettings} gpu={gpu} asrBackendOptions={[]} modelLoading={false}
        onConfigureModel={vi.fn()} onSaveCustomModel={async () => true} onDiagnoseGpu={vi.fn()} />,
    ]) {
      const view = render(<I18nProvider language="ja">{page}</I18nProvider>);
      expect(view.container.textContent).toContain(entry.ja);
      expect(view.container.textContent).not.toContain(en);
      view.unmount();
    }
  }
});
