import { useEffect, useState } from "react";
import { deleteAsrModelCache, getAsrModelCacheStatus, getWorkerHealth } from "../api";
import { translateAppMessage, useI18n } from "../i18n";
import type { MessageKey } from "../i18n";
import {
  catalogEntryFor,
  modelCatalog,
  type CatalogModel,
  type InferenceMode,
  type LanguageScope,
  type RuntimeDevice,
  type VocabularyAdaptation,
  type Cancellation,
} from "../modelCatalog";
import type { AsrBackend, ManagedModelCache, ModelStatus, WorkerHealth } from "../types";

const languageLabels: Record<LanguageScope, MessageKey> = {
  japanese_only: "Japanese only",
  multilingual: "Multilingual (including Japanese)",
  server_defined: "Depends on the API model",
};
const inferenceLabels: Record<InferenceMode, MessageKey> = {
  batch: "Batch (after stopping)",
  chunked: "Chunked final recognition (optional)",
  stateful_streaming: "Stateful streaming",
};
const deviceLabels: Record<RuntimeDevice, MessageKey> = {
  cpu: "CPU",
  cuda: "CUDA (NVIDIA)",
  vulkan: "Vulkan GPU (not CUDA)",
  remote: "Remote API (HTTP)",
};
const vocabularyLabels: Record<VocabularyAdaptation, MessageKey> = {
  hotwords: "Dictionary words as hotwords",
  prompt: "Dictionary words as a prompt",
  post_dictionary_only: "After recognition only (no model hint)",
};
const cancellationLabels: Record<Cancellation, MessageKey> = {
  between_segments: "Between segments",
  between_tokens: "Between tokens",
  between_requests: "Only between requests",
};
const cacheLabels: Record<ManagedModelCache["state"], MessageKey> = {
  present: "Downloaded (size matches; SHA256 is verified at load)",
  incomplete: "Incomplete file",
  missing: "Not downloaded",
  unsafe: "Not a regular file; left unchanged",
};

export function formatBytes(bytes: number) {
  return bytes >= 1024 ** 3 ? `${(bytes / 1024 ** 3).toFixed(2)} GB` : `${Math.round(bytes / 1024 ** 2)} MB`;
}

function Fact({ label, children }: { label: string; children: React.ReactNode }) {
  return <div><dt>{label}</dt><dd>{children}</dd></div>;
}

function deviceSummary(entry: CatalogModel, t: (key: MessageKey) => string) {
  if (entry.devices.length === 0) return t("Not confirmed");
  const devices = entry.devices.map((device) => t(deviceLabels[device])).join(" / ");
  return entry.decoderOnCpu ? `${devices} · ${t("decoder on CPU")}` : devices;
}

