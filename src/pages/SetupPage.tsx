import { SettingRow } from "../components/ui";
import type { AudioDevice, GpuDiagnostics, Settings } from "../types";

function gpuSummary(gpu: GpuDiagnostics | null) {
  if (!gpu) return "Checking GPU automatically...";
  if (gpu.status !== "available") return `Checked automatically. ${gpu.recommendation}`;

  const hardware = [
    gpu.adapterName,
    gpu.memoryTotalMb ? `${gpu.memoryTotalMb} MB VRAM` : null,
    gpu.driverVersion ? `driver ${gpu.driverVersion}` : null,
  ]
    .filter(Boolean)
    .join(" · ");
  return `Detected automatically: ${hardware}. ${gpu.recommendation}`;
}

export function SetupPage({
  settings,
  devices,
  gpu,
  gpuChecking,
  onSettingsChange,
  onConfigureModel,
  onDiagnoseGpu,
  onFinish,
}: {
  settings: Settings;
  devices: AudioDevice[];
  gpu: GpuDiagnostics | null;
  gpuChecking: boolean;
  onSettingsChange: (settings: Settings) => void;
  onConfigureModel: () => void;
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
          title="ASR model"
          detail="Choose a local backend or an OpenAI-compatible API on the Models page. The selected model is prepared automatically before first use."
          control={
            <button className="secondary" onClick={onConfigureModel}>
              Configure model
            </button>
          }
        />
        <SettingRow
          title="GPU"
          detail={gpuSummary(gpu)}
          control={
            <button className="secondary" onClick={onDiagnoseGpu} disabled={gpuChecking}>
              {gpuChecking ? "Checking..." : "Recheck GPU"}
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
