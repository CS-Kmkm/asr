import type { Settings } from "../types";
import { useI18n, type MessageKey } from "../i18n";
import { catalogEntryFor } from "../modelCatalog";
import { executionPresets, executionPresetPatch, type ExecutionPreset } from "../executionPresets";
import { SettingRow, Toggle } from "./ui";

const names: Record<ExecutionPreset, MessageKey> = {
  fast_japanese: "Fast Japanese", live_streaming: "Live Streaming (experimental)",
  smart_local: "Smart Local", hybrid: "Hybrid", cloud_quality: "Cloud Quality", technical: "Technical",
};

export function ExecutionSettings({ settings, onSave, recording }: {
  settings: Settings; onSave: (patch: Partial<Settings>) => void; recording: boolean;
}) {
  const { t } = useI18n();
  const qwenService = settings.asrBackend === "openai-compatible" && settings.qwenDemoStreaming;
  const streamingCapable = qwenService || (catalogEntryFor(settings.asrBackend, settings.modelId)?.entry.inference.includes("stateful_streaming") ?? false);
  const streamingLabel = qwenService ? t("Buffered streaming (Qwen service)") : t("Stateful streaming (experimental)");
  return <>
    <SettingRow title={t("Execution preset")} detail={t("Apply a starting configuration, then edit speech and AI models independently in Models and Writing. Cloud presets still require separate Privacy consent and may incur API charges.")} detailMode="inline"
      control={<select aria-label={t("Execution preset")} value="" disabled={recording} onChange={event => {
        if (event.target.value) onSave(executionPresetPatch(event.target.value as ExecutionPreset));
      }}>
        <option value="">{t("Choose a preset")}</option>
        {executionPresets.map(preset => <option key={preset} value={preset} disabled={settings.offlineOnly && (preset === "hybrid" || preset === "cloud_quality")}>{t(names[preset])}</option>)}
      </select>} />
    <SettingRow title={t("Offline only")} detail={t("Blocks cloud audio, cloud text and new model downloads. Prepare local models first. A failed provider never switches to another cloud service.")} detailMode="inline"
      control={<Toggle label={t("Offline only")} checked={settings.offlineOnly} disabled={recording} onChange={offlineOnly => onSave({ offlineOnly })} />} />
    <dl className="settings-note">
      <dt>{t("Audio destination")}</dt><dd>{settings.asrBackend === "openai-compatible" ? `${settings.offlineOnly ? t("Blocked by offline mode") + ": " : ""}${settings.apiBaseUrl}` : t("This computer")}</dd>
      <dt>{t("Text and selection destination")}</dt><dd>{settings.correctionProvider === "local" ? settings.localCorrectionBaseUrl : `${settings.offlineOnly ? t("Blocked by offline mode") + ": " : ""}${settings.correctionProvider === "openai" ? "OpenAI" : "Google Gemini"}`}</dd>
    </dl>
    <p className="settings-note">{t("Text destination applies to editing, translation and answers, even when dictation correction is off. Local speech and a local LLM may compete for GPU memory; keep only the models you need loaded.")}</p>
    {settings.asrBackend === "openai-compatible" && <SettingRow title={t("Use Qwen official streaming service")}
      detail={t("Requires a separately operated official Qwen vLLM demo service, exposed through a numeric loopback HTTP origin such as http://127.0.0.1:8000. Configure that speech endpoint first. Uses automatic language detection and requires renewed audio consent. No vLLM or model is installed.")} detailMode="inline"
      control={<Toggle label={t("Use Qwen official streaming service")} checked={settings.qwenDemoStreaming} disabled={recording} onChange={qwenDemoStreaming => onSave({ qwenDemoStreaming, speechLocale: null })} />} />}
    {streamingCapable && <SettingRow title={streamingLabel} detail={qwenService ? t("Qwen returns cumulative buffered hypotheses, not encoder-cache streaming. The service does not attest final coverage or model identity, so the complete recording is recognized again at stop. Audio stays on the explicitly configured endpoint.") : t("Keeps recognition state while recording. Partial text is preview only; a verified final flush is inserted once. If streaming fails, the full recording is recognized. Uses fixed manual gain; whole-recording noise suppression and automatic gain apply to batch fallback only. Quality and speed are not yet compared.")} detailMode="inline"
      control={<Toggle label={streamingLabel} checked={settings.statefulStreaming} disabled={recording} onChange={statefulStreaming => onSave({ statefulStreaming })} />} />}
    {qwenService && <p className="settings-note">{t("Qwen preview also requires Live transcription preview in Privacy. When it is off, audio is sent only after stop.")}</p>}
  </>;
}
