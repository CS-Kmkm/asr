import { SettingRow, Toggle } from "../components/ui";
import { HistoryRetentionSelect } from "../components/HistoryRetentionSelect";
import type { Settings } from "../types";
import { useI18n } from "../i18n";

export function PrivacyPage({
  settings,
  settingsLoaded,
  onSave,
}: {
  settings: Settings;
  settingsLoaded: boolean;
  onSave: (patch: Partial<Settings>) => void;
}) {
  const { t } = useI18n();
  return (
    <section className="panel">
      <SettingRow
        title={t("Delete audio after processing")}
        detail={t("On by default: new recordings are deleted after processing and are not kept for History playback, download, or Retry. Turning this on keeps recordings already saved in History until you delete them there or History retention removes them.")}
        control={
          <Toggle
            checked={settings.deleteAudioAfterProcessing}
            onChange={(value) => onSave({ deleteAudioAfterProcessing: value })}
          />
        }
      />
      <SettingRow
        title={t("History retention")}
        detail={t("How long History and its saved recordings are kept. Shortening it deletes older entries immediately.")}
        control={
          <HistoryRetentionSelect
            value={settings.historyRetention}
            disabled={!settingsLoaded}
            onChange={(historyRetention) => onSave({ historyRetention })}
          />
        }
      />
    </section>
  );
}
