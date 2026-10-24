import { useEffect, useState } from "react";
import { getSettingsRepairStatus, persistRepairedSettings } from "../api";
import type { Settings } from "../types";
import { useI18n, type MessageKey } from "../i18n";

// Marker the backend uses when the whole stored settings row is unreadable.
const UNREADABLE_ROW = "unreadable";

// Settings labels for the stored JSON keys the backend reports. A key not
// listed here (for example one added later) is shown as is.
const fieldLabels = {
  uiLanguage: "Language",
  theme: "Appearance",
  interactionSounds: "Interaction sounds",
  speechLocale: "Speech language",
  translationTargetLanguage: "Voice Translate target",
  translationInstruction: "Translation instruction",
  translationHotkey: "Selected-text translation hotkey",
  microphoneId: "Microphone",
  historyRetention: "History retention",
  deleteAudioAfterProcessing: "Delete audio after processing",
  keepFailedTakes: "Keep failed recordings for 24 hours",
  autoStart: "Start with Windows",
  clipboardRestore: "Restore clipboard",
  liveTargetInsertion: "Live text insertion (experimental)",
  noiseSuppression: "Noise suppression",
  inputGainPercent: "Input gain",
  automaticGain: "Automatic gain",
  modelId: "ASR model",
  modelQuantization: "Load format",
  asrBackend: "ASR backend",
  apiBaseUrl: "API base URL",
  apiKeyEnvVar: "API key environment variable",
  openaiCorrectionModel: "OpenAI model",
  openaiReasoningEffort: "Reasoning effort",
  openaiApiKeyEnvVar: "OpenAI API key environment variable",
  geminiCorrectionModel: "Gemini model",
  geminiApiKeyEnvVar: "Gemini API key environment variable",
  localCorrectionBaseUrl: "Local endpoint base URL",
  localCorrectionModel: "Local correction model",
  localCorrectionMaxTokens: "Local output token limit",
  correctionMode: "Correction mode",
  correctionRemoveFillers: "Remove filler words",
  correctionRemoveRepetitions: "Remove accidental repetition",
  correctionResolveSelfCorrections: "Apply spoken self-corrections",
  correctionAutoFormat: "Automatic formatting",
  correctionImproveClarity: "Improve clarity",
  personalizationEnabled: "Personalization",
  globalStyleProfile: "Global profile",
  setupComplete: "Setup completed",
  "shortcuts.dictate": "Dictation shortcuts",
  "shortcuts.translate": "Voice Translate shortcuts",
  "shortcuts.ask": "Ask Anything shortcuts",
  "shortcuts.edit": "Speak to edit shortcuts",
  [UNREADABLE_ROW]: "All settings (the stored settings could not be read)",
} satisfies Record<string, MessageKey>;

// What saving the value in use (the fallback) also does, for the fields
// whose saved value has an effect beyond this setting.
const fieldConsequences = {
  deleteAudioAfterProcessing: "Saving “on” also lets startup delete leftover temporary recordings again.",
  autoStart: "Saving “off” removes the Windows startup entry.",
  historyRetention: "Saving “Forever” keeps History indefinitely.",
} satisfies Record<string, MessageKey>;

// Lists the stored settings fields this version could not read. The backend
// uses fallback values for them in memory only and never overwrites the
// stored values on its own; the button saves those values in use. The status
// is fetched on mount and again whenever the settings change (a save or the
// settings-changed event), so a field fixed through the form disappears, and
// the notice does not depend on the startup status event.
export function SettingsRepairBanner({ settings }: { settings: Settings }) {
  const { t } = useI18n();
  const [fields, setFields] = useState<string[]>([]);
  const [saving, setSaving] = useState(false);
  const [failed, setFailed] = useState(false);
  useEffect(() => {
    let active = true;
    void getSettingsRepairStatus()
      .then((next) => { if (active) setFields(next); })
      .catch(() => {});
    return () => { active = false; };
  }, [settings]);
  if (fields.length === 0) return null;
  const persist = () => {
    setSaving(true);
    setFailed(false);
    return persistRepairedSettings()
      .then(setFields, () => setFailed(true))
      .finally(() => setSaving(false));
  };
  return (
    <section className="panel" aria-labelledby="settings-repair-title">
      <p id="settings-repair-title" className="settings-error" role="alert">
        {t("Some stored settings could not be read by this version, so these fields use fallback values for now:")}
      </p>
      <ul>
        {fields.map((field) => {
          const label = fieldLabels[field as keyof typeof fieldLabels];
          const consequence = fieldConsequences[field as keyof typeof fieldConsequences];
          return (
            <li key={field}>
              {label ? t(label) : <code>{field}</code>}
              {consequence && <> — {t(consequence)}</>}
            </li>
          );
        })}
      </ul>
      <p>{t("The stored values were not changed. The button below saves only the fallback values the app is currently using for these fields, not any other change.")}</p>
      <button type="button" disabled={saving} onClick={() => void persist()}>
        {t("Save the values in use for these fields")}
      </button>
      {failed && <p className="settings-error" role="alert">{t("The current values could not be saved.")}</p>}
    </section>
  );
}
