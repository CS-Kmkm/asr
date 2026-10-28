import { AskAnswerPanel } from "./windows/AskAnswerPanel";
import { RecordingOverlay } from "./windows/RecordingOverlay";
import { useNoticeQueue, type Notice } from "./hooks/useNoticeQueue";
import { useSettings } from "./hooks/useSettings";
import { useAudioDevices } from "./hooks/useAudioDevices";
import { useEffect, useMemo, useRef, useState } from "react";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWebviewWindow } from "@tauri-apps/api/webviewWindow";
import {
  addDictionaryEntry,
  cancelRecording,
  copyToClipboard,
  defaultSettings,
  deleteDictionaryEntry,
  deleteHistoryItem,
  deleteAllHistory,
  getHistoryAudio,
  retryHistoryItem,
  reprocessHistoryText,
  confirmDictionaryCandidate,
  importDictionaryCsv,
  listDictionaryCandidates,
  rejectDictionaryCandidate,
  getAppState,
  getGpuDiagnostics,
  getModelStatus,
  getUiPreferences,
  getStartupHotkeyWarning,
  getShortcutWarning,
  listDictionary,
  listHistory,
  listHistoryPage,
  runGpuDiagnostics,
  openDiagnosticsFolder,
  loadModel,
  startRecording,
  stopRecording,
  updateDictionaryEntry,
} from "./api";
import type {
  AppState,
  AsrBackend,
  CustomModel,
  DictionaryEntry,
  DictionaryCandidate,
  DictionaryEntryInput,
  GpuDiagnostics,
  HistoryItem,
  HistoryFilter,
  ModelProgress,
  ModelStatus,
  Settings,
  SettingsSnapshot,
} from "./types";
import { IconButton, ModelProgressBar } from "./components/ui";
import { DashboardPage } from "./pages/DashboardPage";
import { SetupPage } from "./pages/SetupPage";
import { ApiKeySettings } from "./components/ApiKeySettings";
import { SettingsPage, type SettingsSection } from "./pages/SettingsPage";
import { Icon, type IconName } from "./components/Icon";
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
  { id: "dashboard", label: "Home" },
  { id: "setup", label: "Setup" },
  { id: "models", label: "Models" },
  { id: "settings", label: "Settings" },
  { id: "history", label: "History" },
  { id: "dictionary", label: "Dictionary" },
  { id: "privacy", label: "Privacy" },
  { id: "diagnostics", label: "Diagnostics" },
];

const primaryPages = pages.filter(item => ["dashboard", "history", "dictionary"].includes(item.id));
const pageIcons: Record<Page, IconName> = { dashboard: "home", history: "history", dictionary: "dictionary", settings: "settings", models: "model", privacy: "shield", setup: "setup", diagnostics: "diagnostics" };
const settingsPages: Array<{ id: Page; section?: SettingsSection; label: MessageKey; navLabel?: MessageKey; icon: IconName }> = [
  { id: "settings", section: "input", label: "Input settings", icon: "microphone" },
  { id: "settings", section: "shortcuts", label: "Voice mode shortcuts", navLabel: "Shortcuts", icon: "keyboard" },
  { id: "settings", section: "writing", label: "Writing and translation", icon: "writing" },
  { id: "models", label: "Models", icon: "model" },
  { id: "privacy", label: "Privacy", icon: "shield" },
  { id: "settings", section: "general", label: "General", icon: "appearance" },
];

const phaseMessageKeys: Record<AppState["phase"], MessageKey> = {
  idle: "Idle",
  recording: "Recording",
  processing: "Processing",
  injecting: "Injecting",
  completed: "Completed",
  error: "Error",
};

const windowLabel = "__TAURI_INTERNALS__" in window ? getCurrentWebviewWindow().label : "main";
const isRecordingOverlay = windowLabel === "recording-overlay";
const isAskAnswer = windowLabel === "ask-answer";
// Notices that only describe model preparation and are cleared once it ends.
const MODEL_PREPARATION_KINDS = ["model_loading", "model_downloading", "worker_setup"];

