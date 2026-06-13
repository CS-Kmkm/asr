import { useState } from "react";
import { Empty } from "../components/ui";
import type { DictionaryEntry, DictionaryEntryInput } from "../types";

const EMPTY_FORM = {
  reading: "",
  surface: "",
  category: "",
  aliases: "",
  priority: "0",
};

export function DictionaryPage({
  entries,
  onAdd,
  onDelete,
}: {
  entries: DictionaryEntry[];
  onAdd: (entry: DictionaryEntryInput) => Promise<boolean>;
  onDelete: (id: number) => void;
}) {
  const [form, setForm] = useState(EMPTY_FORM);

  async function handleSubmit(event: React.FormEvent) {
    event.preventDefault();
    const aliases = form.aliases
      .split(",")
      .map((value) => value.trim())
      .filter((value) => value.length > 0);
    const priority = Number(form.priority);
    const input: DictionaryEntryInput = {
      reading: form.reading.trim(),
      surface: form.surface.trim(),
      category: form.category.trim() || null,
      aliases,
      priority: Number.isFinite(priority) ? priority : 0,
    };
    const ok = await onAdd(input);
    if (ok) setForm(EMPTY_FORM);
  }

  return (
    <section className="panel">
      <div className="section-heading">
        <div>
          <h2>Personal dictionary</h2>
          <p>
            Surfaces and aliases are sent to the ASR engine as a prompt, improving
            recognition of proper nouns and domain terms.
          </p>
        </div>
      </div>

      <form className="steps" onSubmit={(e) => void handleSubmit(e)}>
        <div className="setting-row">
          <div>
            <strong>Reading</strong>
            <p>How the term is spoken (e.g. かな or romaji).</p>
          </div>
          <input
            value={form.reading}
            onChange={(e) => setForm({ ...form, reading: e.target.value })}
            required
          />
        </div>
        <div className="setting-row">
          <div>
            <strong>Surface</strong>
            <p>The exact text to produce when recognized.</p>
          </div>
          <input
            value={form.surface}
            onChange={(e) => setForm({ ...form, surface: e.target.value })}
            required
          />
        </div>
        <div className="setting-row">
          <div>
            <strong>Category</strong>
            <p>Optional grouping label.</p>
          </div>
          <input
            value={form.category}
            onChange={(e) => setForm({ ...form, category: e.target.value })}
          />
        </div>
        <div className="setting-row">
          <div>
            <strong>Aliases</strong>
            <p>Optional, comma-separated alternative surfaces.</p>
          </div>
          <input
            value={form.aliases}
            onChange={(e) => setForm({ ...form, aliases: e.target.value })}
          />
        </div>
        <div className="setting-row">
          <div>
            <strong>Priority</strong>
            <p>Higher values win when readings collide.</p>
          </div>
          <input
            type="number"
            value={form.priority}
            onChange={(e) => setForm({ ...form, priority: e.target.value })}
          />
        </div>
        <button className="primary" type="submit">
          Add entry
        </button>
      </form>

      {entries.length === 0 ? (
        <Empty
          title="No dictionary entries yet"
          detail="Add proper nouns and terms to improve recognition accuracy."
        />
      ) : (
        <div className="history-list">
          {entries.map((entry) => (
            <div key={entry.id} className="setting-row">
              <div>
                <strong>{entry.surface}</strong>
                <p>
                  {entry.reading}
                  {entry.category ? ` · ${entry.category}` : ""}
                  {entry.aliases.length > 0 ? ` · aliases: ${entry.aliases.join(", ")}` : ""}
                  {` · priority ${entry.priority}`}
                </p>
              </div>
              <button className="secondary" onClick={() => onDelete(entry.id)}>
                Delete
              </button>
            </div>
          ))}
        </div>
      )}
    </section>
  );
}
