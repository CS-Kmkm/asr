import { SettingRow, Toggle } from "../components/ui";
import type { Settings } from "../types";

export function SettingsPage({
  settings,
  onSave,
}: {
  settings: Settings;
  onSave: (patch: Partial<Settings>) => void;
}) {
  return (
    <div className="settings-stack">
    <section className="panel">
      <h2>Input settings</h2>
      <SettingRow
        title="Recording hotkey"
        detail="The default is Ctrl+Shift+Space."
        control={
          <input
            value={settings.hotkey}
            onChange={(e) => onSave({ hotkey: e.target.value })}
          />
        }
      />
      <SettingRow
        title="Start with Windows"
        detail="Launches Local Voice Input automatically when you sign in to Windows."
        control={
          <Toggle
            checked={settings.autoStart}
            onChange={(value) => onSave({ autoStart: value })}
          />
        }
      />
      <SettingRow
        title="Restore clipboard"
        detail="Restore previous clipboard contents after successful paste."
        control={
          <Toggle
            checked={settings.clipboardRestore}
            onChange={(value) => onSave({ clipboardRestore: value })}
          />
        }
      />
    </section>
    <section className="panel">
      <h2>AI text correction</h2>
      <p className="muted">
        When enabled, the transcript is sent to the selected external provider after local
        transcription. Audio is never sent by this feature.
      </p>
      <SettingRow
        title="Correct transcripts with AI"
        detail="Correct recognition mistakes before inserting text. If the API fails, the original transcript is inserted."
        control={
          <Toggle
            checked={settings.textCorrectionEnabled}
            onChange={(value) => onSave({ textCorrectionEnabled: value })}
          />
        }
      />
      {settings.textCorrectionEnabled && (
        <>
          <SettingRow
            title="Provider"
            detail="Choose the API used for correction."
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
            <p>Each operation is independently applied to the transcript sent to the API.</p>
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
        </>
      )}
    </section>
    </div>
  );
}
