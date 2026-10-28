import { useEffect, useState } from "react";
import { Empty, HelpHint, IconButton } from "../components/ui";
import { HistoryRetentionSelect } from "../components/HistoryRetentionSelect";
import { translationLanguageRegistry, type HistoryAudioPayload, type HistoryFilter, type HistoryItem, type HistoryTextAction, type Settings } from "../types";
import { insertionDetailLabels, insertionOutcomeLabels, useI18n } from "../i18n";

const filters = [
  { value: "all", label: "All", icon: "all" }, { value: "dictate", label: "Dictate", icon: "microphone" },
  { value: "translate", label: "Translate", icon: "translate" }, { value: "edit", label: "Edit", icon: "writing" },
  { value: "ask", label: "Ask", icon: "ask" },
] as const;

// The browser may read a blob download after click() returns, so revoking the
// URL synchronously can cancel the download.
const DOWNLOAD_URL_REVOKE_DELAY_MS = 30_000;

const modeLabels = {
  faithful: "Dictation (faithful)",
  ai_corrected: "Dictation (AI corrected)",
  faithful_fallback: "Dictation (AI fallback)",
  translate: "Voice Translate",
  edit: "Edit",
  ask: "Ask Anything",
  selected_translate: "Selected-text translation",
} as const;

export function HistoryPage({ settings, settingsLoaded, history, filter, query = "", loading = false, failed = false, hasMore = false, onSearch, onLoadMore, onReprocess, reprocessing = false, reprocessedText, onDismissResult, onSave, onFilter, onCopyItem, onRetry, onDelete, onDeleteAll, onLoadAudio, onAudioError, retryActive, onCancelRetry }: {
  settings: Settings;
  settingsLoaded: boolean;
  history: HistoryItem[];
  filter: HistoryFilter;
  query?: string;
  loading?: boolean;
  failed?: boolean;
  hasMore?: boolean;
  onSearch?: (query: string) => void;
  onLoadMore?: () => void;
  onReprocess?: (id: number, action: HistoryTextAction) => void;
  reprocessing?: boolean;
  reprocessedText?: string;
  onDismissResult?: () => void;
  onSave: (patch: Partial<Settings>) => void;
  onFilter: (filter: HistoryFilter) => void;
  onCopyItem: (text: string) => void;
  onRetry: (id: number) => void;
  onDelete: (id: number) => void;
  onDeleteAll: () => void;
  onLoadAudio: (id: number) => Promise<HistoryAudioPayload>;
  onAudioError: () => void;
  retryActive: boolean;
  onCancelRetry: () => void;
}) {
  const { t } = useI18n();
  const [searchDraft, setSearchDraft] = useState(query);
  useEffect(() => setSearchDraft(query), [query]);
  const [audioUrl, setAudioUrl] = useState<string | null>(null);
  const [playingId, setPlayingId] = useState<number | null>(null);
  useEffect(() => () => { if (audioUrl) URL.revokeObjectURL(audioUrl); }, [audioUrl]);
  useEffect(() => {
    // With History off, the list holds only failed takes kept for Retry.
    if (playingId !== null && !history.some((item) => item.id === playingId)) {
      if (audioUrl) URL.revokeObjectURL(audioUrl);
      setAudioUrl(null);
      setPlayingId(null);
    }
  }, [audioUrl, history, playingId, settings.historyRetention]);

  async function audio(item: HistoryItem, download: boolean) {
    let url: string;
    let filename: string;
    try {
      const payload = await onLoadAudio(item.id);
      url = URL.createObjectURL(new Blob([payload.bytes], { type: payload.mimeType }));
      filename = payload.filename;
    } catch {
      // A missing or unreadable recording clears its association on the
      // backend; the caller reports it and refreshes the list.
      onAudioError();
      return;
    }
    if (download) {
      const anchor = document.createElement("a");
      anchor.href = url; anchor.download = filename; anchor.click();
      window.setTimeout(() => URL.revokeObjectURL(url), DOWNLOAD_URL_REVOKE_DELAY_MS);
    } else {
      if (audioUrl) URL.revokeObjectURL(audioUrl);
      setAudioUrl(url);
      setPlayingId(item.id);
    }
  }

  return <section className="panel compact-page-panel">
    <div className="history-toolbar">
      {onSearch && <form onSubmit={(event) => { event.preventDefault(); onSearch(searchDraft.trim()); }}>
        <input type="search" placeholder={t("Search history")} aria-label={t("Search history")} maxLength={500} value={searchDraft} onChange={(event) => setSearchDraft(event.target.value)} />
        <IconButton type="submit" icon="search" label={t("Search")} />
      </form>}
      <div className="icon-options" role="group" aria-label={t("History filter")}>
        {filters.map(({ value, label, icon }) => <IconButton key={value} icon={icon} label={t(label)} aria-pressed={filter === value} onClick={() => onFilter(value)} />)}
      </div>
      <details className="history-management"><summary>{t("Manage history")}</summary><div className="management-content"><HistoryRetentionSelect value={settings.historyRetention} disabled={!settingsLoaded} onChange={(historyRetention) => onSave({ historyRetention })} />
      <IconButton icon="trash" label={t("Delete all")} className="danger-button" onClick={() => { if (window.confirm(t("Delete all history and suggested spellings?"))) onDeleteAll(); }} /></div></details>
      {retryActive && <button className="secondary" onClick={onCancelRetry}>{t("Cancel")}</button>}
    </div>

    {reprocessedText !== undefined && <section className="history-item" aria-label={t("Reprocessed result")}>
      <strong>{t("Reprocessed result")}</strong>
      <HistoryText text={reprocessedText} onCopy={onCopyItem} corrected />
      <div className="history-actions">
        <IconButton icon="copy" label={t("Copy result")} onClick={() => onCopyItem(reprocessedText)} />
        {onDismissResult && <IconButton icon="close" label={t("Dismiss result")} onClick={onDismissResult} />}
      </div>
    </section>}
    {audioUrl && <audio className="history-player" src={audioUrl} controls autoPlay onError={onAudioError} />}
    {loading && <p role="status">{t("Loading...")}</p>}
    {failed && <p role="alert">{t("History could not be loaded.")} {onSearch && <IconButton icon="refresh" label={t("Reload history")} onClick={() => onSearch(query)} />}</p>}
    {history.length === 0 && (loading || failed) ? null
      : history.length === 0 && query ? <Empty icon="search" title={t("No matching history")} />
      : settings.historyRetention === "never" && history.length === 0 ? <Empty icon="history" title={t("History is disabled")} detail={settings.keepFailedTakes
      ? t("New transcripts are not saved. A failed recording (and, for Edit, the selected text) is kept for 24 hours so you can retry it.")
      : t("New transcripts will not be written to SQLite.")} />
      : history.length === 0 ? <Empty icon="microphone" title={t("No dictations yet")} />
      : <>{settings.historyRetention === "never" && <p className="muted">{t("History is off. Failed recordings (and, for Edit, the selected text) are kept here for 24 hours so you can retry them.")}</p>}
      <div className="history-list">{history.map(item => {
          const context = `${new Date(item.createdAt).toLocaleString()} · ${(item.processedText || item.transcriptText || item.sourceText || item.instructionText || "").slice(0, 80)}`;
          return <article className="history-item" key={item.id}>
          <div className="history-meta"><time dateTime={item.createdAt}>{new Date(item.createdAt).toLocaleString()}</time>
            <span className="history-mode">{item.mode in modeLabels
              ? t(modeLabels[item.mode as keyof typeof modeLabels])
              : item.mode}{item.targetLanguage ? ` · ${item.targetLanguage}` : ""}</span>
            {item.appCategory && <small>{t("App category")}: {item.appCategory}</small>}
            {item.insertionResult && <InsertionBadge result={item.insertionResult} detail={item.insertionDetail} />}
            {item.expiresAt && <small>{t("Deleted automatically:")} {new Date(item.expiresAt).toLocaleString()}</small>}</div>
          {item.sourceText && <HistoryText text={item.sourceText} label={t("Selected text")} onCopy={onCopyItem} />}
          {item.instructionText && <HistoryText text={item.instructionText} label={t("Spoken instruction")} onCopy={onCopyItem} />}
          {!item.sourceText && !item.instructionText && <HistoryText text={item.transcriptText} onCopy={onCopyItem} />}
          {item.processedText && <HistoryText text={item.processedText} onCopy={onCopyItem} corrected />}
          <div className="history-actions">
            <IconButton icon="copy" label={t("Copy")} context={context} onClick={() => onCopyItem(item.processedText ?? item.transcriptText)} />
            <IconButton icon="raw" label={t("Copy raw")} context={context} disabled={!item.transcriptText} onClick={() => onCopyItem(item.transcriptText)} />

            <IconButton icon="retry" label={t("Retry")} context={context} disabled={!item.hasAudio || reprocessing} onClick={() => onRetry(item.id)} />
            <IconButton icon="play" label={t("Play")} context={context} disabled={!item.hasAudio} onClick={() => void audio(item, false)} />
            <IconButton icon="download" label={t("Download")} context={context} disabled={!item.hasAudio} onClick={() => void audio(item, true)} />
            <IconButton icon="trash" label={t("Delete")} context={context} className="danger-button" onClick={() => onDelete(item.id)} />
          </div>
          {onReprocess && <HistoryReprocessing settings={settings} settingsLoaded={settingsLoaded} item={item} busy={reprocessing} onReprocess={onReprocess} />}
        </article>; })}</div></>}
    {hasMore && onLoadMore && <button className="secondary load-more" disabled={loading} onClick={onLoadMore}>{t("Load more")}</button>}
  </section>;
}

