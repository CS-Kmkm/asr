import React from "react";
import type { ModelProgress } from "../types";
import { useI18n } from "../i18n";

function formatBytes(bytes: number) {
  const mega = bytes / (1024 * 1024);
  return mega >= 1024 ? `${(mega / 1024).toFixed(1)} GB` : `${Math.round(mega)} MB`;
}

export function ModelProgressBar({ progress }: { progress: ModelProgress }) {
  const { t } = useI18n();
  const { completedBytes, totalBytes } = progress;
  // The download size is unknown until Hugging Face reports file metadata, and
  // the transferred amount can slightly exceed it, so keep the bar in range.
  const ratio =
    completedBytes !== null && totalBytes !== null && totalBytes > 0
      ? Math.min(1, completedBytes / totalBytes)
      : null;
  const label =
    progress.stage === "cpu_fallback"
      ? t("CUDA initialization failed. Preparing CPU fallback")
      : progress.stage === "warmup"
      ? t("Warming up speech model")
      : progress.stage === "load"
      ? t("Loading into memory")
      : progress.stage === "setup"
        ? t("Preparing worker environment")
      : ratio !== null && completedBytes !== null && totalBytes !== null
        ? `${Math.round(ratio * 100)}% (${formatBytes(completedBytes)} / ${formatBytes(totalBytes)})`
        : t("Starting download");
  return (
    <div
      className="model-progress"
      role="progressbar"
      aria-label={t("Speech model preparation")}
      aria-valuemin={0}
      aria-valuemax={100}
      aria-valuenow={ratio !== null ? Math.round(ratio * 100) : undefined}
    >
      <span className="model-progress-track">
        <span
          className={`model-progress-value${ratio === null ? " indeterminate" : ""}`}
          style={ratio === null ? undefined : { width: `${ratio * 100}%` }}
        />
      </span>
      <span className="model-progress-label">{label}</span>
    </div>
  );
}

export function InfoCard({
  label,
  value,
  detail,
  warning = false,
}: {
  label: string;
  value: string;
  detail: string;
  warning?: boolean;
}) {
  return (
    <article className={`info-card${warning ? " warning" : ""}`}>
      <p className="eyebrow">{label}</p>
      <h3>{value}</h3>
      <p>{detail}</p>
    </article>
  );
}

export function SettingRow({
  title,
  detail,
  control,
}: {
  title: string;
  detail: string;
  control: React.ReactNode;
}) {
  const id = React.useId();
  let index = 0;
  function labelControls(children: React.ReactNode): React.ReactNode {
    return React.Children.map(children, (child) => {
      if (!React.isValidElement<{ id?: string; children?: React.ReactNode; "aria-label"?: string; "aria-labelledby"?: string; "aria-describedby"?: string }>(child)) return child;
      if (typeof child.type === "string" && ["input", "select", "textarea"].includes(child.type)) {
        const controlId = `${id}-${index++}`;
        return React.cloneElement(child, {
          id: child.props.id ?? controlId,
          "aria-labelledby": child.props["aria-label"] || child.props["aria-labelledby"] ? child.props["aria-labelledby"] : `${id}-title`,
          "aria-describedby": child.props["aria-describedby"] ?? `${id}-detail`,
        });
      }
      return child.props.children ? React.cloneElement(child, undefined, labelControls(child.props.children)) : child;
    });
  }
  const labelledControl = labelControls(control);
  return (
    <div className="setting-row">
      <div>
        <label id={`${id}-title`} htmlFor={`${id}-0`}><strong>{title}</strong></label>
        <p id={`${id}-detail`}>{detail}</p>
      </div>
      {labelledControl}
    </div>
  );
}

export function Toggle({
  checked,
  onChange,
  label,
}: {
  checked: boolean;
  onChange: (value: boolean) => void;
  label: string;
}) {
  return (
    <button
      type="button"
      className={`toggle ${checked ? "on" : ""}`}
      onClick={() => onChange(!checked)}
      aria-pressed={checked}
      aria-label={label}
    >
      <span aria-hidden="true" />
    </button>
  );
}

export function Empty({ title, detail }: { title: string; detail: string }) {
  return (
    <div className="empty">
      <div className="empty-icon">•••</div>
      <h3>{title}</h3>
      <p>{detail}</p>
    </div>
  );
}
