import catalogJson from "./asr-model-catalog.json";
import type { AsrBackend } from "./types";

export type LanguageScope = "japanese_only" | "multilingual" | "server_defined";
export type InferenceMode = "batch" | "chunked" | "stateful_streaming";
export type RuntimeDevice = "cpu" | "cuda" | "vulkan" | "remote";
export type VocabularyAdaptation = "hotwords" | "prompt" | "post_dictionary_only";
export type Cancellation = "between_segments" | "between_tokens" | "between_requests";

export interface CatalogSource {
  repo: string;
  revision: string | null;
  file: string | null;
  sha256: string | null;
  sizeBytes: number | null;
  /** Every pinned file of a multi-file model; `file`/`sha256` are then null and `sizeBytes` is the total. */
  files?: Array<{ file: string; sha256: string; sizeBytes: number }>;
}

export type CatalogMemory =
  | { kind: "observed_device_peak_mib"; value: number; gpu: string; runtime: string }
  | { kind: "recommended_vram_gb"; value: number };

export interface CatalogModel {
  id: string;
  backend: AsrBackend | null;
  /** The settings `modelId` this entry stands for; null means the backend default. */
  modelId: string | null;
  /** Other configured model names that mean this same model. */
  aliases: string[];
  displayName: string;
  availability: "available" | "planned";
  /** null: not confirmed yet. */
  languageScope: LanguageScope | null;
  /** Empty for planned models: nothing is claimed before it is verified. */
  inference: InferenceMode[];
  devices: RuntimeDevice[];
  decoderOnCpu: boolean;
  vocabulary: VocabularyAdaptation | null;
  wordTimestamps: boolean | null;
  cancellation: Cancellation | null;
  sampleRateHz: number | null;
  license: string | null;
  source: CatalogSource | null;
  weightFormat: string | null;
  memory: CatalogMemory | null;
  issue: number | null;
}

export interface ModelCatalog {
  schemaVersion: number;
  runtimes: Record<string, { version: string; source: string; platform: string; archive?: string; archiveSha256?: string; package?: string }>;
  models: CatalogModel[];
}

export const modelCatalog = catalogJson as ModelCatalog;

/**
 * Whether the torch-based decision model (automatic voice mode) cannot share
 * the speech worker with this ASR runtime. Mirrors Rust `decision_conflicts_with_asr`.
 */
export function decisionConflictsWithAsr(backend: AsrBackend, device: "auto" | "cpu" | "cuda" | null): boolean {
  return backend === "transformers-asr" || (backend === "sherpa-onnx" && device !== "cpu");
}

/** Available catalog models of a backend. */
export function availableModels(backend: AsrBackend): CatalogModel[] {
  return modelCatalog.models.filter((model) => model.availability === "available" && model.backend === backend);
}

/**
 * Whether a backend serves only the catalog's pinned models (speech.cpp,
 * sherpa-onnx, transformers-asr): no free-form model ID is valid for it.
 */
export function isPinnedBackend(backend: AsrBackend): boolean {
  const models = availableModels(backend);
  return models.length > 0 && models.every((model) => model.modelId !== null);
}

/**
 * The catalog entry for a configured backend/model. A custom model ID has no
 * entry of its own; it shares its backend's runtime capabilities but none of
 * the default model's languages, license, provenance or measurements
 * (`custom: true`).
 */
export function catalogEntryFor(
  backend: AsrBackend,
  modelId: string | null,
): { entry: CatalogModel; custom: boolean } | null {
  const available = modelCatalog.models.filter((model) => model.availability === "available" && model.backend === backend);
  const id = modelId?.trim() || null;
  const exact = available.find((model) => model.modelId === id || (id !== null && model.aliases.includes(id)));
  if (exact) return { entry: exact, custom: false };
  if (isPinnedBackend(backend)) return null;
  const backendDefault = available.find((model) => model.modelId === null);
  if (!backendDefault) return null;
  if (backend === "openai-compatible") return { entry: backendDefault, custom: false };
  // Language coverage belongs to the model (for example `tiny.en`), not to
  // its backend, so a custom model's languages stay unconfirmed.
  return { entry: { ...backendDefault, languageScope: null }, custom: true };
}
