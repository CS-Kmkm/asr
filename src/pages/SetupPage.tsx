import { SettingRow } from "../components/ui";
import type { AudioDevice, Settings } from "../types";

export function SetupPage({
  settings,
  devices,
  onSettingsChange,
  onPrepareModel,
  onDiagnoseGpu,
  onFinish,
}: {
  settings: Settings;
  devices: AudioDevice[];
  onSettingsChange: (settings: Settings) => void;
  onPrepareModel: () => void;
  onDiagnoseGpu: () => void;
  onFinish: () => void;
}) {
  return (
    <section className="panel">
      <p className="eyebrow">FIRST RUN</p>
      <h2>Configure local dictation</h2>
      <p className="lead">
        Voice data stays on this device. Model downloads and cloud services require an
        explicit action.
      </p>
      <div className="steps">
        <SettingRow
          title="Microphone"
          detail="Captured only while recording."
          control={
            <select
              value={settings.microphoneId ?? ""}
              onChange={(e) =>
                onSettingsChange({ ...settings, microphoneId: e.target.value || null })
              }
            >
              <option value="">System default</option>
              {devices.map((device) => (
                <option key={device.id} value={device.id}>
                  {device.name}
                  {device.isDefault ? " (default)" : ""}
                </option>
              ))}
            </select>
          }
        />
        <SettingRow
          title="Global hotkey"
          detail="Default recording toggle."
          control={
            <input
              value={settings.hotkey}
              onChange={(e) => onSettingsChange({ ...settings, hotkey: e.target.value })}
            />
          }
        />
        <SettingRow
          title="Local ASR model"
          detail="The default faster-whisper model downloads on first load and runs on CPU or GPU. For long-form transcription, VibeVoice (optional GPU backend) can be selected in Settings."
          control={
            <button className="secondary" onClick={onPrepareModel}>
              Load ASR model
            </button>
          }
        />
        <SettingRow
          title="GPU check"
          detail="Reads hardware metadata only; no transcript or audio is involved."
          control={
            <button className="secondary" onClick={onDiagnoseGpu}>
              Run check
            </button>
          }
        />
      </div>
      <button className="primary" onClick={onFinish}>
        Finish setup
      </button>
    </section>
  );
}
