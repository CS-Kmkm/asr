import { useEffect, useMemo, useRef, useState } from "react";
import { Empty, HelpHint, IconButton, Toggle } from "../components/ui";
import { dictionaryModes, type CandidateScope, type DictionaryCandidate, type DictionaryEntry, type DictionaryEntryInput, type DictionaryMode } from "../types";
import { useI18n, type MessageKey } from "../i18n";

const emptyForm = { reading: "", surface: "", category: "", aliases: "", priority: "0", appScope: "", modes: [...dictionaryModes] as DictionaryMode[] };
type Form = typeof emptyForm;

const modeLabels: Record<DictionaryMode, MessageKey> = { dictate: "Dictate", edit: "Edit", ask: "Ask" };

/** Empty or missing modes mean every mode; the backend stores all three the same way. */
function effectiveModes(modes: DictionaryMode[] | undefined): DictionaryMode[] {
  return modes && modes.length ? modes : [...dictionaryModes];
}

/** Mode checkboxes; the last checked mode cannot be cleared, so an entry always applies somewhere. */
export function ModeChoice({ value, onChange, context }: { value: DictionaryMode[]; onChange: (modes: DictionaryMode[]) => void; context?: string }) {
  const { t } = useI18n();
  return <fieldset className="mode-choice"><legend>{t("Applies to")}</legend>
    {dictionaryModes.map((mode) => {
      const checked = value.includes(mode);
      return <label key={mode}><input type="checkbox" aria-label={context ? `${t(modeLabels[mode])}: ${context}` : t(modeLabels[mode])} checked={checked} disabled={checked && value.length === 1}
        onChange={() => onChange(checked ? value.filter((item) => item !== mode) : dictionaryModes.filter((item) => item === mode || value.includes(item)))} /> {t(modeLabels[mode])}</label>;
    })}
  </fieldset>;
}

// The backend stores priority as i64; a fractional or unsafe number would
// otherwise fail deserialization with an untranslated error. New values must
// fall in this range, but an entry's stored priority (for example from CSV
// import) is kept as is so editing another field still saves.
export const PRIORITY_MIN = -1_000_000;
export const PRIORITY_MAX = 1_000_000;
export function parsePriority(value: string, stored: number | null = null): number | null {
  if (!value.trim()) return null;
  const priority = Number(value);
  if (!Number.isSafeInteger(priority)) return null;
  return priority === stored || (priority >= PRIORITY_MIN && priority <= PRIORITY_MAX) ? priority : null;
}

function toInput(form: Form, priority: number): DictionaryEntryInput {
  return {
    reading: form.reading.trim(), surface: form.surface.trim(), category: form.category.trim() || null,
    aliases: form.aliases.split(/\r?\n/).map((value) => value.trim()).filter(Boolean),
    priority, appScope: form.appScope.trim() || null,
    modes: form.modes.length === dictionaryModes.length ? [] : form.modes,
  };
}

