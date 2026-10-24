import { useEffect, useState } from "react";
import { Empty } from "../components/ui";
import { HistoryRetentionSelect } from "../components/HistoryRetentionSelect";
import type { HistoryAudioPayload, HistoryFilter, HistoryItem, Settings } from "../types";
import { insertionDetailLabels, insertionOutcomeLabels, useI18n } from "../i18n";

const filters = [
  { value: "all", label: "All" }, { value: "dictate", label: "Dictate" },
  { value: "translate", label: "Translate" }, { value: "edit", label: "Edit" },
  { value: "ask", label: "Ask" },
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

export function HistoryPage({ settings, settingsLoaded, history, filter, query = "", loading = false, failed = false, hasMore = false, onSearch, onLoadMore, onSave, onFilter, onCopyItem, onRetry, onDelete, onDeleteAll, onLoadAudio, onAudioError, retryActive, onCancelRetry }: {
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
        <input type="search" aria-label={t("Search history")} maxLength={500} value={searchDraft} onChange={(event) => setSearchDraft(event.target.value)} />
        <button type="submit">{t("Search")}</button>
      </form>}
      <select value={filter} aria-label={t("History filter")} onChange={(event) => {
        onFilter(event.target.value as HistoryFilter);
      }}>
        {filters.map(({ value, label }) => <option key={value} value={value}>{t(label)}</option>)}
      </select>
      <HistoryRetentionSelect value={settings.historyRetention} disabled={!settingsLoaded} onChange={(historyRetention) => onSave({ historyRetention })} />
      <button className="danger-button" onClick={() => { if (window.confirm(t("Delete all history and suggested spellings?"))) onDeleteAll(); }}>{t("Delete all")}</button>
      {retryActive && <button onClick={onCancelRetry}>{t("Cancel")}</button>}
    </div>
    {audioUrl && <audio className="history-player" src={audioUrl} controls autoPlay onError={onAudioError} />}
    {loading && <p role="status">{t("Loading...")}</p>}
    {failed && <p role="alert">{t("History could not be loaded.")} {onSearch && <button onClick={() => onSearch(query)}>{t("Reload history")}</button>}</p>}
    {history.length === 0 && (loading || failed) ? null
      : history.length === 0 && query ? <Empty title={t("No matching history")} detail={t("Try a different search or filter.")} />
      : settings.historyRetention === "never" && history.length === 0 ? <Empty title={t("History is disabled")} detail={settings.keepFailedTakes
      ? t("New transcripts are not saved. A failed recording (and, for Edit, the selected text) is kept for 24 hours so you can retry it.")
      : t("New transcripts will not be written to SQLite.")} />
      : history.length === 0 ? <Empty title={t("No dictations yet")} detail={t("Completed local dictations will appear here.")} />
      : <>{settings.historyRetention === "never" && <p className="muted">{t("History is off. Failed recordings (and, for Edit, the selected text) are kept here for 24 hours so you can retry them.")}</p>}
      <div className="history-list">{history.map(item => <article className="history-item" key={item.id}>
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
            <button onClick={() => onCopyItem(item.processedText ?? item.transcriptText)}>{t("Copy")}</button>
            <button disabled={!item.hasAudio} onClick={() => onRetry(item.id)}>{t("Retry")}</button>
            <button disabled={!item.hasAudio} onClick={() => void audio(item, false)}>{t("Play")}</button>
            <button disabled={!item.hasAudio} onClick={() => void audio(item, true)}>{t("Download")}</button>
            <button className="danger-button" onClick={() => onDelete(item.id)}>{t("Delete")}</button>
          </div>
        </article>)}</div></>}
    {hasMore && onLoadMore && <button disabled={loading} onClick={onLoadMore}>{t("Load more")}</button>}
  </section>;
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
    <button type="button" aria-label={`${t("Copy")}: ${label}`} onClick={() => onCopy(text)}>{t("Copy")}</button>
  </div>;
}
