import { useEffect, useRef, useState } from "react";
import { cancelSetupTest, copyToClipboard, startDictationTest, stopDictationTest, testInsertion } from "../api";
import { translateAppMessage, useI18n } from "../i18n";
import type { AppState } from "../types";
import { HelpHint, IconButton } from "./ui";

export function SetupTests({ state, modelReady, disabled, onPrepare, onBusyChange }: {
  state: AppState; modelReady: boolean; disabled: boolean;
  onPrepare: () => Promise<boolean>; onBusyChange: (busy: boolean) => void;
}) {
  const { t, language } = useI18n();
  const [pending, setPending] = useState(false);
  const [error, setError] = useState(false);
  const [result, setResult] = useState<string | null>(null);
  const [ownedId, setOwnedId] = useState<number | null>(null);
  const [running, setRunning] = useState(false);
  const [cancelling, setCancelling] = useState(false);
  const owned = useRef<number | null>(null);
  const mounted = useRef(false);
  const pendingRef = useRef(false);
  const busy = ["recording", "processing", "injecting"].includes(state.phase);
  const ours = ownedId !== null && state.operationId === ownedId;
  const active = running;
  useEffect(() => {
    mounted.current = true;
    return () => { mounted.current = false; if (owned.current !== null) void cancelSetupTest(owned.current).catch(() => {}); };
  }, []);
  useEffect(() => { onBusyChange(pending || active); }, [pending, active, onBusyChange]);
  useEffect(() => {
    if (ours && !busy) setRunning(false);
    if (ours && state.phase === "completed") setResult(state.lastResult);
  }, [ours, busy, state.phase, state.lastResult]);
  function remember(id: number) {
    owned.current = id;
    if (mounted.current) { setOwnedId(id); setRunning(true); }
    else void cancelSetupTest(id).catch(() => {});
  }
  async function run(action: () => Promise<void>) {
    if (pendingRef.current) return;
    pendingRef.current = true; setPending(true); setError(false);
    try { await action(); }
    catch { if (mounted.current) setError(true); }
    finally { pendingRef.current = false; if (mounted.current) setPending(false); }
  }
  async function start(insertion: boolean) {
    setResult(null);
    if (!await onPrepare() || !mounted.current) return;
    remember(await (insertion ? testInsertion() : startDictationTest()));
  }
  async function cancel() {
    if (ownedId === null || cancelling) return;
    setCancelling(true);
    try { await cancelSetupTest(ownedId); }
    catch { if (mounted.current) setError(true); }
    finally { if (mounted.current) setCancelling(false); }
  }
  return <fieldset className="setup-tests">
    <legend>{t("Try dictation and insertion")} <HelpHint label={t("Try dictation and insertion")}>{t("Dictation tests show raw recognized text without AI correction, insertion or History. Cloud speech still requires consent.")}</HelpHint></legend>
    <button type="button" disabled={disabled || pending || active || busy || !modelReady} onClick={() => { void run(() => start(false)); }}>{t("Test dictation")}</button>
    <button type="button" disabled={pending || cancelling || !ours || !active || state.phase !== "recording" || state.operation !== "dictation_test"} onClick={() => { void run(async () => { await stopDictationTest(ownedId!); }); }}>{t("Stop test and transcribe")}</button>
    <p className="compact-note">{t("Switch to another app's text field within 3 seconds; a sample will be inserted.")}</p>
    <button type="button" disabled={disabled || pending || active || busy} onClick={() => { void run(() => start(true)); }}>{t("Test insertion")}</button>
    <button type="button" disabled={cancelling || !active} onClick={() => { void cancel(); }}>{t("Cancel test")}</button>
    {ours && state.message && <p role="status">{translateAppMessage(language, state.message, state.messageCode)}</p>}
    {result !== null && <div><pre>{result}</pre><IconButton icon="copy" label={t("Copy result")} disabled={pending} onClick={() => { void run(() => copyToClipboard(result)); }} /></div>}
    {error && <p role="alert">{t("Setup test failed. Check the app status and try again.")}</p>}
  </fieldset>;
}
