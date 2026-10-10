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
        detail={t("On by default: new recordings are deleted after processing and are not kept for History playback, download, or Retry, except failed recordings kept by the setting below. Turning this on keeps recordings already saved in History until you delete them there or History retention removes them.")}
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
      <SettingRow
        title={t("Keep failed recordings for 24 hours")}
        detail={t("On by default: when transcription fails, the recording (and, for Edit, the selected text) is kept in History for 24 hours so you can retry it, even if History retention is Never or Delete audio after processing is on. It is then deleted, or as soon as a Retry succeeds, unless your History and audio settings keep it. Turning this off deletes such recordings; failed recordings are then kept only when History keeps audio.")}
        control={
          <Toggle
            checked={settings.keepFailedTakes}
            onChange={(value) => onSave({ keepFailedTakes: value })}
          />
        }
      />
    </section>
  );
}
