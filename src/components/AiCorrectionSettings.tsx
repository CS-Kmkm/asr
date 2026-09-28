import { useEffect, useRef, useState } from "react";
import { SettingRow, Toggle } from "./ui";
import type { Settings } from "../types";
import { useI18n } from "../i18n";

interface AiCorrectionSettingsProps {
  settings: Settings;
  onSave: (patch: Partial<Settings>) => void;
}

export function AiCorrectionSettings({ settings, onSave }: AiCorrectionSettingsProps) {
  const { t } = useI18n();
  const status = settings.textCorrectionEnabled ? t("On") : t("Off");
  const [localBaseUrl, setLocalBaseUrl] = useState(settings.localCorrectionBaseUrl);
  const [localMaxTokens, setLocalMaxTokens] = useState(String(settings.localCorrectionMaxTokens));
  const cancelLocalBaseUrlBlur = useRef(false);
  useEffect(() => setLocalBaseUrl(settings.localCorrectionBaseUrl), [settings.localCorrectionBaseUrl]);
  useEffect(() => setLocalMaxTokens(String(settings.localCorrectionMaxTokens)), [settings.localCorrectionMaxTokens]);

  function commitLocalBaseUrl() {
    if (cancelLocalBaseUrlBlur.current) {
      cancelLocalBaseUrlBlur.current = false;
      return;
    }
    if (localBaseUrl !== settings.localCorrectionBaseUrl) {
      onSave({ localCorrectionBaseUrl: localBaseUrl });
    }
  }

  function commitLocalMaxTokens() {
    const value = Number(localMaxTokens);
    if (Number.isInteger(value) && value >= 128 && value <= 32768 && value !== settings.localCorrectionMaxTokens) {
      onSave({ localCorrectionMaxTokens: value });
    } else if (!Number.isInteger(value) || value < 128 || value > 32768) {
      setLocalMaxTokens(String(settings.localCorrectionMaxTokens));
    }
  }

  return (
    <section className="panel ai-correction-panel">
      <div className="ai-correction-header">
        <div>
      <h2>{t("AI text correction")}</h2>
          <p className="muted">
            {t("When enabled, the transcript is sent to the selected correction provider after local transcription. Audio is never sent by this feature.")}
          </p>
        </div>
        <div className="ai-correction-master">
          <span className={`ai-correction-status ${settings.textCorrectionEnabled ? "on" : ""}`}>
            {status}
          </span>
          <Toggle
            checked={settings.textCorrectionEnabled}
            onChange={(value) => onSave({ textCorrectionEnabled: value })}
            label={`${settings.textCorrectionEnabled ? t("Disable") : t("Enable")} ${t("AI text correction")}`}
          />
        </div>
      </div>
      <p className="ai-correction-master-detail">
        {settings.textCorrectionEnabled
          ? t("AI correction is applied before text is inserted. If the API fails, the original transcript is used.")
          : t("AI correction requests are disabled. You can configure the options below before enabling it.")}
      </p>

      <SettingRow
        title={t("Provider")}
        detail={t("Choose the API used for correction. This can be configured while correction is off.")}
        control={
          <select
            value={settings.correctionProvider}
            onChange={(event) =>
              onSave({
                correctionProvider: event.target.value as Settings["correctionProvider"],
              })
            }
          >
            <option value="openai">OpenAI</option>
            <option value="gemini">Google Gemini</option>
            <option value="local">{t("Local (OpenAI-compatible)")}</option>
          </select>
        }
      />
      <div className="correction-options-heading">
        <strong>{t("Automatic editing")}</strong>
        <p>{t("Each operation is independently applied when AI text correction is enabled.")}</p>
      </div>
      <SettingRow
        title={t("Remove filler words")}
        detail={t("Remove empty hesitations such as ‘えーと’, ‘あのー’, ‘um’, and ‘uh’, while preserving meaningful hesitation or emphasis.")}
        control={
          <Toggle
            checked={settings.correctionRemoveFillers}
            onChange={(value) => onSave({ correctionRemoveFillers: value })}
          />
        }
      />
      <SettingRow
        title={t("Remove accidental repetition")}
        detail={t("Collapse unintended repeated words and false starts, while retaining deliberate rhetorical repetition.")}
        control={
          <Toggle
            checked={settings.correctionRemoveRepetitions}
            onChange={(value) => onSave({ correctionRemoveRepetitions: value })}
          />
        }
      />
      <SettingRow
        title={t("Apply spoken self-corrections")}
        detail={t("For phrases such as ‘Tuesday—actually, Wednesday’, keep the speaker’s final intended revision.")}
        control={
          <Toggle
            checked={settings.correctionResolveSelfCorrections}
            onChange={(value) => onSave({ correctionResolveSelfCorrections: value })}
          />
        }
      />
      <SettingRow
        title={t("Automatic formatting")}
        detail={t("Turn spoken lists, steps, action items, and key points into paragraphs, bullets, or numbered lists when appropriate.")}
        control={
          <Toggle
            checked={settings.correctionAutoFormat}
            onChange={(value) => onSave({ correctionAutoFormat: value })}
          />
        }
      />
      <SettingRow
        title={t("Improve clarity")}
        detail={t("Lightly repair spontaneous-speech grammar and unclear phrasing without changing meaning, tone, or formality.")}
        control={
          <Toggle
            checked={settings.correctionImproveClarity}
            onChange={(value) => onSave({ correctionImproveClarity: value })}
          />
        }
      />
      {settings.correctionProvider === "openai" && (
        <>
          <SettingRow
            title={t("OpenAI model")}
            detail={t("Model ID used through the OpenAI Responses API.")}
            control={
              <input
                value={settings.openaiCorrectionModel}
                onChange={(event) => onSave({ openaiCorrectionModel: event.target.value })}
              />
            }
          />
          <SettingRow
            title={t("Reasoning effort")}
            detail={t("Choose the quality, latency, and cost tradeoff. None is fastest; high and above spend more time reasoning. Availability depends on the selected model.")}
            control={
              <select
                value={settings.openaiReasoningEffort}
                onChange={(event) =>
                  onSave({
                    openaiReasoningEffort: event.target
                      .value as Settings["openaiReasoningEffort"],
                  })
                }
              >
                <option value="none">{t("None — fastest")}</option>
                <option value="low">{t("Low")}</option>
                <option value="medium">{t("Medium — balanced")}</option>
                <option value="high">{t("High")}</option>
                <option value="xhigh">{t("XHigh")}</option>
                <option value="max">{t("Max — quality first")}</option>
              </select>
            }
          />
          <SettingRow
            title={t("OpenAI API key environment variable")}
            detail={t("The key itself is not saved. Restart the app after setting this variable.")}
            control={
              <input
                value={settings.openaiApiKeyEnvVar}
                onChange={(event) => onSave({ openaiApiKeyEnvVar: event.target.value })}
              />
            }
          />
        </>
      )}
      {settings.correctionProvider === "gemini" && (
        <>
          <SettingRow
            title={t("Gemini model")}
            detail={t("Model ID used through the Gemini Interactions API.")}
            control={
              <input
                value={settings.geminiCorrectionModel}
                onChange={(event) => onSave({ geminiCorrectionModel: event.target.value })}
              />
            }
          />
          <SettingRow
            title={t("Gemini API key environment variable")}
            detail={t("The key itself is not saved. Restart the app after setting this variable.")}
            control={
              <input
                value={settings.geminiApiKeyEnvVar}
                onChange={(event) => onSave({ geminiApiKeyEnvVar: event.target.value })}
              />
            }
          />
        </>
      )}
      {settings.correctionProvider === "local" && (
        <>
          <SettingRow
            title={t("Local endpoint base URL")}
            detail={t("Use a numeric loopback URL ending in /v1. Requests bypass proxies and redirects are rejected.")}
            control={
              <input
                value={localBaseUrl}
                placeholder="http://127.0.0.1:11434/v1"
                onChange={(event) => setLocalBaseUrl(event.target.value)}
                onBlur={commitLocalBaseUrl}
                onKeyDown={(event) => {
                  if (event.key === "Enter") {
                    event.preventDefault();
                    event.currentTarget.blur();
                  } else if (event.key === "Escape") {
                    cancelLocalBaseUrlBlur.current = true;
                    setLocalBaseUrl(settings.localCorrectionBaseUrl);
                    event.currentTarget.blur();
                  }
                }}
              />
            }
          />
          <SettingRow
            title={t("Local correction model")}
            detail={t("Model ID exposed by the local OpenAI-compatible Chat Completions server. No API key is sent.")}
            control={
              <input
                value={settings.localCorrectionModel}
                placeholder="qwen3:8b"
                onChange={(event) => onSave({ localCorrectionModel: event.target.value })}
              />
            }
          />
          <SettingRow
            title={t("Local output token limit")}
            detail={t("Includes thinking tokens. Increase for reasoning models; supported range is 128 to 32768.")}
            control={
              <input
                type="number"
                min={128}
                max={32768}
                step={1}
                value={localMaxTokens}
                onChange={(event) => setLocalMaxTokens(event.target.value)}
                onBlur={commitLocalMaxTokens}
                onKeyDown={(event) => {
                  if (event.key === "Enter") {
                    event.preventDefault();
                    event.currentTarget.blur();
                  }
                }}
              />
            }
          />
        </>
      )}
      <SettingRow
        title={t("Additional style and tone guidance")}
        detail={t("Customize the editing style. Safety constraints, enabled operations, and dictionary spellings are applied automatically.")}
        control={
          <textarea
            rows={7}
            maxLength={500}
            value={settings.correctionInstruction}
            placeholder={t("For example: Keep my tone concise and friendly.")}
            onChange={(event) => onSave({ correctionInstruction: event.target.value })}
          />
        }
      />
    </section>
  );
}
