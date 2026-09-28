import { useEffect, useMemo, useRef, useState } from "react";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWebviewWindow } from "@tauri-apps/api/webviewWindow";
import {
  addDictionaryEntry,
  cancelRecording,
  copyToClipboard,
  cycleVoiceTranslationTarget,
  defaultSettings,
  deleteDictionaryEntry,
  deleteHistoryItem,
  deleteAllHistory,
  getHistoryAudio,
  retryHistoryItem,
  getAppState,
  getAskAnswer,
  dismissAskAnswer,
  getGpuDiagnostics,
  getModelStatus,
  getSettings,
  listAudioDevices,
  listDictionary,
  listHistory,
  runGpuDiagnostics,
  loadModel,
  startRecording,
  stopRecording,
  updateSettings,
} from "./api";
import type {
  AppState,
  AsrBackend,
  AudioDevice,
  AudioLevel,
  CustomModel,
  DictionaryEntry,
  DictionaryEntryInput,
  GpuDiagnostics,
  HistoryItem,
  HistoryFilter,
  ModelProgress,
  ModelStatus,
  Settings,
} from "./types";
import { ModelProgressBar } from "./components/ui";
import { DashboardPage } from "./pages/DashboardPage";
import { SetupPage } from "./pages/SetupPage";
import { SettingsPage } from "./pages/SettingsPage";
import { ModelsPage } from "./pages/ModelsPage";
import { HistoryPage } from "./pages/HistoryPage";
import { DictionaryPage } from "./pages/DictionaryPage";
import { PrivacyPage } from "./pages/PrivacyPage";
import { DiagnosticsPage } from "./pages/DiagnosticsPage";
import {
  I18nProvider,
  translate,
  translateAppMessage,
  useI18n,
  type MessageKey,
} from "./i18n";

const asrBackendOptions: Array<{ value: AsrBackend; label: MessageKey }> = [
  { value: "faster-whisper", label: "faster-whisper (recommended / CPU supported)" },
  { value: "vibevoice", label: "VibeVoice (CUDA GPU required)" },
  { value: "openai-compatible", label: "OpenAI-compatible API" },
];

type Page =
  | "setup"
  | "dashboard"
  | "settings"
  | "models"
  | "history"
  | "dictionary"
  | "privacy"
  | "diagnostics";

const pages: Array<{ id: Page; label: MessageKey }> = [
  { id: "dashboard", label: "Status" },
  { id: "setup", label: "Setup" },
  { id: "models", label: "Models" },
  { id: "settings", label: "Settings" },
  { id: "history", label: "History" },
  { id: "dictionary", label: "Dictionary" },
  { id: "privacy", label: "Privacy" },
  { id: "diagnostics", label: "Diagnostics" },
];

const phaseMessageKeys: Record<AppState["phase"], MessageKey> = {
  idle: "Idle",
  recording: "Recording",
  processing: "Processing",
  injecting: "Injecting",
  completed: "Completed",
  error: "Error",
};

const isRecordingOverlay = getCurrentWebviewWindow().label === "recording-overlay";
const isAskAnswer = getCurrentWebviewWindow().label === "ask-answer";
const OVERLAY_WAVE_BAR_COUNT = 9;
const OVERLAY_PREVIEW_CHARS = 140;

interface CorrectionPreview {
  text: string;
  stage: "draft" | "streaming" | "final" | "fallback";
}

interface VoiceModeEvent {
  mode: "dictate" | "translate" | "edit";
  targetLanguage: string | null;
}

const translationLanguageKeys: Record<string, MessageKey> = {
  en: "English",
  ja: "Japanese",
  zh: "Chinese",
  es: "Spanish",
  fr: "French",
  pt: "Portuguese",
  de: "German",
  ko: "Korean",
};

interface Notice {
  message: string;
  severity: "info" | "success" | "warning" | "error";
  // Kind of the backend status event, when the notice came from one.
  kind?: string;
}

// Notices that only describe model preparation and are cleared once it ends.
const MODEL_PREPARATION_KINDS = ["model_loading", "model_downloading"];

const WARNING_STATUS_KINDS = new Set([
  "artifact_cleanup_failed",
  "history_audio_unavailable",
  "history_save_failed",
  "autostart_update_failed",
  "clipboard_only",
  "gpu_unavailable",
  "paste_unverified",
  "streaming_insertion_unavailable",
  "text_correction_failed",
  "voice_translation_failed",
]);

