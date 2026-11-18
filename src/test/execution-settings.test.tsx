import { fireEvent, render, screen } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { ExecutionSettings } from "../components/ExecutionSettings";
import { executionPresetPatch, executionPresets } from "../executionPresets";
import { defaultSettings } from "../api";
import { I18nProvider } from "../i18n";

function renderSettings(props: React.ComponentProps<typeof ExecutionSettings>) {
  return render(<I18nProvider language="en"><ExecutionSettings {...props} /></I18nProvider>);
}

describe("execution settings", () => {
  it("applies presets without granting consent or rewriting endpoints, keys or history", () => {
    for (const preset of executionPresets) {
      const patch = executionPresetPatch(preset);
      for (const key of ["cloudConsents", "apiBaseUrl", "apiKeyEnvVar", "historyRetention", "personalizationLearning", "offlineOnly"]) expect(patch).not.toHaveProperty(key);
    }
    const save = vi.fn();
    renderSettings({ settings: defaultSettings, onSave: save, recording: false });
    fireEvent.change(screen.getByRole("combobox", { name: "Execution preset" }), { target: { value: "hybrid" } });
    expect(save).toHaveBeenCalledWith(expect.objectContaining({ asrBackend: "faster-whisper", correctionProvider: "openai", textCorrectionEnabled: true }));
  });

  it("blocks cloud presets offline and exposes audio and text independently", () => {
    const save = vi.fn();
    renderSettings({ settings: { ...defaultSettings, offlineOnly: true, asrBackend: "openai-compatible", apiBaseUrl: "https://speech.example/v1", correctionProvider: "gemini" }, onSave: save, recording: false });
    expect((screen.getByRole("option", { name: "Hybrid" }) as HTMLOptionElement).disabled).toBe(true);
    expect(screen.getByText(/speech.example/)).toBeTruthy();
    expect(screen.getByText(/Google Gemini/)).toBeTruthy();
    expect(screen.queryByRole("switch", { name: "Stateful streaming (experimental)" })).toBeNull();
  });

  it("only exposes streaming for a capable model and locks changes during a take", () => {
    renderSettings({ settings: { ...defaultSettings, asrBackend: "sherpa-onnx", modelId: "nemotron-3.5-streaming-560ms" }, onSave: vi.fn(), recording: true });
    expect((screen.getByRole("combobox", { name: "Execution preset" }) as HTMLSelectElement).disabled).toBe(true);
    expect((screen.getByRole("switch", { name: "Stateful streaming (experimental)" }) as HTMLButtonElement).disabled).toBe(true);
  });
});
