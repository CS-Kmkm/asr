import { useEffect, useRef, useState } from "react";
import { SettingRow, Toggle } from "../components/ui";
import { AiCorrectionSettings } from "../components/AiCorrectionSettings";
import type { Settings } from "../types";

export function SettingsPage({
  settings,
  onSave,
}: {
  settings: Settings;
  onSave: (patch: Partial<Settings>) => void;
}) {
  const [hotkey, setHotkey] = useState(settings.hotkey);
  const [translationHotkey, setTranslationHotkey] = useState(settings.translationHotkey);
  const cancelHotkeyBlurRef = useRef(false);
  const suppressHotkeyBlurRef = useRef(false);
  const cancelTranslationBlurRef = useRef(false);
  const suppressTranslationBlurRef = useRef(false);

  useEffect(() => setHotkey(settings.hotkey), [settings.hotkey]);
  useEffect(() => setTranslationHotkey(settings.translationHotkey), [settings.translationHotkey]);

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

  return (
    <div className="settings-stack">
      <section className="panel">
        <h2>Input settings</h2>
        <SettingRow
          title="Recording hotkey"
          detail="The default is Ctrl+Shift+Space."
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
          title="Translation hotkey"
          detail="Translates selected text; the default is Ctrl+Shift+T."
          control={
            <input
              value={translationHotkey}
              onChange={(event) => setTranslationHotkey(event.target.value)}
              onBlur={commitTranslationHotkey}
              onKeyDown={(event) => {
                if (event.key === "Enter") { event.preventDefault(); commitTranslationHotkey(); suppressTranslationBlurRef.current = true; event.currentTarget.blur(); }
                if (event.key === "Escape") { setTranslationHotkey(settings.translationHotkey); cancelTranslationBlurRef.current = true; event.currentTarget.blur(); }
              }}
            />
          }
        />
        <SettingRow
          title="Translation instruction"
          detail="Optional guidance appended to the fixed translation-only contract."
          control={
            <input
              value={settings.translationInstruction}
              maxLength={500}
              onChange={(event) => onSave({ translationInstruction: event.target.value })}
            />
          }
        />
        <SettingRow
          title="Start with Windows"
          detail="Launches Local Voice Input automatically when you sign in to Windows."
          control={
            <Toggle
              checked={settings.autoStart}
              onChange={(value) => onSave({ autoStart: value })}
            />
          }
        />
        <SettingRow
          title="Restore clipboard"
          detail="Restore previous clipboard contents after successful paste."
          control={
            <Toggle
              checked={settings.clipboardRestore}
              onChange={(value) => onSave({ clipboardRestore: value })}
            />
          }
        />
        <SettingRow
          title="Noise suppression"
          detail="Reduces steady fan and room noise after recording, without adding work to the live microphone callback."
          control={
            <select
              value={settings.noiseSuppression}
              onChange={(event) =>
                onSave({
                  noiseSuppression: event.target.value as Settings["noiseSuppression"],
                })
              }
            >
              <option value="off">Off</option>
              <option value="low">Low</option>
              <option value="medium">Medium</option>
              <option value="high">High</option>
            </select>
          }
        />
        <SettingRow
          title="Automatic gain"
          detail="Targets a clear speech level after recording. Manual gain below is applied in addition."
          control={
            <Toggle
              checked={settings.automaticGain}
              onChange={(value) => onSave({ automaticGain: value })}
            />
          }
        />
        <SettingRow
          title="Input gain"
          detail="Adjusts the processed microphone level from 25% to 400%."
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
