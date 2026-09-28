import { useEffect, useRef, useState } from "react";
import { listen } from "@tauri-apps/api/event";
import { SettingRow, Toggle } from "../components/ui";
import { AiCorrectionSettings } from "../components/AiCorrectionSettings";
import type { AudioDevice, AudioLevel, Settings, ShortcutMode } from "../types";
import { speechLocaleRegistry, uiLocaleRegistry } from "../types";
import { getShortcutWarning, startMicrophoneTest, stopMicrophoneTest } from "../api";
import { useI18n } from "../i18n";

const translationLanguages = [
  ["en", "English"], ["ja", "Japanese"], ["zh", "Chinese"], ["es", "Spanish"],
  ["fr", "French"], ["pt", "Portuguese"], ["de", "German"], ["ko", "Korean"],
] as const;

const shortcutModes: Array<{ mode: ShortcutMode; label: "Dictation shortcuts" | "Voice Translate shortcuts" | "Ask Anything shortcuts" | "Speak to edit shortcuts" }> = [
  { mode: "dictate", label: "Dictation shortcuts" },
  { mode: "translate", label: "Voice Translate shortcuts" },
  { mode: "ask", label: "Ask Anything shortcuts" },
  { mode: "edit", label: "Speak to edit shortcuts" },
];

export function SettingsPage({
  settings,
  onSave,
  devices,
  recording,
}: {
  settings: Settings;
  onSave: (patch: Partial<Settings>) => void;
  devices: AudioDevice[];
  recording: boolean;
}) {
  const { t } = useI18n();
  const [shortcuts, setShortcuts] = useState(settings.shortcuts);
  const [translationHotkey, setTranslationHotkey] = useState(settings.translationHotkey);
  const [translationInstruction, setTranslationInstruction] = useState(settings.translationInstruction);
  const [languageToAdd, setLanguageToAdd] = useState("zh");
  const [testRunning, setTestRunning] = useState(false);
  const [testStopping, setTestStopping] = useState(false);
  const [testError, setTestError] = useState<string | null>(null);
  const [shortcutError, setShortcutError] = useState<string | null>(null);
  const [startupShortcutWarning, setStartupShortcutWarning] = useState(false);
  const [level, setLevel] = useState<AudioLevel>({ rms: 0, peak: 0 });
  const testRunningRef = useRef(false);
  const mountedRef = useRef(false);
  const testStartPromiseRef = useRef<Promise<void> | null>(null);
  const suppressTranslationHotkeyBlurRef = useRef(false);
  const suppressTranslationInstructionBlurRef = useRef(false);
  const savedShortcutsJsonRef = useRef(JSON.stringify(settings.shortcuts));

  useEffect(() => {
    const savedShortcutsJson = JSON.stringify(settings.shortcuts);
    if (savedShortcutsJson === savedShortcutsJsonRef.current) return;
    savedShortcutsJsonRef.current = savedShortcutsJson;
    setShortcuts(settings.shortcuts);
  }, [settings.shortcuts]);
  useEffect(() => setTranslationHotkey(settings.translationHotkey), [settings.translationHotkey]);

  useEffect(() => setTranslationInstruction(settings.translationInstruction), [settings.translationInstruction]);
  useEffect(() => {
    void getShortcutWarning().then(setStartupShortcutWarning).catch(() => {});
  }, [settings.shortcuts, settings.translationHotkey]);
  useEffect(() => {
    if (!settings.translationTargetLanguages.includes(languageToAdd)) return;
    const firstAvailable = translationLanguages.find(([code]) => !settings.translationTargetLanguages.includes(code));
    if (firstAvailable) setLanguageToAdd(firstAvailable[0]);
  }, [languageToAdd, settings.translationTargetLanguages]);

  useEffect(() => {
    mountedRef.current = true;
    let unlisten: Array<() => void> | undefined;
    let active = true;
    void Promise.all([
      listen<AudioLevel>("audio-level", ({ payload }) => setLevel(payload)),
      listen<{ kind: string; message: string }>("status", ({ payload }) => {
        if (payload.kind !== "microphone_test_failed") return;
        testRunningRef.current = false;
        setTestRunning(false);
        setLevel({ rms: 0, peak: 0 });
        setTestError(payload.message);
      }),
    ]).then((stops) => {
      if (active) unlisten = stops;
      else stops.forEach((stop) => stop());
    });
    return () => {
      active = false;
      mountedRef.current = false;
      unlisten?.forEach((stop) => stop());
      if (testRunningRef.current) {
        testRunningRef.current = false;
        const pendingStart = testStartPromiseRef.current;
        if (pendingStart) void pendingStart.then(() => stopMicrophoneTest(), () => stopMicrophoneTest());
        else void stopMicrophoneTest();
      }
    };
  }, []);

  useEffect(() => {
    if (recording && testRunningRef.current) void stopTest();
  }, [recording]);

  async function stopTest() {
    testRunningRef.current = false;
    setTestRunning(false);
    setTestStopping(true);
    setLevel({ rms: 0, peak: 0 });
    try {
      await testStartPromiseRef.current?.catch(() => undefined);
      await stopMicrophoneTest();
    } catch (error) {
      setTestError(String(error));
    } finally {
      if (mountedRef.current) setTestStopping(false);
    }
  }

  async function startTest() {
    if (testStopping) return;
    setTestError(null);
    setLevel({ rms: 0, peak: 0 });
    testRunningRef.current = true;
    try {
      const pending = startMicrophoneTest(settings.microphoneId);
      testStartPromiseRef.current = pending;
      await pending;
      if (testStartPromiseRef.current === pending) testStartPromiseRef.current = null;
      if (mountedRef.current && testRunningRef.current) setTestRunning(true);
      else await stopMicrophoneTest();
    } catch (error) {
      testStartPromiseRef.current = null;
      testRunningRef.current = false;
      if (mountedRef.current) {
        setTestRunning(false);
        setTestError(String(error));
      }
    }
  }

  function saveShortcuts(next: Settings["shortcuts"]) {
    setShortcuts(next);
    setShortcutError(null);
  }

  function setShortcut(mode: ShortcutMode, index: number, value: string) {
    const next = { ...shortcuts, [mode]: [...shortcuts[mode]] };
    next[mode][index] = value;
    saveShortcuts(next);
  }

  function addShortcut(mode: ShortcutMode) {
    if (shortcuts[mode].length >= 4) return;
    saveShortcuts({ ...shortcuts, [mode]: [...shortcuts[mode], ""] });
  }

  function removeShortcut(mode: ShortcutMode, index: number) {
    if (shortcuts[mode].length <= 1) return;
    saveShortcuts({ ...shortcuts, [mode]: shortcuts[mode].filter((_, item) => item !== index) });
  }

  function commitTranslationHotkey() {
    if (translationHotkey !== settings.translationHotkey) onSave({ translationHotkey });
  }

  function commitShortcuts() {
    const next = Object.fromEntries(Object.entries(shortcuts).map(([mode, chords]) => [mode, chords.map((chord) => chord.trim())])) as Settings["shortcuts"];
    const allChords = [...Object.values(next).flat(), translationHotkey.trim()];
    if (Object.values(next).some((chords) => chords.length < 1 || chords.length > 4 || chords.some((chord) => !chord))) {
      setShortcutError(t("Each voice mode needs one to four non-empty shortcuts."));
      return;
    }
    if (!translationHotkey.trim() || new Set(allChords.map((chord) => chord.toLocaleLowerCase())).size !== allChords.length) {
      setShortcutError(t("Shortcuts must be non-empty and unique across all actions."));
      return;
    }
    setShortcutError(null);
    if (JSON.stringify(next) !== JSON.stringify(settings.shortcuts)) onSave({ shortcuts: next });
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
    if (!languages.length) return;
    onSave({
      translationTargetLanguages: languages,
      translationTargetLanguage: settings.translationTargetLanguage === language
        ? languages[0]
        : settings.translationTargetLanguage,
    });
  }

  function addTargetLanguage() {
    if (settings.translationTargetLanguages.includes(languageToAdd)) return;
    if (settings.translationTargetLanguages.length >= translationLanguages.length) return;
    onSave({ translationTargetLanguages: [...settings.translationTargetLanguages, languageToAdd] });
  }

  const levelPercent = Math.round(Math.max(0, Math.min(1, Math.max(level.rms * 3.5, level.peak))) * 100);

  return (
    <div className="settings-stack">
      <section className="panel">
        <h2>{t("Input settings")}</h2>
        <SettingRow title={t("Language")} detail={t("Choose the language used by the app interface.")}
          control={<select value={settings.uiLanguage} onChange={(event) => onSave({ uiLanguage: event.target.value as Settings["uiLanguage"] })}>
            {uiLocaleRegistry.map(({ tag, label }) => <option key={tag} value={tag}>{t(label)}</option>)}
          </select>} />
        <SettingRow title={t("Appearance")} detail={t("Choose the app color theme.")}
          control={<select value={settings.theme} onChange={(event) => onSave({ theme: event.target.value as Settings["theme"] })}>
            <option value="system">{t("System")}</option><option value="light">{t("Light")}</option><option value="dark">{t("Dark")}</option>
          </select>} />
        <SettingRow title={t("Speech language")} detail={t("Choose a speech recognition language, or use automatic detection. The faster-whisper and OpenAI-compatible backends honor the base language only; VibeVoice currently ignores this setting.")}
          control={<select value={settings.speechLocale ?? "auto"} onChange={(event) => onSave({ speechLocale: event.target.value === "auto" ? null : event.target.value as Settings["speechLocale"] })}>
            <option value="auto">{t("Automatic detection")}</option>
            {speechLocaleRegistry.map(({ tag, label }) => <option key={tag} value={tag}>{t(label)}</option>)}
          </select>} />
        <SettingRow title={t("Microphone")} detail={t("Choose the input device used for recording and microphone testing.")}
          control={<select value={settings.microphoneId ?? ""} onChange={(event) => {
            if (testRunningRef.current) void stopTest();
            onSave({ microphoneId: event.target.value || null });
          }}>
            <option value="">{t("System default")}</option>
            {devices.map((device) => <option key={device.id} value={device.id}>{device.name}{device.isDefault ? ` (${t("Default")})` : ""}</option>)}
          </select>} />
        <SettingRow title={t("Microphone level test")} detail={t("Test the selected microphone. Audio is measured live and never saved.")}
          control={<div className="microphone-test-control">
            <button type="button" className={testRunning ? "secondary" : "primary"} disabled={recording || testStopping} onClick={() => void (testRunning ? stopTest() : startTest())}>
              {testRunning ? t("Stop test") : t("Start test")}
            </button>
            <div className="microphone-level-track" role="meter" aria-label={t("Microphone input level")} aria-valuemin={0} aria-valuemax={100} aria-valuenow={testRunning ? levelPercent : 0}>
              <span style={{ width: `${testRunning ? levelPercent : 0}%` }} />
            </div>
          </div>} />
        {testError && <p className="settings-error" role="alert">{t("Microphone test failed.")} {testError}</p>}
        <SettingRow title={t("Interaction sounds")} detail={t("Play a brief local sound when recording starts and stops.")}
          control={<Toggle checked={settings.interactionSounds} onChange={(value) => onSave({ interactionSounds: value })} label={t("Interaction sounds")} />} />
        <p className="settings-note">{t("Muting or pausing other applications is unavailable because this app cannot safely control their audio.")}</p>

        <h3>{t("Voice mode shortcuts")}</h3>
        <p>{t("Add one to four keyboard shortcuts for each voice mode. A shortcut must be unique across all actions.")}</p>
        {shortcutModes.map(({ mode, label }) => (
          <SettingRow key={mode} title={t(label)} detail={`${t("Enter a shortcut chord such as Ctrl+Shift+Space.")} ${mode === "edit"
            ? t("Speak to edit sends selected source text and the transcribed spoken instruction to the provider shown under AI text correction, even when correction is off. The selected ASR backend may send audio.")
            : mode === "ask"
              ? t("Ask sends only the transcribed spoken instruction to the selected provider for planning. Answer generation using a selection also sends the selected source text with the instruction, even when Dictation AI correction is off. The selected ASR backend may send audio.")
              : ""}`}
            control={<div className="shortcut-list">
              {shortcuts[mode].map((chord, index) => <div className="shortcut-item" key={`${mode}-${index}`}>
                <input aria-label={`${t(label)} ${index + 1}`} value={chord} onChange={(event) => setShortcut(mode, index, event.target.value)} />
                <button type="button" className="secondary" aria-label={`${t("Remove shortcut")} ${chord || index + 1}`} disabled={shortcuts[mode].length <= 1} onClick={() => removeShortcut(mode, index)}>{t("Remove")}</button>
              </div>)}
              <button type="button" className="secondary" disabled={shortcuts[mode].length >= 4} onClick={() => addShortcut(mode)}>{t("Add shortcut")}</button>
            </div>} />
        ))}
        <button type="button" className="primary shortcut-save" onClick={commitShortcuts}>{t("Save voice shortcuts")}</button>
        {shortcutError && <p className="settings-error" role="alert">{shortcutError}</p>}
        {startupShortcutWarning && <p className="settings-error" role="alert">{t("Some saved shortcuts could not be activated at startup. Change them in Settings and restart to verify.")}</p>}
        <SettingRow title={t("Selected-text translation hotkey")} detail={t("Translates selected text; the default is Ctrl+Shift+T.")}
          control={<input value={translationHotkey} aria-label={t("Selected-text translation hotkey")} onChange={(event) => setTranslationHotkey(event.target.value)} onBlur={() => {
            if (suppressTranslationHotkeyBlurRef.current) { suppressTranslationHotkeyBlurRef.current = false; return; }
            commitTranslationHotkey();
          }} onKeyDown={(event) => {
            if (event.key === "Enter") { event.preventDefault(); commitTranslationHotkey(); suppressTranslationHotkeyBlurRef.current = true; event.currentTarget.blur(); }
            if (event.key === "Escape") { setTranslationHotkey(settings.translationHotkey); suppressTranslationHotkeyBlurRef.current = true; event.currentTarget.blur(); }
          }} />} />
        <SettingRow title={t("Voice Translate target")} detail={`${t("The first language is the default. Reorder the list or choose the active target.")} ${t("Voice and selected-text Translate send text to the provider shown under AI text correction, even when correction is off.")}`}
          control={<div className="translation-target-settings">
            <select value={settings.translationTargetLanguage} onChange={(event) => onSave({ translationTargetLanguage: event.target.value })}>
              {settings.translationTargetLanguages.map((language) => {
                const option = translationLanguages.find(([code]) => code === language);
                return <option key={language} value={language}>{option ? t(option[1]) : language}</option>;
              })}
            </select>
            <ol className="translation-target-list">{settings.translationTargetLanguages.map((language, index) => {
              const option = translationLanguages.find(([code]) => code === language);
              return <li key={language}><span>{option ? t(option[1]) : language}</span>
                <button type="button" onClick={() => moveTargetLanguage(index, -1)} disabled={index === 0} aria-label={t("Move language up")}>↑</button>
                <button type="button" onClick={() => moveTargetLanguage(index, 1)} disabled={index === settings.translationTargetLanguages.length - 1} aria-label={t("Move language down")}>↓</button>
                <button type="button" onClick={() => removeTargetLanguage(language)} disabled={settings.translationTargetLanguages.length === 1}>{t("Remove")}</button>
              </li>;
            })}</ol>
            <div className="translation-target-add">
              <select value={languageToAdd} onChange={(event) => setLanguageToAdd(event.target.value)}>
                {translationLanguages
                  .filter(([code]) => !settings.translationTargetLanguages.includes(code))
                  .map(([code, label]) => <option key={code} value={code}>{t(label)}</option>)}
              </select>
              <button
                type="button"
                onClick={addTargetLanguage}
                disabled={settings.translationTargetLanguages.length >= translationLanguages.length || settings.translationTargetLanguages.includes(languageToAdd)}
              >
                {t("Add language")}
              </button>
            </div>
          </div>} />
        <SettingRow title={t("Translation instruction")} detail={t("Optional guidance appended to the fixed translation-only contract.")}
          control={<input value={translationInstruction} maxLength={500} onChange={(event) => setTranslationInstruction(event.target.value)} onBlur={() => {
            if (suppressTranslationInstructionBlurRef.current) { suppressTranslationInstructionBlurRef.current = false; return; }
            if (translationInstruction !== settings.translationInstruction) onSave({ translationInstruction });
          }} onKeyDown={(event) => {
            if (event.key === "Enter") { event.preventDefault(); if (translationInstruction !== settings.translationInstruction) onSave({ translationInstruction }); suppressTranslationInstructionBlurRef.current = true; event.currentTarget.blur(); }
            if (event.key === "Escape") { setTranslationInstruction(settings.translationInstruction); suppressTranslationInstructionBlurRef.current = true; event.currentTarget.blur(); }
          }} />} />
        <SettingRow title={t("Start with Windows")} detail={t("Launches Local Voice Input automatically when you sign in to Windows.")}
          control={<Toggle checked={settings.autoStart} onChange={(value) => onSave({ autoStart: value })} />} />
        <SettingRow title={t("Restore clipboard")} detail={t("Restore previous clipboard contents after successful paste.")}
          control={<Toggle checked={settings.clipboardRestore} onChange={(value) => onSave({ clipboardRestore: value })} />} />
        <SettingRow title={t("Noise suppression")} detail={t("Reduces steady fan and room noise after recording, without adding work to the live microphone callback.")}
          control={<select value={settings.noiseSuppression} onChange={(event) => onSave({ noiseSuppression: event.target.value as Settings["noiseSuppression"] })}>
            <option value="off">{t("Off")}</option><option value="low">{t("Low")}</option><option value="medium">{t("Medium")}</option><option value="high">{t("High")}</option>
          </select>} />
        <SettingRow title={t("Automatic gain")} detail={t("Targets a clear speech level after recording. Manual gain below is applied in addition.")}
          control={<Toggle checked={settings.automaticGain} onChange={(value) => onSave({ automaticGain: value })} />} />
        <SettingRow title={t("Input gain")} detail={t("Adjusts the processed microphone level from 25% to 400%.")}
          control={<label className="range-control"><input type="range" min="25" max="400" step="5" value={settings.inputGainPercent} onChange={(event) => onSave({ inputGainPercent: Number(event.target.value) })} /><output>{settings.inputGainPercent}%</output></label>} />
      </section>
      <AiCorrectionSettings settings={settings} onSave={onSave} />
    </div>
  );
}
