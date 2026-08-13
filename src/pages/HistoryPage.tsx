import { Empty, Toggle } from "../components/ui";
import type { HistoryItem, Settings } from "../types";

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
  return (
    <section className="panel compact-page-panel">
      <div className="history-controls">
        <span>Save history</span>
        <Toggle
          checked={settings.historyEnabled}
          onChange={(value) => onSave({ historyEnabled: value })}
          label="Save history"
        />
      </div>
      {!settings.historyEnabled ? (
        <Empty
          title="History is disabled"
          detail="New transcripts will not be written to SQLite."
        />
      ) : history.length === 0 ? (
        <Empty
          title="No dictations yet"
          detail="Completed local dictations will appear here."
        />
      ) : (
        <div className="history-list">
          {history.map((item) => (
            <article className="history-item" key={item.id}>
              <time dateTime={item.createdAt}>
                {new Date(item.createdAt).toLocaleString()}
              </time>
              <HistoryText text={item.transcriptText} onCopy={onCopyItem} />
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
  onCopy,
}: {
  text: string;
  corrected?: boolean;
  onCopy: (text: string) => void;
}) {
  return (
    <div
      className={`history-text${corrected ? " api-corrected" : ""}`}
      role="button"
      tabIndex={0}
      title="Double-click to copy"
      onDoubleClick={() => onCopy(text)}
      onKeyDown={(event) => {
        if (event.key === "Enter") onCopy(text);
      }}
    >
      {text}
    </div>
  );
}
