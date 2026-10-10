import { useEffect, useRef, useState } from "react";
import { listen } from "@tauri-apps/api/event";
import { SettingRow, Toggle } from "../components/ui";
import { AiCorrectionSettings } from "../components/AiCorrectionSettings";
import type { AudioDevice, AudioLevel, ScopedStyleProfile, Settings, ShortcutMode, StyleProfile } from "../types";
import { speechLocaleRegistry, uiLocaleRegistry } from "../types";
import { getShortcutWarning, startMicrophoneTest, stopMicrophoneTest } from "../api";
import { translateAppMessage, useI18n } from "../i18n";

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

const profileCategories = new Set(["browser", "email", "messaging", "development", "document", "other"]);

function isValidProfileScope(scope: string): boolean {
  if (Array.from(scope).length > 80 || scope.trim() !== scope) return false;
  if (scope.startsWith("app:")) return /^app:[a-z0-9_-]+$/.test(scope);
  if (scope.startsWith("category:")) return profileCategories.has(scope.slice("category:".length));
  return false;
}

function sameProfiles(left: ScopedStyleProfile[], right: ScopedStyleProfile[]): boolean {
  return left.length === right.length && left.every((item, index) => {
    const other = right[index];
    return item.scope === other.scope &&
      item.profile.formality === other.profile.formality &&
      item.profile.detail === other.profile.detail &&
      (item.profile.guidance ?? "") === (other.profile.guidance ?? "");
  });
}

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
  const { language, t } = useI18n();
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
  const [profiles, setProfiles] = useState(settings.scopedStyleProfiles);
  const savedProfilesRef = useRef(settings.scopedStyleProfiles);
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
    const previous = savedProfilesRef.current;
    const incoming = settings.scopedStyleProfiles;
    savedProfilesRef.current = incoming;
    // Every settings save returns a new array. Adopt it only while the local
    // draft has no unsaved edits, so an unrelated save cannot discard a new or
    // invalid scoped row.
    setProfiles((draft) => (sameProfiles(draft, previous) ? incoming : draft));
  }, [settings.scopedStyleProfiles]);

  function saveProfiles(next: ScopedStyleProfile[]) {
    setProfiles(next);
    if (next.every((item) => isValidProfileScope(item.scope)) &&
        new Set(next.map((item) => item.scope)).size === next.length) {
      onSave({ scopedStyleProfiles: next });
    }
  }

  const profileDraftIsInvalid = profiles.some((item) => !isValidProfileScope(item.scope)) ||
    new Set(profiles.map((item) => item.scope)).size !== profiles.length;

  function saveGlobalProfile(profile: StyleProfile | null) {
    onSave({ globalStyleProfile: profile });
  }

  useEffect(() => {
    mountedRef.current = true;
    let unlisten: Array<() => void> | undefined;
    let active = true;
    void Promise.all([
      listen<AudioLevel>("audio-level", ({ payload }) => setLevel(payload)),
      // Hiding the main window to the tray keeps this page mounted while the
      // backend releases the test, so reset the controls it left running.
      listen("microphone-test-stopped", () => {
        testRunningRef.current = false;
        setTestRunning(false);
        setLevel({ rms: 0, peak: 0 });
      }),
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

  function commitShortcuts() {
    const next = Object.fromEntries(Object.entries(shortcuts).map(([mode, chords]) => [mode, chords.map((chord) => chord.trim())])) as Settings["shortcuts"];
    if (Object.values(next).some((chords) => chords.length < 1 || chords.length > 4 || chords.some((chord) => !chord))) {
      setShortcutError(t("Each voice mode needs one to four non-empty shortcuts."));
      return;
    }
    const collisions = (voiceShortcuts: Settings["shortcuts"], selectedTextChord: string) => {
      const entries = [
        ...Object.entries(voiceShortcuts).flatMap(([mode, chords]) => chords.map((chord) => [chord.toLocaleLowerCase(), mode] as const)),
        [selectedTextChord.toLocaleLowerCase(), "selected-text-translate"] as const,
      ];
      const groups = new Map<string, string[]>();
      for (const [chord, action] of entries) groups.set(chord, [...(groups.get(chord) ?? []), action]);
      return groups;
    };
    const previousCollisions = collisions(settings.shortcuts, settings.translationHotkey);
    const nextCollisions = collisions(next, translationHotkey.trim());
    const collisionOrder = ["dictate", "selected-text-translate", "translate", "edit", "ask"];
    const newCollision = [...nextCollisions].find(([chord, actions]) => actions.length > 1
      && JSON.stringify([...actions].sort()) !== JSON.stringify([...(previousCollisions.get(chord) ?? [])].sort()));
    if (!translationHotkey.trim() || newCollision) {
      if (newCollision) {
        const orderedActions = [...newCollision[1]].sort((left, right) => collisionOrder.indexOf(left) - collisionOrder.indexOf(right));
        const losingAction = orderedActions[orderedActions.length - 1];
        const actionLabel = losingAction === "selected-text-translate" ? t("Selected-text translation hotkey")
          : losingAction === "dictate" ? t("Dictation shortcuts")
            : losingAction === "translate" ? t("Voice Translate shortcuts")
              : losingAction === "edit" ? t("Speak to edit shortcuts")
                : t("Ask Anything shortcuts");
        setShortcutError(`${t("This shortcut conflicts with")} ${actionLabel}.`);
      } else {
        setShortcutError(t("Shortcuts must be non-empty and unique across all actions."));
      }
      return;
    }
    setShortcutError(null);
    const nextTranslationHotkey = translationHotkey.trim();
    if (JSON.stringify(next) !== JSON.stringify(settings.shortcuts) || nextTranslationHotkey !== settings.translationHotkey) {
      onSave({ shortcuts: next, translationHotkey: nextTranslationHotkey });
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
        {testError && <p className="settings-error" role="alert">{translateAppMessage(language, testError)}</p>}
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
        {shortcutError && <p className="settings-error" role="alert">{shortcutError}</p>}
        {startupShortcutWarning && <p className="settings-error" role="alert">{t("Some saved shortcuts could not be activated at startup. Change them in Settings and restart to verify.")}</p>}
        <SettingRow title={t("Selected-text translation hotkey")} detail={t("Translates selected text; the default is Ctrl+Shift+T.")}
          control={<input value={translationHotkey} aria-label={t("Selected-text translation hotkey")} onChange={(event) => { setTranslationHotkey(event.target.value); setShortcutError(null); }} onKeyDown={(event) => {
            if (event.key === "Enter") { event.preventDefault(); commitShortcuts(); event.currentTarget.blur(); }
            if (event.key === "Escape") { setTranslationHotkey(settings.translationHotkey); setShortcutError(null); event.currentTarget.blur(); }
          }} />} />
        <button type="button" className="primary shortcut-save" onClick={commitShortcuts}>{t("Save shortcuts")}</button>
        <SettingRow title={t("Voice Translate target")} detail={`${t("The first language is the default. Reorder the list or choose the active target.")} ${t("Voice and selected-text Translate send text to the provider shown under AI text correction, even when correction is off.")} ${t("Provider:")} ${settings.correctionProvider === "openai" ? "OpenAI" : settings.correctionProvider === "gemini" ? "Google Gemini" : t("Local (OpenAI-compatible)")}.`}
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
        <SettingRow title={t("Personalization")} detail={t("Use manually configured abstract style profiles for the captured app category.")}
          control={<Toggle checked={settings.personalizationEnabled} onChange={(value) => onSave({ personalizationEnabled: value })} />} />
        <SettingRow title={t("Start with Windows")} detail={t("Launches Local Voice Input automatically when you sign in to Windows.")}
          control={<Toggle checked={settings.autoStart} onChange={(value) => onSave({ autoStart: value })} />} />
        <SettingRow title={t("Restore clipboard")} detail={t("Restore previous clipboard contents after successful paste.")}
          control={<Toggle checked={settings.clipboardRestore} onChange={(value) => onSave({ clipboardRestore: value })} />} />
        <SettingRow title={t("Live text insertion (experimental)")} detail={t("Off by default: Dictate inserts only the final text once. When on, partial text is typed into the target while you speak; some editors and browser fields may keep only the first part.")}
          control={<Toggle checked={settings.liveTargetInsertion} onChange={(value) => onSave({ liveTargetInsertion: value })} />} />
        <SettingRow title={t("Noise suppression")} detail={t("Reduces steady fan and room noise after recording, without adding work to the live microphone callback.")}
          control={<select value={settings.noiseSuppression} onChange={(event) => onSave({ noiseSuppression: event.target.value as Settings["noiseSuppression"] })}>
            <option value="off">{t("Off")}</option><option value="low">{t("Low")}</option><option value="medium">{t("Medium")}</option><option value="high">{t("High")}</option>
          </select>} />
        <SettingRow title={t("Automatic gain")} detail={t("Targets a clear speech level after recording. Manual gain below is applied in addition.")}
          control={<Toggle checked={settings.automaticGain} onChange={(value) => onSave({ automaticGain: value })} />} />
        <SettingRow title={t("Input gain")} detail={t("Adjusts the processed microphone level from 25% to 400%.")}
          control={<label className="range-control"><input type="range" min="25" max="400" step="5" value={settings.inputGainPercent} onChange={(event) => onSave({ inputGainPercent: Number(event.target.value) })} /><output>{settings.inputGainPercent}%</output></label>} />
      </section>
      <PersonalizationProfiles settings={settings} profiles={profiles} profileDraftIsInvalid={profileDraftIsInvalid} onSaveGlobal={saveGlobalProfile} onSaveProfiles={saveProfiles} />
      <AiCorrectionSettings settings={settings} onSave={onSave} />
    </div>
  );
}

function PersonalizationProfiles({
  settings,
  profiles,
  profileDraftIsInvalid,
  onSaveGlobal,
  onSaveProfiles,
}: {
  settings: Settings;
  profiles: ScopedStyleProfile[];
  profileDraftIsInvalid: boolean;
  onSaveGlobal: (profile: StyleProfile | null) => void;
  onSaveProfiles: (profiles: ScopedStyleProfile[]) => void;
}) {
  const { t } = useI18n();
  const global = settings.globalStyleProfile;
  const defaultGlobal: StyleProfile = { formality: "formal", detail: "concise", guidance: "" };
  const updateGlobal = (patch: Partial<StyleProfile>) => onSaveGlobal({ ...(global ?? defaultGlobal), ...patch });
  return (
    <section className="panel">
      <h2>{t("Personalization profiles")}</h2>
      <p>{t("Structured style settings are retained locally; no transcript examples are stored.")}</p>
      {!settings.personalizationEnabled && <p role="status">{t("Inactive: Personalization is off, so these profiles are not used.")}</p>}
      {!settings.textCorrectionEnabled && <p role="status">{t("Inactive: profiles are used only when AI text correction is on.")}</p>}
      <SettingRow title={t("Global profile")} detail={t("Fallback style used when no app or category profile matches.")} control={
        <div className="profile-controls">
          {global ? <>
            <select value={global.formality} onChange={(e) => updateGlobal({ formality: e.target.value as StyleProfile["formality"] })}><option value="formal">{t("Formal")}</option><option value="casual">{t("Casual")}</option></select>
            <select value={global.detail} onChange={(e) => updateGlobal({ detail: e.target.value as StyleProfile["detail"] })}><option value="concise">{t("Concise")}</option><option value="detailed">{t("Detailed")}</option></select>
            <input maxLength={300} placeholder={t("Optional guidance")} value={global.guidance ?? ""} onChange={(e) => updateGlobal({ guidance: e.target.value })} />
            <button className="secondary" onClick={() => onSaveGlobal(null)}>{t("Clear")}</button>
          </> : <>
            <span>{t("Not configured")}</span>
            <button className="secondary" onClick={() => onSaveGlobal(defaultGlobal)}>{t("Configure global profile")}</button>
          </>}
        </div>
      } />
      <div className="profile-list">
        <strong>{t("Scoped profiles")}</strong>
        <p>{t("Precedence is fixed: exact app > category > global. List order has no effect.")}</p>
        {profileDraftIsInvalid && <p role="alert">{t("Profile scopes must be valid and unique before changes are saved.")}</p>}
        {profiles.map((item, index) => (
          <div className="setting-row" key={index}>
            <input value={item.scope} maxLength={80} placeholder={t("app:code or category:development")} onChange={(e) => { const next = [...profiles]; next[index] = { ...item, scope: e.target.value }; onSaveProfiles(next); }} />
            <select value={item.profile.formality} onChange={(e) => { const next = [...profiles]; next[index] = { ...item, profile: { ...item.profile, formality: e.target.value as StyleProfile["formality"] } }; onSaveProfiles(next); }}><option value="formal">{t("Formal")}</option><option value="casual">{t("Casual")}</option></select>
            <select value={item.profile.detail} onChange={(e) => { const next = [...profiles]; next[index] = { ...item, profile: { ...item.profile, detail: e.target.value as StyleProfile["detail"] } }; onSaveProfiles(next); }}><option value="concise">{t("Concise")}</option><option value="detailed">{t("Detailed")}</option></select>
            <input maxLength={300} placeholder={t("Optional guidance")} value={item.profile.guidance ?? ""} onChange={(e) => { const next = [...profiles]; next[index] = { ...item, profile: { ...item.profile, guidance: e.target.value } }; onSaveProfiles(next); }} />
            <button className="secondary" onClick={() => onSaveProfiles(profiles.filter((_, i) => i !== index))}>{t("Remove")}</button>
          </div>
        ))}
        <button className="primary" onClick={() => onSaveProfiles([...profiles, { scope: "", profile: { formality: "formal", detail: "concise", guidance: "" } }])}>{t("Add scoped profile")}</button>
      </div>
    </section>
  );
}
