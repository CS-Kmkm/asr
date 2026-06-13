import { useEffect, useMemo, useState } from "react";
import { listen } from "@tauri-apps/api/event";
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
  { value: "vibevoice", label: "VibeVoice (GPU)" },
  { value: "faster-whisper", label: "faster-whisper (CPU対応)" },
  { value: "mock", label: "mock (開発用)" },
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
  { id: "settings", label: "Settings" },
  { id: "models", label: "Model & GPU" },
  { id: "history", label: "History" },
  { id: "dictionary", label: "Dictionary" },
  { id: "privacy", label: "Privacy" },
  { id: "diagnostics", label: "Diagnostics" },
];

export default function App() {
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
    try {
      if (state.phase === "recording") {
        await stopRecording();
        setHistory(await listHistory());
      } else {
        await startRecording();
      }
    } catch (error) {
      setNotice(String(error));
    }
  }

  async function prepareModel() {
    setNotice("Loading model. The first run may download several files.");
    try {
      setModel(await loadModel());
      setNotice("Model loaded and ready.");
    } catch (error) {
      setNotice(String(error));
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

        {notice && (
          <button className="notice" onClick={() => setNotice(null)}>
            {notice}
          </button>
        )}

        {page === "dashboard" && (
          <DashboardPage
            state={state}
            settings={settings}
            history={history}
            model={model}
            level={level}
            statusLabel={statusLabel}
            onToggleRecording={() => void toggleRecording()}
            onCancelRecording={() => void cancelRecording()}
          />
        )}

        {page === "setup" && (
          <SetupPage
            settings={settings}
            devices={devices}
            onSettingsChange={setSettings}
            onPrepareModel={() => void prepareModel()}
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
            asrBackendOptions={asrBackendOptions}
            onSave={(patch) => void saveSettings(patch)}
          />
        )}

        {page === "models" && (
          <ModelsPage
            model={model}
            gpu={gpu}
            onPrepareModel={() => void prepareModel()}
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
    </div>
  );
}
