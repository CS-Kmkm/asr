import { SettingRow, Toggle } from "./ui";
import type { Settings } from "../types";

interface AiCorrectionSettingsProps {
  settings: Settings;
  onSave: (patch: Partial<Settings>) => void;
}

export function AiCorrectionSettings({ settings, onSave }: AiCorrectionSettingsProps) {
  const status = settings.textCorrectionEnabled ? "On" : "Off";

  return (
    <section className="panel ai-correction-panel">
      <div className="ai-correction-header">
        <div>
          <h2>AI text correction</h2>
          <p className="muted">
            When enabled, the transcript is sent to the selected external provider after local
            transcription. Audio is never sent by this feature.
          </p>
        </div>
        <div className="ai-correction-master">
          <span className={`ai-correction-status ${settings.textCorrectionEnabled ? "on" : ""}`}>
            {status}
          </span>
          <Toggle
            checked={settings.textCorrectionEnabled}
            onChange={(value) => onSave({ textCorrectionEnabled: value })}
            label={`${settings.textCorrectionEnabled ? "Disable" : "Enable"} AI text correction`}
          />
        </div>
      </div>
      <p className="ai-correction-master-detail">
        {settings.textCorrectionEnabled
          ? "AI correction is applied before text is inserted. If the API fails, the original transcript is used."
          : "AI correction and external API requests are disabled. You can configure the options below before enabling it."}
      </p>

      <SettingRow
        title="Provider"
        detail="Choose the API used for correction. This can be configured while correction is off."
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
          </select>
        }
      />
      <div className="correction-options-heading">
        <strong>Automatic editing</strong>
        <p>Each operation is independently applied when AI text correction is enabled.</p>
      </div>
      <SettingRow
        title="Remove filler words"
        detail="Remove empty hesitations such as ‘えーと’, ‘あのー’, ‘um’, and ‘uh’, while preserving meaningful hesitation or emphasis."
        control={
          <Toggle
            checked={settings.correctionRemoveFillers}
            onChange={(value) => onSave({ correctionRemoveFillers: value })}
          />
        }
      />
      <SettingRow
        title="Remove accidental repetition"
        detail="Collapse unintended repeated words and false starts, while retaining deliberate rhetorical repetition."
        control={
          <Toggle
            checked={settings.correctionRemoveRepetitions}
            onChange={(value) => onSave({ correctionRemoveRepetitions: value })}
          />
        }
      />
      <SettingRow
        title="Apply spoken self-corrections"
        detail="For phrases such as ‘Tuesday—actually, Wednesday’, keep the speaker’s final intended revision."
        control={
          <Toggle
            checked={settings.correctionResolveSelfCorrections}
            onChange={(value) => onSave({ correctionResolveSelfCorrections: value })}
          />
        }
      />
      <SettingRow
        title="Automatic formatting"
        detail="Turn spoken lists, steps, action items, and key points into paragraphs, bullets, or numbered lists when appropriate."
        control={
          <Toggle
            checked={settings.correctionAutoFormat}
            onChange={(value) => onSave({ correctionAutoFormat: value })}
          />
        }
      />
      <SettingRow
        title="Improve clarity"
        detail="Lightly repair spontaneous-speech grammar and unclear phrasing without changing meaning, tone, or formality."
        control={
          <Toggle
            checked={settings.correctionImproveClarity}
            onChange={(value) => onSave({ correctionImproveClarity: value })}
          />
        }
      />
      {settings.correctionProvider === "openai" ? (
        <>
          <SettingRow
            title="OpenAI model"
            detail="Model ID used through the OpenAI Responses API."
            control={
              <input
                value={settings.openaiCorrectionModel}
                onChange={(event) => onSave({ openaiCorrectionModel: event.target.value })}
              />
            }
          />
          <SettingRow
            title="OpenAI API key environment variable"
            detail="The key itself is not saved. Restart the app after setting this variable."
            control={
              <input
                value={settings.openaiApiKeyEnvVar}
                onChange={(event) => onSave({ openaiApiKeyEnvVar: event.target.value })}
              />
            }
          />
        </>
      ) : (
        <>
          <SettingRow
            title="Gemini model"
            detail="Model ID used through the Gemini Interactions API."
            control={
              <input
                value={settings.geminiCorrectionModel}
                onChange={(event) => onSave({ geminiCorrectionModel: event.target.value })}
              />
            }
          />
          <SettingRow
            title="Gemini API key environment variable"
            detail="The key itself is not saved. Restart the app after setting this variable."
            control={
              <input
                value={settings.geminiApiKeyEnvVar}
                onChange={(event) => onSave({ geminiApiKeyEnvVar: event.target.value })}
              />
            }
          />
        </>
      )}
      <SettingRow
        title="Additional style and tone guidance"
        detail="Customize the editing style. Safety constraints, enabled operations, and dictionary spellings are applied automatically."
        control={
          <textarea
            rows={7}
            maxLength={500}
            value={settings.correctionInstruction}
            placeholder="For example: Keep my tone concise and friendly."
            onChange={(event) => onSave({ correctionInstruction: event.target.value })}
          />
        }
      />
    </section>
  );
}
