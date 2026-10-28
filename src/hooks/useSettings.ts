import { useEffect, useRef, useState } from "react";
import { listen } from "@tauri-apps/api/event";
import { defaultSettings, getSettings, updateSettings } from "../api";
import type { Settings, SettingsSnapshot } from "../types";

// A settings key shown with a value changed in this window that the backend
// has not confirmed yet.
interface LocalEdit {
  // Taken anew by every change, so a save confirms only the value it sent.
  generation: number;
  // An unsaved Setup choice: sent with the next save, but kept rather than
  // reverted when that save fails.
  draft: boolean;
}

// How a queued settings save ended.
type SettingsSaveOutcome =
  | { saved: true; settings: Settings }
  | { saved: false; error: unknown };

// `target` with the values of `keys` taken from `source`.
function withKeys(target: Settings, source: Settings, keys: ReadonlyArray<keyof Settings>): Settings {
  const result = { ...target };
  const copy = <K extends keyof Settings>(key: K) => { result[key] = source[key]; };
  keys.forEach(copy);
  return result;
}

// Only the values of `keys` from `source`.
function pickKeys(source: Settings, keys: ReadonlyArray<keyof Settings>): Partial<Settings> {
  const picked: Partial<Settings> = {};
  const copy = <K extends keyof Settings>(key: K) => { picked[key] = source[key]; };
  keys.forEach(copy);
  return picked;
}


interface SettingsCallbacks {
  onLanguageChange: (language: Settings["uiLanguage"]) => void;
  onSetupNeeded: () => void;
  onError: (error: unknown) => void;
  onNotReady: () => void;
}

