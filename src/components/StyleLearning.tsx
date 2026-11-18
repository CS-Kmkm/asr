import { useEffect, useRef, useState } from "react";
import { proposeStyleFromEdit, setLearnedStyleRule } from "../api";
import { useI18n, type MessageKey } from "../i18n";
import type { HistoryItem, Settings, StylePreference, DictionaryMode } from "../types";
import { RecentAppPicker } from "./RecentAppPicker";
import { ModeChoice } from "../pages/DictionaryPage";

const names: Record<StylePreference, MessageKey> = {
  formal: "Prefer polite endings", casual: "Prefer plain endings", bullets: "Prefer lists for enumerations", paragraphs: "Prefer connected paragraphs",
};

export function TeachStyle({ item }: { item: HistoryItem }) {
  const { t } = useI18n();
  const shown = item.processedText ?? item.transcriptText;
  const [draft, setDraft] = useState(shown);
  const [suggestions, setSuggestions] = useState<StylePreference[]>([]);
  const [scope, setScope] = useState(item.appCategory ? `category:${item.appCategory}` : "global");
  const [modes, setModes] = useState<DictionaryMode[]>(["dictate"]);
  const [busy, setBusy] = useState(false);
  const [message, setMessage] = useState("");
  const request = useRef(0);
  useEffect(() => () => { request.current += 1; }, []);
  async function propose() {
    const token = ++request.current;
    setBusy(true); setMessage("");
    try {
      const result = await proposeStyleFromEdit(item.id, draft);
      if (token === request.current) { setSuggestions(result); if (!result.length) setMessage(t("No supported style change found.")); }
    } catch { if (token === request.current) setMessage(t("Style preference could not be saved.")); }
    finally { if (token === request.current) setBusy(false); }
  }
  async function confirm(preference: StylePreference) {
    setBusy(true); setMessage("");
    try {
      await setLearnedStyleRule({ preference, scope, modes }, true);
      setSuggestions(current => current.filter(value => value !== preference));
      setMessage(t("Style preference saved. Enable Personalization to apply it."));
    } catch { setMessage(t("Style preference could not be saved.")); }
    finally { setBusy(false); }
  }
  return <details className="reprocess-controls">
    <summary>{t("Learn a style from my edit")}</summary>
    <p className="settings-note">{t("Suggests polite/plain endings and list/paragraph preferences only when the content stays the same. Only confirmed preference codes are stored; your edited text is not saved or sent to AI.")}</p>
    <textarea aria-label={t("Style edit")} maxLength={10000} value={draft} disabled={busy} onChange={event => {
      request.current += 1; setDraft(event.target.value); setSuggestions([]); setMessage("");
    }} />
    <button disabled={busy || draft === shown || !draft.trim()} onClick={() => void propose()}>{t("Suggest style preferences")}</button>
    {!!suggestions.length && <>
      <p>{t("Scope")}: {scope}</p>
      <button disabled={busy} onClick={() => setScope("global")}>{t("Global")}</button>
      <RecentAppPicker scopes={[]} onChoose={setScope} />
      <ModeChoice value={modes} onChange={setModes} context={t("Style preference")} />
      {suggestions.map(preference => <button key={preference} disabled={busy} onClick={() => void confirm(preference)}>{t("Confirm")}: {t(names[preference])}</button>)}
      <button disabled={busy} onClick={() => setSuggestions([])}>{t("Cancel")}</button>
    </>}
    {message && <p role="status">{message}</p>}
  </details>;
}

export function LearnedStyleList({ settings }: { settings: Settings }) {
  const { t } = useI18n();
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState(false);
  if (!settings.learnedStyleRules.length) return null;
  return <section className="panel">
    <h3>{t("Confirmed style preferences")}</h3>
    {!settings.personalizationEnabled && <p>{t("Style preference saved. Enable Personalization to apply it.")}</p>}
    {settings.learnedStyleRules.map((rule, index) => <div className="setting-row" key={`${rule.preference}-${rule.scope}-${rule.modes.join(",")}-${index}`}>
      <span>{t(names[rule.preference])} · {rule.scope} · {rule.modes.join(", ")}</span>
      <button disabled={busy} onClick={async () => {
        setBusy(true); setError(false);
        try { await setLearnedStyleRule(rule, false); } catch { setError(true); } finally { setBusy(false); }
      }}>{t("Delete")}</button>
    </div>)}
    {error && <p role="alert">{t("Style preference could not be saved.")}</p>}
  </section>;
}