const WARNING_STATUS_KINDS = new Set([
  "decision_model_unavailable",
  "decision_model_unload_failed",
  "voice_mode_fallback",
  "artifact_cleanup_failed",
  "history_audio_unavailable",
  "history_audio_and_metric_save_failed",
  "history_save_failed",
  "autostart_update_failed",
  "clipboard_only",
  "gpu_unavailable",
  "history_and_metric_save_failed",
  "history_metric_save_failed",
  "hotkey_unavailable",
  "metric_save_failed",
  "microphone_fallback",
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
  if (kind === "error" || kind === "model_load_failed" || kind === "ai_provider_failed") return "error";
  if (WARNING_STATUS_KINDS.has(kind)) return "warning";
  if (SUCCESS_STATUS_KINDS.has(kind)) return "success";
  return "info";
}

if (isRecordingOverlay) {
  document.body.classList.add("recording-overlay-body");
}

export default function App() {
  const [language, setLanguage] = useState<Settings["uiLanguage"] | null>(null);
  const [theme, setTheme] = useState<Settings["theme"]>(defaultSettings.theme);

  useEffect(() => {
    // The load and settings-changed events can arrive in any order; only a
    // snapshot newer than the last one applied changes the language or theme.
    let revision = -1;
    const apply = (snapshot: { settings: Pick<Settings, "uiLanguage" | "theme">; revision: number }) => {
      if (snapshot.revision <= revision) return;
      revision = snapshot.revision;
      setLanguage(snapshot.settings.uiLanguage);
      setTheme(snapshot.settings.theme);
    };
    getUiPreferences()
      .then(apply)
      .catch(() => setLanguage(defaultSettings.uiLanguage));
    let unlisten: (() => void) | undefined;
    let active = true;
    void listen<SettingsSnapshot>("settings-changed", ({ payload }) => apply(payload)).then((stop) => {
      if (active) unlisten = stop;
      else stop();
    });
    return () => {
      active = false;
      unlisten?.();
    };
  }, []);

  useEffect(() => {
    const root = document.documentElement;
    const media = window.matchMedia("(prefers-color-scheme: dark)");
    const apply = () => {
      root.dataset.theme = theme;
      root.dataset.systemTheme = media.matches ? "dark" : "light";
    };
    apply();
    media.addEventListener("change", apply);
    return () => media.removeEventListener("change", apply);
  }, [theme]);

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
  const [settingsSection, setSettingsSection] = useState<SettingsSection>("input");
  const settingsArea = !primaryPages.some(item => item.id === page);
  const pageLabel = settingsPages.find(item => item.id === page && (!item.section || item.section === settingsSection))?.label
    ?? pages.find(item => item.id === page)?.label ?? "Home";
  // Set when Setup sends the user to Models, so they can come back to it.
  const [returnToSetup, setReturnToSetup] = useState(false);
  useEffect(() => {
    document.documentElement.scrollTop = 0;
    document.body.scrollTop = 0;
    if (page !== "models") setReturnToSetup(false);
  }, [page, settingsSection]);
  const [state, setState] = useState<AppState>({
    phase: "idle",
    stage: "idle",
    operation: null,
    operationId: null,
    message: null,
    messageCode: null,
    lastResult: null,
    updatedAt: new Date().toISOString(),
  });
  const [history, setHistory] = useState<HistoryItem[]>([]);
  const [historyFilter, setHistoryFilter] = useState<HistoryFilter>("all");
  const historyFilterRef = useRef<HistoryFilter>("all");
  const historyRequestRef = useRef(0);
  const [historyQuery, setHistoryQuery] = useState("");
  const historyQueryRef = useRef("");
  const historyCursorRef = useRef<Pick<HistoryItem, "createdAt" | "id"> | null>(null);
  const historyLoadingRef = useRef(false);
  const [historyLoading, setHistoryLoading] = useState(false);
  const [historyFailed, setHistoryFailed] = useState(false);
  const [historyHasMore, setHistoryHasMore] = useState(false);
  const [historyReprocessing, setHistoryReprocessing] = useState(false);
  const [historyTextResult, setHistoryTextResult] = useState<{ sourceId: number; text: string } | null>(null);
  const historyReprocessingRef = useRef(false);
  const historyTextRequestRef = useRef(0);
  const historyTextPendingSourceRef = useRef<number | null>(null);
  const [model, setModel] = useState<ModelStatus | null>(null);
  const modelPreparationActiveRef = useRef(false);
  const modelStatusRevisionRef = useRef(0);
  const [modelLoading, setModelLoading] = useState(false);
  const [modelProgress, setModelProgress] = useState<ModelProgress | null>(null);
  const [progressNoticeHidden, setProgressNoticeHidden] = useState(false);
  const [recordingAction, setRecordingAction] = useState(false);
  const recordingActionRef = useRef(false);
  const [gpu, setGpu] = useState<GpuDiagnostics | null>(null);
  const [gpuChecking, setGpuChecking] = useState(false);
  const { notice, push: pushNotice, dismiss: dismissNotice, clear: clearNotices, removeKinds: removeNoticeKinds } = useNoticeQueue();
  const noticeOperationRef = useRef(0);
  const [startupHotkeyWarnings, setStartupHotkeyWarnings] = useState<string[]>([]);
  const [inactiveShortcuts, setInactiveShortcuts] = useState<string[]>([]);
  const showNotice = (message: string, severity: Notice["severity"] = "info") =>
    pushNotice({ message, severity });
  const applyModelStatus = (next: ModelStatus) => {
    modelStatusRevisionRef.current += 1;
    modelPreparationActiveRef.current = next.state === "loading";
    setModel(next);
    setProgressNoticeHidden(false);
    if (next.state === "loading") {
      pushNotice({ kind: "model_loading", message: "Loading the speech model.", severity: "info" });
    } else {
      setModelProgress(null);
      removeNoticeKinds(MODEL_PREPARATION_KINDS);
    }
  };
  const { settings, settingsRef, settingsLoaded, editSettingsLocally, settingsReady, queueSettingsSave } = useSettings({
    onLanguageChange,
    onSetupNeeded: () => setPage("setup"),
    onError: (error) => showNotice(String(error), "error"),
    onNotReady: () => showNotice(t("Settings have not been loaded yet, so changes were not saved. Try again in a moment."), "warning"),
  });
  const [noticeCopied, setNoticeCopied] = useState(false);
  const { devices, refresh: refreshDevices } = useAudioDevices(page === "settings" || page === "setup", (error) => showNotice(String(error), "error"));
  useEffect(() => {
    let active = true;
    void getShortcutWarning().then((value) => { if (active) setInactiveShortcuts(value); }).catch(() => {});
    return () => { active = false; };
  }, [settings.shortcuts, settings.translationHotkey]);
  const [dictionary, setDictionary] = useState<DictionaryEntry[]>([]);
  const [dictionaryCandidates, setDictionaryCandidates] = useState<DictionaryCandidate[]>([]);
  const dictionaryRequestRef = useRef(0);

  useEffect(() => {
    historyFilterRef.current = historyFilter;
  }, [historyFilter]);

  useEffect(() => {
    let active = true;
    void getStartupHotkeyWarning().then(setStartupHotkeyWarnings).catch(() => {});
    const applyAppState = (next: AppState) => {
      const id = next.operationId;
      if (typeof id === "number") {
        if (id < noticeOperationRef.current) return;
        if (id > noticeOperationRef.current) clearNotices();
        noticeOperationRef.current = id;
      } else if (noticeOperationRef.current > 0) return;
      setState(next);
    };
    Promise.all([
      getAppState(),
      listHistory(),
      getGpuDiagnostics(),
      listDictionary(),
    ])
      .then(
        ([nextState, nextHistory, nextGpu, nextDictionary]) => {
          applyAppState(nextState);
          if (historyRequestRef.current === 0 && historyFilterRef.current === "all") {
            setHistory(nextHistory);
            const last = nextHistory.at(-1);
            historyCursorRef.current = last ? { createdAt: last.createdAt, id: last.id } : null;
            setHistoryHasMore(nextHistory.length === 100);
          }
          setGpu(nextGpu);
          setDictionary(nextDictionary);
        },
      )
      .catch((error: unknown) => showNotice(String(error), "error"));
    const modelListener = listen<ModelStatus>("model-status", (event) => {
      applyModelStatus(event.payload);
    });
    const listeners = [
      listen<AppState>("app-state", (event) => applyAppState(event.payload)),
      modelListener,
      listen<ModelProgress>("model-progress", (event) => {
        if (modelPreparationActiveRef.current) setModelProgress(event.payload);
      }),
      listen<GpuDiagnostics>("gpu-diagnostics", (event) => setGpu(event.payload)),
      listen<{ kind: string; code?: string; outcome?: string; message: string }>("status", (event) => {
        if (MODEL_PREPARATION_KINDS.includes(event.payload.kind) && !modelPreparationActiveRef.current) return;
        pushNotice({
          message: event.payload.message,
          code: event.payload.code,
          outcome: event.payload.outcome,
          severity: statusSeverity(event.payload.kind),
          kind: event.payload.kind,
        });
      }),
    ];
    const subscriptions = Promise.allSettled(listeners);
    void subscriptions.then((results) => {
      if (active) for (const result of results) {
        if (result.status === "rejected") showNotice(String(result.reason), "error");
      }
    });
    // Register first: otherwise ready can be lost between a loading snapshot
    // and the asynchronous event subscription becoming active.
    void modelListener.catch(() => {}).then(async () => {
      if (!active) return;
      const revision = modelStatusRevisionRef.current;
      const snapshot = await getModelStatus();
      if (active && modelStatusRevisionRef.current === revision) applyModelStatus(snapshot);
    }).catch((error: unknown) => { if (active) showNotice(String(error), "error"); });
    return () => {
      active = false;
      void subscriptions.then((results) => results.forEach((result) => {
        if (result.status === "fulfilled") result.value();
      }));
    };
  }, [onLanguageChange]);

  useEffect(() => {
    setNoticeCopied(false);
  }, [notice, startupHotkeyWarnings]);

  const statusLabel = t(phaseMessageKeys[state.phase]);
  const localizedState = useMemo(
    () => ({ ...state, message: translateAppMessage(language, state.message, state.messageCode) }),
    [language, state],
  );
  // A load started from the Models page or automatically at startup.
  const preparingModel = modelLoading || model?.state === "loading";
  // Each hotkey warning is a fixed backend sentence naming one action.
  const startupHotkeyWarning =
    startupHotkeyWarnings.length > 0
      ? startupHotkeyWarnings.map((message) => translateAppMessage(language, message)).join(" ")
      : null;
  const shownNotice: Notice | null = notice ?? (startupHotkeyWarning
    ? { message: startupHotkeyWarning, severity: "warning" }
    : null);
  const showingStartupWarning = !notice && Boolean(startupHotkeyWarning);
  const shownProgress = showingStartupWarning ? null : modelProgress;

  async function copyNotice() {
    if (!shownNotice) return;
    try {
      await copyToClipboard(translateAppMessage(language, shownNotice.message, shownNotice.code, shownNotice.outcome) ?? shownNotice.message);
      setNoticeCopied(true);
    } catch (error) {
      showNotice(String(error), "error");
      setNoticeCopied(false);
    }
  }

  async function copyHistoryText(text: string) {
    try {
      await copyToClipboard(text);
      showNotice(t("Copied"), "success");
    } catch (error) {
      showNotice(String(error), "error");
    }
  }

  async function refreshHistory(filter: HistoryFilter = historyFilterRef.current, append = false) {
    if (append && historyLoadingRef.current) return;
    const request = ++historyRequestRef.current;
    const query = historyQueryRef.current;
    const before = append ? historyCursorRef.current : null;
    historyLoadingRef.current = true;
    setHistoryLoading(true);
    setHistoryFailed(false);
    try {
      const { items, retainedIds } = await listHistoryPage(filter, { query, before, knownIds: append ? history.map((item) => item.id) : [] });
      if (request === historyRequestRef.current) {
        setHistory((previous) => {
          if (!append) return items;
          const retained = new Set(retainedIds);
          const kept = previous.filter((item) => retained.has(item.id));
          const existingIds = new Set(kept.map((item) => item.id));
          return [...kept, ...items.filter((item) => !existingIds.has(item.id))];
        });
        const last = items.at(-1);
        historyCursorRef.current = last ? { createdAt: last.createdAt, id: last.id } : null;
        setHistoryHasMore(items.length === 100);
      }
    } catch (error) {
      if (request === historyRequestRef.current) setHistoryFailed(true);
      throw error;
    } finally {
      if (request === historyRequestRef.current) {
        historyLoadingRef.current = false;
        setHistoryLoading(false);
      }
    }
  }

  async function refreshDictionary() {
    const request = ++dictionaryRequestRef.current;
    const [entries, candidates] = await Promise.all([listDictionary(), listDictionaryCandidates()]);
    if (request === dictionaryRequestRef.current) {
      setDictionary(entries);
      setDictionaryCandidates(candidates);
    }
  }

  useEffect(() => {
    // Shortcut-driven recordings finish through app-state events, without the
    // Dashboard stop handler. Read the retained data when either page opens
    // and after lifecycle or retention changes while it is visible.
    let active = true;
    if (page === "history") {
      void refreshHistory().catch((error) => { if (active) showNotice(String(error), "error"); });
    }
    if (page === "dictionary") {
      setDictionaryCandidates([]);
      void refreshDictionary().catch((error) => { if (active) showNotice(String(error), "error"); });
    }
    return () => {
      active = false;
      historyRequestRef.current += 1;
      dictionaryRequestRef.current += 1;
    };
  }, [page, state.phase, settings.historyRetention]);

  async function saveSettings(patch: Partial<Settings>): Promise<boolean> {
    if (!settingsReady()) return false;
    const outcome = await queueSettingsSave(patch);
    if (!outcome.saved) {
      showNotice(String(outcome.error), "error");
      return false;
    }
    const saved = outcome.settings;
    let hotkeyWarnings: string[] = [];
    try {
      hotkeyWarnings = await getStartupHotkeyWarning();
      setStartupHotkeyWarnings(hotkeyWarnings);
    } catch {
      // Keep the known warning if diagnostics cannot be refreshed.
    }
    // A saved hotkey that still cannot be registered does not fail the save,
    // but the notice names the action that stays unavailable.
    if (hotkeyWarnings.length > 0) showNotice(hotkeyWarnings.map((message) => translateAppMessage(saved.uiLanguage, message)).join(" "), "warning");
    if (patch.historyRetention !== undefined || patch.deleteAudioAfterProcessing !== undefined) {
      // The optimistic render can read before the settings transaction has
      // purged old rows. Refresh again after its acknowledgement so that read
      // cannot restore candidates removed by a shorter retention window.
      try {
        setDictionaryCandidates([]);
        await Promise.all([refreshHistory(), refreshDictionary()]);
      }
      catch (error) { showNotice(String(error), "error"); }
    }
    return true;
  }

  async function openLogs() {
    try { await openDiagnosticsFolder(); }
    catch { showNotice(t("The diagnostics folder could not be opened."), "error"); }
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
        try {
          await refreshHistory();
        } catch {
          showNotice(t("Recording completed, but history could not be refreshed."), "warning");
        }
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
      | "asrDevice"
      | "modelId"
      | "modelQuantization"
      | "apiBaseUrl"
      | "apiKeyEnvVar"
    >,
  ) {
    if (modelLoading || !settingsReady()) return;
    setModelLoading(true);
    showNotice(t("Saving ASR settings and preparing the selected model..."));
    try {
      // Flush pending Setup edits as well as explicit model configuration.
      const outcome = await queueSettingsSave(configuration ?? {});
      if (!outcome.saved) throw outcome.error;
      const saved = outcome.settings;
      applyModelStatus(await loadModel(saved.modelId, saved.modelQuantization));
      showNotice(t("Model loaded and ready."), "success");
    } catch (error) {
      showNotice(String(error), "error");
    } finally {
      setModelLoading(false);
    }
  }

  async function saveCustomModel(customModel: CustomModel): Promise<boolean> {
    if (!settingsReady()) return false;
    const customModels = [
      ...settingsRef.current.customModels.filter(
        (saved) =>
          saved.asrBackend !== customModel.asrBackend || saved.modelId !== customModel.modelId,
      ),
      customModel,
    ];
    const outcome = await queueSettingsSave({
      asrBackend: customModel.asrBackend,
      modelId: customModel.modelId,
      customModels,
    });
    if (!outcome.saved) {
      showNotice(String(outcome.error), "error");
      return false;
    }
    showNotice(t("Custom model saved locally."), "success");
    return true;
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
    try {
      await deleteHistoryItem(id);
      if (historyTextPendingSourceRef.current === id) historyTextRequestRef.current += 1;
      setHistoryTextResult(null);
      await refreshHistory();
    }
    catch (error) { showNotice(String(error), "error"); }
  }

  async function clearHistory() {
    try {
      await deleteAllHistory();
      historyRequestRef.current += 1;
      setHistory([]);
      historyCursorRef.current = null;
      setHistoryHasMore(false);
      historyLoadingRef.current = false;
      setHistoryLoading(false);
      setHistoryFailed(false);
      setHistoryTextResult(null);
      historyTextRequestRef.current += 1;
      showNotice(t("History deleted."), "success");
    }
    catch (error) { showNotice(String(error), "error"); }
  }

  async function reportHistoryAudioFailure() {
    showNotice(t("The recording could not be loaded. History was refreshed."), "error");
    try { await refreshHistory(); }
    catch { /* The notice already reports the unavailable recording. */ }
  }

  async function retryHistory(id: number) {
    try {
      await retryHistoryItem(id);
      await refreshHistory();
      showNotice(t("History retry completed."), "success");
    } catch (error) { showNotice(String(error), "error"); }
  }

  async function reprocessHistory(id: number, action: import("./types").HistoryTextAction) {
    if (historyReprocessingRef.current) return;
    historyReprocessingRef.current = true;
    const request = ++historyTextRequestRef.current;
    historyTextPendingSourceRef.current = id;
    setHistoryReprocessing(true);
    setHistoryTextResult(null);
    try {
      const result = await reprocessHistoryText(id, action);
      if (request !== historyTextRequestRef.current) return;
      setHistoryTextResult({ sourceId: id, text: result.text });
      await refreshHistory();
      if (request !== historyTextRequestRef.current) return;
      showNotice(t("History retry completed."), "success");
    } catch (error) { if (request === historyTextRequestRef.current) showNotice(String(error), "error"); }
    finally { historyReprocessingRef.current = false; historyTextPendingSourceRef.current = null; setHistoryReprocessing(false); }
  }

  async function updateDictionary(id: number, entry: DictionaryEntryInput): Promise<boolean> {
    try {
      await updateDictionaryEntry(id, entry);
      setDictionary(await listDictionary());
      showNotice(t("Dictionary entry updated."), "success");
      return true;
    } catch (error) {
      showNotice(String(error), "error");
      return false;
    }
  }

  async function importDictionary(csv: string): Promise<boolean> {
    try {
      const count = await importDictionaryCsv(csv);
      setDictionary(await listDictionary());
      showNotice(`${t("Imported dictionary entries.")}: ${count}`, "success");
      return true;
    } catch (error) {
      showNotice(String(error), "error");
      return false;
    }
  }

  async function confirmCandidate(id: number) {
    try {
      await confirmDictionaryCandidate(id);
      await refreshDictionary();
      showNotice(t("Dictionary candidate confirmed."), "success");
    } catch (error) { showNotice(String(error), "error"); }
  }

  async function rejectCandidate(id: number) {
    try {
      await rejectDictionaryCandidate(id);
      await refreshDictionary();
    } catch (error) { showNotice(String(error), "error"); }
  }

  return (
    <div className="app-shell">
      <aside>
        <div className="brand">
          <span className="brand-mark">LV</span>
          <div className="sr-only">
            <strong>{t("Local Voice")}</strong>
            <small>{t("Windows input")}</small>
          </div>
        </div>
        <nav aria-label={t("Main navigation")}>
          {primaryPages.map(item => (
            <button key={item.id} aria-label={t(item.label)} title={t(item.label)} className={page === item.id ? "active" : ""} aria-current={page === item.id ? "page" : undefined} onClick={() => setPage(item.id)}><Icon name={pageIcons[item.id]} /></button>
          ))}
          <div className="nav-divider" />
          <button aria-label={t("Settings")} title={t("Settings")} className={settingsArea ? "active" : ""} onClick={() => setPage("settings")}><Icon name="settings" /></button>
          {settingsArea && <div className="settings-nav">
            {settingsPages.map(item => {
              const active = page === item.id && (!item.section || settingsSection === item.section);
              return <button key={item.section ?? item.id} aria-label={t(item.navLabel ?? item.label)} title={t(item.navLabel ?? item.label)} className={active ? "active" : ""} aria-current={active ? "page" : undefined} onClick={() => {
                if (item.section) setSettingsSection(item.section);
                setPage(item.id);
              }}><Icon name={item.icon} /></button>;
            })}
            <div className="nav-divider" />
            {pages.filter(item => ["setup", "diagnostics"].includes(item.id)).map(item => <button key={item.id} aria-label={t(item.label)} title={t(item.label)} className={page === item.id ? "active" : ""} aria-current={page === item.id ? "page" : undefined} onClick={() => setPage(item.id)}><Icon name={pageIcons[item.id]} /></button>)}
          </div>}
        </nav>
      </aside>

      <main>
        <header>
          <div>
            <h1>{t(pageLabel)}</h1>
          </div>
          {page !== "dashboard" && <div className={`status-pill ${state.phase}`}>
            <span />
            {statusLabel}
          </div>}
        </header>

        {page === "dashboard" && (
          <DashboardPage
            state={localizedState}
            settings={settings}
            onOpenModels={() => setPage("models")}
            onOpenPrivacy={() => setPage("privacy")}
            onOpenHistory={() => setPage("history")}
            onOpenShortcuts={() => { setSettingsSection("shortcuts"); setPage("settings"); }}
            model={model}
            statusLabel={statusLabel}
            recordingAction={recordingAction}
            onToggleRecording={() => void toggleRecording()}
            onCancelRecording={() => void cancelRecording()}
            onCopyResult={(text) => void copyHistoryText(text)}
            inactiveShortcuts={inactiveShortcuts}
          />
        )}

        {page === "setup" && (
          <SetupPage
            appState={state}
            testsDisabled={!settingsLoaded}
            onPrepareTest={() => saveSettings({})}
            settings={settings}
            devices={devices}
            onRefreshDevices={() => void refreshDevices()}
            gpu={gpu}
            gpuChecking={gpuChecking}
            onSettingsChange={editSettingsLocally}
            onConfigureModel={() => { setPage("models"); setReturnToSetup(true); }}
            onLoadModel={() => void prepareModel()}
            modelLoading={preparingModel}
            modelReady={model?.state === "ready" && model.quantization === settings.modelQuantization}
            progress={modelProgress}
            onDiagnoseGpu={() => void diagnoseGpu()}
            onFinish={() => {
              void saveSettings({ setupComplete: true }).then((saved) => { if (saved) setPage("dashboard"); });
            }}
          />
        )}

        {settingsArea && (
        <SettingsPage
            section={page === "settings" ? settingsSection : "none"}
            settings={settings}
            devices={devices}
            onRefreshDevices={() => void refreshDevices()}
            recording={state.phase === "recording"}
            onSave={(patch) => void saveSettings(patch)}
          />
        )}

        {settingsArea && (
          <div className="settings-stack" hidden={page !== "models"}>{returnToSetup && <button className="text-button back-link" onClick={() => setPage("setup")}><span aria-hidden="true">←</span> {t("Back to setup")}</button>}<ModelsPage
            onOpenLogs={() => void openLogs()}
            gpu={gpu}
            settings={settings}
            asrBackendOptions={asrBackendOptions}
            modelLoading={modelLoading}
            model={model}
            progress={modelProgress}
            onConfigureModel={(configuration) => void prepareModel(configuration)}
            onSaveCustomModel={saveCustomModel}
            onDiagnoseGpu={() => void diagnoseGpu()}
          /><ApiKeySettings settings={settings} /></div>
        )}

        {page === "history" && (
          <HistoryPage
            settings={settings}
            settingsLoaded={settingsLoaded}
            history={history}
            filter={historyFilter}
            query={historyQuery}
            loading={historyLoading}
            failed={historyFailed}
            hasMore={historyHasMore}
            onSearch={(query) => {
              setHistoryQuery(query);
              historyQueryRef.current = query;
              setHistory([]);
              setHistoryHasMore(false);
              void refreshHistory().catch((error) => showNotice(String(error), "error"));
            }}
            onLoadMore={() => void refreshHistory(historyFilterRef.current, true).catch((error) => showNotice(String(error), "error"))}
            onSave={(patch) => void saveSettings(patch)}
            onCopyItem={(text) => void copyHistoryText(text)}
            onFilter={(filter) => {
              setHistoryFilter(filter);
              historyFilterRef.current = filter;
              setHistory([]);
              setHistoryHasMore(false);
              void refreshHistory(filter).catch((error) => showNotice(String(error), "error"));
            }}
            onRetry={(id) => void retryHistory(id)}
            onReprocess={(id, action) => void reprocessHistory(id, action)}
            reprocessedText={historyTextResult?.text}
            onDismissResult={() => setHistoryTextResult(null)}
            reprocessing={historyReprocessing || state.phase === "recording" || state.phase === "processing" || state.phase === "injecting"}
            onDelete={(id) => void removeHistory(id)}
            onDeleteAll={() => void clearHistory()}
            onLoadAudio={getHistoryAudio}
            onAudioError={() => void reportHistoryAudioFailure()}
            retryActive={state.operation === "retry" && (state.phase === "processing" || state.phase === "injecting")}
            onCancelRetry={() => void cancelRecording()}
          />
        )}

        {page === "dictionary" && (
          <DictionaryPage
            entries={dictionary}
            candidates={settings.historyRetention === "never" ? [] : dictionaryCandidates}
            onAdd={addDictionary}
            onUpdate={updateDictionary}
            onDelete={(id) => void removeDictionary(id)}
            onImport={importDictionary}
            onConfirmCandidate={(id) => void confirmCandidate(id)}
            onRejectCandidate={(id) => void rejectCandidate(id)}
          />
        )}

        {page === "privacy" && (
          <PrivacyPage settings={settings} settingsLoaded={settingsLoaded} onSave={(patch) => void saveSettings(patch)} />
        )}

        {page === "diagnostics" && (
          <DiagnosticsPage
            onOpenLogs={() => void openLogs()}
            state={localizedState}
            gpu={gpu}
            statusLabel={statusLabel}
            onDiagnoseGpu={() => void diagnoseGpu()}
          />
        )}
      </main>

      {(shownNotice || (shownProgress && !progressNoticeHidden)) && (
        <div
          className={`notice ${shownNotice?.severity ?? "info"}${preparingModel && !showingStartupWarning ? " loading" : ""}`}
          aria-live="polite"
          role={shownNotice?.severity === "error" ? "alert" : "status"}
        >
          {preparingModel && !showingStartupWarning && <span className="progress-ring" aria-hidden="true" />}
          <div className="notice-body">
            <button
              className="notice-message"
              onDoubleClick={() => void copyNotice()}
              title={t("Double-click to copy")}
            >
              {translateAppMessage(language, shownNotice?.message ?? null, shownNotice?.code, shownNotice?.outcome) ??
                (shownProgress?.stage === "download"
                  ? t("Downloading the speech model files.")
                  : t("Loading the speech model."))}
            </button>
            {shownProgress && !progressNoticeHidden && <ModelProgressBar progress={shownProgress} />}
          </div>
          <IconButton icon="copy" label={t("Copy notification")} className="notice-action" onClick={() => void copyNotice()} />
          {noticeCopied && <span className="notice-copied">{t("Copied")}</span>}
          <IconButton
            icon="close"
            label={t("Dismiss notification")}
            className="notice-action"
            onClick={() => {
              if (preparingModel) setProgressNoticeHidden(true);
              if (notice) {
                dismissNotice();
              } else setStartupHotkeyWarnings([]);
            }}
          />
        </div>
      )}
    </div>
  );
}