function HistoryReprocessing({ settings, settingsLoaded, item, busy, onReprocess }: {
  settings: Settings;
  settingsLoaded: boolean;
  item: HistoryItem;
  busy: boolean;
  onReprocess: (id: number, action: HistoryTextAction) => void;
}) {
  const { t } = useI18n();
  const [target, setTarget] = useState(settings.translationTargetLanguage);
  const [profile, setProfile] = useState("");
  const effectiveTarget = settings.translationTargetLanguages.includes(target) ? target : settings.translationTargetLanguage;
  const effectiveProfile = profile === "global" ? (settings.globalStyleProfile ? profile : "")
    : settings.scopedStyleProfiles.some(entry => entry.scope === profile) ? profile : "";
  const disabled = !settingsLoaded || busy || !item.transcriptText.trim();
  return <details className="reprocess-controls">
    <summary>{t("Reprocess text")}</summary>
    <div className="reprocess-actions">
      <div className="reprocess-action"><label>{t("Correction profile")} <select value={effectiveProfile} disabled={busy} onChange={event => setProfile(event.target.value)}>
        <option value="">{t("Current profile")}</option>
        {settings.globalStyleProfile && <option value="global">{t("Global")}</option>}
        {settings.scopedStyleProfiles.map(entry => <option key={entry.scope} value={entry.scope}>{entry.scope}</option>)}
      </select></label>
      <button disabled={disabled} onClick={() => onReprocess(item.id, { kind: "correct", profile: effectiveProfile || null })}>{t("Re-correct")}</button></div>
      <div className="reprocess-action"><label>{t("Translation target")} <select value={effectiveTarget} disabled={busy} onChange={event => setTarget(event.target.value)}>
        {settings.translationTargetLanguages.map(language => {
          const known = translationLanguageRegistry.find(({ code }) => code === language);
          return <option key={language} value={language}>{known ? t(known.label) : language}</option>;
        })}
      </select></label>
      <button disabled={disabled} onClick={() => onReprocess(item.id, { kind: "translate", target: effectiveTarget })}>{t("Translate to…")}</button></div>
      <HelpHint label={t("Reprocess text")}>{t("Reprocessing uses the raw transcript and does not insert text.")}</HelpHint>
    </div>
  </details>;
}

