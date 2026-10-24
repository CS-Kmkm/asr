import { InfoCard } from "../components/ui";
import { useEffect, useState } from "react";
import { copyToClipboard, getStorageDiagnostics, getWorkerHealth } from "../api";
import type { AppState, GpuDiagnostics, StorageDiagnostics, WorkerHealth } from "../types";
import { useI18n } from "../i18n";

export function DiagnosticsPage({
  state,
  gpu,
  statusLabel,
  onDiagnoseGpu,
  onOpenLogs,
}: {
  state: AppState;
  gpu: GpuDiagnostics | null;
  statusLabel: string;
  onDiagnoseGpu: () => void;
  onOpenLogs?: () => void;
}) {
  const { t } = useI18n();
  const [report, setReport] = useState<StorageDiagnostics | null>(null);
  const [checking, setChecking] = useState(true);
  const [failed, setFailed] = useState(false);
  const [copyStatus, setCopyStatus] = useState<"copied" | "failed" | null>(null);
  const [worker, setWorker] = useState<WorkerHealth | null>(null);
  const [workerChecking, setWorkerChecking] = useState(false);
  const [workerFailed, setWorkerFailed] = useState(false);
  useEffect(() => {
    let active = true;
    void getStorageDiagnostics().then(
      (value) => { if (active) { setReport(value); setChecking(false); } },
      () => { if (active) { setFailed(true); setChecking(false); } },
    );
    return () => { active = false; };
  }, []);
  async function refresh() {
    setChecking(true);
    setReport(null);
    setFailed(false);
    setCopyStatus(null);
    try { setReport(await getStorageDiagnostics()); }
    catch { setFailed(true); }
    finally { setChecking(false); }
  }
  async function copyReport() {
    if (!report) return;
    try {
      // Deliberately omit AppState: lastResult may contain dictated text.
      // The interpreter path is useful locally, but can contain an account
      // name. Keep it out of shareable diagnostics along with model paths.
      const shareableWorker = worker ? { ...worker, pythonPath: undefined } : null;
      await copyToClipboard(JSON.stringify({ storage: report, gpu, worker: shareableWorker }, null, 2));
      setCopyStatus("copied");
    } catch { setCopyStatus("failed"); }
  }
  async function checkWorker() {
    setWorkerChecking(true);
    setWorker(null);
    setWorkerFailed(false);
    setCopyStatus(null);
    try { setWorker(await getWorkerHealth()); }
    catch { setWorkerFailed(true); }
    finally { setWorkerChecking(false); }
  }
  return (
    <section className="panel">
      <p className="lead">{t("Copied diagnostics contain status codes, software versions and hardware metadata. Audio, transcripts, clipboard contents, window titles, paths and API keys are excluded.")}</p>
      <div className="diagnostic-grid">
        <InfoCard label={t("DATABASE")} value={t(failed || report?.databaseOk === false ? "Database check failed" : report?.databaseOk ? "Connected" : "Not checked")} detail={report ? `${t("Schema version")}: ${report.schemaVersion}` : t("Run database check")} />
        <InfoCard
          label={t("APP STATE")}
          value={statusLabel}
          detail={`${t("Updated")} ${new Date(state.updatedAt).toLocaleTimeString()}`}
        />
        <InfoCard
          label={t("GPU")}
          value={gpu ? t(gpu.status === "available" ? "Available" : gpu.status === "unsupported" ? "Unsupported" : "Unavailable") : t("Not checked")}
          detail={gpu?.adapterName ?? t("Run the hardware probe")}
        />
      </div>
      <button className="secondary" onClick={onDiagnoseGpu}>
        {t("Run GPU probe")}
      </button>
      {onOpenLogs && <button className="secondary" onClick={onOpenLogs}>{t("Open diagnostic logs")}</button>}
      <button className="secondary" disabled={checking} onClick={() => void refresh()}>{t("Run database check")}</button>
      <button className="secondary" disabled={!report || checking} onClick={() => void copyReport()}>{t("Copy diagnostics")}</button>
      {copyStatus && <p role="status">{t(copyStatus === "copied" ? "Diagnostics copied" : "Diagnostics could not be copied")}</p>}
      <h2>{t("Worker environment")}</h2>
      <p>{t("This check starts a temporary worker if needed and may prepare dependencies. It does not load or restore a model. The Python path is shown locally and excluded from copied diagnostics.")}</p>
      <button className="secondary" disabled={workerChecking} onClick={() => void checkWorker()}>{t("Check worker environment")}</button>
      {workerFailed && <p role="alert">{t("The worker health check could not be completed.")}</p>}
      {!worker ? <p>{t("Not checked")}</p> : <dl>
        <dt>{t("Python path")}</dt><dd>{worker.pythonPath}</dd>
        <dt>Python</dt><dd>{worker.pythonVersion}</dd>
        <dt>faster-whisper</dt><dd>{worker.fasterWhisperVersion ?? t("Not installed")}</dd>
        <dt>CTranslate2</dt><dd>{worker.ctranslate2Version ?? t("Not installed")}</dd>
        <dt>{t("ASR BACKEND")}</dt><dd>{worker.backend}</dd>
        <dt>{t("Model residency")}</dt><dd>{t(worker.loaded ? "Loaded" : "Not loaded")}</dd>
        <dt>{t("Selected device")}</dt><dd>{worker.device ?? t("Not checked")}</dd>
        <dt>{t("Compute type")}</dt><dd>{worker.computeType ?? t("Not checked")}</dd>
      </dl>}
      <h2>{t("Retained performance metrics")}</h2>
      <p>{t("Timings include successes and failures with a recorded duration. Percentiles use nearest rank. History retention and deletion also apply to these metrics.")}</p>
      <p>{t("Custom model identifiers are grouped as other to exclude local paths from diagnostics.")}</p>
      {!report ? <p>{t("Not checked")}</p> : !report.summaries.length ? <p>{t("No retained metrics")}</p> : <>
        <table>
          <caption>{t("Performance by event and provider")}</caption>
          <thead><tr>{(["Event", "Provider", "Samples", "Successes", "Timed samples", "p50 (ms)", "p95 (ms)"] as const).map((label) => <th scope="col" key={label}>{t(label)}</th>)}</tr></thead>
          <tbody>{report.summaries.map((row) => <tr key={JSON.stringify([row.eventType, row.provider])}>
            <th scope="row">{row.eventType}</th><td>{row.provider ?? "—"}</td><td>{row.count}</td><td>{row.successes}</td><td>{row.timedCount}</td><td>{row.p50Ms ?? "—"}</td><td>{row.p95Ms ?? "—"}</td>
          </tr>)}</tbody>
        </table>
        <table>
          <caption>{t("Outcome codes")}</caption>
          <thead><tr>{(["Event", "Provider", "Outcome", "Code", "Samples"] as const).map((label) => <th scope="col" key={label}>{t(label)}</th>)}</tr></thead>
          <tbody>{report.outcomes.map((row) => <tr key={JSON.stringify([row.eventType, row.provider, row.success, row.code])}>
            <th scope="row">{row.eventType}</th><td>{row.provider ?? "—"}</td><td>{t(row.success ? "Success" : "Failure")}</td><td>{row.code ?? "—"}</td><td>{row.count}</td>
          </tr>)}</tbody>
        </table>
      </>}
    </section>
  );
}
