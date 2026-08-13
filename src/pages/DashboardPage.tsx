import { InfoCard } from "../components/ui";
import type { AppState, AudioLevel, HistoryItem, ModelStatus, Settings } from "../types";

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
}) {
  return (
    <section className="grid">
      <article className="hero-card span-2">
        <div>
          <p className="eyebrow">RECORDING STATUS</p>
          <h2>{state.phase === "idle" ? "Ready when you are" : statusLabel}</h2>
          <p>{state.message ?? `Use ${settings.hotkey} to start or stop dictation.`}</p>
          <button
            className="primary"
            onClick={onToggleRecording}
            disabled={
              recordingAction || state.phase === "processing" || state.phase === "injecting"
            }
          >
            {state.phase === "recording"
              ? "Stop and transcribe"
              : state.phase === "processing"
                ? "Transcribing..."
                : state.phase === "injecting"
                  ? "Inserting..."
                  : "Start recording"}
          </button>
          {state.phase === "recording" && (
            <button className="secondary" onClick={onCancelRecording}>
              Cancel
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

      <InfoCard label="HOTKEY" value={settings.hotkey} detail="Global toggle shortcut" />
      <InfoCard
        label="MODEL"
        value={model?.installed ? model.modelId ?? "Ready" : "Not installed"}
        detail={model?.detail ?? "Checking local cache"}
      />
      <InfoCard
        label="PRIVACY"
        value="Local only"
        detail={
          settings.historyEnabled
            ? "History stored locally on this device"
            : "History disabled; nothing is stored"
        }
      />
      <InfoCard
        label="RECENT ITEMS"
        value={String(history.length)}
        detail="No transcript content is logged"
      />
    </section>
  );
}
