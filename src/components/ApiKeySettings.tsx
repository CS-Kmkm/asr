import { useEffect, useRef, useState } from "react";
import { getProviderCredentials, updateProviderCredential } from "../api";
import { useI18n } from "../i18n";
import type { CredentialProvider, CredentialStatus, Settings } from "../types";

const providers = ["openai", "gemini", "speech"] as const;

export function ApiKeySettings({ settings }: { settings: Settings }) {
  const { t } = useI18n();
  const [statuses, setStatuses] = useState<CredentialStatus[]>([]);
  const [drafts, setDrafts] = useState<Record<CredentialProvider, string>>({ openai: "", gemini: "", speech: "" });
  const [loading, setLoading] = useState(false);
  const [writing, setWriting] = useState(false);
  const busy = loading || writing;
  const [notice, setNotice] = useState<"saved" | "deleted" | null>(null);
  const [failed, setFailed] = useState(false);
  const generation = useRef(0);
  const statusRequest = useRef(0);
  const openai = settings.openaiApiKeyEnvVar;
  const gemini = settings.geminiApiKeyEnvVar;
  const speech = settings.apiKeyEnvVar;
  const aliases = { openai, gemini, speech };
  useEffect(() => {
    generation.current++;
    setWriting(false);
    setFailed(false);
    setNotice(null);
    setStatuses([]);
    setDrafts({ openai: "", gemini: "", speech: "" });
    return () => { generation.current++; };
  }, [openai, gemini, speech]);

  useEffect(() => {
    // useSettings publishes a new snapshot on both optimistic edits and
    // backend confirmation. Reread on confirmation even if aliases match.
    const request = ++statusRequest.current;
    let active = true;
    setLoading(true);
    void getProviderCredentials().then((values) => {
      if (active && request === statusRequest.current) setStatuses(values);
    }, () => { if (active && request === statusRequest.current) setFailed(true); }).finally(() => {
      if (active && request === statusRequest.current) setLoading(false);
    });
    return () => { active = false; };
  }, [settings]);

  async function update(provider: CredentialProvider, secret: string | null) {
    const id = generation.current;
    statusRequest.current++;
    setLoading(false);
    setWriting(true);
    setFailed(false);
    setNotice(null);
    try {
      const status = await updateProviderCredential(provider, aliases[provider], secret);
      if (id !== generation.current) return;
      setDrafts((old) => ({ ...old, [provider]: "" }));
      setStatuses((old) => [...old.filter((entry) => entry.provider !== provider), status]);
      setNotice(secret === null ? "deleted" : "saved");
      // Aliases can be shared, so refresh presence for the other rows too.
      // A refresh failure must not misreport a successful key write as failed.
      try {
        const request = ++statusRequest.current;
        const values = await getProviderCredentials();
        if (id === generation.current && request === statusRequest.current) setStatuses(values);
      } catch { if (id === generation.current) setFailed(true); }
    } catch { if (id === generation.current) setFailed(true); }
    finally { if (id === generation.current) setWriting(false); }
  }

  return <section className="panel">
    <h2>{t("Provider API keys")}</h2>
    <p>{t("Keys saved here use Windows Credential Manager and take effect without restarting. They are not saved in Settings or History. Environment and .env keys remain a fallback. Finish voice operations before changing keys.")}</p>
    {providers.map((provider) => {
      const status = statuses.find((entry) => entry.provider === provider && entry.alias === aliases[provider].trim().toUpperCase());
      const label = provider === "speech" ? t("Speech API key") : provider === "openai" ? t("OpenAI API key") : t("Gemini API key");
      const valid = /^[\x21-\x7e]{1,2048}$/.test(drafts[provider]);
      return <form key={provider} onSubmit={(event) => { event.preventDefault(); if (!busy && valid && status?.storeAvailable) void update(provider, drafts[provider]); }}>
        <label>{label}<input type="password" autoComplete="new-password" spellCheck={false} maxLength={2048} disabled={busy || !status?.storeAvailable} value={drafts[provider]} onChange={(event) => setDrafts((old) => ({ ...old, [provider]: event.target.value }))} /></label>
        <p>{aliases[provider]} — {t(!status ? "Not checked" : status.source === "credential_manager" ? "Saved in Credential Manager" : status.source === "environment" ? "Environment fallback" : "Not configured")}</p>
        {status && !status.storeAvailable && <p>{t("Credential Manager is unavailable in this environment.")}</p>}
        <button type="submit" className="secondary" disabled={busy || !valid || !status?.storeAvailable}>{t("Save key")}</button>
        <button type="button" className="secondary" disabled={busy || !status?.storeAvailable || status.source !== "credential_manager"} onClick={() => void update(provider, null)}>{t("Delete saved key")}</button>
      </form>;
    })}
    {notice && <p role="status">{t(notice === "saved" ? "API key saved" : "Saved key deleted. Environment fallback may still apply.")}</p>}
    {failed && <p role="alert">{t("API key status or update failed. Finish voice operations, check the saved environment variable names and Windows credentials, then reopen Settings.")}</p>}
  </section>;
}