export function useSettings(options: SettingsCallbacks) {
  const callbacks = useRef(options);
  callbacks.current = options;
  const [settings, setSettings] = useState<Settings>(defaultSettings);
  // A save can run from a callback captured renders ago (the retention select
  // awaits a count and a confirmation first); it must merge its patch into
  // the latest settings, or it would revert changes made in the meantime.
  const settingsRef = useRef(settings);
  settingsRef.current = settings;
  // Saves can follow each other quickly (a select saves at once, text on
  // blur). They are sent one at a time, and each sends only the keys changed
  // here that the backend has not confirmed yet, so a save never rewrites a
  // key changed elsewhere (such as a voice translation's target).
  const saveQueueRef = useRef<Promise<void>>(Promise.resolve());
  const latestSaveRef = useRef(0);
  const pendingSavesRef = useRef(0);
  // The newest backend snapshot applied. Acknowledgements and settings-changed
  // events can arrive in any order; the revision decides which is newer.
  const settingsRevisionRef = useRef(-1);
  const confirmedSettingsRef = useRef(settings);
  // Keys shown with their local value instead of the confirmed one, until a
  // save confirms that value; a failed latest save returns them to the
  // confirmed values.
  const localEditsRef = useRef(new Map<keyof Settings, LocalEdit>());
  const editGenerationRef = useRef(0);
  // A shorter history retention purges data, so nothing is saved until the
  // stored settings are known.
  const [settingsLoaded, setSettingsLoaded] = useState(false);
  const settingsLoadedRef = useRef(false);
  const settingsRequestRef = useRef(0);

  useEffect(() => {
    let active = true;
    let unlisten: (() => void) | undefined;
    void loadSettings();
    void listen<SettingsSnapshot>("settings-changed", ({ payload }) => {
      if (active) applyReadSnapshot(payload);
    }).then((stop) => {
      if (active) unlisten = stop;
      else stop();
    }).catch((error: unknown) => { if (active) callbacks.current.onError(error); });
    return () => { active = false; settingsRequestRef.current += 1; unlisten?.(); };
  }, []);

  // Shows the confirmed settings with every key changed here keeping its
  // local value.
  function showConfirmedWithLocalEdits() {
    const shown = withKeys(confirmedSettingsRef.current, settingsRef.current, Array.from(localEditsRef.current.keys()));
    settingsRef.current = shown;
    setSettings(shown);
  }

  // Records that `keys` now show local values not yet confirmed.
  function recordLocalEdits(keys: ReadonlyArray<keyof Settings>, draft: boolean) {
    for (const key of keys) {
      const previous = localEditsRef.current.get(key);
      localEditsRef.current.set(key, {
        generation: ++editGenerationRef.current,
        // A key a save already changed stays one to revert on failure.
        draft: draft && (previous?.draft ?? true),
      });
    }
  }

  // Applies a backend snapshot (the load, a save's acknowledgement or
  // settings-changed) unless a newer one is applied already, keeping local
  // values of unconfirmed keys. The first one also decides whether
  // onboarding is still needed.
  function applyBackendSnapshot(snapshot: SettingsSnapshot): boolean {
    if (snapshot.revision <= settingsRevisionRef.current) return false;
    settingsRevisionRef.current = snapshot.revision;
    const first = !settingsLoadedRef.current;
    settingsLoadedRef.current = true;
    confirmedSettingsRef.current = snapshot.settings;
    showConfirmedWithLocalEdits();
    setSettingsLoaded(true);
    if (first && !snapshot.settings.setupComplete) callbacks.current.onSetupNeeded();
    return true;
  }

  // Applies a snapshot that is not a save's acknowledgement. While saves are
  // pending, the latest one's acknowledgement switches the UI language.
  function applyReadSnapshot(snapshot: SettingsSnapshot) {
    if (applyBackendSnapshot(snapshot) && pendingSavesRef.current === 0) {
      callbacks.current.onLanguageChange(snapshot.settings.uiLanguage);
    }
  }

  async function loadSettings() {
    const request = ++settingsRequestRef.current;
    try {
      const snapshot = await getSettings();
      // A superseded read is dropped, and an older one cannot replace newer
      // settings (for example from settings-changed after a save).
      if (request !== settingsRequestRef.current) return;
      applyReadSnapshot(snapshot);
    } catch (error) {
      if (request === settingsRequestRef.current && !settingsLoadedRef.current) callbacks.current.onError(error);
    }
  }

  // Setup changes choices locally and saves them with Finish. They go with
  // whichever save comes next, and a failed save keeps them.
  function editSettingsLocally(next: Settings) {
    const keys = Object.keys(next) as Array<keyof Settings>;
    recordLocalEdits(keys.filter((key) => !Object.is(next[key], settingsRef.current[key])), true);
    settingsRef.current = next;
    setSettings(next);
  }

  // Refuses a save while only the defaults are known, and retries the load so
  // a transient startup failure does not block saving for the whole session.
  function settingsReady() {
    if (settingsLoadedRef.current) return true;
    callbacks.current.onNotReady();
    void loadSettings();
    return false;
  }

  // Shows `patch` at once and queues its save. Saves are sent one at a time
  // in the order they were made; when its turn comes, a save sends every key
  // still unconfirmed with the value shown then, or nothing at all when an
  // earlier save already confirmed them. The outcome carries the settings
  // the backend stored.
  async function queueSettingsSave(patch: Partial<Settings>): Promise<SettingsSaveOutcome> {
    recordLocalEdits(Object.keys(patch) as Array<keyof Settings>, false);
    const next = { ...settingsRef.current, ...patch };
    settingsRef.current = next;
    setSettings(next);
    const save = ++latestSaveRef.current;
    pendingSavesRef.current += 1;
    const turn = saveQueueRef.current;
    let endTurn = () => {};
    saveQueueRef.current = new Promise<void>((resolve) => { endTurn = resolve; });
    await turn;
    const sent = Array.from(localEditsRef.current, ([key, edit]) => ({ key, generation: edit.generation }));
    let snapshot: SettingsSnapshot | null = null;
    try {
      if (sent.length > 0) {
        snapshot = await updateSettings(pickKeys(settingsRef.current, sent.map(({ key }) => key)));
      }
    } catch (error) {
      // A later queued save sends the unconfirmed keys again and decides.
      // Otherwise each of them returns to its confirmed value; unsaved Setup
      // choices stay.
      if (save === latestSaveRef.current) {
        const reverted = Array.from(localEditsRef.current)
          .filter(([, edit]) => !edit.draft)
          .map(([key]) => key);
        for (const key of reverted) localEditsRef.current.delete(key);
        const restored = withKeys(settingsRef.current, confirmedSettingsRef.current, reverted);
        settingsRef.current = restored;
        setSettings(restored);
        // A save can fail after its settings were stored (a later step of the
        // command failed) without a settings-changed; a newer stored revision
        // then replaces the restored values.
        void loadSettings();
      }
      pendingSavesRef.current -= 1;
      endTurn();
      return { saved: false, error };
    }
    // A key changed again after it was sent stays unconfirmed for the next save.
    for (const { key, generation } of sent) {
      if (localEditsRef.current.get(key)?.generation === generation) localEditsRef.current.delete(key);
    }
    // An acknowledgement older than an applied settings-changed only
    // confirms keys; the newer snapshot stays shown.
    if (!snapshot || !applyBackendSnapshot(snapshot)) showConfirmedWithLocalEdits();
    pendingSavesRef.current -= 1;
    endTurn();
    // An older acknowledgement must not switch back a language chosen by a
    // newer save, so only the latest save switches the UI language.
    if (save === latestSaveRef.current) callbacks.current.onLanguageChange(settingsRef.current.uiLanguage);
    return { saved: true, settings: snapshot?.settings ?? confirmedSettingsRef.current };
  }


  return { settings, settingsRef, settingsLoaded, editSettingsLocally, settingsReady, queueSettingsSave };
}
