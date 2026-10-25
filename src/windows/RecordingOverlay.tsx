import { useEffect, useRef, useState } from "react";
import { listen } from "@tauri-apps/api/event";
import { cancelRecording, cycleVoiceTranslationTarget } from "../api";
import { insertionDetailLabels, insertionOutcomeNotices, isAppMessage, translateAppMessage, useI18n, type MessageKey } from "../i18n";
import type { AppState, AudioLevel } from "../types";

const OVERLAY_WAVE_BAR_COUNT = 9;
const OVERLAY_PREVIEW_CHARS = 140;

interface CorrectionPreview {
  text: string;
  stage: "draft" | "streaming" | "final" | "fallback";
}

interface VoiceModeEvent {
  // "selected_translate" is the selected-text translation, which has no recording.
  mode: "dictate" | "translate" | "edit" | "ask" | "selected_translate";
  targetLanguage: string | null;
}

// Fixed outcome and reason codes for a non-confirmed insertion.
interface InsertionNoticeEvent {
  outcome: string;
  detail: string | null;
}

// Matches the backend timer that hides the overlay window.
const INSERTION_NOTICE_MS = 5_000;

// A failure or warning sentence from the backend, shown while the main
// window may be hidden. The backend chooses how long it stays up.
interface OverlayMessageEvent {
  severity: "failure" | "warning";
  message: string;
  durationMs: number;
}

// A refused hotkey press, flashed inside the live view without replacing it.
interface OverlayHintEvent {
  message: string;
  durationMs: number;
}

type OverlayNotice =
  | ({ kind: "insertion" } & InsertionNoticeEvent)
  | ({ kind: "message" } & OverlayMessageEvent);

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

function compactOverlayPreview(text: string): string {
  const characters = Array.from(text.trim());
  return characters.length > OVERLAY_PREVIEW_CHARS
    ? `…${characters.slice(-OVERLAY_PREVIEW_CHARS).join("")}`
    : characters.join("");
}

