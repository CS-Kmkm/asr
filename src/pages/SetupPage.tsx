import { ShortcutField } from "../components/ShortcutField";
import { useState } from "react";
import { SetupTests } from "../components/SetupTests";
import { HelpHint, IconButton, ModelProgressBar, SettingRow } from "../components/ui";
import type { AppState, AudioDevice, GpuDiagnostics, ModelProgress, Settings, UiLanguage } from "../types";
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
  onRefreshDevices,
  appState,
  testsDisabled = false,
  onPrepareTest,
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
  onRefreshDevices?: () => void;
  appState?: AppState;
  testsDisabled?: boolean;
  onPrepareTest?: () => Promise<boolean>;
}) {
  const { t, language } = useI18n();
  const [testing, setTesting] = useState(false);
  return (
    <section className="panel">
      <div className="setting-title section-title"><h2>{t("Configure local dictation")}</h2><HelpHint label={t("Configure local dictation")}>{t("Speech recognition is local by default. Cloud services require consent; downloads begin only when you load a model.")}</HelpHint></div>
      <div className="setup-step"><h3><span>1</span>{t("Microphone and shortcut")}</h3><div className="steps">
        <SettingRow
          title={t("Microphone")}
          detail={t("Captured only while recording.")}
          control={
            <div className="inline-control"><select
              value={settings.microphoneId ?? ""}
              onChange={(e) =>
                onSettingsChange({ ...settings, microphoneId: e.target.value || null })
              }
            >
              <option value="">{t("System default")}</option>
              {settings.microphoneId && !devices.some((device) => device.id === settings.microphoneId) && <option value={settings.microphoneId}>{t("Unavailable device")}</option>}
              {devices.map((device) => (
                <option key={device.id} value={device.id}>
                  {device.name}
                  {device.isDefault ? ` (${t("default")})` : ""}
                </option>
              ))}
            </select>{onRefreshDevices && <IconButton icon="refresh" label={t("Refresh microphones")} onClick={onRefreshDevices} />}</div>
          }
        />
        <SettingRow
          title={t("Global hotkey")}
          detail={t("Default recording toggle.")}
          control={
            <ShortcutField label={t("Global hotkey")} value={settings.shortcuts.dictate[0]} onChange={(value) => onSettingsChange({ ...settings, shortcuts: { ...settings.shortcuts, dictate: [value, ...settings.shortcuts.dictate.slice(1)] } })} />
          }
        />
      </div></div>
      <div className="setup-step"><h3><span>2</span>{t("Prepare speech model")}</h3><div className="steps">
        <SettingRow
          title={t("ASR model")}
          detail={t("Choose a speech backend on the Models page, then load it explicitly before finishing setup.")}
          control={
            <button className="secondary" onClick={onConfigureModel}>
              {t("Configure model")}
            </button>
          }
        />
        <details className="utility-disclosure"><summary>{t("GPU")}</summary>        <SettingRow
          title={t("GPU")}
          detail={gpuSummary(gpu, t, language)}
          control={
            <button className="secondary" onClick={onDiagnoseGpu} disabled={gpuChecking}>
              {gpuChecking ? t("Checking") : t("Recheck GPU")}
            </button>
          }
        />
</details>
        {onLoadModel && <SettingRow title={t("Download and load speech model")}
          detail={t("This action may download model files. Progress and download size appear while the model is prepared.")}
          control={<button className="secondary" disabled={modelLoading} onClick={onLoadModel}>{t("Download and load speech model")}</button>} />}
      </div>
      {progress && <ModelProgressBar progress={progress} />}
      </div>
      <div className="setup-step"><h3><span>3</span>{t("Check your setup")}</h3>
      {appState && onPrepareTest && <SetupTests state={appState} modelReady={modelReady} disabled={testsDisabled || modelLoading} onPrepare={onPrepareTest} onBusyChange={setTesting} />}
      </div>
      <div className="action-icons">
      <button className="primary" disabled={!modelReady || modelLoading || testing} onClick={onFinish}>
        {t("Finish setup")}
      </button>
      {!modelReady && <HelpHint label={t("Finish setup")}>{t("Load the selected speech model before finishing setup.")}</HelpHint>}
      </div>
    </section>
  );
}
