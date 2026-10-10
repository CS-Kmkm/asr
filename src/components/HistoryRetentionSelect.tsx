import { useRef, useState } from "react";
import { previewHistoryRetentionPurge, type HistoryPurgePreview } from "../api";
import type { HistoryRetention } from "../types";
import { useI18n, type MessageKey } from "../i18n";

// Ordered from shortest to longest window; the order defines "shorter".
export const historyRetentionOptions = [
  { value: "never", label: "Never" }, { value: "24_hours", label: "24 hours" },
  { value: "one_week", label: "1 week" }, { value: "one_month", label: "1 month" },
  { value: "one_year", label: "1 year" }, { value: "forever", label: "Forever" },
] as const satisfies readonly { value: HistoryRetention; label: MessageKey }[];

const rank = (value: HistoryRetention) => historyRetentionOptions.findIndex((option) => option.value === value);
const label = (value: HistoryRetention) => historyRetentionOptions[rank(value)]?.label ?? "Never";

// Saving a shorter window immediately and irreversibly purges older History
// rows, their recordings, and suggested spellings on the backend.
export function shortensHistoryRetention(current: HistoryRetention, next: HistoryRetention) {
  return rank(next) < rank(current);
}

// A count that has not arrived by then is treated as failed, so a stuck
// backend call cannot block every later change.
const PURGE_COUNT_TIMEOUT_MS = 3_000;

function purgeCountWithin(next: HistoryRetention, timeoutMs: number) {
  return new Promise<HistoryPurgePreview>((resolve, reject) => {
    const timer = window.setTimeout(() => reject(new Error("history purge count timed out")), timeoutMs);
    previewHistoryRetentionPurge(next).then(
      (preview) => { window.clearTimeout(timer); resolve(preview); },
      (error: unknown) => { window.clearTimeout(timer); reject(error); },
    );
  });
}

// Callers disable it until the stored retention is known; otherwise the
// comparison would run against the default value and could skip the prompt.
export function HistoryRetentionSelect({ value, disabled = false, onChange }: {
  value: HistoryRetention;
  disabled?: boolean;
  onChange: (value: HistoryRetention) => void;
}) {
  const { t } = useI18n();
  const current = useRef(value);
  current.current = value;
  const confirming = useRef(false);
  // Disables the select while counting, which also shows the pending state.
  const [counting, setCounting] = useState(false);

  async function confirmShortening(next: HistoryRetention) {
    // Ignore further changes (for example repeated arrow keys) while one
    // confirmation is being prepared or shown.
    if (confirming.current) return;
    confirming.current = true;
    setCounting(true);
    try {
      let preview: HistoryPurgePreview | null = null;
      try {
        preview = await purgeCountWithin(next, PURGE_COUNT_TIMEOUT_MS);
      } catch {
        // Without counts the generic warning below still asks for consent.
      } finally {
        setCounting(false);
      }
      // The stored retention changed while counting (for example from another
      // window); drop this change rather than confirm against a stale value.
      if (current.current !== value) return;
      const message = [
        `${t("History retention")}: ${t(label(value))} → ${t(label(next))}`,
        ...(preview ? [[
          `${t("History entries to delete")}: ${preview.historyItems}`,
          `${t("Saved recordings to delete")}: ${preview.recordings}`,
        ].join("\n")] : []),
        t(next === "never"
          ? "All History entries, saved recordings, and suggested spellings will be deleted immediately. This cannot be undone. Continue?"
          : "History entries, saved recordings, and suggested spellings older than the new period will be deleted immediately. This cannot be undone. Continue?"),
      ].join("\n\n");
      // Confirm even when both counts are 0: suggested spellings are purged
      // too, and the purge computes its own cutoff a moment later. A declined
      // change is not saved, so the controlled select keeps the stored value.
      if (window.confirm(message)) onChange(next);
    } finally {
      confirming.current = false;
    }
  }

  return <select value={value} disabled={disabled || counting} aria-busy={counting} aria-label={t("History retention")} onChange={(event) => {
    const next = event.target.value as HistoryRetention;
    if (shortensHistoryRetention(value, next)) void confirmShortening(next);
    else onChange(next);
  }}>
    {historyRetentionOptions.map((option) => <option key={option.value} value={option.value}>{t(option.label)}</option>)}
  </select>;
}
