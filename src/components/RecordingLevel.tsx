import { useEffect, useState } from "react";
import { listen } from "@tauri-apps/api/event";
import type { AudioLevel } from "../types";
import { useI18n } from "../i18n";

const WAVE_SCALES = [0.4, 0.7, 1, 0.6, 0.9, 0.5, 0.8, 0.55, 0.35];

/** Frequent meter events update only the meter, while it is visible. */
export function RecordingLevel() {
  const { t } = useI18n();
  const [peak, setPeak] = useState(0);
  useEffect(() => {
    let active = true;
    let unlisten: (() => void) | undefined;
    void listen<AudioLevel>("audio-level", ({ payload }) => {
      if (active) setPeak(Number.isFinite(payload.peak) ? Math.min(1, Math.max(0, payload.peak)) : 0);
    }).then((stop) => {
      if (active) unlisten = stop;
      else stop();
    }).catch(() => {});
    return () => { active = false; unlisten?.(); };
  }, []);
  return <div className="wave" aria-label={`${t("Input level")} ${Math.round(peak * 100)} ${t("percent")}`}>
    {WAVE_SCALES.map((scale, index) => <i key={index} style={{ height: Math.max(8, peak * 100 * scale) }} />)}
  </div>;
}
