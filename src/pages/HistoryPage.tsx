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
  onCopyItem: (id: number) => void;
}) {
  return (
    <section className="panel">
      <div className="section-heading">
        <div>
          <h2>Dictation history</h2>
          <p>Stored only when history is enabled.</p>
        </div>
        <Toggle
          checked={settings.historyEnabled}
          onChange={(value) => onSave({ historyEnabled: value })}
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
            <button key={item.id} onClick={() => onCopyItem(item.id)}>
              <span>{item.processedText ?? item.transcriptText}</span>
              <small>
                {new Date(item.createdAt).toLocaleString()} · Copy
              </small>
            </button>
          ))}
        </div>
      )}
    </section>
  );
}
