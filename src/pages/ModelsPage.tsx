import { useEffect, useMemo, useState, type FormEvent } from "react";
import { SettingRow } from "../components/ui";
import type {
  AsrBackend,
  CustomModel,
  GpuDiagnostics,
  ModelQuantization,
  Settings,
} from "../types";
import { useI18n } from "../i18n";
import type { MessageKey } from "../i18n";

type ModelConfiguration = Pick<
  Settings,
  | "asrBackend"
  | "modelId"
  | "modelQuantization"
  | "apiBaseUrl"
  | "apiKeyEnvVar"
>;
const ADDITIONAL_MODEL_VALUE = "__additional_model__";

function builtinValue(backend: AsrBackend) {
  return `builtin:${backend}`;
}

function customModelValue(model: CustomModel) {
  return `custom:${encodeURIComponent(model.asrBackend)}:${encodeURIComponent(model.modelId)}`;
}

const modelTypeOptions: Array<{ value: AsrBackend; label: MessageKey }> = [
  { value: "faster-whisper", label: "Whisper model" },
  { value: "vibevoice", label: "VibeVoice model" },
  { value: "openai-compatible", label: "OpenAI-compatible API model" },
];

function modelTypeLabel(backend: AsrBackend) {
  return modelTypeOptions.find((option) => option.value === backend)?.label ?? "Model ID";
}

