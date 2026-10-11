import { useEffect, useState } from "react";
import { getSettingsRepairStatus, persistRepairedSettings } from "../api";
import { useI18n } from "../i18n";

// Marker the backend uses when the whole stored settings row is unreadable.
const UNREADABLE_ROW = "unreadable";

// Lists the stored settings fields this version could not read. The backend
// uses fallback values for them in memory only and never overwrites the
// stored values on its own; the button saves the values shown now. The
// status is fetched on mount, so the notice does not depend on the startup
// status event arriving before the page listens.
export function SettingsRepairBanner() {
  const { t } = useI18n();
  const [fields, setFields] = useState<string[]>([]);
  const [saving, setSaving] = useState(false);
  const [failed, setFailed] = useState(false);
  useEffect(() => {
    let active = true;
    void getSettingsRepairStatus()
      .then((next) => { if (active) setFields(next); })
      .catch(() => {});
    return () => { active = false; };
  }, []);
  if (fields.length === 0) return null;
  const persist = () => {
    setSaving(true);
    setFailed(false);
    return persistRepairedSettings()
      .then(setFields, () => setFailed(true))
      .finally(() => setSaving(false));
  };
  return (
    <section className="panel" aria-labelledby="settings-repair-title">
      <p id="settings-repair-title" className="settings-error" role="alert">
        {t("Some stored settings could not be read by this version, so these fields use fallback values for now:")}
      </p>
      <ul>
        {fields.map((field) => (
          <li key={field}>
            {field === UNREADABLE_ROW ? t("All settings (the stored settings could not be read)") : <code>{field}</code>}
          </li>
        ))}
      </ul>
      <p>{t("The stored values were not changed. To keep the values shown now instead, save them for these fields.")}</p>
      <button type="button" disabled={saving} onClick={() => void persist()}>
        {t("Save current values for these fields")}
      </button>
      {failed && <p className="settings-error" role="alert">{t("The current values could not be saved.")}</p>}
    </section>
  );
}