export function DictionaryPage({ entries, candidates, onAdd, onUpdate, onDelete, onImport, onConfirmCandidate, onRejectCandidate, learning, onToggleLearning, onDeleteLearned }: {
  entries: DictionaryEntry[]; candidates: DictionaryCandidate[];
  onAdd: (entry: DictionaryEntryInput) => Promise<boolean>;
  onUpdate: (id: number, entry: DictionaryEntryInput) => Promise<boolean>;
  onDelete: (id: number) => void; onImport: (csv: string) => Promise<boolean>;
  /** `scope` is passed only when the user narrowed it from every mode, globally. */
  onConfirmCandidate: (id: number, scope?: CandidateScope) => void; onRejectCandidate: (id: number) => void;
  /** Opt-in learning from History edits; the section is shown when a toggle handler is given. */
  learning?: boolean; onToggleLearning?: (value: boolean) => void; onDeleteLearned?: () => void;
}) {
  const { t } = useI18n();
  const [form, setForm] = useState<Form>(emptyForm);
  const [editorOpen, setEditorOpen] = useState(false);
  const editorRef = useRef<HTMLFormElement>(null);
  const [editing, setEditing] = useState<number | null>(null);
  useEffect(() => { if (editorOpen) editorRef.current?.querySelector<HTMLInputElement>("input")?.focus(); }, [editorOpen, editing]);
  const [query, setQuery] = useState("");
  const [source, setSource] = useState<"all" | "manual" | "auto" | "learned">("all");
  const [candidateScopes, setCandidateScopes] = useState<Record<number, CandidateScope>>({});
  const scopeOf = (id: number): CandidateScope => candidateScopes[id] ?? { appScope: null, modes: [...dictionaryModes] };
  function confirmCandidate(id: number) {
    const scope = scopeOf(id);
    const appScope = scope.appScope?.trim() || null;
    if (!appScope && scope.modes.length === dictionaryModes.length) onConfirmCandidate(id);
    else onConfirmCandidate(id, { appScope, modes: scope.modes.length === dictionaryModes.length ? [] : scope.modes });
  }
  const [importError, setImportError] = useState(false);
  const importInput = useRef<HTMLInputElement>(null);
  const priorityInput = useRef<HTMLInputElement>(null);
  const storedPriority = editing === null ? null : entries.find((entry) => entry.id === editing)?.priority ?? null;
  const priorityInvalid = parsePriority(form.priority, storedPriority) === null;
  const priorityMessage = t("Priority must be a whole number from -1000000 to 1000000.");
  // Native step/min/max checks block submission before onSubmit runs, so a
  // custom validity message keeps that native prompt translated too.
  useEffect(() => priorityInput.current?.setCustomValidity(priorityInvalid ? priorityMessage : ""), [priorityInvalid, priorityMessage]);
  const visible = useMemo(() => {
    const needle = query.trim().toLocaleLowerCase();
    return entries.filter((entry) => (source === "all" || entry.source === source) && (!needle || [entry.reading, entry.surface, entry.category ?? "", entry.aliases.join(" "), entry.appScope ?? ""].some((value) => value.toLocaleLowerCase().includes(needle))));
  }, [entries, query, source]);

  async function submit(event: React.FormEvent) {
    event.preventDefault();
    const priority = parsePriority(form.priority, storedPriority);
    if (priority === null) return;
    const ok = editing === null ? await onAdd(toInput(form, priority)) : await onUpdate(editing, toInput(form, priority));
    if (ok) { setForm(emptyForm); setEditing(null); setEditorOpen(false); }
  }
  function edit(entry: DictionaryEntry) {
    setEditorOpen(true);
    setEditing(entry.id);
    setForm({ reading: entry.reading, surface: entry.surface, category: entry.category ?? "", aliases: entry.aliases.join("\n"), priority: String(entry.priority), appScope: entry.appScope ?? "", modes: effectiveModes(entry.modes) });
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
    <div className="dictionary-toolbar">
      <input type="search" aria-label={t("Search dictionary")} placeholder={t("Search dictionary")} value={query} onChange={(event) => setQuery(event.target.value)} />
      <select aria-label={t("Dictionary source")} value={source} onChange={(event) => setSource(event.target.value as typeof source)}><option value="all">{t("All")}</option><option value="auto">{t("Auto-added")}</option><option value="manual">{t("Manually-added")}</option><option value="learned">{t("Learned from edits")}</option></select>
      {!editorOpen && <IconButton icon="add" label={t("Add entry")} className="primary" onClick={() => { setEditing(null); setForm(emptyForm); setEditorOpen(true); }} />}
    </div>
    <details className="utility-disclosure"><summary>{t("Import CSV")}</summary><HelpHint label={t("Import CSV")}>{t("CSV format help")}</HelpHint><input aria-label={t("Import CSV")} ref={importInput} type="file" accept=".csv,text/csv" onChange={(event) => void importFile(event.target.files?.[0])} /></details>
    {importError && <p role="alert">{t("CSV encoding error")}</p>}
    <form ref={editorRef} hidden={!editorOpen} className="steps dictionary-form" onSubmit={(event) => void submit(event)}>
      {([ ["Reading", "reading", true], ["Surface", "surface", true], ["Category", "category", false], ["Aliases", "aliases", false], ["Scope", "appScope", false] ] as const).map(([label, key, required]) => <div className="setting-row" key={key}><div className="setting-title"><strong>{t(label)}</strong>{key === "aliases" && <HelpHint label={t(label)}>{t("One alias per line")}</HelpHint>}</div>{key === "aliases" ? <textarea aria-label={t(label)} placeholder={t("One alias per line")} value={form.aliases} onChange={(event) => setForm({ ...form, aliases: event.target.value })} /> : <input aria-label={t(label)} value={form[key]} required={required} onChange={(event) => setForm({ ...form, [key]: event.target.value })} />}</div>)}
      <div className="setting-row"><div><strong>{t("Priority")}</strong></div><input ref={priorityInput} type="number" step={1} min={Math.min(PRIORITY_MIN, storedPriority ?? 0)} max={Math.max(PRIORITY_MAX, storedPriority ?? 0)} aria-label={t("Priority")} aria-invalid={priorityInvalid} value={form.priority} onChange={(event) => setForm({ ...form, priority: event.target.value })} /></div>
      {priorityInvalid && <p className="settings-error" role="alert">{priorityMessage}</p>}
      <ModeChoice value={form.modes} onChange={(modes) => setForm({ ...form, modes })} />
      <div><IconButton icon={editing === null ? "add" : "save"} label={editing === null ? t("Add entry") : t("Save entry")} className="primary" type="submit" /><IconButton icon="close" label={t("Cancel")} type="button" onClick={() => { setEditing(null); setForm(emptyForm); setEditorOpen(false); }} /></div>
    </form>


    {onToggleLearning && <div className="setting-row learning-settings">
      <div className="setting-title"><strong>{t("Learn from my edits")}</strong><HelpHint label={t("Learning from your edits")}>{[t("Off by default. When on, you can submit an edit of a dictation in History. Only the changed word pair is kept as a suggestion for you to review; the edited text is not saved and nothing is learned from typing."), t("Suggestions from edits are stored only while History is on. Turning learning off deletes pending suggestions; applied entries stay until you delete them."), t("Applied entries are used like other dictionary entries, including in requests to the AI provider you selected.")].join("\n\n")}</HelpHint></div>
      <div><Toggle label={t("Learn from my edits")} checked={learning ?? false} onChange={onToggleLearning} />
      {onDeleteLearned && <IconButton icon="trash" label={t("Delete learned data")} className="danger-button" onClick={() => { if (window.confirm(t("Delete all suggestions and entries learned from your edits? Manually added entries stay."))) onDeleteLearned(); }} />}</div>
    </div>}
    {candidates.length > 0 && <div className="history-list"><h2>{t("Suggested spellings")}</h2>{candidates.map((candidate) => { const context = `${candidate.originalSpan} → ${candidate.preferredSpan}`; const scope = scopeOf(candidate.id); return <div className="setting-row" key={candidate.id}><div><p className="candidate-spans"><span>{candidate.originalSpan}</span> → <strong>{candidate.preferredSpan}</strong>{candidate.origin === "user_edit" && <span className="entry-badge">{t("From your edit")}</span>}</p>
      <details className="candidate-scope"><summary>{t("Applies to")}: {scope.modes.length === dictionaryModes.length ? t("All modes") : scope.modes.map((mode) => t(modeLabels[mode])).join(", ")}{scope.appScope?.trim() ? ` · ${scope.appScope.trim()}` : ""}</summary>
        <ModeChoice value={scope.modes} context={context} onChange={(modes) => setCandidateScopes({ ...candidateScopes, [candidate.id]: { ...scope, modes } })} />
        <label>{t("Scope")} <input aria-label={`${t("Scope")}: ${context}`} placeholder={t("Optional global, app:key, or category:name scope.")} value={scope.appScope ?? ""} onChange={(event) => setCandidateScopes({ ...candidateScopes, [candidate.id]: { ...scope, appScope: event.target.value } })} /></label>
      </details></div><div><IconButton icon="check" label={t("Confirm")} context={context} className="primary" onClick={() => confirmCandidate(candidate.id)} /><IconButton icon="close" label={t("Reject")} context={`${candidate.originalSpan} → ${candidate.preferredSpan}`} onClick={() => onRejectCandidate(candidate.id)} /></div></div>; })}</div>}
    {entries.length === 0 ? <Empty icon="dictionary" title={t("No dictionary entries yet")} /> : visible.length === 0 ? <Empty icon="search" title={t("No matching dictionary entries")} /> : <div className="history-list">{visible.map((entry) => <div key={entry.id} className="setting-row"><div className="dictionary-entry"><div className="dictionary-entry-title"><strong>{entry.surface}</strong>{entry.source === "auto" && <span className="entry-badge">{t("Auto-added")}</span>}{entry.source === "learned" && <span className="entry-badge">{t("Learned")}</span>}{entry.modes && entry.modes.length > 0 && entry.modes.length < dictionaryModes.length && <span className="entry-badge">{entry.modes.map((mode) => t(modeLabels[mode])).join(", ")}</span>}{entry.priority !== 0 && <span className="entry-badge">{t("priority")} {entry.priority}</span>}{entry.appScope && <span className="entry-badge">{t("scope:")} {entry.appScope}</span>}</div><p>{entry.reading}{entry.category ? ` · ${entry.category}` : ""}{entry.aliases.length ? ` · ${t("aliases:")} ${entry.aliases.join(", ")}` : ""}</p></div><div><IconButton icon="writing" label={t("Edit")} context={`${entry.surface} (${entry.reading})${entry.appScope ? ` · ${entry.appScope}` : ""}`} onClick={() => edit(entry)} /><IconButton icon="trash" label={t("Delete")} context={`${entry.surface} (${entry.reading})${entry.appScope ? ` · ${entry.appScope}` : ""}`} className="danger-button" onClick={() => { if (window.confirm(`${t("Delete dictionary entry")}: “${entry.surface}”`)) onDelete(entry.id); }} /></div></div>)}</div>}
  </section>;
}
