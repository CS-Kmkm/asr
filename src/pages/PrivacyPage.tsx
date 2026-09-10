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
        title={t("Save text history")}
        detail={t("When disabled, transcript and processed text are never inserted into dictation_history.")}
        control={
          <Toggle
            checked={settings.historyEnabled}
            onChange={(value) => onSave({ historyEnabled: value })}
          />
        }
      />
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
            value={settings.historyRetentionDays}
            onChange={(e) => onSave({ historyRetentionDays: Number(e.target.value) })}
          >
            <option value={7}>{t("7 days")}</option>
            <option value={30}>{t("30 days")}</option>
            <option value={90}>{t("90 days")}</option>
          </select>
        }
      />
    </section>
  );
}