export function RecordingOverlay() {
  const { language, t } = useI18n();
  const [waveform, setWaveform] = useState<number[]>(
    () => Array(OVERLAY_WAVE_BAR_COUNT).fill(0),
  );
  const [phase, setPhase] = useState<AppState["phase"]>("idle");
  const [stage, setStage] = useState<AppState["stage"]>("idle");
  const previousPhase = useRef<AppState["phase"]>("idle");
  const [message, setMessage] = useState<string | null>(null);
  const [messageCode, setMessageCode] = useState<string | null>(null);
  const [preview, setPreview] = useState<CorrectionPreview | null>(null);
  const [voiceMode, setVoiceMode] = useState<VoiceModeEvent>({
    mode: "dictate",
    targetLanguage: null,
  });
  const [notice, setNotice] = useState<OverlayNotice | null>(null);
  const noticeTimer = useRef<number | undefined>(undefined);
  const [hint, setHint] = useState<string | null>(null);
  const hintTimer = useRef<number | undefined>(undefined);

  useEffect(() => {
    // The latest notice replaces any earlier one, like the backend window.
    const showNotice = (next: OverlayNotice, durationMs: number) => {
      window.clearTimeout(noticeTimer.current);
      setNotice(next);
      noticeTimer.current = window.setTimeout(() => setNotice(null), durationMs);
    };
    const listeners = Promise.all([
      listen<InsertionNoticeEvent>("insertion-notice", ({ payload }) => {
        showNotice({ kind: "insertion", ...payload }, INSERTION_NOTICE_MS);
      }),
      listen<OverlayMessageEvent>("overlay-message", ({ payload }) => {
        showNotice({ kind: "message", ...payload }, payload.durationMs);
      }),
      listen<OverlayHintEvent>("overlay-hint", ({ payload }) => {
        window.clearTimeout(hintTimer.current);
        setHint(payload.message);
        hintTimer.current = window.setTimeout(() => setHint(null), payload.durationMs);
      }),
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
        setStage(payload.stage);
        setMessage(payload.message);
        setMessageCode(payload.messageCode);
        if (recordingStarted) {
          setWaveform(Array(OVERLAY_WAVE_BAR_COUNT).fill(0));
          setPreview(null);
          window.clearTimeout(noticeTimer.current);
          setNotice(null);
        } else if (
          payload.phase === "processing" &&
          (["preparing_model", "finishing_audio", "transcribing", "retrying"].includes(payload.stage)
            || (payload.stage === "translating" && payload.operation === "selected_text"))
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
      window.clearTimeout(noticeTimer.current);
      window.clearTimeout(hintTimer.current);
      void listeners.then((unlisten) => unlisten.forEach((fn) => fn()));
    };
  }, []);

  if (phase !== "recording" && phase !== "processing" && phase !== "injecting" && notice?.kind === "message") {
    const failed = notice.severity === "failure";
    return (
      <div className={`recording-overlay notice message${failed ? " failed" : ""}`} role="alert" aria-live="assertive">
        <span className="notice-mark" aria-hidden="true" />
        <div className="processing-copy">
          <span className="recording-overlay-label">{t(failed ? "Not completed" : "Completed with a warning")}</span>
          <span className="notice-detail">
            {isAppMessage(notice.message)
              ? translateAppMessage(language, notice.message)
              : t("Open the app window for details.")}
          </span>
        </div>
      </div>
    );
  }

  if (phase !== "recording" && phase !== "processing" && phase !== "injecting" && notice?.kind === "insertion") {
    // A lost previous clipboard does not change how the insertion went.
    const outcome = notice.outcome.replace(/_not_restored$/, "");
    const failed = outcome === "insertion_failed";
    // The paste was sent; only its confirmation is unavailable in this field.
    const sent = outcome === "paste_unverified";
    const outcomeKey = insertionOutcomeNotices[notice.outcome];
    const detailKey = notice.detail ? insertionDetailLabels[notice.detail] : undefined;
    return (
      <div className={`recording-overlay notice${failed ? " failed" : sent ? " sent" : ""}`} role="status" aria-live="polite">
        <span className="notice-mark" aria-hidden="true" />
        <div className="processing-copy">
          <span className="recording-overlay-label">{outcomeKey ? t(outcomeKey) : notice.outcome}</span>
          {notice.detail && <span className="notice-detail">{detailKey ? t(detailKey) : notice.detail}</span>}
        </div>
      </div>
    );
  }

  // Cancels like Esc: nothing is inserted. Clicking never focuses the overlay.
  const cancelButton = (
    <button
      className="overlay-cancel"
      type="button"
      title={t("Cancel (Esc)")}
      aria-label={t("Cancel (Esc)")}
      onClick={() => void cancelRecording().catch(() => undefined)}
    >
      ×
    </button>
  );

  if (phase === "recording") {
    return (
      <div className="recording-overlay recording" role="status" aria-label={t("Recording in progress")}>
        <span className="recording-live-dot" aria-hidden="true" />
        <span
          className={`recording-overlay-label${hint ? " overlay-hint" : ""}`}
          role={hint ? "alert" : undefined}
        >
          {hint
            ? translateAppMessage(language, hint)
            : voiceMode.mode === "translate"
            ? t("Translating")
            : voiceMode.mode === "edit"
              ? t("Editing")
              : voiceMode.mode === "ask"
                ? t("Ask Anything")
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
            title={t("Cycle the target language for this recording.")}
            onClick={() => void cycleVoiceTranslationTarget().catch(() => undefined)}
          >
            {t(translationLanguageKeys[voiceMode.targetLanguage] ?? "Language")} ↻
          </button>
        )}
        {cancelButton}
      </div>
    );
  }

  if (phase !== "processing" && phase !== "injecting") return null;

  const compactPreview = compactOverlayPreview(preview?.text ?? "");
  // A start waiting for the model has not published its voice mode yet.
  const preparing = stage === "preparing_model";
  const label = preparing
    ? t("Preparing speech model")
    : voiceMode.mode === "selected_translate"
    ? t("Selected-text translation")
    : voiceMode.mode === "ask"
    ? t("Ask Anything")
    : voiceMode.mode === "edit"
    ? t("Editing")
    : phase === "injecting"
      ? stage === "updating_text"
        ? t("Updating text")
        : t("Inserting")
      : stage === "finishing_audio"
        ? t("Finishing audio")
        : stage === "transcribing" || stage === "retrying"
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
        <span className={`correction-preview${compactPreview && !preparing ? "" : " pending"}`}>
          {preparing
            ? t("Starts when ready")
            : compactPreview || translateAppMessage(language, message, messageCode) || t("Preparing text…")}
        </span>
      </div>
      {phase === "processing" && cancelButton}
    </div>
  );
}

