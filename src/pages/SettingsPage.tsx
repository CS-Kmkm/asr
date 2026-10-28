import { useEffect, useRef, useState, type KeyboardEvent } from "react";
import { listen } from "@tauri-apps/api/event";
import { ShortcutField, normalizeShortcutText } from "../components/ShortcutField";
import { GainSlider, StepSlider } from "../components/CommitField";
import { HelpHint, IconButton, SettingRow, Toggle } from "../components/ui";
import { AiCorrectionSettings } from "../components/AiCorrectionSettings";
import { RecentAppPicker } from "../components/RecentAppPicker";
import { stylePresets, profileCategories } from "../stylePresets";
import { SettingsRepairBanner } from "../components/SettingsRepairBanner";
import type { AudioDevice, AudioLevel, ScopedStyleProfile, Settings, ShortcutMode, StyleProfile } from "../types";
import { speechLocaleRegistry, translationLanguageRegistry, uiLocaleRegistry } from "../types";
import { getShortcutWarning, startMicrophoneTest, stopMicrophoneTest } from "../api";
import { translateAppMessage, useI18n } from "../i18n";

const translationLanguages = translationLanguageRegistry.map(({ code, label }) => [code, label] as const);

const shortcutModes: Array<{ mode: ShortcutMode; label: "Dictation shortcuts" | "Voice Translate shortcuts" | "Ask Anything shortcuts" | "Speak to edit shortcuts" }> = [
  { mode: "dictate", label: "Dictation shortcuts" },
  { mode: "translate", label: "Voice Translate shortcuts" },
  { mode: "ask", label: "Ask Anything shortcuts" },
  { mode: "edit", label: "Speak to edit shortcuts" },
];

/**
 * Whether a chord is plain Esc, the key that cancels a recording. A route the
 * user assigns to Esc keeps it (the app never takes a chord back), so Esc no
 * longer cancels and the overlay's × button is the cancel path.
 */
const ESC_ROUTE_WARNING = "Esc is also the cancel key while recording; with this assignment, cancel with the × button instead.";

export function isCancelKeyChord(chord: string): boolean {
  const key = chord.trim().toUpperCase();
  return key === "ESC" || key === "ESCAPE";
}

// Inactive shortcuts arrive as "Action (chord)" with the English action label.
const shortcutActionLabels = ["Dictation", "Selected-text translation", "Voice Translate", "Speak to edit", "Ask Anything"] as const;

