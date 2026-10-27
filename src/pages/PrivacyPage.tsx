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
        detail={t("New audio is deleted; failed-take exceptions and existing History still apply.")}
        detailMode="inline"
        help={t("On by default: new recordings are deleted after processing and are not kept for History playback, download, or Retry, except failed recordings kept by the setting below. Turning this on keeps recordings already saved in History until you delete them there or History retention removes them.")}
        control={
          <Toggle label={t("Delete audio after processing")}
            checked={settings.deleteAudioAfterProcessing}
            onChange={(value) => onSave({ deleteAudioAfterProcessing: value })}
          />
        }
      />
      <SettingRow
        title={t("History retention")}
        detail={t("History retention also covers saved audio; shortening it deletes older items.")}
        detailMode="inline"
        help={[t("How long History and its saved recordings are kept. Shortening it deletes older entries immediately."), t("Usage metrics follow History retention and are removed by Delete all. Failed recordings kept for Retry follow the separate 24-hour setting above.")].join("\n\n")}
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
        detail={t("Failed audio and Edit selections are retained even with History off. Disabling deletes retry-only data.")}
        detailMode="inline"
        help={t("On by default: when transcription fails, the recording (and, for Edit, the selected text) is kept in History for 24 hours so you can retry it, even if History retention is Never or Delete audio after processing is on. It is then deleted, or as soon as a Retry succeeds, unless your History and audio settings keep it. Turning this off deletes such recordings; failed recordings are then kept only when History keeps audio.")}
        control={
          <Toggle label={t("Keep failed recordings for 24 hours")}
            checked={settings.keepFailedTakes}
            onChange={(value) => onSave({ keepFailedTakes: value })}
          />
        }
      />
      <h2>{t("Cloud provider consent")}</h2>
      <p className="compact-note">{t("Cloud modes send transcripts or selections, even with correction off.")}</p>
      {(["openai", "gemini"] as const).map((provider) => (
        <SettingRow key={provider} title={provider === "openai" ? "OpenAI" : "Gemini"}
          control={<Toggle label={`${provider} ${t("Cloud provider consent")}`} checked={consents.includes(provider)} onChange={(allowed) => changeConsent(provider, allowed)} />} />
      ))}
      {settings.asrBackend === "openai-compatible" && <SettingRow
        title={t("Cloud speech provider")}
        detail={`${t("Allow recordings to be sent to this endpoint:")} ${settings.apiBaseUrl}`}
        detailMode="inline"
        control={<Toggle label={t("Cloud speech provider")} checked={consents.includes(asrConsent)} onChange={(allowed) => changeConsent(asrConsent, allowed)} />} />}
      <SettingRow title={t("Live transcription preview")}
        detail={settings.asrBackend === "openai-compatible" ? t("Cloud preview uploads audio repeatedly.") : undefined}
        detailMode="inline"
        help={t("Preview can transcribe the same audio more than once. With a cloud speech backend, each preview sends another audio upload. Off by default for cloud speech.")}
        control={<Toggle label={t("Live transcription preview")} checked={settings.asrBackend === "openai-compatible" ? settings.cloudLivePreviewEnabled : settings.livePreviewEnabled ?? true} onChange={(enabled) => onSave(settings.asrBackend === "openai-compatible" ? { cloudLivePreviewEnabled: enabled } : { livePreviewEnabled: enabled })} />} />
    </section>
  );
}
