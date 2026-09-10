import { SettingRow } from "../components/ui";
import type { AudioDevice, GpuDiagnostics, Settings } from "../types";
import { useI18n, type MessageKey } from "../i18n";

function gpuSummary(gpu: GpuDiagnostics | null, t: (key: MessageKey) => string) {
  if (!gpu) return t("Checking GPU automatically...");
  if (gpu.status !== "available") return `${t("Checked automatically.")} ${gpu.recommendation}`;

  const hardware = [
    gpu.adapterName,
    gpu.memoryTotalMb ? `${gpu.memoryTotalMb} MB VRAM` : null,
    gpu.driverVersion ? `${t("driver")} ${gpu.driverVersion}` : null,
  ]
    .filter(Boolean)
    .join(" · ");
  return `${t("Detected automatically:")} ${hardware}. ${gpu.recommendation}`;
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
  const { t } = useI18n();
  return (
    <section className="panel">
      <p className="eyebrow">{t("FIRST RUN")}</p>
      <h2>{t("Configure local dictation")}</h2>
      <p className="lead">{t("Voice data stays on this device. Model downloads and cloud services require an explicit action.")}</p>
      <div className="steps">
        <SettingRow
          title={t("Microphone")}
          detail={t("Captured only while recording.")}
          control={
            <select
              value={settings.microphoneId ?? ""}
              onChange={(e) =>
                onSettingsChange({ ...settings, microphoneId: e.target.value || null })
              }
            >
              <option value="">{t("System default")}</option>
              {devices.map((device) => (
                <option key={device.id} value={device.id}>
                  {device.name}
                  {device.isDefault ? ` (${t("default")})` : ""}
                </option>
              ))}
            </select>
          }
        />
        <SettingRow
          title={t("Global hotkey")}
          detail={t("Default recording toggle.")}
          control={
            <input
              value={settings.hotkey}
              onChange={(e) => onSettingsChange({ ...settings, hotkey: e.target.value })}
            />
          }
        />
        <SettingRow
          title={t("ASR model")}
          detail={t("Choose a local backend or an OpenAI-compatible API on the Models page. The selected model is prepared automatically before first use.")}
          control={
            <button className="secondary" onClick={onConfigureModel}>
              {t("Configure model")}
            </button>
          }
        />
        <SettingRow
          title={t("GPU")}
          detail={gpuSummary(gpu, t)}
          control={
            <button className="secondary" onClick={onDiagnoseGpu} disabled={gpuChecking}>
              {gpuChecking ? t("Checking") : t("Recheck GPU")}
            </button>
          }
        />
      </div>
      <button className="primary" onClick={onFinish}>
        {t("Finish setup")}
      </button>
    </section>
  );
}
