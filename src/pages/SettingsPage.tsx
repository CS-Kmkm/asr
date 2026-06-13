import { SettingRow, Toggle } from "../components/ui";
import type { AsrBackend, Settings } from "../types";

export function SettingsPage({
  settings,
  asrBackendOptions,
  onSave,
}: {
  settings: Settings;
  asrBackendOptions: Array<{ value: AsrBackend; label: string }>;
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
        title="ASR backend"
        detail="Changing this restarts the local ASR worker; reload the model afterward."
        control={
          <select
            value={settings.asrBackend}
            onChange={(e) => onSave({ asrBackend: e.target.value as AsrBackend })}
          >
            {asrBackendOptions.map((option) => (
              <option key={option.value} value={option.value}>
                {option.label}
              </option>
            ))}
          </select>
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
