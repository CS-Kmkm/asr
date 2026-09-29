import { useEffect, useRef, useState } from "react";
import { SettingRow, Toggle } from "../components/ui";
import { AiCorrectionSettings } from "../components/AiCorrectionSettings";
import type { ScopedStyleProfile, Settings, StyleProfile } from "../types";
import { useI18n } from "../i18n";

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
}: {
  settings: Settings;
  onSave: (patch: Partial<Settings>) => void;
}) {
  const { t } = useI18n();
  const [hotkey, setHotkey] = useState(settings.hotkey);
  const [translationHotkey, setTranslationHotkey] = useState(settings.translationHotkey);
  const [translationInstruction, setTranslationInstruction] = useState(settings.translationInstruction);
  const [profiles, setProfiles] = useState(settings.scopedStyleProfiles);
  const savedProfilesRef = useRef(settings.scopedStyleProfiles);
  const cancelHotkeyBlurRef = useRef(false);
  const suppressHotkeyBlurRef = useRef(false);
  const cancelTranslationBlurRef = useRef(false);
  const suppressTranslationBlurRef = useRef(false);
  const cancelTranslationInstructionBlurRef = useRef(false);
  const suppressTranslationInstructionBlurRef = useRef(false);

  useEffect(() => setHotkey(settings.hotkey), [settings.hotkey]);
  useEffect(() => setTranslationHotkey(settings.translationHotkey), [settings.translationHotkey]);
  useEffect(() => setTranslationInstruction(settings.translationInstruction), [settings.translationInstruction]);
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
          title={t("Personalization")}
          detail={t("Use manually configured abstract style profiles for the captured app category.")}
          control={<Toggle checked={settings.personalizationEnabled} onChange={(value) => onSave({ personalizationEnabled: value })} />}
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
