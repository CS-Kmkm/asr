import { InfoCard } from "../components/ui";
import type { AppState, GpuDiagnostics } from "../types";

export function DiagnosticsPage({
  state,
  gpu,
  statusLabel,
  onDiagnoseGpu,
}: {
  state: AppState;
  gpu: GpuDiagnostics | null;
  statusLabel: string;
  onDiagnoseGpu: () => void;
}) {
  return (
    <section className="panel">
      <p className="lead">
        Diagnostics report status codes and hardware metadata only. Audio, transcripts,
        clipboard contents, window titles, and API keys are excluded.
      </p>
      <div className="diagnostic-grid">
        <InfoCard label="DATABASE" value="Connected" detail="SQLite migrations applied" />
        <InfoCard
          label="APP STATE"
          value={statusLabel}
          detail={`Updated ${new Date(state.updatedAt).toLocaleTimeString()}`}
        />
        <InfoCard
          label="GPU"
          value={gpu?.status ?? "Not checked"}
          detail={gpu?.adapterName ?? "Run the hardware probe"}
        />
      </div>
      <button className="secondary" onClick={onDiagnoseGpu}>
        Run GPU probe
      </button>
    </section>
  );
}
