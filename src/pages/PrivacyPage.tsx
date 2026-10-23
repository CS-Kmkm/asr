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
  const consents = settings.cloudConsents ?? [];
  const changeConsent = (key: string, allowed: boolean) => onSave({ cloudConsents: allowed
    ? [...new Set([...consents, key])] : consents.filter((saved) => saved !== key) });
  const asrConsent = `asr:${settings.apiBaseUrl.trim().replace(/\/+$/, "")}`;
  return (
    <section className="panel">
      <SettingRow
        title={t("Delete audio after processing")}
        detail={t("On by default: new recordings are deleted after processing and are not kept for History playback, download, or Retry, except failed recordings kept by the setting below. Turning this on keeps recordings already saved in History until you delete them there or History retention removes them.")}
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
      <SettingRow
        title={t("Keep failed recordings for 24 hours")}
        detail={t("On by default: when transcription fails, the recording (and, for Edit, the selected text) is kept in History for 24 hours so you can retry it, even if History retention is Never or Delete audio after processing is on. It is then deleted, or as soon as a Retry succeeds, unless your History and audio settings keep it. Turning this off deletes such recordings; failed recordings are then kept only when History keeps audio.")}
        control={
          <Toggle
            checked={settings.keepFailedTakes}
            onChange={(value) => onSave({ keepFailedTakes: value })}
          />
        }
      />
      <p>{t("Usage metrics follow History retention and are removed by Delete all. Failed recordings kept for Retry follow the separate 24-hour setting above.")}</p>
      <h2>{t("Cloud provider consent")}</h2>
      <p>{t("Allow a provider only if you agree to send text to it. Translate, Edit and Ask send transcripts or selected text even when Dictate AI correction is off. You can revoke consent here.")}</p>
      {(["openai", "gemini"] as const).map((provider) => (
        <SettingRow key={provider} title={provider === "openai" ? "OpenAI" : "Gemini"}
          detail={t("Allow text to be sent to this provider.")}
          control={<Toggle label={`${provider} ${t("Cloud provider consent")}`} checked={consents.includes(provider)} onChange={(allowed) => changeConsent(provider, allowed)} />} />
      ))}
      {settings.asrBackend === "openai-compatible" && <SettingRow
        title={t("Cloud speech provider")}
        detail={`${t("Allow recordings to be sent to this endpoint:")} ${settings.apiBaseUrl}`}
        control={<Toggle label={t("Cloud speech provider")} checked={consents.includes(asrConsent)} onChange={(allowed) => changeConsent(asrConsent, allowed)} />} />}
      <SettingRow title={t("Live transcription preview")}
        detail={t("Preview can transcribe the same audio more than once. With a cloud speech backend, each preview sends another audio upload. Off by default for cloud speech.")}
        control={<Toggle label={t("Live transcription preview")} checked={settings.asrBackend === "openai-compatible" ? settings.cloudLivePreviewEnabled : settings.livePreviewEnabled ?? true} onChange={(enabled) => onSave(settings.asrBackend === "openai-compatible" ? { cloudLivePreviewEnabled: enabled } : { livePreviewEnabled: enabled })} />} />
    </section>
  );
}