const SUCCESS_STATUS_KINDS = new Set([
  "clipboard_paste",
  "gpu_available",
  "provisional_replace",
  "success",
]);

function statusSeverity(kind: string): Notice["severity"] {
  if (kind === "error" || kind === "model_load_failed") return "error";
  if (WARNING_STATUS_KINDS.has(kind)) return "warning";
  if (SUCCESS_STATUS_KINDS.has(kind)) return "success";
  return "info";
}

function compactOverlayPreview(text: string): string {
  const characters = Array.from(text.trim());
  return characters.length > OVERLAY_PREVIEW_CHARS
    ? `…${characters.slice(-OVERLAY_PREVIEW_CHARS).join("")}`
    : characters.join("");
}

if (isRecordingOverlay) {
  document.body.classList.add("recording-overlay-body");
}

function AskAnswerPanel() {
  const [answer, setAnswer] = useState("");
  const [operationId, setOperationId] = useState(0);
  const operationRef = useRef(0);
  useEffect(() => {
    let active = true;
    const setup = async () => {
      const unlisten = await listen<{ operationId: number; payload: string }>("ask-answer", ({ payload }) => {
        if (!active || payload.operationId < operationRef.current) return;
        operationRef.current = payload.operationId;
        setOperationId(payload.operationId);
        setAnswer(payload.payload);
      });
      if (!active) {
        unlisten();
        return;
      }
      const current = await getAskAnswer();
      if (active && current && current.operationId >= operationRef.current) {
        operationRef.current = current.operationId;
        setOperationId(current.operationId);
        setAnswer(current.payload);
      }
      return unlisten;
    };
    let unlisten: (() => void) | undefined;
    void setup().then((cleanup) => { unlisten = cleanup; if (!active) cleanup?.(); });
    return () => { active = false; unlisten?.(); };
  }, []);
  const dismiss = async () => {
    if (await dismissAskAnswer(operationId)) {
      await getCurrentWebviewWindow().hide();
    }
  };
  return <main className="ask-answer-panel" aria-live="polite">
    <p className="eyebrow">ASK</p><div className="ask-answer-text">{answer}</div>
    <div className="ask-answer-actions"><button className="secondary" type="button" disabled={!answer} onClick={() => void copyToClipboard(answer)}>Copy</button><button className="secondary" type="button" disabled={!answer} onClick={() => void dismiss()}>Dismiss</button></div>
  </main>;
}

