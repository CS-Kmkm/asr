import { SettingRow, Toggle } from "../components/ui";
import type { Settings } from "../types";
import { useI18n } from "../i18n";

export function PrivacyPage({
  settings,
  onSave,
}: {
  settings: Settings;
  onSave: (patch: Partial<Settings>) => void;
}) {
  const { t } = useI18n();
  return (
    <section className="panel">
      <SettingRow
        title={t("Delete audio after processing")}
        detail={t("Audio cleanup is enabled by default.")}
        control={
          <Toggle
            checked={settings.deleteAudioAfterProcessing}
            onChange={(value) => onSave({ deleteAudioAfterProcessing: value })}
          />
        }
      />
      <SettingRow
        title={t("History retention")}
        detail={t("Text history cleanup window.")}
        control={
          <select
            value={settings.historyRetention}
            onChange={(e) => onSave({ historyRetention: e.target.value as Settings["historyRetention"] })}
          >
            <option value="never">{t("Never")}</option>
            <option value="24_hours">{t("24 hours")}</option>
            <option value="one_week">{t("1 week")}</option>
            <option value="one_month">{t("1 month")}</option>
            <option value="one_year">{t("1 year")}</option>
            <option value="forever">{t("Forever")}</option>
          </select>
        }
      />
    </section>
  );
}
