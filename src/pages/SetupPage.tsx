import { ModelProgressBar, SettingRow } from "../components/ui";
import type { AudioDevice, GpuDiagnostics, ModelProgress, Settings, UiLanguage } from "../types";
import { translateAppMessage, useI18n, type MessageKey } from "../i18n";

function gpuSummary(gpu: GpuDiagnostics | null, t: (key: MessageKey) => string, language: UiLanguage) {
  if (!gpu) return t("Checking GPU automatically...");
  const recommendation = translateAppMessage(language, gpu.recommendation);
  if (gpu.status !== "available") return `${t("Checked automatically.")} ${recommendation}`;

  const hardware = [
    gpu.adapterName,
    gpu.memoryTotalMb ? `${gpu.memoryTotalMb} MB VRAM` : null,
    gpu.driverVersion ? `${t("driver")} ${gpu.driverVersion}` : null,
  ]
    .filter(Boolean)
    .join(" · ");
  return `${t("Detected automatically:")} ${hardware}. ${recommendation}`;
}

export function SetupPage({
  settings,
  devices,
  gpu,
  gpuChecking,
  onSettingsChange,
  onConfigureModel,
  onLoadModel,
  modelLoading = false,
  modelReady = false,
  progress = null,
  onDiagnoseGpu,
  onFinish,
}: {
  settings: Settings;
  devices: AudioDevice[];
  gpu: GpuDiagnostics | null;
  gpuChecking: boolean;
  onSettingsChange: (settings: Settings) => void;
  onConfigureModel: () => void;
  onLoadModel?: () => void;
  modelLoading?: boolean;
  modelReady?: boolean;
  progress?: ModelProgress | null;
  onDiagnoseGpu: () => void;
  onFinish: () => void;
}) {
  const { t, language } = useI18n();
  return (
    <section className="panel">
      <p className="eyebrow">{t("FIRST RUN")}</p>
      <h2>{t("Configure local dictation")}</h2>
      <p className="lead">{t("Speech recognition is local by default. Cloud services require consent; downloads begin only when you load a model.")}</p>
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
              value={settings.shortcuts.dictate[0]}
              onChange={(e) => onSettingsChange({ ...settings, shortcuts: { ...settings.shortcuts, dictate: [e.target.value, ...settings.shortcuts.dictate.slice(1)] } })}
            />
          }
        />
        <SettingRow
          title={t("ASR model")}
          detail={t("Choose a speech backend on the Models page, then load it explicitly before finishing setup.")}
          control={
            <button className="secondary" onClick={onConfigureModel}>
              {t("Configure model")}
            </button>
          }
        />
        <SettingRow
          title={t("GPU")}
          detail={gpuSummary(gpu, t, language)}
          control={
            <button className="secondary" onClick={onDiagnoseGpu} disabled={gpuChecking}>
              {gpuChecking ? t("Checking") : t("Recheck GPU")}
            </button>
          }
        />
        {onLoadModel && <SettingRow title={t("Download and load speech model")}
          detail={t("This action may download model files. Progress and download size appear while the model is prepared.")}
          control={<button className="secondary" disabled={modelLoading} onClick={onLoadModel}>{t("Download and load speech model")}</button>} />}
      </div>
      {progress && <ModelProgressBar progress={progress} />}
      {!modelReady && <p>{t("Load the selected speech model before finishing setup.")}</p>}
      <button className="primary" disabled={!modelReady || modelLoading} onClick={onFinish}>
        {t("Finish setup")}
      </button>
    </section>
  );
}