function RecordingOverlay() {
  const { language, t } = useI18n();
  const [waveform, setWaveform] = useState<number[]>(
    () => Array(OVERLAY_WAVE_BAR_COUNT).fill(0),
  );
  const [phase, setPhase] = useState<AppState["phase"]>("idle");
  const previousPhase = useRef<AppState["phase"]>("idle");
  const [message, setMessage] = useState<string | null>(null);
  const [preview, setPreview] = useState<CorrectionPreview | null>(null);
  const [voiceMode, setVoiceMode] = useState<VoiceModeEvent>({
    mode: "dictate",
    targetLanguage: null,
  });

  useEffect(() => {
    const listeners = Promise.all([
      listen<AudioLevel>("audio-level", ({ payload }) => {
        const rms = Number.isFinite(payload.rms) ? payload.rms : 0;
        const peak = Number.isFinite(payload.peak) ? payload.peak : 0;
        const inputLevel = Math.max(rms * 3.5, peak);
        const normalizedLevel = inputLevel < 0.015 ? 0 : Math.min(1, inputLevel);
        setWaveform((current) => [...current.slice(1), normalizedLevel]);
      }),
      listen<AppState>("app-state", ({ payload }) => {
        const recordingStarted = payload.phase === "recording" && previousPhase.current !== "recording";
        previousPhase.current = payload.phase;
        setPhase(payload.phase);
        setMessage(payload.message);
        if (recordingStarted) {
          setWaveform(Array(OVERLAY_WAVE_BAR_COUNT).fill(0));
          setPreview(null);
        } else if (
          payload.phase === "processing" &&
          (payload.message?.startsWith("Stopping") || payload.message?.startsWith("Transcribing"))
        ) {
          setPreview(null);
        }
      }),
      listen<CorrectionPreview>("correction-preview", ({ payload }) => {
        setPreview((current) => {
          if (payload.stage !== "streaming") return payload;
          return {
            ...payload,
            text: current?.stage === "streaming" ? current.text + payload.text : payload.text,
          };
        });
      }),
      listen<VoiceModeEvent>("voice-mode", ({ payload }) => setVoiceMode(payload)),
    ]);

    return () => {
      void listeners.then((unlisten) => unlisten.forEach((fn) => fn()));
    };
  }, []);

  if (phase === "recording") {
    return (
      <div className="recording-overlay recording" role="status" aria-label={t("Recording in progress")}>
        <span className="recording-live-dot" aria-hidden="true" />
        <span className="recording-overlay-label">
          {voiceMode.mode === "translate"
            ? t("Translating")
            : voiceMode.mode === "edit"
              ? t("Editing")
              : t("Listening")}
        </span>
        <span className="recording-wave" aria-hidden="true">
          {waveform.map((amplitude, index) => (
            <i
              key={index}
              style={{
                height: `${3 + amplitude * 14}px`,
                opacity: 0.45 + amplitude * 0.55,
              }}
            />
          ))}
        </span>
        {voiceMode.mode === "translate" && voiceMode.targetLanguage && (
          <button
            className="translation-target-button"
            type="button"
            title={t("Cycle target language; this recording will use clipboard fallback.")}
            onClick={() => void cycleVoiceTranslationTarget().catch(() => undefined)}
          >
            {t(translationLanguageKeys[voiceMode.targetLanguage] ?? "Language")} ↻
          </button>
        )}
      </div>
    );
  }

  if (phase !== "processing" && phase !== "injecting") return null;

  const compactPreview = compactOverlayPreview(preview?.text ?? "");
  const label = voiceMode.mode === "edit"
    ? t("Editing")
    : phase === "injecting"
      ? message?.startsWith("Finalizing")
        ? t("Updating text")
        : t("Inserting")
      : message?.startsWith("Stopping")
        ? t("Finishing audio")
        : message?.startsWith("Transcribing")
          ? t("Transcribing")
          : preview?.stage === "draft"
            ? t("Transcript ready")
            : preview?.stage === "streaming"
              ? t("AI correcting")
              : preview?.stage === "fallback"
                ? t("Using transcript")
                : t("Processing");

  return (
    <div className="recording-overlay processing" role="status" aria-live="polite">
      <span className="processing-spinner" aria-hidden="true" />
      <div className="processing-copy">
        <span className="recording-overlay-label">{label}</span>
        <span className={`correction-preview${compactPreview ? "" : " pending"}`}>
          {compactPreview || translateAppMessage(language, message) || t("Preparing text…")}
        </span>
      </div>
    </div>
  );
}

export default function App() {
  const [language, setLanguage] = useState<Settings["uiLanguage"] | null>(null);

  useEffect(() => {
    getSettings()
      .then((settings) => setLanguage(settings.uiLanguage))
      .catch(() => setLanguage(defaultSettings.uiLanguage));
  }, []);

  if (language === null) return null;
  return (
    <I18nProvider language={language}>
      {isRecordingOverlay ? (
        <RecordingOverlay />
      ) : isAskAnswer ? (
        <AskAnswerPanel />
      ) : (
        <MainAppContent onLanguageChange={setLanguage} />
      )}
    </I18nProvider>
  );
}

