import { useEffect, useMemo, useRef, useState, type FormEvent } from "react";
import { HelpHint, IconButton, ModelProgressBar, SettingRow } from "../components/ui";
import type {
  AsrBackend,
  CustomModel,
  GpuDiagnostics,
  ModelQuantization,
  Settings,
  ModelStatus,
  ModelProgress,
} from "../types";
import { translateAppMessage, useI18n } from "../i18n";
import type { MessageKey } from "../i18n";

type ModelConfiguration = Pick<
  Settings,
  | "asrBackend"
  | "asrDevice"
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
  model,
  progress,
  onOpenLogs,
}: {
  gpu: GpuDiagnostics | null;
  settings: Settings;
  asrBackendOptions: Array<{ value: AsrBackend; label: MessageKey }>;
  modelLoading: boolean;
  onConfigureModel: (configuration: ModelConfiguration) => void;
  onSaveCustomModel: (model: CustomModel) => Promise<boolean>;
  onDiagnoseGpu: () => void;
  model?: ModelStatus | null;
  progress?: ModelProgress | null;
  onOpenLogs?: () => void;
}) {
  const { t, language } = useI18n();
  const [backend, setBackend] = useState(settings.asrBackend);
  const [device, setDevice] = useState(settings.asrDevice);
  const [additionalModelId, setAdditionalModelId] = useState(settings.modelId ?? "");
  const [quantization, setQuantization] = useState<ModelQuantization>(
    settings.modelQuantization,
  );
  const [apiBaseUrl, setApiBaseUrl] = useState(settings.apiBaseUrl);
  const [apiKeyEnvVar, setApiKeyEnvVar] = useState(settings.apiKeyEnvVar);
  const [modalOpen, setModalOpen] = useState(false);
  const modalRef = useRef<HTMLDialogElement>(null);
  const launcherRef = useRef<HTMLSelectElement>(null);
  const modelIdRef = useRef<HTMLInputElement>(null);
  const [draftBackend, setDraftBackend] = useState<AsrBackend>(settings.asrBackend);
  const [draftModelId, setDraftModelId] = useState("");
  const [savingCustomModel, setSavingCustomModel] = useState(false);

  useEffect(() => {
    setBackend(settings.asrBackend);
    setDevice(settings.asrDevice);
    setAdditionalModelId(settings.modelId ?? "");
    setQuantization(settings.modelQuantization);
    setApiBaseUrl(settings.apiBaseUrl);
    setApiKeyEnvVar(settings.apiKeyEnvVar);
  }, [
    settings.apiBaseUrl,
    settings.apiKeyEnvVar,
    settings.asrBackend,
    settings.asrDevice,
    settings.modelId,
    settings.modelQuantization,
  ]);

  useEffect(() => {
    if (!modalOpen) return;
    const dialog = modalRef.current;
    const launcher = launcherRef.current;
    dialog?.showModal();
    modelIdRef.current?.focus();
    return () => {
      dialog?.close();
      if (launcher?.isConnected) launcher.focus();
    };
  }, [modalOpen]);

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
        <div className="setting-title section-title"><h2>{t("ASR model")}</h2>{backend !== "openai-compatible" && <HelpHint label={t("ASR model")}>{t("First load may download a model.")}</HelpHint>}</div>

        <div className="steps model-settings">
          <SettingRow
            title={t("ASR backend")}
            detail={localizedBackendDetails[backend]}
            control={
              <select
                ref={launcherRef}
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
          {backend === "faster-whisper" && <SettingRow title={t("Speech device")} help={t("Auto retries caught CUDA failures on CPU. Explicit CUDA reports an error instead. The environment option preserves ASR_DEVICE.")}
            control={<select aria-label={t("Speech device")} value={device ?? ""} onChange={(event) => setDevice((event.target.value || null) as Settings["asrDevice"])}>
              <option value="">{t("Environment (auto by default)")}</option>
              <option value="auto">{t("Auto (CUDA with CPU fallback)")}</option>
              <option value="cpu">{t("CPU only")}</option>
              <option value="cuda">{t("CUDA only")}</option>
            </select>} />}
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
        <div className="model-load-actions"><button
          className="primary"
          onClick={() =>
            onConfigureModel({
              asrBackend: backend,
              asrDevice: device,
              modelId: additionalModelId || null,
              modelQuantization: quantization,
              apiBaseUrl: apiBaseUrl.trim(),
              apiKeyEnvVar: apiKeyEnvVar.trim(),
            })
          }
          disabled={modelLoading}
        >
          {modelLoading ? t("Loading...") : model?.state === "error" ? t("Retry") : t("Load Model")}
        </button>
        {model && <div role={model.state === "error" ? "alert" : "status"}>
          <strong>{t(model.state === "ready" ? "Ready" : model.state === "loading" ? "Model loading" : model.state === "error" ? "Model error" : "Model not loaded")}</strong>
          {model.state === "error" || model.state === "loading" ? <p>{translateAppMessage(language, model.detail)}</p>
            : <HelpHint label={t("ASR model")}>{[translateAppMessage(language, model.detail), t("If recording starts before loading, Local Voice automatically prepares the selected backend.")].filter(Boolean).join("\n\n")}</HelpHint>}
          {model.state === "error" && onOpenLogs && <button className="secondary" onClick={onOpenLogs}>{t("Open diagnostic logs")}</button>}
        </div>}
        </div>
        {progress && <ModelProgressBar progress={progress} />}
      </article>

      <details className="panel span-2 utility-disclosure"><summary>{t("GPU")}</summary>
        <h2>{gpu?.adapterName ?? t("Not checked")}</h2>
        {gpu && <p>{translateAppMessage(language, gpu.recommendation)}</p>}
        {gpu?.memoryTotalMb && <strong>{gpu.memoryTotalMb} MB VRAM</strong>}
        <br />
        <button className="secondary" onClick={onDiagnoseGpu}>
          {t("Run diagnostics")}
        </button>
      </details>

      {modalOpen && (
        <dialog ref={modalRef} className="model-dialog" aria-labelledby="additional-model-title"
          onCancel={event => { event.preventDefault(); if (!savingCustomModel) setModalOpen(false); }}
          onMouseDown={event => { if (event.target === event.currentTarget && !savingCustomModel) setModalOpen(false); }}>
          <div
            className="modal-card"
            onMouseDown={(event) => event.stopPropagation()}
          >
            <form onSubmit={(event) => void saveAdditionalModel(event)}>
              <h2 id="additional-model-title">{t("Add an ASR model")}</h2>
              <label className="modal-field">
                  <strong>{t("Model type")}</strong>
                <select
                  value={draftBackend}
                  onChange={(event) => setDraftBackend(event.target.value as AsrBackend)}
                >
                  {modelTypeOptions.map((option) => (
                    <option key={option.value} value={option.value}>
                      {t(option.label)}
                    </option>
                  ))}
                </select>
              </label>
              <label className="modal-field">
                  <strong>{t("Model ID")}</strong>
                <input
                  ref={modelIdRef}
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
                <IconButton icon="close" label={t("Cancel")}
                  onClick={() => setModalOpen(false)}
                  disabled={savingCustomModel}
                />
                <IconButton icon="add" label={savingCustomModel ? t("Saving...") : t("Add Model")}
                  type="submit"
                  className="primary"
                  disabled={savingCustomModel || !draftModelId.trim()}
                />
              </div>
            </form>
          </div>
        </dialog>
      )}
    </section>
  );
}
