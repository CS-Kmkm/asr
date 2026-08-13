import { Empty, Toggle } from "../components/ui";
import type { HistoryItem, Settings } from "../types";

function historyDetail(item: HistoryItem) {
  const timestamp = new Date(item.createdAt).toLocaleString();
  if (item.processedText) {
    const provider = item.llmProvider === "gemini" ? "Gemini" : "OpenAI";
    return `${timestamp} · AI edited with ${provider} · Copy`;
  }
  if (item.mode === "faithful_fallback") {
    return `${timestamp} · Original used after AI fallback · Copy`;
  }
  return `${timestamp} · Original transcript · Copy`;
}

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
              <small>{historyDetail(item)}</small>
            </button>
          ))}
        </div>
      )}
    </section>
  );
}