export function ModelsPage({
  gpu,
  settings,
  asrBackendOptions,
  modelLoading,
  onConfigureModel,
  onSaveCustomModel,
  onDiagnoseGpu,
}: {
  gpu: GpuDiagnostics | null;
  settings: Settings;
  asrBackendOptions: Array<{ value: AsrBackend; label: MessageKey }>;
  modelLoading: boolean;
  onConfigureModel: (configuration: ModelConfiguration) => void;
  onSaveCustomModel: (model: CustomModel) => Promise<boolean>;
  onDiagnoseGpu: () => void;
}) {
  const { t } = useI18n();
  const [backend, setBackend] = useState(settings.asrBackend);
  const [additionalModelId, setAdditionalModelId] = useState(settings.modelId ?? "");
  const [quantization, setQuantization] = useState<ModelQuantization>(
    settings.modelQuantization,
  );
  const [apiBaseUrl, setApiBaseUrl] = useState(settings.apiBaseUrl);
  const [apiKeyEnvVar, setApiKeyEnvVar] = useState(settings.apiKeyEnvVar);
  const [modalOpen, setModalOpen] = useState(false);
  const [draftBackend, setDraftBackend] = useState<AsrBackend>(settings.asrBackend);
  const [draftModelId, setDraftModelId] = useState("");
  const [savingCustomModel, setSavingCustomModel] = useState(false);

  useEffect(() => {
    setBackend(settings.asrBackend);
    setAdditionalModelId(settings.modelId ?? "");
    setQuantization(settings.modelQuantization);
    setApiBaseUrl(settings.apiBaseUrl);
    setApiKeyEnvVar(settings.apiKeyEnvVar);
  }, [
    settings.apiBaseUrl,
    settings.apiKeyEnvVar,
    settings.asrBackend,
    settings.modelId,
    settings.modelQuantization,
  ]);

  useEffect(() => {
    if (!modalOpen) return;
    const closeOnEscape = (event: KeyboardEvent) => {
      if (event.key === "Escape" && !savingCustomModel) setModalOpen(false);
    };
    document.addEventListener("keydown", closeOnEscape);
    return () => document.removeEventListener("keydown", closeOnEscape);
  }, [modalOpen, savingCustomModel]);

  const customModels = useMemo(() => {
    const saved = [...settings.customModels];
    if (
      settings.modelId &&
      !saved.some(
        (model) =>
          model.asrBackend === settings.asrBackend && model.modelId === settings.modelId,
      )
    ) {
      saved.push({ asrBackend: settings.asrBackend, modelId: settings.modelId });
    }
    return saved;
  }, [settings.asrBackend, settings.customModels, settings.modelId]);

  const localizedBackendDetails: Record<AsrBackend, string> = {
    "faster-whisper": t("Fast local transcription on CPU or CUDA."),
    vibevoice: t("Long-form transcription on a CUDA GPU."),
    "openai-compatible": t("OpenAI Audio Transcriptions API or a compatible local server."),
  };
  const selectedValue = additionalModelId
    ? customModelValue({ asrBackend: backend, modelId: additionalModelId })
    : builtinValue(backend);

  function openAdditionalModelModal() {
    setDraftBackend(backend);
    setDraftModelId("");
    setModalOpen(true);
  }

  async function saveAdditionalModel(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    const modelId = draftModelId.trim();
    if (!modelId || savingCustomModel) return;
    setSavingCustomModel(true);
    const saved = await onSaveCustomModel({ asrBackend: draftBackend, modelId });
    setSavingCustomModel(false);
    if (!saved) return;
    setBackend(draftBackend);
    setAdditionalModelId(modelId);
    setModalOpen(false);
  }

  return (
    <section className="grid">
      <article className="panel span-2">
        <p className="eyebrow">{t("ASR BACKEND")}</p>
        <h2>{t("Select a backend and load it")}</h2>
        <p className="lead">
          {t("Choose a local model or an OpenAI-compatible transcription endpoint. Local model files are downloaded on first use and cached.")}
        </p>
        <div className="steps model-settings">
          <SettingRow
            title={t("ASR backend")}
            detail={localizedBackendDetails[backend]}
            control={
              <select
                value={selectedValue}
                onChange={(event) => {
                  const value = event.target.value;
                  if (value === ADDITIONAL_MODEL_VALUE) {
                    openAdditionalModelModal();
                    return;
                  }
                  const builtin = asrBackendOptions.find(
                    (option) => builtinValue(option.value) === value,
                  );
                  if (builtin) {
                    setBackend(builtin.value);
                    setAdditionalModelId("");
                    return;
                  }
                  const customModel = customModels.find(
                    (model) => customModelValue(model) === value,
                  );
                  if (customModel) {
                    setBackend(customModel.asrBackend);
                    setAdditionalModelId(customModel.modelId);
                  }
                }}
              >
                <optgroup label={t("Built-in backends")}>
                  {asrBackendOptions.map((option) => (
                    <option key={option.value} value={builtinValue(option.value)}>
                      {t(option.label)}
                    </option>
                  ))}
                </optgroup>
                {customModels.length > 0 && (
                  <optgroup label={t("Additional Models")}>
                    {customModels.map((model) => (
                    <option key={customModelValue(model)} value={customModelValue(model)}>
                        {model.modelId} ({t(modelTypeLabel(model.asrBackend))})
                      </option>
                    ))}
                  </optgroup>
                )}
                <option value={ADDITIONAL_MODEL_VALUE}>{t("+ Additional Model...")}</option>
              </select>
            }
          />
          {backend === "openai-compatible" ? (
            <>
              <SettingRow
                title={t("API base URL")}
                detail={t("Use https://api.openai.com/v1 for OpenAI, or a local server such as http://127.0.0.1:8000/v1.")}
                control={
                  <input
                    value={apiBaseUrl}
                    onChange={(event) => setApiBaseUrl(event.target.value)}
                    placeholder="https://api.openai.com/v1"
                    maxLength={2048}
                  />
                }
              />
              <SettingRow
                title={t("API key environment variable")}
                detail={t("The secret itself is not saved. OpenAI uses OPENAI_API_KEY; an unauthenticated local server needs no value set.")}
                control={
                  <input
                    value={apiKeyEnvVar}
                    onChange={(event) => setApiKeyEnvVar(event.target.value)}
                    placeholder="OPENAI_API_KEY"
                    maxLength={128}
                  />
                }
              />
            </>
          ) : (
            <SettingRow
              title={t("Load format")}
              detail={t("The recommended setting minimizes memory usage. Use bf16 only with sufficient GPU memory.")}
              control={
                <select
                  value={quantization}
                  onChange={(event) =>
                    setQuantization(event.target.value as ModelQuantization)
                  }
                >
                  <option value="4bit">{t("Memory saving (recommended)")}</option>
                  <option value="8bit">{t("8-bit")}</option>
                  <option value="bf16">{t("bf16 / float16")}</option>
                </select>
              }
            />
          )}
        </div>
        <button
          className="primary"
          onClick={() =>
            onConfigureModel({
              asrBackend: backend,
              modelId: additionalModelId || null,
              modelQuantization: quantization,
              apiBaseUrl: apiBaseUrl.trim(),
              apiKeyEnvVar: apiKeyEnvVar.trim(),
            })
          }
          disabled={modelLoading}
        >
          {modelLoading ? t("Loading...") : t("Load Model")}
        </button>
        <p className="model-note">
          {t("If recording starts before loading, Local Voice automatically prepares the selected backend.")}
        </p>
      </article>

      <article className="panel span-2">
        <p className="eyebrow">{t("GPU")}</p>
        <h2>{gpu?.adapterName ?? t("Not checked")}</h2>
        <p>{gpu?.recommendation ?? t("Run diagnostics to check NVIDIA availability and VRAM.")}</p>
        {gpu?.memoryTotalMb && <strong>{gpu.memoryTotalMb} MB VRAM</strong>}
        <br />
        <button className="secondary" onClick={onDiagnoseGpu}>
          {t("Run diagnostics")}
        </button>
      </article>

      {modalOpen && (
        <div className="modal-backdrop" onMouseDown={() => !savingCustomModel && setModalOpen(false)}>
          <div
            className="modal-card"
            role="dialog"
            aria-modal="true"
            aria-labelledby="additional-model-title"
            onMouseDown={(event) => event.stopPropagation()}
          >
            <form onSubmit={(event) => void saveAdditionalModel(event)}>
              <p className="eyebrow">{t("ADDITIONAL MODEL")}</p>
              <h2 id="additional-model-title">{t("Add an ASR model")}</h2>
              <p>{t("The model will be saved and available from the ASR backend list.")}</p>
              <label className="modal-field">
                  <strong>{t("Model type")}</strong>
                <select
                  value={draftBackend}
                  onChange={(event) => setDraftBackend(event.target.value as AsrBackend)}
                >
                  {modelTypeOptions.map((option) => (
                    <option key={option.value} value={option.value}>
                      {option.label}
                    </option>
                  ))}
                </select>
              </label>
              <label className="modal-field">
                  <strong>{t("Model ID")}</strong>
                <input
                  value={draftModelId}
                  placeholder={
                    draftBackend === "openai-compatible"
                      ? t("API model ID (for example gpt-4o-mini-transcribe)")
                      : t("Model name or Hugging Face repository ID")
                  }
                  onChange={(event) => setDraftModelId(event.target.value)}
                  maxLength={512}
                  autoFocus
                />
              </label>
              <div className="modal-actions">
                <button
                  type="button"
                  className="secondary"
                  onClick={() => setModalOpen(false)}
                  disabled={savingCustomModel}
                >
                  {t("Cancel")}
                </button>
                <button
                  type="submit"
                  className="primary"
                  disabled={savingCustomModel || !draftModelId.trim()}
                >
                  {savingCustomModel ? t("Saving...") : t("Add Model")}
                </button>
              </div>
            </form>
          </div>
        </div>
      )}
    </section>
  );
}
