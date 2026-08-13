import { SettingRow, Toggle } from "../components/ui";
import type { Settings } from "../types";

export function PrivacyPage({
  settings,
  onSave,
}: {
  settings: Settings;
  onSave: (patch: Partial<Settings>) => void;
}) {
  return (
    <section className="panel">
      <SettingRow
        title="Save text history"
        detail="When disabled, transcript and processed text are never inserted into dictation_history."
        control={
          <Toggle
            checked={settings.historyEnabled}
            onChange={(value) => onSave({ historyEnabled: value })}
          />
        }
      />
      <SettingRow
        title="Delete audio after processing"
        detail="Audio cleanup is enabled by default."
        control={
          <Toggle
            checked={settings.deleteAudioAfterProcessing}
            onChange={(value) => onSave({ deleteAudioAfterProcessing: value })}
          />
        }
      />
      <SettingRow
        title="History retention"
        detail="Text history cleanup window."
        control={
          <select
            value={settings.historyRetentionDays}
            onChange={(e) => onSave({ historyRetentionDays: Number(e.target.value) })}
          >
            <option value={7}>7 days</option>
            <option value={30}>30 days</option>
            <option value={90}>90 days</option>
          </select>
        }
      />
    </section>
  );
}
