import { act, fireEvent, render, screen, within } from "@testing-library/react";
import { afterEach, beforeEach, expect, test, vi } from "vitest";
import { I18nProvider } from "../i18n";
import { ModelsPage } from "../pages/ModelsPage";
import { catalogEntryFor, modelCatalog } from "../modelCatalog";
import type { GpuDiagnostics, ManagedModelCache, Settings } from "../types";
import * as api from "../api";

vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn(async () => () => {}) }));
vi.mock("../api", async (importOriginal) => ({
  ...(await importOriginal<typeof import("../api")>()),
  getAsrModelCacheStatus: vi.fn(),
  deleteAsrModelCache: vi.fn(),
  getWorkerHealth: vi.fn(async () => null),
}));

const gpu: GpuDiagnostics = { status: "unavailable", adapterName: null, driverVersion: null, memoryTotalMb: null, recommendation: "", speechCppSupported: true };
const options = [{ value: "faster-whisper", label: "Whisper model" }, { value: "speech-cpp", label: "Japanese models (Vulkan)" }] as const;
const haasPresent: ManagedModelCache = { modelId: "haas-ja-v1.0", state: "present", sizeBytes: 656381472, expectedBytes: 656381472 };
const parakeetMissing: ManagedModelCache = { modelId: "parakeet-tdt_ctc-0.6b-ja", state: "missing", sizeBytes: null, expectedBytes: 661012416 };

function renderPage(settings: Partial<Settings>, language: "en" | "ja" = "en") {
  return render(<I18nProvider language={language}><ModelsPage settings={{ ...api.defaultSettings, ...settings }} gpu={gpu} asrBackendOptions={[...options]}
    modelLoading={false} onConfigureModel={vi.fn()} onSaveCustomModel={async () => true} onDiagnoseGpu={vi.fn()} /></I18nProvider>);
}

function details() {
  return within(screen.getByRole("region", { name: "Model details" }));
}

beforeEach(() => {
  vi.mocked(api.getAsrModelCacheStatus).mockResolvedValue([haasPresent, parakeetMissing]);
});

afterEach(() => {
  vi.restoreAllMocks();
  vi.mocked(api.deleteAsrModelCache).mockReset();
});

test("catalog entries resolve from the configured backend and model ID", () => {
  expect(catalogEntryFor("faster-whisper", null)).toMatchObject({ entry: { id: "faster-whisper:large-v3-turbo" }, custom: false });
  expect(catalogEntryFor("faster-whisper", " large-v3-turbo ")).toMatchObject({ custom: false });
  expect(catalogEntryFor("faster-whisper", "tiny.en")).toMatchObject({ entry: { backend: "faster-whisper", languageScope: null }, custom: true });
  expect(catalogEntryFor("speech-cpp", "parakeet-tdt_ctc-0.6b-ja")?.entry.license).toBe("CC-BY-4.0");
  expect(catalogEntryFor("speech-cpp", "stale-id")).toBeNull();
  expect(catalogEntryFor("openai-compatible", "gpt-4o-mini-transcribe")).toMatchObject({ custom: false, entry: { devices: ["remote"] } });
});

test("planned catalog entries claim no unverified capability", () => {
  const planned = modelCatalog.models.filter((model) => model.availability === "planned");
  expect(planned.map((model) => model.displayName)).toEqual(expect.arrayContaining(["Qwen3-ASR 0.6B", "Qwen3-ASR 1.7B"]));
  for (const model of planned) {
    expect(model.backend).toBeNull();
    expect(model.inference).toEqual([]);
    expect(model.devices).toEqual([]);
  }
  expect(modelCatalog.models.some((model) => model.inference.includes("stateful_streaming"))).toBe(false);
});

test("Japanese model details show Japanese-only Vulkan runtime and pinned provenance", async () => {
  renderPage({ asrBackend: "speech-cpp", modelId: "parakeet-tdt_ctc-0.6b-ja" });
  const facts = details();
  expect(facts.getByText("Japanese only")).toBeTruthy();
  expect(facts.getByText(/Vulkan GPU \(not CUDA\) · decoder on CPU · Q8_0/)).toBeTruthy();
  expect(facts.queryByText(/CUDA \(NVIDIA\)/)).toBeNull();
  expect(facts.getByText("CC-BY-4.0")).toBeTruthy();
  expect(facts.getByText("sakasegawa/parakeet-tdt_ctc-0.6b-ja-GGUF")).toBeTruthy();
  expect(facts.getByText("After recognition only (no model hint)")).toBeTruthy();
  expect(await facts.findByText("Not downloaded")).toBeTruthy();
  expect(facts.queryByRole("button", { name: "Remove download" })).toBeNull();
});

