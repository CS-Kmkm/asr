import { useEffect, useMemo, useRef, useState } from "react";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWebviewWindow } from "@tauri-apps/api/webviewWindow";
import {
  addDictionaryEntry,
  cancelRecording,
  copyHistoryItem,
  defaultSettings,
  deleteDictionaryEntry,
  getAppState,
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
  ModelStatus,
  Settings,
} from "./types";
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

if (isRecordingOverlay) {
  document.body.classList.add("recording-overlay-body");
}

function RecordingOverlay() {
  return (
    <div className="recording-overlay" role="status" aria-label="Recording in progress">
      <span className="recording-live-dot" aria-hidden="true" />
      <span className="recording-overlay-label">Listening</span>
      <span className="recording-wave" aria-hidden="true">
        <i />
        <i />
        <i />
        <i />
        <i />
      </span>
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
  const [recordingAction, setRecordingAction] = useState(false);
  const recordingActionRef = useRef(false);
  const [gpu, setGpu] = useState<GpuDiagnostics | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [devices, setDevices] = useState<AudioDevice[]>([]);
  const [level, setLevel] = useState<AudioLevel>({ rms: 0, peak: 0 });
  const [dictionary, setDictionary] = useState<DictionaryEntry[]>([]);

  useEffect(() => {
    Promise.all([
      getAppState(),
      getSettings(),
      listHistory(),
      getModelStatus(),
      listAudioDevices(),
      listDictionary(),
    ])
      .then(([nextState, nextSettings, nextHistory, nextModel, nextDevices, nextDictionary]) => {
        setState(nextState);
        setSettings(nextSettings);
        setHistory(nextHistory);
        setModel(nextModel);
        setDevices(nextDevices);
        setDictionary(nextDictionary);
        if (!nextSettings.setupComplete) setPage("setup");
      })
      .catch((error: unknown) => setNotice(String(error)));
    const listeners = Promise.all([
      listen<AppState>("app-state", (event) => setState(event.payload)),
      listen<AudioLevel>("audio-level", (event) => setLevel(event.payload)),
      listen<ModelStatus>("model-status", (event) => setModel(event.payload)),
      listen<{ message: string }>("status", (event) => setNotice(event.payload.message)),
    ]);
    return () => {
      void listeners.then((unlisten) => unlisten.forEach((fn) => fn()));
    };
  }, []);

  const statusLabel = useMemo(
    () => state.phase.charAt(0).toUpperCase() + state.phase.slice(1),
    [state.phase],
  );

  async function saveSettings(patch: Partial<Settings>) {
    const next = { ...settings, ...patch };
    setSettings(next);
    try {
      setSettings(await updateSettings(next));
      setNotice("Settings saved locally.");
    } catch (error) {
      setNotice(String(error));
    }
  }

  async function diagnoseGpu() {
    setNotice("Running local GPU diagnostics...");
    try {
      setGpu(await runGpuDiagnostics());
      setNotice("Diagnostics complete.");
    } catch (error) {
      setNotice(String(error));
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
      setNotice(String(error));
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
    setNotice("Saving ASR settings and preparing the selected model...");
    try {
      const next = configuration ? { ...settings, ...configuration } : settings;
      const saved = configuration ? await updateSettings(next) : next;
      setSettings(saved);
      setModel(await loadModel(saved.modelId, saved.modelQuantization));
      setNotice("Model loaded and ready.");
    } catch (error) {
      setNotice(String(error));
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
      setNotice("Custom model saved locally.");
      return true;
    } catch (error) {
      setNotice(String(error));
      return false;
    }
  }

  async function addDictionary(entry: DictionaryEntryInput): Promise<boolean> {
    try {
      await addDictionaryEntry(entry);
      setDictionary(await listDictionary());
      setNotice("Dictionary entry added.");
      return true;
    } catch (error) {
      setNotice(String(error));
      return false;
    }
  }

  async function removeDictionary(id: number) {
    try {
      await deleteDictionaryEntry(id);
      setDictionary(await listDictionary());
      setNotice("Dictionary entry removed.");
    } catch (error) {
      setNotice(String(error));
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
          />
        )}

        {page === "setup" && (
          <SetupPage
            settings={settings}
            devices={devices}
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
            onCopyItem={(id) => void copyHistoryItem(id)}
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

      {notice && (
        <button
          className={`notice${modelLoading ? " loading" : ""}`}
          onClick={() => setNotice(null)}
          aria-live="polite"
          aria-label="Dismiss notification"
        >
          {modelLoading && <span className="progress-ring" aria-hidden="true" />}
          {notice}
        </button>
      )}
    </div>
  );
}
