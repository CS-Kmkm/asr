import { useCallback, useEffect, useState } from "react";

export interface Notice {
  message: string; code?: string; outcome?: string;
  severity: "info" | "success" | "warning" | "error";
  kind?: string;
}
function priority(notice: Notice) {
  return ({ info: 0, success: 1, warning: 2, error: 3 }[notice.severity]) * 2
    + Number(Boolean(notice.code?.startsWith("ai_") && notice.outcome !== undefined));
}
export function useNoticeQueue() {
  const [queue, setQueue] = useState<Notice[]>([]);
  const notice = queue[0] ?? null;
  const push = useCallback((next: Notice) => setQueue((current) => {
    if (current.some((item) => item.message === next.message && item.code === next.code && item.kind === next.kind)) return current;
    return [...current, next].sort((a, b) => priority(b) - priority(a)).slice(0, 5);
  }), []);
  const dismiss = useCallback(() => setQueue((current) => current.slice(1)), []);
  const clear = useCallback(() => setQueue([]), []);
  const removeKinds = useCallback((kinds: readonly string[]) => setQueue((current) => current.filter((item) => !item.kind || !kinds.includes(item.kind))), []);
  useEffect(() => {
    if (!notice || !["info", "success"].includes(notice.severity) || ["model_loading", "model_downloading", "worker_setup"].includes(notice.kind ?? "")) return;
    const timer = window.setTimeout(() => setQueue((current) => current[0] === notice ? current.slice(1) : current), 4000);
    return () => window.clearTimeout(timer);
  }, [notice]);
  return { notice, push, dismiss, clear, removeKinds };
}
