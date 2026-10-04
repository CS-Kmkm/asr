import { useEffect, useState } from "react";
import { Empty } from "../components/ui";
import type { HistoryAudioPayload, HistoryFilter, HistoryItem, Settings } from "../types";
import { useI18n } from "../i18n";

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
} as const;

export function HistoryPage({ settings, history, filter, onSave, onFilter, onCopyItem, onRetry, onDelete, onDeleteAll, onLoadAudio, onAudioError, retryActive, onCancelRetry }: {
  settings: Settings;
  history: HistoryItem[];
  filter: HistoryFilter;
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
  const [audioUrl, setAudioUrl] = useState<string | null>(null);
  const [playingId, setPlayingId] = useState<number | null>(null);
  useEffect(() => () => { if (audioUrl) URL.revokeObjectURL(audioUrl); }, [audioUrl]);
  useEffect(() => {
    if (playingId !== null && (settings.historyRetention === "never" || !history.some((item) => item.id === playingId))) {
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
      url = URL.createObjectURL(new Blob([new Uint8Array(payload.bytes)], { type: payload.mimeType }));
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
      <select value={filter} aria-label={t("History filter")} onChange={(event) => {
        onFilter(event.target.value as HistoryFilter);
      }}>
        {filters.map(({ value, label }) => <option key={value} value={value}>{t(label)}</option>)}
      </select>
      <select value={settings.historyRetention} onChange={(event) => onSave({ historyRetention: event.target.value as Settings["historyRetention"] })}>
        <option value="never">{t("Never")}</option><option value="24_hours">{t("24 hours")}</option>
        <option value="one_week">{t("1 week")}</option><option value="one_month">{t("1 month")}</option>
        <option value="one_year">{t("1 year")}</option><option value="forever">{t("Forever")}</option>
      </select>
      <button className="danger-button" onClick={() => { if (window.confirm(t("Delete all history and suggested spellings?"))) onDeleteAll(); }}>{t("Delete all")}</button>
      {retryActive && <button onClick={onCancelRetry}>{t("Cancel")}</button>}
    </div>
    {audioUrl && <audio className="history-player" src={audioUrl} controls autoPlay onError={onAudioError} />}
    {settings.historyRetention === "never" ? <Empty title={t("History is disabled")} detail={t("New transcripts will not be written to SQLite.")} />
      : history.length === 0 ? <Empty title={t("No dictations yet")} detail={t("Completed local dictations will appear here.")} />
      : <div className="history-list">{history.map(item => <article className="history-item" key={item.id}>
          <div className="history-meta"><time dateTime={item.createdAt}>{new Date(item.createdAt).toLocaleString()}</time>
            <span className="history-mode">{item.mode in modeLabels
              ? t(modeLabels[item.mode as keyof typeof modeLabels])
              : item.mode}{item.targetLanguage ? ` · ${item.targetLanguage}` : ""}</span>
            {item.appCategory && <small>{t("App category")}: {item.appCategory}</small>}</div>
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
        </article>)}</div>}
  </section>;
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
