import { SettingRow, Toggle } from "../components/ui";
import type { Settings } from "../types";

export function SettingsPage({
  settings,
  onSave,
}: {
  settings: Settings;
  onSave: (patch: Partial<Settings>) => void;
}) {
  return (
    <section className="panel">
      <h2>Input settings</h2>
      <SettingRow
        title="Recording hotkey"
        detail="The default is Ctrl+Shift+Space."
        control={
          <input
            value={settings.hotkey}
            onChange={(e) => onSave({ hotkey: e.target.value })}
          />
        }
      />
      <SettingRow
        title="Start with Windows"
        detail="Launches Local Voice Input automatically when you sign in to Windows."
        control={
          <Toggle
            checked={settings.autoStart}
            onChange={(value) => onSave({ autoStart: value })}
          />
        }
      />
      <SettingRow
        title="Restore clipboard"
        detail="Restore previous clipboard contents after successful paste."
        control={
          <Toggle
            checked={settings.clipboardRestore}
            onChange={(value) => onSave({ clipboardRestore: value })}
          />
        }
      />
    </section>
  );
}
