import { useEffect, useRef, useState } from "react";
import { listen } from "@tauri-apps/api/event";
import { clearRecentApps, listRecentApps } from "../api";
import { useI18n } from "../i18n";
import { categoryLabels, profileCategories } from "../stylePresets";
import { HelpHint, IconButton } from "./ui";
import type { RecentApp } from "../types";

export function RecentAppPicker({ scopes, onChoose }: { scopes: string[]; onChoose: (scope: string) => void }) {
  const { t } = useI18n();
  const [apps, setApps] = useState<RecentApp[]>([]);
  const [failed, setFailed] = useState(false);
  const [clearing, setClearing] = useState(false);
  const requestRef = useRef(0);
  const mounted = useRef(false);
  async function refresh() {
    const request = ++requestRef.current;
    try {
      const items = await listRecentApps();
      if (mounted.current && request === requestRef.current) { setApps(items); setFailed(false); }
    } catch { if (mounted.current && request === requestRef.current) setFailed(true); }
  }
  useEffect(() => {
    mounted.current = true;
    // Subscribe before the initial snapshot, so an observation cannot fall
    // between that snapshot and listener registration without reconciliation.
    const listening = listen("recent-apps-changed", () => { void refresh(); })
      .then((unlisten) => { if (mounted.current) void refresh(); return unlisten; })
      .catch(() => { if (mounted.current) void refresh(); return undefined; });
    return () => { mounted.current = false; requestRef.current += 1; void listening.then((unlisten) => unlisten?.()); };
  }, []);
  async function clear() {
    if (clearing) return;
    setClearing(true);
    requestRef.current += 1;
    try {
      await clearRecentApps();
      if (mounted.current) { requestRef.current += 1; setApps([]); setFailed(false); await refresh(); }
    } catch { if (mounted.current) setFailed(true); }
    finally { if (mounted.current) setClearing(false); }
  }
  return <div className="profile-controls">
    <div className="section-heading recent-apps-heading">
      <div className="setting-title"><span>{t("Recent apps")}</span><HelpHint label={t("Recent apps")}>{t("Recent apps stay in memory until cleared or the app exits. Window titles and paths are not stored.")}</HelpHint></div>
      <div className="action-icons">
        <IconButton icon="trash" label={t("Clear recent apps")} disabled={clearing} onClick={() => { void clear(); }} />
        <IconButton icon="refresh" label={t("Refresh")} disabled={clearing} onClick={() => { void refresh(); }} />
      </div>
    </div>
    <div className="recent-app-selectors">
    <select aria-label={t("Recent apps")} value="" disabled={clearing || apps.length === 0} onChange={(event) => { if (event.target.value) onChoose(`app:${event.target.value}`); }}>
      <option value="">{t("Choose an app")}</option>
      {apps.map((app) => <option key={app.appKey} value={app.appKey} disabled={scopes.includes(`app:${app.appKey}`)}>{app.appKey} · {t(categoryLabels[app.category as keyof typeof categoryLabels] ?? categoryLabels.other)}</option>)}
    </select>
    <label><span className="sr-only">{t("App category")}</span><select aria-label={t("Add category profile")} value="" onChange={(event) => { if (event.target.value) onChoose(`category:${event.target.value}`); }}>
      <option value="">{t("Choose a category")}</option>
      {profileCategories.map((category) => <option key={category} value={category} disabled={scopes.includes(`category:${category}`)}>{t(categoryLabels[category])}</option>)}
    </select></label>
    </div>
    {failed && <p role="alert">{t("Recent apps could not be loaded or cleared.")}</p>}
  </div>;
}
