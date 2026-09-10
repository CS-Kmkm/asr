import { useEffect, useRef, useState } from "react";
import { SettingRow, Toggle } from "../components/ui";
import { AiCorrectionSettings } from "../components/AiCorrectionSettings";
import type { Settings } from "../types";
import { useI18n } from "../i18n";

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
  const [translationInstruction, setTranslationInstruction] = useState(settings.translationInstruction);
  const cancelHotkeyBlurRef = useRef(false);
  const suppressHotkeyBlurRef = useRef(false);
  const cancelTranslationBlurRef = useRef(false);
  const suppressTranslationBlurRef = useRef(false);
  const cancelTranslationInstructionBlurRef = useRef(false);
  const suppressTranslationInstructionBlurRef = useRef(false);

  useEffect(() => setHotkey(settings.hotkey), [settings.hotkey]);
  useEffect(() => setTranslationHotkey(settings.translationHotkey), [settings.translationHotkey]);
  useEffect(() => setTranslationInstruction(settings.translationInstruction), [settings.translationInstruction]);

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
          title={t("Translation hotkey")}
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