function InsertionBadge({ result, detail }: { result: string; detail: string | null }) {
  const { t } = useI18n();
  const resultKey = insertionOutcomeLabels[result];
  const detailKey = detail ? insertionDetailLabels[detail] : undefined;
  const inserted = result === "clipboard_paste" || result === "provisional_replace";
  const state = inserted ? "inserted" : result === "paste_unverified" ? "sent" : "not-inserted";
  const detailText = detail ? (detailKey ? t(detailKey) : detail) : undefined;
  return <small className={`history-insertion ${state}`} title={detailText}>
    {resultKey ? t(resultKey) : result}{detailText ? ` · ${detailText}` : ""}
  </small>;
}

function HistoryText({ text, corrected = false, label, onCopy }: {
  text: string;
  corrected?: boolean;
  label?: string;
  onCopy: (text: string) => void;
}) {
  const { t } = useI18n();
  const field = <div
    className={`history-text${corrected ? " api-corrected" : ""}`}
    role="button"
    tabIndex={0}
    title={t("Double-click to copy")}
    onDoubleClick={() => onCopy(text)}
    onKeyDown={(event) => { if (event.key === "Enter") onCopy(text); }}
  >
    {label && <strong>{label}: </strong>}{text}
  </div>;
  // Labeled operands (selected text, spoken instruction) are not what the
  // row-level Copy button copies, so each gets its own explicit Copy.
  if (!label) return field;
  return <div className="history-field">
    {field}
    <IconButton icon="copy" label={`${t("Copy")}: ${label}`} onClick={() => onCopy(text)} />
  </div>;
}