function isValidProfileScope(scope: string): boolean {
  if (Array.from(scope).length > 80 || scope.trim() !== scope) return false;
  if (scope.startsWith("app:")) return /^app:[a-z0-9_-]+$/.test(scope);
  if (scope.startsWith("category:")) return profileCategories.some((category) => category === scope.slice("category:".length));
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

export type SettingsSection = "all" | "none" | "input" | "shortcuts" | "writing" | "general";

export function SettingsPage({
  settings,
  onSave,
  devices,
  recording,
  onRefreshDevices,
  section = "all",
}: {
  section?: SettingsSection;
  settings: Settings;
  onSave: (patch: Partial<Settings>) => void;
  devices: AudioDevice[];
  recording: boolean;
  onRefreshDevices?: () => void;
}) {
  const { language, t } = useI18n();
  const [shortcuts, setShortcuts] = useState(settings.shortcuts);
  const [translationHotkey, setTranslationHotkey] = useState(settings.translationHotkey);
  const [translationInstruction, setTranslationInstruction] = useState(settings.translationInstruction);
  const [testRunning, setTestRunning] = useState(false);
  const [testStopping, setTestStopping] = useState(false);
  const [testError, setTestError] = useState<string | null>(null);
  const [shortcutError, setShortcutError] = useState<string | null>(null);
  const [inactiveStartupShortcuts, setInactiveStartupShortcuts] = useState<string[]>([]);
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
    void getShortcutWarning().then(setInactiveStartupShortcuts).catch(() => {});
  }, [settings.shortcuts, settings.translationHotkey]);
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

  // Scope and guidance text stays a local draft until its field loses focus,
  // like the translation instruction, so typing does not save per keystroke.
  function commitProfiles() {
    if (!sameProfiles(profiles, savedProfilesRef.current)) saveProfiles(profiles);
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

  useEffect(() => {
    if (section !== "all" && section !== "input" && testRunningRef.current) void stopTest();
  }, [section]);

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
    if ([...Object.values(shortcuts).flat(), translationHotkey].some((chord) => !normalizeShortcutText(chord))) { setShortcutError(t("Unsupported shortcut key")); return; }
    const next = Object.fromEntries(Object.entries(shortcuts).map(([mode, chords]) => [mode, chords.map((chord) => normalizeShortcutText(chord) ?? chord.trim())])) as Settings["shortcuts"];
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
    const nextTranslationHotkey = normalizeShortcutText(translationHotkey)!;
    if (JSON.stringify(next) !== JSON.stringify(settings.shortcuts) || nextTranslationHotkey !== settings.translationHotkey) {
      onSave({ shortcuts: next, translationHotkey: nextTranslationHotkey });
    }
  }

  const targetLanguage = settings.translationTargetLanguage;
  const sourceLanguage = settings.translationTargetLanguages.find(code => code !== targetLanguage)
    ?? translationLanguages.find(([code]) => code !== targetLanguage)![0];

  function saveTranslationPair(source: string, target: string) {
    if (source === target) return;
    onSave({ translationTargetLanguages: [source, target], translationTargetLanguage: target });
  }

  const levelPercent = Math.round(Math.max(0, Math.min(1, Math.max(level.rms * 3.5, level.peak))) * 100);

  return (
    <div className="settings-stack" hidden={section === "none"}>
      <SettingsRepairBanner settings={settings} />
      <section className="panel" hidden={section !== "all" && section !== "input"}>
        {section === "all" && <h2>{t("Input settings")}</h2>}
        <SettingRow title={t("Speech language")} detail={t("Only the base language is used. VibeVoice ignores this setting.")}
          detailMode={settings.asrBackend === "vibevoice" ? "inline" : "hint"}
          control={<select value={settings.speechLocale ?? "auto"} onChange={(event) => onSave({ speechLocale: event.target.value === "auto" ? null : event.target.value as Settings["speechLocale"] })}>
            <option value="auto">{t("Automatic detection")}</option>
            {speechLocaleRegistry.map(({ tag, label }) => <option key={tag} value={tag}>{t(label)}</option>)}
          </select>} />
        <SettingRow title={t("Microphone")}
          control={<div className="inline-control"><select value={settings.microphoneId ?? ""} onChange={(event) => {
            if (testRunningRef.current) void stopTest();
            onSave({ microphoneId: event.target.value || null });
          }}>
            <option value="">{t("System default")}</option>
            {settings.microphoneId && !devices.some((device) => device.id === settings.microphoneId) && <option value={settings.microphoneId}>{t("Unavailable device")}</option>}
            {devices.map((device) => <option key={device.id} value={device.id}>{device.name}{device.isDefault ? ` (${t("Default")})` : ""}</option>)}
          </select>{onRefreshDevices && <IconButton icon="refresh" label={t("Refresh microphones")} onClick={onRefreshDevices} />}</div>} />
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

        <SettingRow title={t("Restore clipboard")} detail={t("Restore previous clipboard contents after successful paste.")}
          control={<Toggle label={t("Restore clipboard")} checked={settings.clipboardRestore} onChange={(value) => onSave({ clipboardRestore: value })} />} />
        <SettingRow title={t("Live text insertion (experimental)")} detail={t("Partial insertion may leave incomplete text in some apps.")}
          detailMode="inline"
          help={t("Off by default: Dictate inserts only the final text once. When on, partial text is typed into the target while you speak; some editors and browser fields may keep only the first part.")}
          control={<Toggle label={t("Live text insertion (experimental)")} checked={settings.liveTargetInsertion} onChange={(value) => onSave({ liveTargetInsertion: value })} />} />
        <SettingRow title={t("Noise suppression")}
          control={<StepSlider label={t("Noise suppression")} value={settings.noiseSuppression}
            options={[{ value: "off", label: t("Off") }, { value: "low", label: t("Low") }, { value: "medium", label: t("Medium") }, { value: "high", label: t("High") }] as const}
            onCommit={noiseSuppression => onSave({ noiseSuppression })} />} />
        <SettingRow title={t("Automatic gain")} detail={t("Targets a clear speech level after recording. Manual gain below is applied in addition.")}
          control={<Toggle label={t("Automatic gain")} checked={settings.automaticGain} onChange={(value) => onSave({ automaticGain: value })} />} />
        <SettingRow title={t("Input gain")}
          control={<GainSlider label={t("Input gain")} value={settings.inputGainPercent} onCommit={(value) => onSave({ inputGainPercent: value })} />} />
      </section>
      <section className="panel" hidden={section !== "all" && section !== "shortcuts"}>
        {section === "all" && <h2>{t("Voice mode shortcuts")}</h2>}
        {shortcutModes.map(({ mode, label }) => (
          <SettingRow key={mode} title={t(label)}
            detail={mode === "edit" ? t("Uses provider even with correction off: selected text and instruction. ASR may send audio.")
              : mode === "ask" ? t("Uses provider even with correction off: instruction and any used selection. ASR may send audio.") : undefined}
            detailMode="inline"
            help={[mode === "edit"
            ? t("Speak to edit sends selected source text and the transcribed spoken instruction to the provider shown under AI text correction, even when correction is off. The selected ASR backend may send audio.")
            : mode === "ask"
              ? t("Ask sends only the transcribed spoken instruction to the selected provider for planning. Answer generation using a selection also sends the selected source text with the instruction, even when Dictation AI correction is off. The selected ASR backend may send audio.")
              : undefined,
              t("Hold to talk waits about 200 ms before recording and stops on key release. Turn it off to toggle recording with two presses."),
              t("Add one to four keyboard shortcuts for each voice mode. A shortcut must be unique across all actions.")].filter(Boolean).join("\n\n")}
            control={<div className="shortcut-list">
              <div className="shortcut-toolbar">
                <label className="hold-control"><span>{t("Hold to talk")}</span><Toggle checked={settings.holdToTalk[mode]} label={`${t(label)}: ${t("Hold to talk")}`} onChange={(enabled) => onSave({ holdToTalk: { ...settings.holdToTalk, [mode]: enabled } })} /></label>
                <IconButton icon="add" label={t("Add shortcut")} disabled={shortcuts[mode].length >= 4} onClick={() => addShortcut(mode)} />
              </div>
              {shortcuts[mode].map((chord, index) => <div className="shortcut-item" key={`${mode}-${index}`}>
                <ShortcutField label={`${t(label)} ${index + 1}`} value={chord} onChange={(value) => setShortcut(mode, index, value)} conflict={[...Object.entries(shortcuts).flatMap(([other, values]) => values.filter((_, otherIndex) => other !== mode || otherIndex !== index)), translationHotkey].some((other) => normalizeShortcutText(other) !== null && normalizeShortcutText(other) === normalizeShortcutText(chord))} />
                <IconButton icon="trash" label={`${t("Remove shortcut")} ${chord || index + 1}`} disabled={shortcuts[mode].length <= 1} onClick={() => removeShortcut(mode, index)} />
                {isCancelKeyChord(chord) && <p className="settings-warning" role="note">{t(ESC_ROUTE_WARNING)}</p>}
              </div>)}
            </div>} />
        ))}
        {shortcutError && <p className="settings-error" role="alert">{shortcutError}</p>}
        {inactiveStartupShortcuts.length > 0 && <p className="settings-error" role="alert">{t("Some saved shortcuts could not be activated at startup. Change them in Settings and restart to verify.")} {t("Inactive shortcuts:")} {inactiveStartupShortcuts.map((entry) => {
          const action = shortcutActionLabels.find((label) => entry.startsWith(`${label} (`));
          return action ? `${t(action)}${entry.slice(action.length)}` : entry;
        }).join(", ")}</p>}
        <SettingRow title={t("Selected-text translation hotkey")} detail={t("Translates selected text; the default is Ctrl+Shift+T.")}
          control={<div className="shortcut-item">
            <ShortcutField value={translationHotkey} label={t("Selected-text translation hotkey")} onChange={(value) => { setTranslationHotkey(value); setShortcutError(null); }} onKeyDown={(event) => {
              if (event.nativeEvent.isComposing || event.keyCode === 229) return;
              if (event.key === "Enter") { event.preventDefault(); commitShortcuts(); event.currentTarget.blur(); }
              if (event.key === "Escape") { setTranslationHotkey(settings.translationHotkey); setShortcutError(null); event.currentTarget.blur(); }
            }} />
            {isCancelKeyChord(translationHotkey) && <p className="settings-warning" role="note">{t(ESC_ROUTE_WARNING)}</p>}
          </div>} />
        {(JSON.stringify(shortcuts) !== JSON.stringify(settings.shortcuts) || translationHotkey !== settings.translationHotkey) && <p role="status">{t("Unsaved shortcut changes")}</p>}
        <IconButton icon="save" label={t("Save shortcuts")} className="primary shortcut-save" onClick={commitShortcuts} />
      </section>
      <section className="panel" hidden={section !== "all" && section !== "writing"}>
        {section === "all" && <h2>{t("Writing and translation")}</h2>}
        <SettingRow title={t("Translation languages")} detail={`${t("Provider:")} ${settings.correctionProvider === "openai" ? "OpenAI" : settings.correctionProvider === "gemini" ? "Google Gemini" : t("Local (OpenAI-compatible)")}`}
          detailMode="inline"
          help={[t("Choose two languages to switch between. The input language is detected automatically; swapping changes the translation target."), t("Translation uses this provider even with correction off.")].join("\n\n")}
          control={<div className="translation-pair">
            <select aria-label={t("Translation source language")} value={sourceLanguage} onChange={event => saveTranslationPair(event.target.value, targetLanguage)}>
              {translationLanguages.filter(([code]) => code !== targetLanguage).map(([code, label]) => <option key={code} value={code}>{t(label)}</option>)}
            </select>
            <span className="translation-direction" aria-hidden="true">→</span>
            <select aria-label={t("Current translation target")} value={targetLanguage} onChange={event => saveTranslationPair(sourceLanguage, event.target.value)}>
              {translationLanguages.filter(([code]) => code !== sourceLanguage).map(([code, label]) => <option key={code} value={code}>{t(label)}</option>)}
            </select>
            <IconButton icon="swap" label={t("Swap languages")} onClick={() => saveTranslationPair(targetLanguage, sourceLanguage)} />
          </div>} />
        <SettingRow title={t("Translation instruction")} detail={t("Optional guidance appended to the fixed translation-only contract.")}
          control={<input value={translationInstruction} maxLength={500} onChange={(event) => setTranslationInstruction(event.target.value)} onBlur={() => {
            if (suppressTranslationInstructionBlurRef.current) { suppressTranslationInstructionBlurRef.current = false; return; }
            if (translationInstruction !== settings.translationInstruction) onSave({ translationInstruction });
          }} onKeyDown={(event) => {
              if (event.nativeEvent.isComposing || event.keyCode === 229) return;
            if (event.key === "Enter") { event.preventDefault(); if (translationInstruction !== settings.translationInstruction) onSave({ translationInstruction }); suppressTranslationInstructionBlurRef.current = true; event.currentTarget.blur(); }
            if (event.key === "Escape") { setTranslationInstruction(settings.translationInstruction); suppressTranslationInstructionBlurRef.current = true; event.currentTarget.blur(); }
          }} />} />
      </section>
      <div hidden={section !== "all" && section !== "writing"} className="settings-stack">
      <PersonalizationProfiles settings={settings} profiles={profiles} profileDraftIsInvalid={profileDraftIsInvalid} onToggle={value => onSave({ personalizationEnabled: value })} onSaveGlobal={saveGlobalProfile} onSaveProfiles={saveProfiles} onEditProfiles={setProfiles} onCommitProfiles={commitProfiles} />
      <AiCorrectionSettings settings={settings} onSave={onSave} />
      </div>
      <section className="panel" hidden={section !== "all" && section !== "general"}>
        {section === "all" && <h2>{t("General")}</h2>}
        <SettingRow title={t("Language")}
          control={<select value={settings.uiLanguage} onChange={(event) => onSave({ uiLanguage: event.target.value as Settings["uiLanguage"] })}>
            {uiLocaleRegistry.map(({ tag, label }) => <option key={tag} value={tag}>{t(label)}</option>)}
          </select>} />
        <SettingRow title={t("Appearance")}
          control={<select className="appearance-select" value={settings.theme} onChange={(event) => onSave({ theme: event.target.value as Settings["theme"] })}>
            <option value="system">{t("System")}</option><option value="light">{t("Light")}</option><option value="dark">{t("Dark")}</option>
          </select>} />
        <SettingRow title={t("Start with Windows")}
          control={<Toggle label={t("Start with Windows")} checked={settings.autoStart} onChange={(value) => onSave({ autoStart: value })} />} />
      </section>

    </div>
  );
}

