import { useEffect, useMemo, useState } from "react";
import { listen } from "@tauri-apps/api/event";
import {
  cancelRecording,
  copyHistoryItem,
  defaultSettings,
  getAppState,
  getModelStatus,
  getSettings,
  listAudioDevices,
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
  GpuDiagnostics,
  HistoryItem,
  ModelStatus,
  Settings,
} from "./types";

const asrBackendOptions: Array<{ value: AsrBackend; label: string }> = [
  { value: "vibevoice", label: "VibeVoice (GPU)" },
  { value: "faster-whisper", label: "faster-whisper (CPU対応)" },
  { value: "mock", label: "mock (開発用)" },
];

type Page = "setup" | "dashboard" | "settings" | "models" | "history" | "privacy" | "diagnostics";

const pages: Array<{ id: Page; label: string }> = [
  { id: "dashboard", label: "Status" },
  { id: "setup", label: "Setup" },
  { id: "settings", label: "Settings" },
  { id: "models", label: "Model & GPU" },
  { id: "history", label: "History" },
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

  useEffect(() => {
    Promise.all([getAppState(), getSettings(), listHistory(), getModelStatus(), listAudioDevices()])
      .then(([nextState, nextSettings, nextHistory, nextModel, nextDevices]) => {
        setState(nextState);
        setSettings(nextSettings);
        setHistory(nextHistory);
        setModel(nextModel);
        setDevices(nextDevices);
        if (!nextSettings.setupComplete) setPage("setup");
      })
      .catch((error: unknown) => setNotice(String(error)));
    const listeners = Promise.all([
      listen<AppState>("app-state", (event) => setState(event.payload)),
      listen<AudioLevel>("audio-level", (event) => setLevel(event.payload)),
      listen<{ message: string }>("status", (event) => setNotice(event.payload.message)),
    ]);
    return () => { void listeners.then((unlisten) => unlisten.forEach((fn) => fn())); };
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

  return (
    <div className="app-shell">
      <aside>
        <div className="brand">
          <span className="brand-mark">LV</span>
          <div><strong>Local Voice</strong><small>Windows input</small></div>
        </div>
        <nav>
          {pages.map((item) => (
            <button key={item.id} className={page === item.id ? "active" : ""} onClick={() => setPage(item.id)}>
              {item.label}
            </button>
          ))}
        </nav>
        <div className="local-badge"><span /> Local processing default</div>
      </aside>

      <main>
        <header>
          <div>
            <p className="eyebrow">LOCAL AI VOICE INPUT</p>
            <h1>{pages.find((item) => item.id === page)?.label}</h1>
          </div>
          <div className={`status-pill ${state.phase}`}><span />{statusLabel}</div>
        </header>

        {notice && <button className="notice" onClick={() => setNotice(null)}>{notice}</button>}

        {page === "dashboard" && (
          <section className="grid">
            <article className="hero-card span-2">
              <div>
                <p className="eyebrow">RECORDING STATUS</p>
                <h2>{state.phase === "idle" ? "Ready when you are" : statusLabel}</h2>
                <p>{state.message ?? `Use ${settings.hotkey} to start or stop dictation.`}</p>
                <button className="primary" onClick={() => void toggleRecording()}>
                  {state.phase === "recording" ? "Stop and transcribe" : "Start recording"}
                </button>
                {state.phase === "recording" && <button className="secondary" onClick={() => void cancelRecording()}>Cancel</button>}
              </div>
              <div className="wave" aria-label={`Input level ${Math.round(level.peak * 100)} percent`}>{[.4, .7, 1, .6, .9, .5, .8, .55, .35].map((scale, i) => <i key={i} style={{ height: Math.max(8, level.peak * 100 * scale) }} />)}</div>
            </article>
            <InfoCard label="HOTKEY" value={settings.hotkey} detail="Global toggle shortcut" />
            <InfoCard label="MODEL" value={model?.installed ? model.modelId ?? "Ready" : "Not installed"} detail={model?.detail ?? "Checking local cache"} />
            <InfoCard label="PRIVACY" value={settings.cloudEnabled ? "Cloud enabled" : "Local only"} detail={settings.historyEnabled ? "History stored locally" : "History disabled"} />
            <InfoCard label="RECENT ITEMS" value={String(history.length)} detail="No transcript content is logged" />
          </section>
        )}

        {page === "setup" && (
          <section className="panel">
            <p className="eyebrow">FIRST RUN</p>
            <h2>Configure local dictation</h2>
            <p className="lead">Voice data stays on this device. Model downloads and cloud services require an explicit action.</p>
            <div className="steps">
              <SettingRow title="Microphone" detail="Captured only while recording." control={<select value={settings.microphoneId ?? ""} onChange={(e) => setSettings({ ...settings, microphoneId: e.target.value || null })}><option value="">System default</option>{devices.map((device) => <option key={device.id} value={device.id}>{device.name}{device.isDefault ? " (default)" : ""}</option>)}</select>} />
              <SettingRow title="Global hotkey" detail="Default recording toggle." control={<input value={settings.hotkey} onChange={(e) => setSettings({ ...settings, hotkey: e.target.value })} />} />
              <SettingRow title="Local ASR model" detail="VibeVoice downloads from Hugging Face on first load." control={<button className="secondary" onClick={() => void prepareModel()}>Load VibeVoice</button>} />
              <SettingRow title="GPU check" detail="Reads hardware metadata only; no transcript or audio is involved." control={<button className="secondary" onClick={diagnoseGpu}>Run check</button>} />
            </div>
            <button className="primary" onClick={() => { void saveSettings({ setupComplete: true }); setPage("dashboard"); }}>Finish setup</button>
          </section>
        )}

        {page === "settings" && (
          <section className="panel">
            <h2>Input settings</h2>
            <SettingRow title="Recording hotkey" detail="The default is Ctrl+Shift+Space." control={<input value={settings.hotkey} onChange={(e) => void saveSettings({ hotkey: e.target.value })} />} />
            <SettingRow title="ASR backend" detail="Changing this restarts the local ASR worker; reload the model afterward." control={<select value={settings.asrBackend} onChange={(e) => void saveSettings({ asrBackend: e.target.value as AsrBackend })}>{asrBackendOptions.map((option) => <option key={option.value} value={option.value}>{option.label}</option>)}</select>} />
            <SettingRow title="Start with Windows" detail="Autostart wiring will be enabled by the platform integration." control={<Toggle checked={settings.autoStart} onChange={(value) => void saveSettings({ autoStart: value })} />} />
            <SettingRow title="Restore clipboard" detail="Restore previous clipboard contents after successful paste." control={<Toggle checked={settings.clipboardRestore} onChange={(value) => void saveSettings({ clipboardRestore: value })} />} />
          </section>
        )}

        {page === "models" && (
          <section className="grid">
            <article className="panel span-2"><p className="eyebrow">ASR MODEL</p><h2>{model?.modelId ?? "No model selected"}</h2><p>{model?.detail}</p><span className="tag">{model?.state.replace("_", " ")}</span><br /><button className="secondary" onClick={() => void prepareModel()}>Load 4-bit model</button></article>
            <article className="panel span-2"><p className="eyebrow">GPU</p><h2>{gpu?.adapterName ?? "Not checked"}</h2><p>{gpu?.recommendation ?? "Run diagnostics to check NVIDIA availability and VRAM."}</p>{gpu?.memoryTotalMb && <strong>{gpu.memoryTotalMb} MB VRAM</strong>}<br /><button className="secondary" onClick={diagnoseGpu}>Run diagnostics</button></article>
          </section>
        )}

        {page === "history" && (
          <section className="panel">
            <div className="section-heading"><div><h2>Dictation history</h2><p>Stored only when history is enabled.</p></div><Toggle checked={settings.historyEnabled} onChange={(value) => void saveSettings({ historyEnabled: value })} /></div>
            {!settings.historyEnabled ? <Empty title="History is disabled" detail="New transcripts will not be written to SQLite." /> :
              history.length === 0 ? <Empty title="No dictations yet" detail="Completed local dictations will appear here." /> :
              <div className="history-list">{history.map((item) => <button key={item.id} onClick={() => void copyHistoryItem(item.id)}><span>{item.processedText ?? item.transcriptText}</span><small>{new Date(item.createdAt).toLocaleString()} · Copy</small></button>)}</div>}
          </section>
        )}

        {page === "privacy" && (
          <section className="panel">
            <h2>Privacy controls</h2>
            <SettingRow title="Save text history" detail="When disabled, transcript and processed text are never inserted into dictation_history." control={<Toggle checked={settings.historyEnabled} onChange={(value) => void saveSettings({ historyEnabled: value })} />} />
            <SettingRow title="Delete audio after processing" detail="Audio cleanup is enabled by default." control={<Toggle checked={settings.deleteAudioAfterProcessing} onChange={(value) => void saveSettings({ deleteAudioAfterProcessing: value })} />} />
            <SettingRow title="Cloud services" detail="Off by default. Enabling this alone does not configure a provider or send data." control={<Toggle checked={settings.cloudEnabled} onChange={(value) => void saveSettings({ cloudEnabled: value })} />} />
            <SettingRow title="History retention" detail="Text history cleanup window." control={<select value={settings.historyRetentionDays} onChange={(e) => void saveSettings({ historyRetentionDays: Number(e.target.value) })}><option value={7}>7 days</option><option value={30}>30 days</option><option value={90}>90 days</option></select>} />
          </section>
        )}

        {page === "diagnostics" && (
          <section className="panel">
            <h2>Local diagnostics</h2>
            <p className="lead">Diagnostics report status codes and hardware metadata only. Audio, transcripts, clipboard contents, window titles, and API keys are excluded.</p>
            <div className="diagnostic-grid">
              <InfoCard label="DATABASE" value="Connected" detail="SQLite migrations applied" />
              <InfoCard label="APP STATE" value={statusLabel} detail={`Updated ${new Date(state.updatedAt).toLocaleTimeString()}`} />
              <InfoCard label="GPU" value={gpu?.status ?? "Not checked"} detail={gpu?.adapterName ?? "Run the hardware probe"} />
            </div>
            <button className="secondary" onClick={diagnoseGpu}>Run GPU probe</button>
          </section>
        )}
      </main>
    </div>
  );
}

function InfoCard({ label, value, detail }: { label: string; value: string; detail: string }) {
  return <article className="info-card"><p className="eyebrow">{label}</p><h3>{value}</h3><p>{detail}</p></article>;
}

function SettingRow({ title, detail, control }: { title: string; detail: string; control: React.ReactNode }) {
  return <div className="setting-row"><div><strong>{title}</strong><p>{detail}</p></div>{control}</div>;
}

function Toggle({ checked, onChange }: { checked: boolean; onChange: (value: boolean) => void }) {
  return <button className={`toggle ${checked ? "on" : ""}`} onClick={() => onChange(!checked)} aria-pressed={checked}><span /></button>;
}

function Empty({ title, detail }: { title: string; detail: string }) {
  return <div className="empty"><div className="empty-icon">•••</div><h3>{title}</h3><p>{detail}</p></div>;
}
