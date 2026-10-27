import { useEffect, useId, useRef, useState } from "react";
import { useI18n, type MessageKey } from "../i18n";

export type FieldValidation = (value: string) => MessageKey | null;
export const validateModel: FieldValidation = (value) =>
  value.trim() && new TextEncoder().encode(value).length <= 512 && !/[\x00-\x1f\x7f-\x9f]/.test(value) ? null : "Enter a non-empty model ID without control characters (up to 512 bytes).";
export const validateEnvironmentName: FieldValidation = (value) =>
  /^[A-Za-z0-9_]{1,128}$/.test(value.trim()) ? null : "Use 1–128 ASCII letters, digits or underscores.";
export const validateLocalEndpoint: FieldValidation = (value) => {
  try {
    const url = new URL(value);
    if ((url.protocol === "http:" || url.protocol === "https:") && ["127.0.0.1", "[::1]"].includes(url.hostname) && !url.username && !url.password && !url.search && !url.hash && url.pathname.replace(/\/$/, "") === "/v1") return null;
  } catch { /* The draft can remain incomplete until committed. */ }
  return "Use a numeric loopback HTTP(S) endpoint ending in /v1.";
};

export function CommitField({ label, value, onCommit, validate, multiline = false, placeholder, maxLength = 512, type = "text", id, "aria-describedby": describedBy }: {
  label: string; value: string; onCommit: (value: string) => void;
  validate?: FieldValidation; multiline?: boolean; placeholder?: string; maxLength?: number; type?: "text" | "number";
  id?: string; "aria-describedby"?: string;
}) {
  const { t } = useI18n();
  const [draft, setDraft] = useState(value);
  const previous = useRef(value);
  const suppressBlur = useRef(false);
  const composing = useRef(false);
  const deferredBlur = useRef(false);
  const [error, setError] = useState<MessageKey | null>(null);
  const errorId = useId();
  useEffect(() => {
    const old = previous.current;
    previous.current = value;
    setDraft((current) => current === old ? value : current);
  }, [value]);
  function commit(candidate = draft) {
    const error = validate?.(candidate) ?? null;
    setError(error);
    if (!error && candidate !== value) onCommit(candidate);
  }
  const props = {
    id, "aria-label": label, "aria-invalid": Boolean(error), "aria-describedby": [describedBy, error ? errorId : undefined].filter(Boolean).join(" ") || undefined,
    value: draft, placeholder, maxLength,
    onChange: (event: React.ChangeEvent<HTMLInputElement | HTMLTextAreaElement>) => { setDraft(event.target.value); setError(null); },
    onCompositionStart: () => { composing.current = true; },
    onCompositionEnd: (event: React.CompositionEvent<HTMLInputElement | HTMLTextAreaElement>) => {
      composing.current = false;
      if (deferredBlur.current) { deferredBlur.current = false; commit(event.currentTarget.value); }
    },
    onBlur: () => {
      if (suppressBlur.current) { suppressBlur.current = false; return; }
      if (composing.current) { deferredBlur.current = true; return; }
      commit();
    },
    onKeyDown: (event: React.KeyboardEvent<HTMLInputElement | HTMLTextAreaElement>) => {
      if (event.nativeEvent.isComposing || event.keyCode === 229) return;
      if (event.key === "Enter" && (!multiline || event.ctrlKey || event.metaKey)) {
        event.preventDefault(); commit(); suppressBlur.current = true; event.currentTarget.blur();
      } else if (event.key === "Escape") {
        event.preventDefault(); setDraft(value); setError(null); suppressBlur.current = true; event.currentTarget.blur();
      }
    },
  };
  return <div className="commit-field">
    {multiline ? <textarea {...props} rows={7} /> : <input {...props} type={type} />}
    {error && <p id={errorId} className="settings-warning" role="alert">{t(error)}</p>}
  </div>;
}

export function GainSlider({ value, onCommit, label, id, "aria-describedby": describedBy }: { value: number; onCommit: (value: number) => void; label: string; id?: string; "aria-describedby"?: string }) {
  const [draft, setDraft] = useState(value);
  const latest = useRef(value);
  const committed = useRef(value);
  useEffect(() => { setDraft(value); latest.current = value; committed.current = value; }, [value]);
  function commit() {
    if (latest.current !== committed.current) {
      committed.current = latest.current;
      onCommit(latest.current);
    }
  }
  return <label className="range-control"><input id={id} aria-label={label} aria-describedby={describedBy} type="range" min="25" max="400" step="5" value={draft}
    onPointerDown={(event) => event.currentTarget.setPointerCapture?.(event.pointerId)}
    onChange={(event) => { const next = Number(event.target.value); latest.current = next; setDraft(next); }}
    onPointerUp={commit} onPointerCancel={commit} onKeyUp={commit} onBlur={commit} /><output>{draft}%</output></label>;
}

export function StepSlider<T extends string>({ value, options, onCommit, label, id, "aria-describedby": describedBy }: {
  value: T; options: readonly { value: T; label: string }[]; onCommit: (value: T) => void; label: string; id?: string; "aria-describedby"?: string;
}) {
  const [draft, setDraft] = useState(value);
  const latest = useRef(value);
  const committed = useRef(value);
  useEffect(() => { setDraft(value); latest.current = value; committed.current = value; }, [value]);
  const index = Math.max(0, options.findIndex(option => option.value === draft));
  function commit() {
    if (latest.current !== committed.current) { committed.current = latest.current; onCommit(latest.current); }
  }
  return <div className="step-slider">
    <div className="step-slider-track">
      <input id={id} type="range" aria-label={label} aria-describedby={describedBy} aria-valuetext={options[index]?.label}
        min={0} max={options.length - 1} step={1} value={index}
        onPointerDown={event => event.currentTarget.setPointerCapture?.(event.pointerId)}
        onChange={event => { const next = options[Number(event.target.value)]?.value; if (next !== undefined) { latest.current = next; setDraft(next); } }}
        onPointerUp={commit} onPointerCancel={commit} onKeyUp={commit} onBlur={commit} />
      <div className="step-slider-ticks" aria-hidden="true">{options.map(option => <span key={option.value} />)}</div>
    </div>
    <output>{options[index]?.label}</output>
  </div>;
}