function MainAppContent({ onLanguageChange }: { onLanguageChange: (language: Settings["uiLanguage"]) => void }) {
  const { language, t } = useI18n();
  const [page, setPage] = useState<Page>("dashboard");
  const [state, setState] = useState<AppState>({
    phase: "idle",
    message: null,
    lastResult: null,
    updatedAt: new Date().toISOString(),
  });
  const [settings, setSettings] = useState<Settings>(defaultSettings);
  const [history, setHistory] = useState<HistoryItem[]>([]);
  const [historyFilter, setHistoryFilter] = useState<HistoryFilter>("all");
  const historyFilterRef = useRef<HistoryFilter>("all");
  const historyRequestRef = useRef(0);
  const [model, setModel] = useState<ModelStatus | null>(null);
  const [modelLoading, setModelLoading] = useState(false);
  const [modelProgress, setModelProgress] = useState<ModelProgress | null>(null);
  const [recordingAction, setRecordingAction] = useState(false);
  const recordingActionRef = useRef(false);
  const [gpu, setGpu] = useState<GpuDiagnostics | null>(null);
  const [gpuChecking, setGpuChecking] = useState(false);
  const [notice, setNotice] = useState<Notice | null>(null);
  const showNotice = (message: string, severity: Notice["severity"] = "info") =>
    setNotice({ message, severity });
  const [noticeCopied, setNoticeCopied] = useState(false);
  const [devices, setDevices] = useState<AudioDevice[]>([]);
  const [level, setLevel] = useState<AudioLevel>({ rms: 0, peak: 0 });
  const [dictionary, setDictionary] = useState<DictionaryEntry[]>([]);

  useEffect(() => {
    historyFilterRef.current = historyFilter;
  }, [historyFilter]);

  useEffect(() => {
    Promise.all([
      getAppState(),
      getSettings(),
      listHistory(),
      getModelStatus(),
      getGpuDiagnostics(),
      listAudioDevices(),
      listDictionary(),
    ])
      .then(
        ([nextState, nextSettings, nextHistory, nextModel, nextGpu, nextDevices, nextDictionary]) => {
          setState(nextState);
          setSettings(nextSettings);
          onLanguageChange(nextSettings.uiLanguage);
          if (historyRequestRef.current === 0 && historyFilterRef.current === "all") {
            setHistory(nextHistory);
          }
          setModel(nextModel);
          setGpu(nextGpu);
          setDevices(nextDevices);
          setDictionary(nextDictionary);
          if (!nextSettings.setupComplete) setPage("setup");
        },
      )
      .catch((error: unknown) => showNotice(String(error), "error"));
    const listeners = Promise.all([
      listen<AppState>("app-state", (event) => setState(event.payload)),
      listen<AudioLevel>("audio-level", (event) => setLevel(event.payload)),
      listen<ModelStatus>("model-status", (event) => {
        setModel(event.payload);
        // Preparation is over once the worker reports an outcome, so its
        // progress and its running commentary both stop here.
        if (event.payload.state !== "loading") {
          setModelProgress(null);
          setNotice((current) =>
            current?.kind && MODEL_PREPARATION_KINDS.includes(current.kind) ? null : current,
          );
        }
      }),
      listen<ModelProgress>("model-progress", (event) => setModelProgress(event.payload)),
      listen<GpuDiagnostics>("gpu-diagnostics", (event) => setGpu(event.payload)),
      listen<Settings>("settings-changed", (event) => {
        setSettings(event.payload);
        onLanguageChange(event.payload.uiLanguage);
      }),
      listen<{ kind: string; message: string }>("status", (event) =>
        setNotice({
          message: event.payload.message,
          severity: statusSeverity(event.payload.kind),
          kind: event.payload.kind,
        }),
      ),
    ]);
    return () => {
      void listeners.then((unlisten) => unlisten.forEach((fn) => fn()));
    };
  }, [onLanguageChange]);

  useEffect(() => {
    setNoticeCopied(false);
  }, [notice]);

  const statusLabel = t(phaseMessageKeys[state.phase]);
  const localizedState = useMemo(
    () => ({ ...state, message: translateAppMessage(language, state.message) }),
    [language, state],
  );
  // A load started from the Models page or automatically at startup.
  const preparingModel = modelLoading || model?.state === "loading";

  async function copyNotice() {
    if (!notice) return;
    try {
      await copyToClipboard(translateAppMessage(language, notice.message) ?? notice.message);
      setNoticeCopied(true);
    } catch (error) {
      showNotice(String(error), "error");
      setNoticeCopied(false);
    }
  }

  async function copyHistoryText(text: string) {
    try {
      await copyToClipboard(text);
    } catch (error) {
      showNotice(String(error), "error");
    }
  }

  async function refreshHistory(filter: HistoryFilter = historyFilterRef.current) {
    const request = ++historyRequestRef.current;
    const items = await listHistory(filter);
    if (request === historyRequestRef.current && filter === historyFilterRef.current) {
      setHistory(items);
    }
  }

  async function saveSettings(patch: Partial<Settings>) {
    const previous = settings;
    const next = { ...settings, ...patch };
    setSettings(next);
    try {
      const saved = await updateSettings(next);
      setSettings(saved);
      onLanguageChange(saved.uiLanguage);
      showNotice(translate(saved.uiLanguage, "Settings saved locally."), "success");
    } catch (error) {
      setSettings(previous);
      showNotice(String(error), "error");
      return;
    }
    if (patch.historyRetention !== undefined || patch.deleteAudioAfterProcessing !== undefined) {
      try { await refreshHistory(); }
      catch (error) { showNotice(String(error), "error"); }
    }
  }

  async function diagnoseGpu() {
    if (gpuChecking) return;
    setGpuChecking(true);
    showNotice(t("Running local GPU diagnostics..."));
    try {
      setGpu(await runGpuDiagnostics());
      showNotice(t("Diagnostics complete."), "success");
    } catch (error) {
      showNotice(String(error), "error");
    } finally {
      setGpuChecking(false);
    }
  }

  async function toggleRecording() {
    if (recordingActionRef.current) return;
    recordingActionRef.current = true;
    setRecordingAction(true);
    try {
      if (state.phase === "recording") {
        await stopRecording();
        await refreshHistory();
      } else {
        await startRecording();
      }
    } catch (error) {
      showNotice(String(error), "error");
      try {
        setState(await getAppState());
      } catch {
        // The status event normally keeps this synchronized. If refreshing
        // fails, preserve the latest event-driven state.
      }
    } finally {
      recordingActionRef.current = false;
      setRecordingAction(false);
    }
  }

  async function prepareModel(
    configuration?: Pick<
      Settings,
      | "asrBackend"
      | "modelId"
      | "modelQuantization"
      | "apiBaseUrl"
      | "apiKeyEnvVar"
    >,
  ) {
    if (modelLoading) return;
    setModelLoading(true);
    showNotice(t("Saving ASR settings and preparing the selected model..."));
    try {
      const next = configuration ? { ...settings, ...configuration } : settings;
      const saved = configuration ? await updateSettings(next) : next;
      setSettings(saved);
      setModel(await loadModel(saved.modelId, saved.modelQuantization));
      showNotice(t("Model loaded and ready."), "success");
    } catch (error) {
      showNotice(String(error), "error");
    } finally {
      setModelLoading(false);
    }
  }

  async function saveCustomModel(customModel: CustomModel): Promise<boolean> {
    const customModels = [
      ...settings.customModels.filter(
        (saved) =>
          saved.asrBackend !== customModel.asrBackend || saved.modelId !== customModel.modelId,
      ),
      customModel,
    ];
    const next = {
      ...settings,
      asrBackend: customModel.asrBackend,
      modelId: customModel.modelId,
      customModels,
    };
    try {
      const saved = await updateSettings(next);
      setSettings(saved);
      showNotice(t("Custom model saved locally."), "success");
      return true;
    } catch (error) {
      showNotice(String(error), "error");
      return false;
    }
  }

  async function addDictionary(entry: DictionaryEntryInput): Promise<boolean> {
    try {
      await addDictionaryEntry(entry);
      setDictionary(await listDictionary());
      showNotice(t("Dictionary entry added."), "success");
      return true;
    } catch (error) {
      showNotice(String(error), "error");
      return false;
    }
  }

  async function removeDictionary(id: number) {
    try {
      await deleteDictionaryEntry(id);
      setDictionary(await listDictionary());
      showNotice(t("Dictionary entry removed."), "success");
    } catch (error) {
      showNotice(String(error), "error");
    }
  }

  async function removeHistory(id: number) {
    try { await deleteHistoryItem(id); await refreshHistory(); }
    catch (error) { showNotice(String(error), "error"); }
  }

  async function clearHistory() {
    try { await deleteAllHistory(); historyRequestRef.current += 1; setHistory([]); showNotice(t("History deleted."), "success"); }
    catch (error) { showNotice(String(error), "error"); }
  }

  async function retryHistory(id: number) {
    try {
      await retryHistoryItem(id);
      await refreshHistory();
      showNotice(t("History retry completed."), "success");
    } catch (error) { showNotice(String(error), "error"); }
  }

  return (
    <div className="app-shell">
      <aside>
        <div className="brand">
          <span className="brand-mark">LV</span>
          <div>
            <strong>{t("Local Voice")}</strong>
            <small>{t("Windows input")}</small>
          </div>
        </div>
        <nav>
          {pages.map((item) => (
            <button
              key={item.id}
              className={page === item.id ? "active" : ""}
              onClick={() => setPage(item.id)}
            >
              {t(item.label)}
            </button>
          ))}
        </nav>
        <div className="local-badge">
          <span /> {t("Local processing default")}
        </div>
      </aside>

      <main>
        <header>
          <div>
            <p className="eyebrow">{t("LOCAL AI VOICE INPUT")}</p>
            <h1>{t(pages.find((item) => item.id === page)?.label ?? "Status")}</h1>
          </div>
          <div className={`status-pill ${state.phase}`}>
            <span />
            {statusLabel}
          </div>
        </header>

        {page === "dashboard" && (
          <DashboardPage
            state={localizedState}
            settings={settings}
            history={history}
            model={model}
            level={level}
            statusLabel={statusLabel}
            recordingAction={recordingAction}
            onToggleRecording={() => void toggleRecording()}
            onCancelRecording={() => void cancelRecording()}
            onCopyResult={(text) => void copyHistoryText(text)}
          />
        )}

        {page === "setup" && (
          <SetupPage
            settings={settings}
            devices={devices}
            gpu={gpu}
            gpuChecking={gpuChecking}
            onSettingsChange={setSettings}
            onConfigureModel={() => setPage("models")}
            onDiagnoseGpu={() => void diagnoseGpu()}
            onFinish={() => {
              void saveSettings({ setupComplete: true });
              setPage("dashboard");
            }}
          />
        )}

        {page === "settings" && (
          <SettingsPage
            settings={settings}
            onSave={(patch) => void saveSettings(patch)}
          />
        )}

        {page === "models" && (
          <ModelsPage
            gpu={gpu}
            settings={settings}
            asrBackendOptions={asrBackendOptions}
            modelLoading={modelLoading}
            onConfigureModel={(configuration) => void prepareModel(configuration)}
            onSaveCustomModel={saveCustomModel}
            onDiagnoseGpu={() => void diagnoseGpu()}
          />
        )}

        {page === "history" && (
          <HistoryPage
            settings={settings}
            history={history}
            filter={historyFilter}
            onSave={(patch) => void saveSettings(patch)}
            onCopyItem={(text) => void copyHistoryText(text)}
            onFilter={(filter) => {
              setHistoryFilter(filter);
              historyFilterRef.current = filter;
              void refreshHistory(filter).catch((error) => showNotice(String(error), "error"));
            }}
            onRetry={(id) => void retryHistory(id)}
            onDelete={(id) => void removeHistory(id)}
            onDeleteAll={() => void clearHistory()}
            onLoadAudio={getHistoryAudio}
            retryActive={state.phase === "processing" && state.message === "Retrying the saved recording."}
            onCancelRetry={() => void cancelRecording()}
          />
        )}

        {page === "dictionary" && (
          <DictionaryPage
            entries={dictionary}
            onAdd={addDictionary}
            onDelete={(id) => void removeDictionary(id)}
          />
        )}

        {page === "privacy" && (
          <PrivacyPage settings={settings} onSave={(patch) => void saveSettings(patch)} />
        )}

        {page === "diagnostics" && (
          <DiagnosticsPage
            state={localizedState}
            gpu={gpu}
            statusLabel={statusLabel}
            onDiagnoseGpu={() => void diagnoseGpu()}
          />
        )}
      </main>

      {(notice || modelProgress) && (
        <div
          className={`notice ${notice?.severity ?? "info"}${preparingModel ? " loading" : ""}`}
          aria-live="polite"
        >
          {preparingModel && <span className="progress-ring" aria-hidden="true" />}
          <div className="notice-body">
            <button
              className="notice-message"
              onDoubleClick={() => void copyNotice()}
              title={t("Double-click to copy")}
            >
              {translateAppMessage(language, notice?.message ?? null) ??
                (modelProgress?.stage === "download"
                  ? t("Downloading the speech model files.")
                  : t("Loading the speech model."))}
            </button>
            {modelProgress && <ModelProgressBar progress={modelProgress} />}
          </div>
          {noticeCopied && <span className="notice-copied">{t("Copied")}</span>}
          <button
            className="notice-dismiss"
            onClick={() => {
              setNotice(null);
              setModelProgress(null);
            }}
            aria-label={t("Dismiss notification")}
          >
            ×
          </button>
        </div>
      )}
    </div>
  );
}