export function ModelDetails({ backend, modelId, model }: { backend: AsrBackend; modelId: string | null; model?: ModelStatus | null }) {
  const { t, language } = useI18n();
  const resolved = catalogEntryFor(backend, modelId);
  const managed = resolved?.entry.backend === "speech-cpp" && !resolved.custom ? resolved.entry : null;
  const [cache, setCache] = useState<ManagedModelCache[] | null>(null);
  const [health, setHealth] = useState<WorkerHealth | null>(null);
  const [removing, setRemoving] = useState(false);
  const [cacheError, setCacheError] = useState<string | null>(null);
  const managedId = managed?.modelId ?? null;

  useEffect(() => {
    if (!managedId) return;
    let active = true;
    setCacheError(null);
    getAsrModelCacheStatus().then((status) => { if (active) setCache(status); }).catch(() => { if (active) setCache(null); });
    return () => { active = false; };
  }, [managedId, model?.state]);

  useEffect(() => {
    if (model?.state !== "ready") { setHealth(null); return; }
    let active = true;
    getWorkerHealth().then((value) => { if (active) setHealth(value); }).catch(() => { if (active) setHealth(null); });
    return () => { active = false; };
  }, [model?.state, model?.modelId]);

  if (!resolved) return null;
  const { entry, custom } = resolved;
  const devices = deviceSummary(entry, t);
  const cached = managedId ? cache?.find((item) => item.modelId === managedId) : undefined;
  const loadedRuntime = health?.loaded && health.backend === entry.backend && health.device
    ? [model?.modelId, health.device === "vulkan" ? "Vulkan" : health.device.toUpperCase(), health.computeType].filter(Boolean).join(" · ")
    : null;

  async function remove() {
    if (!managedId || removing) return;
    if (!window.confirm(`${t("Remove the downloaded model file? It is downloaded again on the next load.")}\n${entry.displayName}`)) return;
    setRemoving(true);
    setCacheError(null);
    try {
      setCache(await deleteAsrModelCache(managedId));
    } catch (error) {
      setCacheError(translateAppMessage(language, error instanceof Error ? error.message : String(error)));
    } finally {
      setRemoving(false);
    }
  }

  return (
    <section className="model-details" aria-label={t("Model details")}>
      <h3>{t("Model details")}: {custom ? modelId : entry.displayName}</h3>
      {custom && <p>{t("Custom model: languages, license, provenance and memory are not in the catalog. Runtime capabilities follow its backend.")}</p>}
      <dl className="data-flow model-facts">
        <Fact label={t("Speech languages")}>{entry.languageScope ? t(languageLabels[entry.languageScope]) : t("Not confirmed")}</Fact>
        <Fact label={t("Recognition method")}>{entry.inference.map((mode) => t(inferenceLabels[mode])).join(" / ")}</Fact>
        <Fact label={t("Runtime")}>{devices}{entry.weightFormat ? ` · ${entry.weightFormat}` : ""}</Fact>
        {loadedRuntime && <Fact label={t("Loaded runtime")}>{loadedRuntime}</Fact>}
        {entry.vocabulary && <Fact label={t("Dictionary adaptation")}>{t(vocabularyLabels[entry.vocabulary])}</Fact>}
        {entry.cancellation && <Fact label={t("Cancellation")}>{t(cancellationLabels[entry.cancellation])}</Fact>}
        {!custom && <Fact label={t("License")}>{entry.license ?? t("Depends on the API provider")}</Fact>}
        {!custom && entry.source && <Fact label={t("Source")}>{entry.source.repo}</Fact>}
        {!custom && entry.source && <Fact label={t("Revision")}>{entry.source.revision ? <code title={entry.source.revision}>{entry.source.revision.slice(0, 12)}</code> : t("Not pinned (latest at download)")}</Fact>}
        {!custom && entry.source?.sha256 && <Fact label="SHA256"><code title={entry.source.sha256}>{entry.source.sha256.slice(0, 16)}…</code></Fact>}
        {!custom && entry.source?.sizeBytes && <Fact label={t("Download size")}>{formatBytes(entry.source.sizeBytes)}</Fact>}
        {!custom && entry.backend !== "openai-compatible" && <Fact label={t("Memory")}>{
          entry.memory?.kind === "observed_device_peak_mib"
            ? <>{t("Observed device-wide GPU memory peak")}: {entry.memory.value.toLocaleString()} MiB ({entry.memory.gpu}, {entry.memory.runtime}). {t("Includes other apps; not a per-process value.")}</>
            : entry.memory?.kind === "recommended_vram_gb"
              ? <>{t("Recommended GPU memory")}: {entry.memory.value} GB+</>
              : t("Not measured")
        }</Fact>}
        {managed && <Fact label={t("Downloaded file")}>{cached ? `${t(cacheLabels[cached.state])}${cached.sizeBytes !== null && cached.state !== "present" ? ` (${formatBytes(cached.sizeBytes)})` : ""}` : t("Not checked")}</Fact>}
      </dl>
      {managed && cached && cached.state !== "missing" && cached.state !== "unsafe" && (
        <button className="secondary danger-button" onClick={() => void remove()} disabled={removing}>{removing ? t("Removing...") : t("Remove download")}</button>
      )}
      {cacheError && <p role="alert">{cacheError}</p>}
    </section>
  );
}

export function ModelCatalogOverview() {
  const { t } = useI18n();
  return (
    <details className="panel span-2 utility-disclosure"><summary>{t("Model catalog")}</summary>
      <p>{t("Values marked Not confirmed have not been verified in this app.")}</p>
      <ul className="model-catalog-list">
        {modelCatalog.models.map((entry) => (
          <li key={entry.id}>
            <strong>{entry.displayName}</strong>{" "}
            <span className="entry-badge">{entry.availability === "available" ? t("Available") : t("Planned (not available yet)")}</span>
            <p>
              {[
                entry.languageScope ? t(languageLabels[entry.languageScope]) : `${t("Speech languages")}: ${t("Not confirmed")}`,
                entry.devices.length ? deviceSummary(entry, t) : `${t("Runtime")}: ${t("Not confirmed")}`,
                `${t("License")}: ${entry.license ?? (entry.backend === "openai-compatible" ? t("Depends on the API provider") : t("Not confirmed"))}`,
                entry.issue ? `#${entry.issue}` : null,
              ].filter(Boolean).join(" · ")}
            </p>
          </li>
        ))}
      </ul>
    </details>
  );
}
