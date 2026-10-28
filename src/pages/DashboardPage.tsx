import { RecordingLevel } from "../components/RecordingLevel";
import type { AppState, ModelStatus, Settings } from "../types";
import { HelpHint, IconButton } from "../components/ui";
import { translateAppMessage, useI18n } from "../i18n";

export function DashboardPage({ state, settings, model, statusLabel, recordingAction, onToggleRecording,
  onCancelRecording, onCopyResult, inactiveShortcuts = [], onOpenModels, onOpenPrivacy, onOpenHistory, onOpenShortcuts,
}: {
  state: AppState;
  settings: Settings;
  model: ModelStatus | null;
  statusLabel: string;
  recordingAction: boolean;
  onToggleRecording: () => void;
  onCancelRecording: () => void;
  onCopyResult: (text: string) => void;
  inactiveShortcuts?: string[];
  onOpenModels: () => void;
  onOpenPrivacy: () => void;
  onOpenHistory: () => void;
  onOpenShortcuts: () => void;
}) {
  const { t, language } = useI18n();
  const consents = settings.cloudConsents ?? [];
  const cloudSpeech = settings.asrBackend === "openai-compatible" && consents.includes(`asr:${settings.apiBaseUrl.trim().replace(/\/+$/, "")}`);
  const cloudText = settings.correctionProvider !== "local" && consents.includes(settings.correctionProvider);
  const activeChords = settings.shortcuts.dictate.filter(chord => !inactiveShortcuts.includes(`Dictation (${chord})`));
  const busy = state.phase === "processing" || state.phase === "injecting";
  const modelLabel = model?.state === "ready" ? model.modelId ?? t("Ready")
    : model?.state === "loading" ? t("Model loading") : model?.state === "error" ? t("Model error") : t("Model not loaded");

  return <section className="dashboard-layout">
    <article className="recording-card" aria-labelledby="recording-title">
      <span className={`recording-symbol ${state.phase}`} aria-hidden="true">{state.phase === "recording" ? "■" : "●"}</span>
      <h2 id="recording-title">{state.phase === "idle" ? t("Ready when you are") : statusLabel}</h2>
      {state.phase === "recording" && <RecordingLevel />}
      {state.message && <p className="recording-message" role={state.phase === "error" ? "alert" : "status"}>{state.message}</p>}
      <div className="recording-actions">
        <button className="primary" onClick={onToggleRecording} disabled={recordingAction || busy}>
          {state.phase === "recording" ? t("Stop and transcribe") : state.phase === "processing" ? t("Transcribing...") : state.phase === "injecting" ? t("Inserting...") : t("Start recording")}
        </button>
        {state.phase === "recording" && <button className="secondary" onClick={onCancelRecording}>{t("Cancel")}</button>}
      </div>
      <div className="recording-shortcuts">
        {activeChords.map(chord => <kbd key={chord}>{chord}</kbd>)}
        <button className="text-button" onClick={onOpenShortcuts}>{t("Voice mode shortcuts")}</button>
      </div>
    </article>

    <article className="latest-result" aria-labelledby="latest-result-title">
      <div className="section-heading"><h2 id="latest-result-title">{t("Latest result")}</h2>
        {state.lastResult && <IconButton icon="copy" label={t("Copy")} onClick={() => onCopyResult(state.lastResult!)} />}
      </div>
      {state.lastResult ? <div className="result-text" onDoubleClick={() => onCopyResult(state.lastResult!)}>{state.lastResult}</div>
        : <p className="result-placeholder">{t("No result yet")}</p>}
      <button className="text-button" onClick={onOpenHistory}>{t("View history")} <span aria-hidden="true">→</span></button>
    </article>

    <div className="dashboard-support">
      <section className="support-item">
        <div className="section-heading"><h3>{t("ASR model")}</h3><button className="text-button" onClick={onOpenModels}>{t("Configure model")}</button></div>
        <p className={model?.state === "error" ? "settings-error" : "support-value"}>{modelLabel}</p>
        {model?.detail && <HelpHint label={t("ASR model")}>{translateAppMessage(language, model.detail) ?? model.detail}</HelpHint>}
      </section>
      <section className="support-item">
        <div className="section-heading"><h3>{t("Privacy")}</h3><button className="text-button" onClick={onOpenPrivacy}>{t("Review consent")}</button></div>
        <dl className="data-flow">
          <div><dt>{t("Speech recognition")}</dt><dd>{settings.asrBackend !== "openai-compatible" ? t("Local only") : cloudSpeech ? t("Cloud") : t("Cloud consent required")}</dd></div>
          <div><dt>{t("AI text processing")}</dt><dd>{settings.correctionProvider === "local" ? t("Local only") : cloudText ? t("Cloud") : t("Cloud consent required")}</dd></div>
        </dl>
      </section>
    </div>
  </section>;
}
