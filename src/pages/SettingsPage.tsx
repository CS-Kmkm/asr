import { useEffect, useRef, useState } from "react";
import { SettingRow, Toggle } from "../components/ui";
import { AiCorrectionSettings } from "../components/AiCorrectionSettings";
import type { Settings } from "../types";
import { useI18n } from "../i18n";

const translationLanguages = [
  ["en", "English"],
  ["ja", "Japanese"],
  ["zh", "Chinese"],
  ["es", "Spanish"],
  ["fr", "French"],
  ["pt", "Portuguese"],
  ["de", "German"],
  ["ko", "Korean"],
] as const;

export function SettingsPage({
  settings,
  onSave,
}: {
  settings: Settings;
  onSave: (patch: Partial<Settings>) => void;
}) {
  const { t } = useI18n();
  const [hotkey, setHotkey] = useState(settings.hotkey);
  const [translationHotkey, setTranslationHotkey] = useState(settings.translationHotkey);
  const [voiceTranslateHotkey, setVoiceTranslateHotkey] = useState(settings.voiceTranslateHotkey);
  const [translationInstruction, setTranslationInstruction] = useState(settings.translationInstruction);
  const [languageToAdd, setLanguageToAdd] = useState("zh");
  const cancelHotkeyBlurRef = useRef(false);
  const suppressHotkeyBlurRef = useRef(false);
  const cancelTranslationBlurRef = useRef(false);
  const suppressTranslationBlurRef = useRef(false);
  const cancelVoiceTranslateBlurRef = useRef(false);
  const suppressVoiceTranslateBlurRef = useRef(false);
  const cancelTranslationInstructionBlurRef = useRef(false);
  const suppressTranslationInstructionBlurRef = useRef(false);

  useEffect(() => setHotkey(settings.hotkey), [settings.hotkey]);
  useEffect(() => setTranslationHotkey(settings.translationHotkey), [settings.translationHotkey]);
  useEffect(() => setVoiceTranslateHotkey(settings.voiceTranslateHotkey), [settings.voiceTranslateHotkey]);
  useEffect(() => setTranslationInstruction(settings.translationInstruction), [settings.translationInstruction]);
  useEffect(() => {
    if (!settings.translationTargetLanguages.includes(languageToAdd)) return;
    const available = translationLanguages.find(
      ([code]) => !settings.translationTargetLanguages.includes(code),
    );
    if (available) setLanguageToAdd(available[0]);
  }, [languageToAdd, settings.translationTargetLanguages]);

  function commitHotkey() {
    if (cancelHotkeyBlurRef.current || suppressHotkeyBlurRef.current) {
      cancelHotkeyBlurRef.current = false;
      suppressHotkeyBlurRef.current = false;
      return;
    }
    if (hotkey !== settings.hotkey) onSave({ hotkey });
  }

  function commitTranslationHotkey() {
    if (cancelTranslationBlurRef.current || suppressTranslationBlurRef.current) {
      cancelTranslationBlurRef.current = false;
      suppressTranslationBlurRef.current = false;
      return;
    }
    if (translationHotkey !== settings.translationHotkey) onSave({ translationHotkey });
  }

  function commitTranslationInstruction() {
    if (cancelTranslationInstructionBlurRef.current || suppressTranslationInstructionBlurRef.current) {
      cancelTranslationInstructionBlurRef.current = false;
      suppressTranslationInstructionBlurRef.current = false;
      return;
    }
    if (translationInstruction !== settings.translationInstruction) {
      onSave({ translationInstruction });
    }
  }

  function commitVoiceTranslateHotkey() {
    if (cancelVoiceTranslateBlurRef.current || suppressVoiceTranslateBlurRef.current) {
      cancelVoiceTranslateBlurRef.current = false;
      suppressVoiceTranslateBlurRef.current = false;
      return;
    }
    if (voiceTranslateHotkey !== settings.voiceTranslateHotkey) {
      onSave({ voiceTranslateHotkey });
    }
  }

  function moveTargetLanguage(index: number, direction: -1 | 1) {
    const destination = index + direction;
    if (destination < 0 || destination >= settings.translationTargetLanguages.length) return;
    const languages = [...settings.translationTargetLanguages];
    [languages[index], languages[destination]] = [languages[destination], languages[index]];
    onSave({ translationTargetLanguages: languages });
  }

  function removeTargetLanguage(language: string) {
    const languages = settings.translationTargetLanguages.filter((item) => item !== language);
    if (languages.length === 0) return;
    onSave({
      translationTargetLanguages: languages,
      translationTargetLanguage:
        settings.translationTargetLanguage === language
          ? languages[0]
          : settings.translationTargetLanguage,
    });
  }

  function addTargetLanguage() {
    if (settings.translationTargetLanguages.includes(languageToAdd)) return;
    onSave({
      translationTargetLanguages: [...settings.translationTargetLanguages, languageToAdd],
    });
  }

  return (
    <div className="settings-stack">
      <section className="panel">
        <h2>{t("Input settings")}</h2>
        <SettingRow
          title={t("Language")}
          detail={t("Choose the language used by the app interface.")}
          control={<select value={settings.uiLanguage} onChange={(e) => onSave({ uiLanguage: e.target.value as Settings["uiLanguage"] })}>
            <option value="ja">{t("Japanese")}</option>
            <option value="en">{t("English")}</option>
          </select>}
        />
        <SettingRow
          title={t("Recording hotkey")}
          detail={t("The default is Ctrl+Shift+Space.")}
          control={
            <input
              value={hotkey}
              onChange={(e) => setHotkey(e.target.value)}
              onBlur={commitHotkey}
              onKeyDown={(e) => {
                if (e.key === "Enter") {
                  e.preventDefault();
                  commitHotkey();
                  suppressHotkeyBlurRef.current = true;
                  e.currentTarget.blur();
                } else if (e.key === "Escape") {
                  setHotkey(settings.hotkey);
                  cancelHotkeyBlurRef.current = true;
                  e.currentTarget.blur();
                }
              }}
            />
          }
        />
        <SettingRow
          title={t("Selected-text translation hotkey")}
          detail={t("Translates selected text; the default is Ctrl+Shift+T.")}
          control={
            <input
              value={translationHotkey}
              onChange={(event) => setTranslationHotkey(event.target.value)}
              onBlur={commitTranslationHotkey}
              onKeyDown={(event) => {
                if (event.key === "Enter") {
                  event.preventDefault();
                  commitTranslationHotkey();
                  suppressTranslationBlurRef.current = true;
                  event.currentTarget.blur();
                } else if (event.key === "Escape") {
                  setTranslationHotkey(settings.translationHotkey);
                  cancelTranslationBlurRef.current = true;
                  event.currentTarget.blur();
                }
              }}
            />
          }
      />
        <SettingRow
          title={t("Voice Translate hotkey")}
          detail={t("Starts or stops speech translation; the default is Ctrl+Shift+Y.")}
          control={
            <input
              value={voiceTranslateHotkey}
              onChange={(event) => setVoiceTranslateHotkey(event.target.value)}
              onBlur={commitVoiceTranslateHotkey}
              onKeyDown={(event) => {
                if (event.key === "Enter") {
                  event.preventDefault();
                  commitVoiceTranslateHotkey();
                  suppressVoiceTranslateBlurRef.current = true;
                  event.currentTarget.blur();
                } else if (event.key === "Escape") {
                  setVoiceTranslateHotkey(settings.voiceTranslateHotkey);
                  cancelVoiceTranslateBlurRef.current = true;
                  event.currentTarget.blur();
                }
              }}
            />
          }
        />
        <SettingRow
          title={t("Voice Translate target")}
          detail={`${t("The first language is the default. Reorder the list or choose the active target.")} ${t("Voice and selected-text Translate send text to the provider shown under AI text correction, even when correction is off.")} ${t("Provider")}: ${settings.correctionProvider === "gemini" ? "Google Gemini" : settings.correctionProvider === "local" ? t("Local (OpenAI-compatible)") : "OpenAI"}.`}
          control={
            <div className="translation-target-settings">
              <select
                value={settings.translationTargetLanguage}
                onChange={(event) => onSave({ translationTargetLanguage: event.target.value })}
              >
                {settings.translationTargetLanguages.map((language) => {
                  const option = translationLanguages.find(([code]) => code === language);
                  return <option key={language} value={language}>{option ? t(option[1]) : language}</option>;
                })}
              </select>
              <ol className="translation-target-list">
                {settings.translationTargetLanguages.map((language, index) => {
                  const option = translationLanguages.find(([code]) => code === language);
                  return (
                    <li key={language}>
                      <span>{option ? t(option[1]) : language}</span>
                      <button type="button" onClick={() => moveTargetLanguage(index, -1)} disabled={index === 0} aria-label={t("Move language up")}>↑</button>
                      <button type="button" onClick={() => moveTargetLanguage(index, 1)} disabled={index === settings.translationTargetLanguages.length - 1} aria-label={t("Move language down")}>↓</button>
                      <button type="button" onClick={() => removeTargetLanguage(language)} disabled={settings.translationTargetLanguages.length === 1}>{t("Remove")}</button>
                    </li>
                  );
                })}
              </ol>
              <div className="translation-target-add">
                <select value={languageToAdd} onChange={(event) => setLanguageToAdd(event.target.value)}>
                  {translationLanguages
                    .filter(([code]) => !settings.translationTargetLanguages.includes(code))
                    .map(([code, label]) => <option key={code} value={code}>{t(label)}</option>)}
                </select>
                <button
                  type="button"
                  onClick={addTargetLanguage}
                  disabled={settings.translationTargetLanguages.length === translationLanguages.length || settings.translationTargetLanguages.includes(languageToAdd)}
                >
                  {t("Add language")}
                </button>
              </div>
            </div>
          }
        />
      <SettingRow
          title={t("Translation instruction")}
          detail={t("Optional guidance appended to the fixed translation-only contract.")}
          control={
            <input
              value={translationInstruction}
              maxLength={500}
              onChange={(event) => setTranslationInstruction(event.target.value)}
              onBlur={commitTranslationInstruction}
              onKeyDown={(event) => {
                if (event.key === "Enter") {
                  event.preventDefault();
                  commitTranslationInstruction();
                  suppressTranslationInstructionBlurRef.current = true;
                  event.currentTarget.blur();
                } else if (event.key === "Escape") {
                  setTranslationInstruction(settings.translationInstruction);
                  cancelTranslationInstructionBlurRef.current = true;
                  event.currentTarget.blur();
                }
              }}
            />
          }
        />
        <SettingRow
          title={t("Start with Windows")}
          detail={t("Launches Local Voice Input automatically when you sign in to Windows.")}
          control={
            <Toggle
              checked={settings.autoStart}
              onChange={(value) => onSave({ autoStart: value })}
            />
          }
        />
        <SettingRow
          title={t("Restore clipboard")}
          detail={t("Restore previous clipboard contents after successful paste.")}
          control={
            <Toggle
              checked={settings.clipboardRestore}
              onChange={(value) => onSave({ clipboardRestore: value })}
            />
          }
        />
        <SettingRow
          title={t("Noise suppression")}
          detail={t("Reduces steady fan and room noise after recording, without adding work to the live microphone callback.")}
          control={
            <select
              value={settings.noiseSuppression}
              onChange={(event) =>
                onSave({
                  noiseSuppression: event.target.value as Settings["noiseSuppression"],
                })
              }
            >
              <option value="off">{t("Off")}</option>
              <option value="low">{t("Low")}</option>
              <option value="medium">{t("Medium")}</option>
              <option value="high">{t("High")}</option>
            </select>
          }
        />
        <SettingRow
          title={t("Automatic gain")}
          detail={t("Targets a clear speech level after recording. Manual gain below is applied in addition.")}
          control={
            <Toggle
              checked={settings.automaticGain}
              onChange={(value) => onSave({ automaticGain: value })}
            />
          }
        />
        <SettingRow
          title={t("Input gain")}
          detail={t("Adjusts the processed microphone level from 25% to 400%.")}
          control={
            <label className="range-control">
              <input
                type="range"
                min="25"
                max="400"
                step="5"
                value={settings.inputGainPercent}
                onChange={(event) => onSave({ inputGainPercent: Number(event.target.value) })}
              />
              <output>{settings.inputGainPercent}%</output>
            </label>
          }
        />
      </section>
      <AiCorrectionSettings settings={settings} onSave={onSave} />
    </div>
  );
}
