import { InfoCard } from "../components/ui";
import { RecordingLevel } from "../components/RecordingLevel";
import type { AppState, HistoryItem, ModelStatus, Settings } from "../types";
import { translateAppMessage, useI18n } from "../i18n";

export function DashboardPage({
  state,
  settings,
  history,
  model,
  statusLabel,
  recordingAction,
  onToggleRecording,
  onCancelRecording,
  onCopyResult,
}: {
  state: AppState;
  settings: Settings;
  history: HistoryItem[];
  model: ModelStatus | null;
  statusLabel: string;
  recordingAction: boolean;
  onToggleRecording: () => void;
  onCancelRecording: () => void;
  onCopyResult: (text: string) => void;
}) {
  const { t, language } = useI18n();
  const cloudConfigured = settings.asrBackend === "openai-compatible" || settings.correctionProvider !== "local";
  const consents = settings.cloudConsents ?? [];
  const cloudSpeech = settings.asrBackend === "openai-compatible" && consents.includes(`asr:${settings.apiBaseUrl.trim().replace(/\/+$/, "")}`);
  const cloudText = settings.correctionProvider !== "local" && consents.includes(settings.correctionProvider);
  const privacyDetail = [
    settings.asrBackend === "openai-compatible"
      ? cloudSpeech ? t("Recordings are sent to the configured speech endpoint after consent.") : t("Cloud speech is blocked until you grant endpoint consent in Privacy.")
      : t("Speech recognition stays local."),
    settings.correctionProvider !== "local"
      ? cloudText ? t("AI modes send text to the selected provider after consent.") : t("Cloud text requests are blocked until you grant provider consent in Privacy.")
      : t("AI text processing stays local."),
  ].join(" ");
  return (
    <section className="grid">
      <article className="hero-card span-2">
        <div>
          <p className="eyebrow">{t("RECORDING STATUS")}</p>
          <h2>{state.phase === "idle" ? t("Ready when you are") : statusLabel}</h2>
          <p>
            {state.message ?? `${t("Use the recording hotkey to start or stop dictation:")} ${settings.shortcuts.dictate[0]}`}
          </p>
          <button
            className="primary"
            onClick={onToggleRecording}
            disabled={
              recordingAction || state.phase === "processing" || state.phase === "injecting"
            }
          >
            {state.phase === "recording"
              ? t("Stop and transcribe")
              : state.phase === "processing"
                ? t("Transcribing...")
                : state.phase === "injecting"
                ? t("Inserting...")
                : t("Start recording")}
          </button>
          {state.phase === "recording" && (
            <button className="secondary" onClick={onCancelRecording}>
              {t("Cancel")}
            </button>
          )}
        </div>
        {state.phase === "recording" && <RecordingLevel />}
      </article>

      <InfoCard label={t("HOTKEY")} value={settings.shortcuts.dictate[0]} detail={t("Global toggle shortcut")} />
      <InfoCard
        label={t("MODEL")}
        value={model?.installed ? model.modelId ?? t("Ready") : t("Not installed")}
        detail={model ? translateAppMessage(language, model.detail) ?? model.detail : t("Checking local cache")}
      />
      {state.lastResult && (
        <article className="info-card span-2">
          <p className="eyebrow">{t("LAST RESULT")}</p>
          <div
            className="history-text"
            role="button"
            tabIndex={0}
            title={t("Double-click to copy")}
            onDoubleClick={() => onCopyResult(state.lastResult!)}
            onKeyDown={(event) => {
              if (event.key === "Enter" || event.key === " ") {
                event.preventDefault();
                onCopyResult(state.lastResult!);
              }
            }}
          >
            {state.lastResult}
          </div>
          <p>{t("Double-click to copy")}</p>
        </article>
      )}
      <InfoCard
        label={t("PRIVACY")}
        warning={cloudSpeech || cloudText}
        value={cloudSpeech || cloudText ? t("Cloud services configured") : cloudConfigured ? t("Cloud consent required") : t("Local only")}
        detail={privacyDetail}
      />
      <InfoCard
        label={t("RECENT ITEMS")}
        value={String(history.length)}
        detail={t("No transcript content is logged")}
      />
    </section>
  );
}
