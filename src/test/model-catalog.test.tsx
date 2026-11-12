import { act, cleanup, fireEvent, render, screen, within } from "@testing-library/react";
import { afterEach, beforeEach, expect, test, vi } from "vitest";
import { I18nProvider } from "../i18n";
import { ModelsPage } from "../pages/ModelsPage";
import { catalogEntryFor, modelCatalog } from "../modelCatalog";
import type { GpuDiagnostics, ManagedModelCache, ModelStatus, Settings } from "../types";
import * as api from "../api";

vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn(async () => () => {}) }));
vi.mock("../api", async (importOriginal) => ({
  ...(await importOriginal<typeof import("../api")>()),
  getAsrModelCacheStatus: vi.fn(),
  deleteAsrModelCache: vi.fn(),
  getWorkerHealth: vi.fn(async () => null),
}));

const gpu: GpuDiagnostics = { status: "unavailable", adapterName: null, driverVersion: null, memoryTotalMb: null, recommendation: "", speechCppSupported: true };
const options = [{ value: "faster-whisper", label: "Whisper model" }, { value: "speech-cpp", label: "Japanese models (Vulkan)" },
  { value: "sherpa-onnx", label: "ONNX Runtime models (CUDA / CPU)" }, { value: "transformers-asr", label: "Qwen3-ASR (transformers, CUDA)" }] as const;
const haasPresent: ManagedModelCache = { modelId: "haas-ja-v1.0", state: "present", sizeBytes: 656381472, expectedBytes: 656381472 };
const parakeetMissing: ManagedModelCache = { modelId: "parakeet-tdt_ctc-0.6b-ja", state: "missing", sizeBytes: null, expectedBytes: 661012416 };

function renderPage(settings: Partial<Settings>, language: "en" | "ja" = "en", extra: { onConfigureModel?: (configuration: unknown) => void; model?: ModelStatus } = {}) {
  return render(<I18nProvider language={language}><ModelsPage settings={{ ...api.defaultSettings, ...settings }} gpu={gpu} asrBackendOptions={[...options]}
    modelLoading={false} onConfigureModel={extra.onConfigureModel ?? vi.fn()} onSaveCustomModel={async () => true} onDiagnoseGpu={vi.fn()} model={extra.model} /></I18nProvider>);
}

function cleanupAndRender(settings: Partial<Settings>) {
  cleanup();
  return renderPage(settings);
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
  expect(planned.map((model) => model.displayName)).toEqual(expect.arrayContaining(["Qwen3-ASR 1.7B", "Nemotron ASR Streaming (candidate)"]));
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

test("pinned runtime adapters resolve only their catalog models", () => {
  expect(catalogEntryFor("sherpa-onnx", "reazonspeech-k2-v2")?.entry).toMatchObject({ languageScope: "japanese_only", devices: ["cuda", "cpu"], license: "Apache-2.0" });
  expect(catalogEntryFor("transformers-asr", "qwen3-asr-0.6b")?.entry).toMatchObject({ languageScope: "multilingual", vocabulary: "prompt", cancellation: "between_tokens" });
  expect(catalogEntryFor("sherpa-onnx", "large-v3-turbo")).toBeNull();
  expect(catalogEntryFor("transformers-asr", "Qwen/Qwen3-ASR-1.7B")).toBeNull();
});

test("the ONNX Runtime adapter offers its pinned model, a speech device and Japanese-only details", () => {
  const onConfigureModel = vi.fn();
  renderPage({ asrBackend: "sherpa-onnx", modelId: null }, "en", { onConfigureModel });
  expect((screen.getByRole("combobox", { name: "Model" }) as HTMLSelectElement).value).toBe("reazonspeech-k2-v2");
  expect(screen.getByRole("combobox", { name: "Speech device" })).toBeTruthy();
  const facts = details();
  expect(facts.getByText("Japanese only")).toBeTruthy();
  expect(facts.getByText(/CUDA \(NVIDIA\) \/ CPU/)).toBeTruthy();
  expect(facts.getByText("reazon-research/reazonspeech-k2-v2")).toBeTruthy();
  expect(facts.getByText(/2,828 MiB/)).toBeTruthy();
  fireEvent.click(screen.getByRole("button", { name: "Load Model" }));
  expect(onConfigureModel).toHaveBeenCalledWith(expect.objectContaining({ asrBackend: "sherpa-onnx", modelId: "reazonspeech-k2-v2" }));
  expect(api.getAsrModelCacheStatus).not.toHaveBeenCalled();
});

test("a Japanese-only adapter cannot be loaded with another speech language", () => {
  renderPage({ asrBackend: "sherpa-onnx", speechLocale: "en-US" });
  expect((screen.getByRole("button", { name: "Load Model" }) as HTMLButtonElement).disabled).toBe(true);
  cleanupAndRender({ asrBackend: "transformers-asr", speechLocale: "en-US" });
  expect((screen.getByRole("button", { name: "Load Model" }) as HTMLButtonElement).disabled).toBe(false);
  expect(details().getByText("Multilingual (including Japanese)")).toBeTruthy();
});

test("loaded runtime shows the provider and GPU the worker actually used", async () => {
  vi.mocked(api.getWorkerHealth).mockResolvedValueOnce({
    pythonPath: "python", pythonVersion: "3.12", fasterWhisperVersion: null, ctranslate2Version: null,
    backend: "sherpa-onnx", loaded: true, device: "cuda", computeType: "float32", deviceName: "NVIDIA GeForce RTX 3060",
    runtime: "sherpa-onnx 2.0.1 / ONNX Runtime 1.24.4 CUDAExecutionProvider",
  });
  const model: ModelStatus = { modelId: "sherpa-onnx:reazonspeech-k2-v2", quantization: "4bit", installed: true, state: "ready", detail: "" };
  renderPage({ asrBackend: "sherpa-onnx" }, "en", { model });
  expect(await details().findByText(/CUDA · NVIDIA GeForce RTX 3060 · float32 · .*CUDAExecutionProvider/)).toBeTruthy();
});

test("automatic voice mode blocks loading ASR runtimes that cannot share its worker", () => {
  renderPage({ asrBackend: "sherpa-onnx", asrDevice: "auto", automaticVoiceMode: true }, "ja");
  expect((screen.getByRole("button", { name: "モデルを読み込む" }) as HTMLButtonElement).disabled).toBe(true);
  expect(screen.getByRole("alert").textContent).toContain("音声モードの自動判定は、このASR実行環境と同じワーカーでは使えません");
  cleanup();
  renderPage({ asrBackend: "sherpa-onnx", asrDevice: "cpu", automaticVoiceMode: true });
  expect((screen.getByRole("button", { name: "Load Model" }) as HTMLButtonElement).disabled).toBe(false);
  cleanup();
  renderPage({ asrBackend: "transformers-asr", asrDevice: "cpu", automaticVoiceMode: true });
  expect((screen.getByRole("button", { name: "Load Model" }) as HTMLButtonElement).disabled).toBe(true);
});
