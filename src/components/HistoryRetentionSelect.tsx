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

// Callers disable it until the stored retention is known; otherwise the
// comparison would run against the default value and could skip the prompt.
export function HistoryRetentionSelect({ value, disabled = false, onChange }: {
  value: HistoryRetention;
  disabled?: boolean;
  onChange: (value: HistoryRetention) => void;
}) {
  const { t } = useI18n();
  return <select value={value} disabled={disabled} aria-label={t("History retention")} onChange={(event) => {
    const next = event.target.value as HistoryRetention;
    if (shortensHistoryRetention(value, next)) {
      const message = [
        `${t("History retention")}: ${t(label(value))} → ${t(label(next))}`,
        t(next === "never"
          ? "All History entries, saved recordings, and suggested spellings will be deleted immediately. This cannot be undone. Continue?"
          : "History entries, saved recordings, and suggested spellings older than the new period will be deleted immediately. This cannot be undone. Continue?"),
      ].join("\n\n");
      // A declined change is not saved, so the controlled select keeps showing
      // the stored value.
      if (!window.confirm(message)) return;
    }
    onChange(next);
  }}>
    {historyRetentionOptions.map((option) => <option key={option.value} value={option.value}>{t(option.label)}</option>)}
  </select>;
}
