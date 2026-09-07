import { useEffect, useMemo, useRef, useState } from "react";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWebviewWindow } from "@tauri-apps/api/webviewWindow";
import {
  addDictionaryEntry,
  cancelRecording,
  copyToClipboard,
  defaultSettings,
  deleteDictionaryEntry,
  getAppState,
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

const asrBackendOptions: Array<{ value: AsrBackend; label: string }> = [
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

const pages: Array<{ id: Page; label: string }> = [
  { id: "dashboard", label: "Status" },
  { id: "setup", label: "Setup" },
  { id: "models", label: "Models" },
  { id: "settings", label: "Settings" },
  { id: "history", label: "History" },
  { id: "dictionary", label: "Dictionary" },
  { id: "privacy", label: "Privacy" },
  { id: "diagnostics", label: "Diagnostics" },
];

const isRecordingOverlay = getCurrentWebviewWindow().label === "recording-overlay";
const OVERLAY_WAVE_BAR_COUNT = 9;
const OVERLAY_PREVIEW_CHARS = 140;

interface CorrectionPreview {
  text: string;
  stage: "draft" | "streaming" | "final" | "fallback";
}

interface Notice {
  message: string;
  severity: "info" | "error";
  // Kind of the backend status event, when the notice came from one.
  kind?: string;
}

// Notices that only describe model preparation and are cleared once it ends.
const MODEL_PREPARATION_KINDS = ["model_loading", "model_downloading"];

function compactOverlayPreview(text: string): string {
  const characters = Array.from(text.trim());
  return characters.length > OVERLAY_PREVIEW_CHARS
    ? `…${characters.slice(-OVERLAY_PREVIEW_CHARS).join("")}`
    : characters.join("");
}

if (isRecordingOverlay) {
  document.body.classList.add("recording-overlay-body");
}

function RecordingOverlay() {
  const [waveform, setWaveform] = useState<number[]>(
    () => Array(OVERLAY_WAVE_BAR_COUNT).fill(0),
  );
  const [phase, setPhase] = useState<AppState["phase"]>("idle");
  const [message, setMessage] = useState<string | null>(null);
  const [preview, setPreview] = useState<CorrectionPreview | null>(null);

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
        setPhase(payload.phase);
        setMessage(payload.message);
        if (payload.phase === "recording") {
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
    ]);

    return () => {
      void listeners.then((unlisten) => unlisten.forEach((fn) => fn()));
    };
  }, []);

  if (phase === "recording") {
    return (
      <div className="recording-overlay recording" role="status" aria-label="Recording in progress">
        <span className="recording-live-dot" aria-hidden="true" />
        <span className="recording-overlay-label">Listening</span>
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
      </div>
    );
  }

  const compactPreview = compactOverlayPreview(preview?.text ?? "");
  const label =
    phase === "injecting"
      ? "Inserting"
      : preview?.stage === "draft"
        ? "Transcript ready"
        : preview?.stage === "streaming"
          ? "AI correcting"
          : preview?.stage === "fallback"
            ? "Using transcript"
            : "Processing";

  return (
    <div className="recording-overlay processing" role="status" aria-live="polite">
      <span className="processing-spinner" aria-hidden="true" />
      <div className="processing-copy">
        <span className="recording-overlay-label">{label}</span>
        <span className={`correction-preview${compactPreview ? "" : " pending"}`}>
          {compactPreview || message || "Preparing text…"}
        </span>
      </div>
    </div>
  );
}

export default function App() {
  return isRecordingOverlay ? <RecordingOverlay /> : <MainApp />;
}

function MainApp() {
  const [page, setPage] = useState<Page>("dashboard");
  const [state, setState] = useState<AppState>({
    phase: "idle",
    message: null,
    lastResult: null,
    updatedAt: new Date().toISOString(),
  });
  const [settings, setSettings] = useState<Settings>(defaultSettings);
  const [history, setHistory] = useState<HistoryItem[]>([]);
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
          setHistory(nextHistory);
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
      listen<{ kind: string; message: string }>("status", (event) =>
        setNotice({ message: event.payload.message, severity: "info", kind: event.payload.kind }),
      ),
    ]);
    return () => {
      void listeners.then((unlisten) => unlisten.forEach((fn) => fn()));
    };
  }, []);

  useEffect(() => {
    setNoticeCopied(false);
  }, [notice]);

  const statusLabel = useMemo(
    () => state.phase.charAt(0).toUpperCase() + state.phase.slice(1),
    [state.phase],
  );
  // A load started from the Models page or automatically at startup.
  const preparingModel = modelLoading || model?.state === "loading";

  async function copyNotice() {
    if (!notice) return;
    try {
      await copyToClipboard(notice.message);
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

  async function saveSettings(patch: Partial<Settings>) {
    const previous = settings;
    const next = { ...settings, ...patch };
    setSettings(next);
    try {
      setSettings(await updateSettings(next));
      showNotice("Settings saved locally.");
    } catch (error) {
      setSettings(previous);
      showNotice(String(error), "error");
    }
  }

  async function diagnoseGpu() {
    if (gpuChecking) return;
    setGpuChecking(true);
    showNotice("Running local GPU diagnostics...");
    try {
      setGpu(await runGpuDiagnostics());
      showNotice("Diagnostics complete.");
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
        setHistory(await listHistory());
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
    showNotice("Saving ASR settings and preparing the selected model...");
    try {
      const next = configuration ? { ...settings, ...configuration } : settings;
      const saved = configuration ? await updateSettings(next) : next;
      setSettings(saved);
      setModel(await loadModel(saved.modelId, saved.modelQuantization));
      showNotice("Model loaded and ready.");
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
      showNotice("Custom model saved locally.");
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
      showNotice("Dictionary entry added.");
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
      showNotice("Dictionary entry removed.");
    } catch (error) {
      showNotice(String(error), "error");
    }
  }

  return (
    <div className="app-shell">
      <aside>
        <div className="brand">
          <span className="brand-mark">LV</span>
          <div>
            <strong>Local Voice</strong>
            <small>Windows input</small>
          </div>
        </div>
        <nav>
          {pages.map((item) => (
            <button
              key={item.id}
              className={page === item.id ? "active" : ""}
              onClick={() => setPage(item.id)}
            >
              {item.label}
            </button>
          ))}
        </nav>
        <div className="local-badge">
          <span /> Local processing default
        </div>
      </aside>

      <main>
        <header>
          <div>
            <p className="eyebrow">LOCAL AI VOICE INPUT</p>
            <h1>{pages.find((item) => item.id === page)?.label}</h1>
          </div>
          <div className={`status-pill ${state.phase}`}>
            <span />
            {statusLabel}
          </div>
        </header>

        {page === "dashboard" && (
          <DashboardPage
            state={state}
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
            onSave={(patch) => void saveSettings(patch)}
            onCopyItem={(text) => void copyHistoryText(text)}
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
            state={state}
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
              title="Double-click to copy"
            >
              {notice?.message ??
                (modelProgress?.stage === "download"
                  ? "Downloading the speech model files."
                  : "Loading the speech model.")}
            </button>
            {modelProgress && <ModelProgressBar progress={modelProgress} />}
          </div>
          {noticeCopied && <span className="notice-copied">Copied</span>}
          <button
            className="notice-dismiss"
            onClick={() => {
              setNotice(null);
              setModelProgress(null);
            }}
            aria-label="Dismiss notification"
          >
            ×
          </button>
        </div>
      )}
    </div>
  );
}
