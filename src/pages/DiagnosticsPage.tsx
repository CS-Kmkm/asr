import { InfoCard } from "../components/ui";
import type { AppState, GpuDiagnostics } from "../types";
import { useI18n } from "../i18n";

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
  const { t } = useI18n();
  return (
    <section className="panel">
      <p className="lead">{t("Diagnostics report status codes and hardware metadata only. Audio, transcripts, clipboard contents, window titles, and API keys are excluded.")}</p>
      <div className="diagnostic-grid">
        <InfoCard label={t("DATABASE")} value={t("Connected")} detail={t("SQLite migrations applied")} />
        <InfoCard
          label={t("APP STATE")}
          value={statusLabel}
          detail={`${t("Updated")} ${new Date(state.updatedAt).toLocaleTimeString()}`}
        />
        <InfoCard
          label={t("GPU")}
          value={gpu?.status ?? t("Not checked")}
          detail={gpu?.adapterName ?? t("Run the hardware probe")}
        />
      </div>
      <button className="secondary" onClick={onDiagnoseGpu}>
        {t("Run GPU probe")}
      </button>
    </section>
  );
}