function PersonalizationProfiles({
  settings,
  profiles,
  profileDraftIsInvalid,
  onSaveGlobal,
  onSaveProfiles,
  onEditProfiles,
  onCommitProfiles,
  onToggle,
}: {
  settings: Settings;
  profiles: ScopedStyleProfile[];
  profileDraftIsInvalid: boolean;
  onSaveGlobal: (profile: StyleProfile | null) => void;
  onSaveProfiles: (profiles: ScopedStyleProfile[]) => void;
  /** Updates the scoped-profile draft without saving it. */
  onEditProfiles: (profiles: ScopedStyleProfile[]) => void;
  /** Saves the scoped-profile draft when it differs from the saved profiles. */
  onCommitProfiles: () => void;
  onToggle: (enabled: boolean) => void;
}) {
  const { t } = useI18n();
  const global = settings.globalStyleProfile;
  const defaultGlobal: StyleProfile = { formality: "formal", detail: "concise", guidance: "" };
  const updateGlobal = (patch: Partial<StyleProfile>) => onSaveGlobal({ ...(global ?? defaultGlobal), ...patch });
  // The guidance text is a local draft saved on blur or Enter, like the
  // translation instruction: a save per keystroke lets an older save's
  // acknowledgement re-render the field with stale text.
  const savedGuidance = global?.guidance ?? "";
  const [guidance, setGuidance] = useState(savedGuidance);
  const savedGuidanceRef = useRef(savedGuidance);
  const suppressGuidanceBlurRef = useRef(false);
  useEffect(() => {
    const previous = savedGuidanceRef.current;
    savedGuidanceRef.current = savedGuidance;
    // Adopt a saved change only while the draft has no unsaved edit.
    setGuidance((draft) => (draft === previous ? savedGuidance : draft));
  }, [savedGuidance]);
  const commitGuidance = () => {
    if (global && guidance !== savedGuidance) updateGlobal({ guidance });
  };
  const commitOnEnter = (event: KeyboardEvent<HTMLInputElement>) => {
    if (event.nativeEvent.isComposing || event.keyCode === 229) return;
    if (event.key === "Enter") { event.preventDefault(); event.currentTarget.blur(); }
  };
  return (
    <section className="panel personalization-panel">
      <div className="section-heading personalization-heading">
        <div className="setting-title"><h2>{t("Personalization profiles")}</h2><HelpHint label={t("Personalization profiles")}>{[t("Structured style settings are retained locally; no transcript examples are stored."), t("For Dictate, profiles require AI text correction. Ask and Edit use them when Personalization is on.")].join("\n\n")}</HelpHint></div>
        <Toggle label={t("Personalization")} checked={settings.personalizationEnabled} onChange={onToggle} />
      </div>
      <div hidden={!settings.personalizationEnabled}>
      <SettingRow title={t("Global profile")} detail={t("Fallback style used when no app or category profile matches.")} control={
        <div className="profile-controls global-profile-controls">
          <div className="profile-preset-actions"><select aria-label={t("Global style preset")} value="" onChange={(event) => {
            const preset = stylePresets.find((preset) => preset.id === event.target.value);
            if (preset) { setGuidance(preset.profile.guidance); onSaveGlobal({ ...preset.profile }); }
          }}>
            <option value="">{t("Apply a preset")}</option>
            {stylePresets.map((preset) => <option key={preset.id} value={preset.id}>{t(preset.label)}</option>)}
          </select>
          {global ? <IconButton icon="trash" label={t("Clear")} onClick={() => onSaveGlobal(null)} />
            : <IconButton icon="add" label={t("Configure global profile")} onClick={() => onSaveGlobal(defaultGlobal)} />}
          </div>
          {global ? <>
            <select aria-label={t("Global profile formality")} value={global.formality} onChange={(e) => updateGlobal({ formality: e.target.value as StyleProfile["formality"] })}><option value="formal">{t("Formal")}</option><option value="casual">{t("Casual")}</option></select>
            <select aria-label={t("Global profile detail")} value={global.detail} onChange={(e) => updateGlobal({ detail: e.target.value as StyleProfile["detail"] })}><option value="concise">{t("Concise")}</option><option value="detailed">{t("Detailed")}</option></select>
            <input aria-label={t("Global profile guidance")} maxLength={300} placeholder={t("Optional guidance")} value={guidance} onChange={(e) => setGuidance(e.target.value)} onBlur={() => {
              if (suppressGuidanceBlurRef.current) { suppressGuidanceBlurRef.current = false; return; }
              commitGuidance();
            }} onKeyDown={(event) => {
              if (event.nativeEvent.isComposing || event.keyCode === 229) return;
              if (event.key === "Enter") { event.preventDefault(); commitGuidance(); suppressGuidanceBlurRef.current = true; event.currentTarget.blur(); }
              if (event.key === "Escape") { setGuidance(savedGuidance); suppressGuidanceBlurRef.current = true; event.currentTarget.blur(); }
            }} />
          </> : null}
        </div>
      } />
      <div className="profile-list">
        <div className="setting-title"><strong>{t("Scoped profiles")}</strong><HelpHint label={t("Scoped profiles")}>{t("Precedence is fixed: exact app > category > global. List order has no effect.")}</HelpHint></div>
        <RecentAppPicker scopes={profiles.map((entry) => entry.scope)} onChoose={(scope) => onSaveProfiles([...profiles, { scope, profile: { ...defaultGlobal } }])} />
        {profileDraftIsInvalid && <p role="alert">{t("Profile scopes must be valid and unique before changes are saved.")}</p>}
        {profiles.map((item, index) => (
          <div className="setting-row" key={index}>
            <label className="profile-field"><span>{t("Applies to")}</span>
              <input aria-label={`${t("Applies to")} ${index + 1}`} value={item.scope} maxLength={80} placeholder={t("app:code or category:development")} onChange={(e) => { const next = [...profiles]; next[index] = { ...item, scope: e.target.value }; onEditProfiles(next); }} onBlur={onCommitProfiles} onKeyDown={commitOnEnter} />
            </label>
            <label className="profile-field"><span>{t("Preset")}</span>
              <select aria-label={`${t("Scoped style preset")} ${index + 1}`} value="" onChange={(event) => {
                const preset = stylePresets.find((preset) => preset.id === event.target.value);
                if (preset) onSaveProfiles(profiles.map((entry, current) => current === index ? { ...entry, profile: { ...preset.profile } } : entry));
              }}>
                <option value="">{t("Apply a preset")}</option>
                {stylePresets.map((preset) => <option key={preset.id} value={preset.id}>{t(preset.label)}</option>)}
              </select>
            </label>
            <label className="profile-field"><span>{t("Formality")}</span>
              <select aria-label={`${t("Profile formality")} ${index + 1}`} value={item.profile.formality} onChange={(e) => { const next = [...profiles]; next[index] = { ...item, profile: { ...item.profile, formality: e.target.value as StyleProfile["formality"] } }; onSaveProfiles(next); }}><option value="formal">{t("Formal")}</option><option value="casual">{t("Casual")}</option></select>
            </label>
            <label className="profile-field"><span>{t("Level of detail")}</span>
              <select aria-label={`${t("Level of detail")} ${index + 1}`} value={item.profile.detail} onChange={(e) => { const next = [...profiles]; next[index] = { ...item, profile: { ...item.profile, detail: e.target.value as StyleProfile["detail"] } }; onSaveProfiles(next); }}><option value="concise">{t("Concise")}</option><option value="detailed">{t("Detailed")}</option></select>
            </label>
            <label className="profile-field wide"><span>{t("Guidance")}</span>
              <input aria-label={`${t("Optional guidance")} ${index + 1}`} maxLength={300} placeholder={t("Optional guidance")} value={item.profile.guidance ?? ""} onChange={(e) => { const next = [...profiles]; next[index] = { ...item, profile: { ...item.profile, guidance: e.target.value } }; onEditProfiles(next); }} onBlur={onCommitProfiles} onKeyDown={commitOnEnter} />
            </label>
            <IconButton icon="trash" className="profile-remove" label={t("Remove")} context={item.scope || String(index + 1)} onClick={() => onSaveProfiles(profiles.filter((_, i) => i !== index))} />
          </div>
        ))}
        <IconButton icon="add" label={t("Add scoped profile")} className="primary" onClick={() => onSaveProfiles([...profiles, { scope: "", profile: { formality: "formal", detail: "concise", guidance: "" } }])} />
      </div>
      </div>
    </section>
  );
}
