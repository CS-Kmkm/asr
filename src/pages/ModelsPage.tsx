import type { GpuDiagnostics, ModelStatus } from "../types";

export function ModelsPage({
  model,
  gpu,
  onPrepareModel,
  onDiagnoseGpu,
}: {
  model: ModelStatus | null;
  gpu: GpuDiagnostics | null;
  onPrepareModel: () => void;
  onDiagnoseGpu: () => void;
}) {
  return (
    <section className="grid">
      <article className="panel span-2">
        <p className="eyebrow">ASR MODEL</p>
        <h2>{model?.modelId ?? "No model selected"}</h2>
        <p>{model?.detail}</p>
        <span className="tag">{model?.state.replace("_", " ")}</span>
        <br />
        <button className="secondary" onClick={onPrepareModel}>
          Load model
        </button>
      </article>
      <article className="panel span-2">
        <p className="eyebrow">GPU</p>
        <h2>{gpu?.adapterName ?? "Not checked"}</h2>
        <p>{gpu?.recommendation ?? "Run diagnostics to check NVIDIA availability and VRAM."}</p>
        {gpu?.memoryTotalMb && <strong>{gpu.memoryTotalMb} MB VRAM</strong>}
        <br />
        <button className="secondary" onClick={onDiagnoseGpu}>
          Run diagnostics
        </button>
      </article>
    </section>
  );
}
