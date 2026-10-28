import React from "react";
import type { ModelProgress } from "../types";
import { useI18n } from "../i18n";
import { CommitField, GainSlider, StepSlider } from "./CommitField";
import { HistoryRetentionSelect } from "./HistoryRetentionSelect";
import { ShortcutField } from "./ShortcutField";
import { Icon, type IconName } from "./Icon";

export function IconButton({ icon, label, context, className = "", type = "button", ...props }: React.ButtonHTMLAttributes<HTMLButtonElement> & { icon: IconName; label: string; context?: string }) {
  return <button {...props} type={type} className={`icon-button ${className}`} aria-label={context ? `${label}: ${context}` : label} title={label}><Icon name={icon} /></button>;
}

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
  detail?: string;
  warning?: boolean;
}) {
  return (
    <article className={`info-card${warning ? " warning" : ""}`}>
      <p className="eyebrow">{label}</p>
      <h3>{value}</h3>
      {detail && <p>{detail}</p>}
    </article>
  );
}

export function SettingRow({
  title,
  detail,
  help,
  detailMode = "hint",
  control,
}: {
  title: string;
  detail?: string;
  help?: string;
  detailMode?: "hint" | "inline";
  control: React.ReactNode;
}) {
  const id = React.useId();
  let index = 0;
  function labelControls(children: React.ReactNode): React.ReactNode {
    return React.Children.map(children, (child) => {
      if (!React.isValidElement<{ id?: string; children?: React.ReactNode; "aria-label"?: string; "aria-labelledby"?: string; "aria-describedby"?: string }>(child)) return child;
      const nativeField = typeof child.type === "string" && ["input", "select", "textarea"].includes(child.type);
      const componentField = child.type === Toggle || child.type === CommitField || child.type === GainSlider || child.type === StepSlider || child.type === HistoryRetentionSelect || child.type === ShortcutField;
      const description = [child.props["aria-describedby"], detail ? `${id}-detail` : undefined].filter(Boolean).join(" ") || undefined;
      if (child.type === "button") return React.cloneElement(child, { "aria-describedby": description });
      if (nativeField || componentField) {
        const controlId = `${id}-${index++}`;
        return React.cloneElement(child, {
          id: child.props.id ?? controlId,
          ...(nativeField ? { "aria-labelledby": child.props["aria-label"] || child.props["aria-labelledby"] ? child.props["aria-labelledby"] : `${id}-title` } : {}),
          "aria-describedby": description,
        });
      }
      return child.props.children ? React.cloneElement(child, undefined, labelControls(child.props.children)) : child;
    });
  }
  const labelledControl = labelControls(control);
  return (
    <div className="setting-row">
      <div>
        <div className="setting-title">
          <label id={`${id}-title`} htmlFor={`${id}-0`}><strong>{title}</strong></label>
          {(help || (detail && detailMode === "hint")) && <HelpHint label={title}>{[detailMode === "hint" ? detail : undefined, help].filter(Boolean).join("\n\n")}</HelpHint>}
        </div>
        {detail && (detailMode === "inline" ? <p id={`${id}-detail`}>{detail}</p>
          : <span className="sr-only" id={`${id}-detail`}>{detail}</span>)}
      </div>
      {labelledControl}
    </div>
  );
}

export function HelpHint({ label, children }: { label: string; children: string }) {
  const { t } = useI18n();
  const id = React.useId();
  const anchor = React.useRef<HTMLSpanElement>(null);
  const tooltip = React.useRef<HTMLSpanElement>(null);
  const [hovered, setHovered] = React.useState(false);
  const [focused, setFocused] = React.useState(false);
  const [dismissed, setDismissed] = React.useState(false);
  const [position, setPosition] = React.useState({ left: 0, above: false });
  const visible = (hovered || focused) && !dismissed;

  React.useLayoutEffect(() => {
    if (!visible || !anchor.current || !tooltip.current) return;
    const bounds = anchor.current.getBoundingClientRect();
    const width = tooltip.current.getBoundingClientRect().width;
    const height = tooltip.current.getBoundingClientRect().height;
    const viewport = document.documentElement;
    setPosition({
      left: Math.max(16 - bounds.left, Math.min(0, viewport.clientWidth - 16 - bounds.left - width)),
      above: bounds.bottom + height > viewport.clientHeight - 16 && bounds.top > height + 16,
    });
  }, [visible, children]);

  React.useEffect(() => {
    if (!visible) return;
    const dismiss = () => setDismissed(true);
    const onKey = (event: KeyboardEvent) => {
      if (event.key === "Escape") {
        if (tooltip.current?.contains(document.activeElement)) anchor.current?.querySelector("button")?.focus();
        dismiss();
      }
    };
    const onOutside = (event: Event) => { if (event.target instanceof Node && !anchor.current?.contains(event.target)) dismiss(); };
    const onScroll = (event: Event) => { if (!(event.target instanceof Node) || !tooltip.current?.contains(event.target)) dismiss(); };
    document.addEventListener("keydown", onKey);
    document.addEventListener("pointerdown", onOutside);
    document.addEventListener("focusin", onOutside);
    window.addEventListener("scroll", onScroll, true);
    window.addEventListener("resize", dismiss);
    return () => {
      document.removeEventListener("keydown", onKey);
      document.removeEventListener("pointerdown", onOutside);
      document.removeEventListener("focusin", onOutside);
      window.removeEventListener("scroll", onScroll, true);
      window.removeEventListener("resize", dismiss);
    };
  }, [visible]);

  return <span ref={anchor} className="help-hint"
    onFocusCapture={() => { setFocused(true); setDismissed(false); }}
    onBlurCapture={event => { if (!event.currentTarget.contains(event.relatedTarget as Node | null)) setFocused(false); }}
    onMouseEnter={() => { setHovered(true); setDismissed(false); }} onMouseLeave={() => setHovered(false)}>
    <button type="button" className="help-trigger" aria-label={`${t("Help")}: ${label}`}
      aria-describedby={visible ? id : undefined}
      onClick={() => { setFocused(true); setDismissed(false); }}><span aria-hidden="true">?</span></button>
    {visible && <span ref={tooltip} role="tooltip" id={id} className={`help-tooltip${position.above ? " above" : ""}`} style={{ left: position.left }}>
      <span className="help-tooltip-content" tabIndex={0}>{children}</span>
    </span>}
  </span>;
}

export function Toggle({
  checked,
  onChange,
  label,
  id,
  "aria-describedby": describedBy,
}: {
  checked: boolean;
  onChange: (value: boolean) => void;
  label: string;
  id?: string;
  "aria-describedby"?: string;
}) {
  return (
    <button
      type="button"
      id={id}
      className={`toggle ${checked ? "on" : ""}`}
      role="switch"
      onClick={() => onChange(!checked)}
      aria-checked={checked}
      aria-label={label}
      aria-describedby={describedBy}
    >
      <span aria-hidden="true" />
    </button>
  );
}

export function Empty({ title, detail, icon = "all" }: { title: string; detail?: string; icon?: IconName }) {
  return (
    <div className="empty">
      <div className="empty-icon" aria-hidden="true"><Icon name={icon} /></div>
      <h3>{title}</h3>
      {detail && <p>{detail}</p>}
    </div>
  );
}
