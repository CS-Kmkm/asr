import { useEffect, useRef, useState, type KeyboardEvent } from "react";
import { listen } from "@tauri-apps/api/event";
import { setShortcutCapture } from "../api";
import { translateAppMessage, useI18n } from "../i18n";

const keyNames: Record<string, string> = { Escape: "Esc", Space: "Space", ArrowUp: "Up", ArrowDown: "Down", ArrowLeft: "Left", ArrowRight: "Right" };
function keyName(code: string): string | null {
  if (/^Key[A-Z]$/i.test(code)) return code.slice(3).toUpperCase();
  if (/^Digit[0-9]$/.test(code)) return code.slice(5);
  if (/^F(?:[1-9]|1[0-9]|2[0-4])$/i.test(code)) return code.toUpperCase();
  const names = ["Enter", "Tab", "Backspace", "Delete", "Home", "End", "PageUp", "PageDown", "Insert", "Backquote", "Backslash", "BracketLeft", "BracketRight", "Comma", "Period", "Minus", "Equal", "Semicolon", "Quote", "Slash", "Pause", "PrintScreen", "ScrollLock", "NumLock", "CapsLock", "NumpadAdd", "NumpadSubtract", "NumpadMultiply", "NumpadDivide", "NumpadDecimal", "NumpadEnter", ...Array.from({ length: 10 }, (_, index) => `Numpad${index}`)];
  return names.find((name) => name.toLowerCase() === code.toLowerCase()) ?? Object.entries(keyNames).find(([name, value]) => name.toLowerCase() === code.toLowerCase() || value.toLowerCase() === code.toLowerCase())?.[1] ?? null;
}
export function shortcutFromEvent(event: Pick<KeyboardEvent, "code" | "ctrlKey" | "altKey" | "shiftKey" | "metaKey">): string | null {
  const key = keyName(event.code);
  return key ? [...(event.ctrlKey ? ["Ctrl"] : []), ...(event.altKey ? ["Alt"] : []), ...(event.shiftKey ? ["Shift"] : []), ...(event.metaKey ? ["Super"] : []), key].join("+") : null;
}
export function normalizeShortcutText(text: string): string | null {
  const parts = text.split("+").map((part) => part.trim());
  if (!parts.length || parts.some((part) => !part)) return null;
  const mods = new Set<string>();
  let key: string | null = null;
  for (const part of parts) {
    const modifier = ({ ctrl: "Ctrl", control: "Ctrl", alt: "Alt", shift: "Shift", super: "Super", meta: "Super", cmd: "Super", command: "Super" } as Record<string, string>)[part.toLowerCase()];
    if (modifier) { if (mods.has(modifier)) return null; mods.add(modifier); continue; }
    const raw = part.length === 1 && /[a-z0-9]/i.test(part) ? part.toUpperCase() : keyName(part) ?? (["Esc", "Up", "Down", "Left", "Right"].includes(part) ? part : null);
    if (!raw || key) return null;
    key = raw;
  }
  return key ? [...["Ctrl", "Alt", "Shift", "Super"].filter((mod) => mods.has(mod)), key].join("+") : null;
}

let newestCapture = 0;
let activeRecorder: { id: number; release: () => void } | null = null;
export function ShortcutField({ label, value, onChange, onKeyDown, conflict = false }: {
  label: string; value: string; onChange: (value: string) => void;
  onKeyDown?: (event: KeyboardEvent<HTMLInputElement>) => void; conflict?: boolean;
}) {
  const { t, language } = useI18n();
  const [recording, setRecording] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const lease = useRef<number | null>(null);
  const mounted = useRef(false);
  const input = useRef<HTMLInputElement>(null);
  const callback = useRef(onChange);
  const invalid = Boolean(value) && normalizeShortcutText(value) === null;
  callback.current = onChange;
  function finish(chord?: string) {
    const id = lease.current;
    lease.current = null;
    if (activeRecorder?.id === id) activeRecorder = null;
    setRecording(false);
    if (id !== null) void setShortcutCapture(id, false).catch(() => {});
    if (chord) callback.current(chord);
  }
  useEffect(() => {
    mounted.current = true;
    let active = true;
    const listener = listen<{ captureId: number; chord: string }>("shortcut-captured", ({ payload }) => {
      if (active && payload.captureId === lease.current) finish(normalizeShortcutText(payload.chord) ?? payload.chord);
    });
    return () => {
      active = false; mounted.current = false;
      void listener.then((stop) => stop()).catch(() => {});
      const id = lease.current; lease.current = null;
      if (activeRecorder?.id === id) activeRecorder = null;
      if (id !== null) void setShortcutCapture(id, false).catch(() => {});
    };
  }, []);
  useEffect(() => {
    if (!recording) return;
    const timer = window.setTimeout(() => finish(), 15000);
    return () => window.clearTimeout(timer);
  }, [recording]);
  async function start() {
    activeRecorder?.release();
    const id = Math.max(Date.now() * 1000, newestCapture + 1);
    newestCapture = id;
    lease.current = id;
    activeRecorder = { id, release: () => finish() };
    try {
      await setShortcutCapture(id, true);
      if (!mounted.current || lease.current !== id) { void setShortcutCapture(id, false).catch(() => {}); return; }
      setError(null); setRecording(true); input.current?.focus();
    } catch (error) { if (lease.current === id) { lease.current = null; setError(String(error)); } }
  }
  return <div className="shortcut-field">
    <input ref={input} aria-label={label} value={value} readOnly={recording} aria-invalid={invalid || conflict || Boolean(error)}
      placeholder={recording ? t("Press a key combination") : undefined}
      onChange={(event) => { setError(null); onChange(event.target.value); }}
      onBlur={() => { if (recording) finish(); }}
      onKeyDown={(event) => {
        if (!recording) { onKeyDown?.(event); return; }
        event.preventDefault(); event.stopPropagation();
        if (event.key === "Escape") { finish(); return; }
        if (event.nativeEvent.isComposing || event.repeat) return;
        const chord = shortcutFromEvent(event);
        if (chord) finish(chord);
        else if (!["Control", "Alt", "Shift", "Meta"].includes(event.key)) setError(t("Unsupported shortcut key"));
      }} />
    <button type="button" className="secondary" onClick={() => recording ? finish() : void start()}>{recording ? t("Cancel") : t("Record shortcut")}</button>
    {recording && <span role="status">{t("Press a key combination")}</span>}
    {(error || conflict || invalid) && <p className="settings-warning" role="alert">{error ? translateAppMessage(language, error) : invalid ? t("Unsupported shortcut key") : t("Shortcuts must be non-empty and unique across all actions.")}</p>}
  </div>;
}
