import { Empty, Toggle } from "../components/ui";
import type { HistoryItem, Settings } from "../types";
import { useI18n } from "../i18n";

export function HistoryPage({
  settings,
  history,
  onSave,
  onCopyItem,
}: {
  settings: Settings;
  history: HistoryItem[];
  onSave: (patch: Partial<Settings>) => void;
  onCopyItem: (text: string) => void;
}) {
  const { t } = useI18n();
  return (
    <section className="panel compact-page-panel">
      <div className="history-controls">
        <span>{t("Save history")}</span>
        <Toggle
          checked={settings.historyEnabled}
          onChange={(value) => onSave({ historyEnabled: value })}
          label={t("Save history")}
        />
      </div>
      {!settings.historyEnabled ? (
        <Empty
          title={t("History is disabled")}
          detail={t("New transcripts will not be written to SQLite.")}
        />
      ) : history.length === 0 ? (
        <Empty
          title={t("No dictations yet")}
          detail={t("Completed local dictations will appear here.")}
        />
      ) : (
        <div className="history-list">
          {history.map((item) => (
            <article className="history-item" key={item.id}>
              <div className="history-meta">
                <time dateTime={item.createdAt}>
                  {new Date(item.createdAt).toLocaleString()}
                </time>
                <span className="history-mode">
                  {item.mode}{item.targetLanguage ? ` · ${item.targetLanguage}` : ""}
                </span>
              </div>
              {item.sourceText && (
                <HistoryText text={item.sourceText} onCopy={onCopyItem} label={t("Selected text")} />
              )}
              {item.instructionText && (
                <HistoryText text={item.instructionText} onCopy={onCopyItem} label={t("Spoken instruction")} />
              )}
              {!item.sourceText && !item.instructionText && (
                <HistoryText text={item.transcriptText} onCopy={onCopyItem} />
              )}
              {item.processedText && (
                <HistoryText
                  text={item.processedText}
                  onCopy={onCopyItem}
                  corrected
                />
              )}
            </article>
          ))}
        </div>
      )}
    </section>
  );
}

function HistoryText({
  text,
  corrected = false,
  label,
  onCopy,
}: {
  text: string;
  corrected?: boolean;
  label?: string;
  onCopy: (text: string) => void;
}) {
  const { t } = useI18n();
  return (
    <div
      className={`history-text${corrected ? " api-corrected" : ""}`}
      role="button"
      tabIndex={0}
      title={t("Double-click to copy")}
      onDoubleClick={() => onCopy(text)}
      onKeyDown={(event) => {
        if (event.key === "Enter") onCopy(text);
      }}
    >
      {label && <strong>{label}: </strong>}{text}
    </div>
  );
}