test("Whisper details show CPU/CUDA, unpinned revision and a labeled device-wide memory peak", () => {
  renderPage({ asrBackend: "faster-whisper", modelId: null }, "ja");
  const facts = within(screen.getByRole("region", { name: "モデルの詳細" }));
  expect(facts.getByText("多言語（日本語を含む）")).toBeTruthy();
  expect(facts.getByText("CPU / CUDA（NVIDIA）")).toBeTruthy();
  expect(facts.getByText("固定なし（取得時の最新版）")).toBeTruthy();
  expect(facts.getByText(/3,651 MiB.*他アプリの使用量を含み/)).toBeTruthy();
  expect(api.getAsrModelCacheStatus).not.toHaveBeenCalled();
});

test("custom models do not inherit the default model's license or provenance", () => {
  renderPage({ asrBackend: "faster-whisper", modelId: "kotoba-tech/kotoba-whisper-v2.0-faster" });
  const facts = details();
  expect(facts.getByText(/Custom model: languages, license, provenance and memory are not in the catalog/)).toBeTruthy();
  expect(facts.queryByText("MIT")).toBeNull();
  expect(facts.queryByText("mobiuslabsgmbh/faster-whisper-large-v3-turbo")).toBeNull();
  expect(facts.getByText("Dictionary words as hotwords")).toBeTruthy();
  expect(facts.queryByText("Multilingual (including Japanese)")).toBeNull();
  expect(facts.getAllByText("Not confirmed").length).toBeGreaterThan(0);
});

test("removing a downloaded Japanese model requires confirmation and refreshes its status", async () => {
  const confirm = vi.spyOn(window, "confirm").mockReturnValueOnce(false).mockReturnValueOnce(true);
  vi.mocked(api.deleteAsrModelCache).mockResolvedValue([{ ...haasPresent, state: "missing", sizeBytes: null }, parakeetMissing]);
  renderPage({ asrBackend: "speech-cpp", modelId: "haas-ja-v1.0" });
  const facts = details();
  const remove = await facts.findByRole("button", { name: "Remove download" });
  fireEvent.click(remove);
  expect(confirm).toHaveBeenCalledTimes(1);
  expect(api.deleteAsrModelCache).not.toHaveBeenCalled();
  await act(async () => { fireEvent.click(remove); });
  expect(api.deleteAsrModelCache).toHaveBeenCalledWith("haas-ja-v1.0");
  expect(await facts.findByText("Not downloaded")).toBeTruthy();
  expect(facts.queryByRole("button", { name: "Remove download" })).toBeNull();
});

test("a refused removal shows the translated reason and keeps the file", async () => {
  vi.spyOn(window, "confirm").mockReturnValue(true);
  vi.mocked(api.deleteAsrModelCache).mockRejectedValue(new Error("This model is loaded. Load a different model before removing its downloaded file."));
  renderPage({ asrBackend: "speech-cpp", modelId: "haas-ja-v1.0" }, "ja");
  const facts = within(screen.getByRole("region", { name: "モデルの詳細" }));
  const remove = await facts.findByRole("button", { name: "ダウンロードを削除" });
  await act(async () => { fireEvent.click(remove); });
  expect((await facts.findByRole("alert")).textContent).toContain("別のモデルを読み込んでから");
  expect(facts.getByText("ダウンロード済み（容量一致。SHA256は読み込み時に検証）")).toBeTruthy();
});

test("the catalog overview labels planned models as unavailable", () => {
  renderPage({ asrBackend: "faster-whisper" });
  const item = screen.getByText("Qwen3-ASR 1.7B").closest("li")!;
  expect(within(item).getByText("Planned (not available yet)")).toBeTruthy();
  expect(within(item).getByText(/#89/)).toBeTruthy();
  const nemotron = screen.getByText("Nemotron ASR Streaming (candidate)").closest("li")!;
  expect(within(nemotron).getByText(/Speech languages: Not confirmed/)).toBeTruthy();
});
