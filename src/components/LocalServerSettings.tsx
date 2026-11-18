import { useEffect, useRef, useState } from "react";
import { getLocalServerStatus, startLocalServer, stopLocalServer } from "../api";
import { useI18n, translateAppMessage } from "../i18n";
import type { LocalServerStatus, Settings } from "../types";
import { Toggle } from "./ui";

export function LocalServerSettings({ settings, onSave, recording }: { settings: Settings; onSave: (patch: Partial<Settings>) => void; recording: boolean }) {
  const { language, t } = useI18n();
  const [draft, setDraft] = useState(settings.localServer);
  const [status, setStatus] = useState<LocalServerStatus | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const generation = useRef(0);
  useEffect(() => setDraft(settings.localServer), [settings.localServer]);
  useEffect(() => { let active = true; void getLocalServerStatus().then(value => { if (active) setStatus(value ?? null); }).catch(() => {}); return () => { active = false; }; }, []);
  const unsaved = JSON.stringify(draft) !== JSON.stringify(settings.localServer);
  async function manage(action: "start" | "stop" | "refresh") {
    const request = ++generation.current;
    setBusy(true); setError("");
    try {
      if (action === "stop") await stopLocalServer();
      const next = action === "start" ? await startLocalServer() : await getLocalServerStatus();
      if (request === generation.current) setStatus(next);
    } catch (failure) { if (request === generation.current) setError(translateAppMessage(language, String(failure)) ?? String(failure)); }
    finally { if (request === generation.current) setBusy(false); }
  }
  return <section className="panel">
    <h3>{t("Local AI server")}</h3>
    <p className="settings-note">{t("Manage an existing llama.cpp llama-server and GGUF model on this computer. No server or model is downloaded. vLLM can be used through a separately operated OpenAI-compatible endpoint; Windows-native vLLM is not supported.")}</p>
    <fieldset disabled={busy || recording}>
      <label>{t("Server executable path")}<input value={draft.executablePath} onChange={event => setDraft({ ...draft, executablePath: event.target.value })} /></label>
      <label>{t("GGUF model path")}<input value={draft.modelPath} onChange={event => setDraft({ ...draft, modelPath: event.target.value })} /></label>
      <label>{t("Context size")}<input type="number" min={512} max={32768} value={draft.contextSize} onChange={event => setDraft({ ...draft, contextSize: Number(event.target.value) })} /></label>
      <label>{t("GPU layers")}<input type="number" min={0} max={999} value={draft.gpuLayers} onChange={event => setDraft({ ...draft, gpuLayers: Number(event.target.value) })} /></label>
      <Toggle label={t("Start local server when AI is needed")} checked={draft.autoStart} onChange={autoStart => setDraft({ ...draft, autoStart })} />
      <p className="settings-note">{t("Uses the local endpoint and model name configured above. Bind to numeric loopback HTTP with /v1. GPU layers 0 uses CPU; higher values share GPU memory with speech recognition. Stop this app's server before changing its configuration.")}</p>
      <button disabled={!unsaved || !Number.isInteger(draft.contextSize) || !Number.isInteger(draft.gpuLayers)} onClick={() => onSave({ localServer: draft })}>{t("Save server configuration")}</button>
      <button disabled={unsaved} onClick={() => void manage("start")}>{t("Start server")}</button>
      <button onClick={() => void manage("refresh")}>{t("Refresh")}</button>
    </fieldset>
    <button disabled={recording} onClick={() => void manage("stop")}>{t("Stop server")}</button>
    <p role="status">{status?.state === "ready" ? t("Ready") : status?.state === "starting" ? t("Starting server") : t("Server stopped")}{status?.readyMs != null ? ` · ${status.readyMs} ms` : ""}</p>
    {error && <p role="alert">{error}</p>}
  </section>;
}
