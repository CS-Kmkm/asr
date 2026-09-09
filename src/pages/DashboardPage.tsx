import { InfoCard } from "../components/ui";
import type { AppState, AudioLevel, HistoryItem, ModelStatus, Settings } from "../types";
import { useI18n } from "../i18n";

const WAVE_SCALES = [0.4, 0.7, 1, 0.6, 0.9, 0.5, 0.8, 0.55, 0.35];

export function DashboardPage({
  state,
  settings,
  history,
  model,
  level,
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
  level: AudioLevel;
  statusLabel: string;
  recordingAction: boolean;
  onToggleRecording: () => void;
  onCancelRecording: () => void;
  onCopyResult: (text: string) => void;
}) {
  const { t } = useI18n();
  return (
    <section className="grid">
      <article className="hero-card span-2">
        <div>
          <p className="eyebrow">{t("RECORDING STATUS")}</p>
          <h2>{state.phase === "idle" ? t("Ready when you are") : statusLabel}</h2>
          <p>
            {state.message ?? `${t("Use the recording hotkey to start or stop dictation:")} ${settings.hotkey}`}
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
        {state.phase === "recording" && (
          <div
            className="wave"
            aria-label={`Input level ${Math.round(level.peak * 100)} percent`}
          >
            {WAVE_SCALES.map((scale, i) => (
              <i key={i} style={{ height: Math.max(8, level.peak * 100 * scale) }} />
            ))}
          </div>
        )}
      </article>

      <InfoCard label={t("HOTKEY")} value={settings.hotkey} detail={t("Global toggle shortcut")} />
      <InfoCard
        label={t("MODEL")}
        value={model?.installed ? model.modelId ?? "Ready" : "Not installed"}
        detail={model?.detail ?? "Checking local cache"}
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
        value="Local only"
        detail={
          settings.historyEnabled
            ? "History stored locally on this device"
            : "History disabled; nothing is stored"
        }
      />
      <InfoCard
        label={t("RECENT ITEMS")}
        value={String(history.length)}
        detail={t("No transcript content is logged")}
      />
    </section>
  );
}
