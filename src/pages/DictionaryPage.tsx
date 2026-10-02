import { useMemo, useRef, useState } from "react";
import { Empty } from "../components/ui";
import type { DictionaryCandidate, DictionaryEntry, DictionaryEntryInput } from "../types";
import { useI18n } from "../i18n";

const emptyForm = { reading: "", surface: "", category: "", aliases: "", priority: "0", appScope: "" };
type Form = typeof emptyForm;

function toInput(form: Form): DictionaryEntryInput {
  const priority = Number(form.priority);
  return {
    reading: form.reading.trim(), surface: form.surface.trim(), category: form.category.trim() || null,
    aliases: form.aliases.split(/\r?\n/).map((value) => value.trim()).filter(Boolean),
    priority: Number.isFinite(priority) ? priority : 0, appScope: form.appScope.trim() || null,
  };
}

export function DictionaryPage({ entries, candidates, onAdd, onUpdate, onDelete, onImport, onConfirmCandidate, onRejectCandidate }: {
  entries: DictionaryEntry[]; candidates: DictionaryCandidate[];
  onAdd: (entry: DictionaryEntryInput) => Promise<boolean>;
  onUpdate: (id: number, entry: DictionaryEntryInput) => Promise<boolean>;
  onDelete: (id: number) => void; onImport: (csv: string) => Promise<boolean>;
  onConfirmCandidate: (id: number) => void; onRejectCandidate: (id: number) => void;
}) {
  const { t } = useI18n();
  const [form, setForm] = useState<Form>(emptyForm);
  const [editing, setEditing] = useState<number | null>(null);
  const [query, setQuery] = useState("");
  const [source, setSource] = useState<"all" | "manual" | "auto">("all");
  const [importError, setImportError] = useState(false);
  const importInput = useRef<HTMLInputElement>(null);
  const visible = useMemo(() => {
    const needle = query.trim().toLocaleLowerCase();
    return entries.filter((entry) => (source === "all" || entry.source === source) && (!needle || [entry.reading, entry.surface, entry.category ?? "", entry.aliases.join(" "), entry.appScope ?? ""].some((value) => value.toLocaleLowerCase().includes(needle))));
  }, [entries, query, source]);

  async function submit(event: React.FormEvent) {
    event.preventDefault();
    const ok = editing === null ? await onAdd(toInput(form)) : await onUpdate(editing, toInput(form));
    if (ok) { setForm(emptyForm); setEditing(null); }
  }
  function edit(entry: DictionaryEntry) {
    setEditing(entry.id);
    setForm({ reading: entry.reading, surface: entry.surface, category: entry.category ?? "", aliases: entry.aliases.join("\n"), priority: String(entry.priority), appScope: entry.appScope ?? "" });
  }
  async function importFile(file: File | undefined) {
    if (!file) return;
    let csv: string;
    try {
      const bytes = await file.arrayBuffer();
      try {
        csv = new TextDecoder("utf-8", { fatal: true }).decode(bytes);
      } catch {
        csv = new TextDecoder("shift_jis", { fatal: true }).decode(bytes);
      }
    } catch {
      setImportError(true);
      return;
    } finally {
      if (importInput.current) importInput.current.value = "";
    }
    setImportError(false);
    await onImport(csv);
  }
  return <section className="panel compact-page-panel">
    <div className="setting-row"><div><strong>{t("Import CSV")}</strong><p>{t("CSV format help")}</p></div><div><input ref={importInput} type="file" accept=".csv,text/csv" onChange={(event) => void importFile(event.target.files?.[0])} /></div></div>
    {importError && <p role="alert">{t("CSV encoding error")}</p>}
    <form className="steps dictionary-form" onSubmit={(event) => void submit(event)}>
      {([ ["Reading", "reading", true], ["Surface", "surface", true], ["Category", "category", false], ["Aliases", "aliases", false], ["Scope", "appScope", false] ] as const).map(([label, key, required]) => <div className="setting-row" key={key}><div><strong>{t(label)}</strong>{key === "aliases" && <p>{t("One alias per line")}</p>}</div>{key === "aliases" ? <textarea value={form.aliases} onChange={(event) => setForm({ ...form, aliases: event.target.value })} /> : <input value={form[key]} required={required} onChange={(event) => setForm({ ...form, [key]: event.target.value })} />}</div>)}
      <div className="setting-row"><div><strong>{t("Priority")}</strong></div><input type="number" value={form.priority} onChange={(event) => setForm({ ...form, priority: event.target.value })} /></div>
      <div><button className="primary" type="submit">{editing === null ? t("Add entry") : t("Save entry")}</button>{editing !== null && <button className="secondary" type="button" onClick={() => { setEditing(null); setForm(emptyForm); }}>{t("Cancel")}</button>}</div>
    </form>

    <div className="setting-row"><input aria-label={t("Search dictionary")} placeholder={t("Search dictionary")} value={query} onChange={(event) => setQuery(event.target.value)} /><select value={source} onChange={(event) => setSource(event.target.value as typeof source)}><option value="all">{t("All")}</option><option value="auto">{t("Auto-added")}</option><option value="manual">{t("Manually-added")}</option></select></div>
    {candidates.length > 0 && <div className="history-list"><h2>{t("Suggested spellings")}</h2>{candidates.map((candidate) => <div className="setting-row" key={candidate.id}><div><strong>{candidate.preferredSpan}</strong><p>{candidate.originalSpan} → {candidate.preferredSpan}</p></div><div><button className="primary" onClick={() => onConfirmCandidate(candidate.id)}>{t("Confirm")}</button><button className="secondary" onClick={() => onRejectCandidate(candidate.id)}>{t("Reject")}</button></div></div>)}</div>}
    {entries.length === 0 ? <Empty title={t("No dictionary entries yet")} detail={t("Add proper nouns and terms to improve recognition accuracy.")} /> : visible.length === 0 ? <Empty title={t("No matching dictionary entries")} detail={t("Change the search text or source filter.")} /> : <div className="history-list">{visible.map((entry) => <div key={entry.id} className="setting-row"><div><strong>{entry.surface}</strong><p>{entry.reading}{entry.category ? ` · ${entry.category}` : ""}{entry.aliases.length ? ` · ${t("aliases:")} ${entry.aliases.join(", ")}` : ""}{` · ${t("priority")} ${entry.priority} · ${t(entry.source === "auto" ? "Auto-added" : "Manually-added")}`}</p>{entry.appScope && <small>{t("scope:")} {entry.appScope}</small>}</div><div><button className="secondary" onClick={() => edit(entry)}>{t("Edit")}</button><button className="secondary" onClick={() => { if (window.confirm(`${t("Delete dictionary entry")}: “${entry.surface}”`)) onDelete(entry.id); }}>{t("Delete")}</button></div></div>)}</div>}
  </section>;
}
