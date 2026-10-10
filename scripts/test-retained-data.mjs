// Exercise MainAppContent effects and IPC/event ordering without a browser.
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { createRequire } from "node:module";
import { test } from "node:test";
import { runInNewContext } from "node:vm";
import ts from "typescript";

const require = createRequire(import.meta.url);
function load(path, mocks = {}, exposeMain = false, runtimeWindow = {}) {
  let source = readFileSync(new URL(path, import.meta.url), "utf8");
  if (exposeMain) source = source.replace("function MainAppContent(", "export function MainAppContent(");
  const output = ts.transpileModule(source, {
    compilerOptions: { module: ts.ModuleKind.CommonJS, jsx: ts.JsxEmit.ReactJSX },
  }).outputText;
  const module = { exports: {} };
  runInNewContext(output, {
    module, exports: module.exports, window: runtimeWindow,
    require: (name) => name in mocks ? mocks[name] : require(name),
  });
  return module.exports;
}
const { defaultSettings } = load("../src/api.ts");
function startupWarningApi(response) {
  return load("../src/api.ts", {
    "@tauri-apps/api/core": { invoke: async (command) => {
      assert.equal(command, "get_startup_hotkey_warning");
      return response;
    } },
  }, false, { __TAURI_INTERNALS__: {} });
}

test("No native startup shortcut warning becomes an empty warning list", async () => {
  const api = startupWarningApi(null);
  assert.deepEqual(Array.from(await api.getStartupHotkeyWarning()), []);
});

test("Native startup shortcut warning text is retained as one warning", async () => {
  const warning = "Some saved hotkeys overlap or could not be registered. Change them in Settings. Inactive: Ask Anything (Ctrl+Shift+A)";
  const api = startupWarningApi(warning);
  assert.deepEqual(Array.from(await api.getStartupHotkeyWarning()), [warning]);
});
const candidate = (id) => ({ id, originalSpan: "open ai", preferredSpan: `OpenAI ${id}` });
function deferred() {
  let resolve;
  const promise = new Promise((done) => { resolve = done; });
  return { promise, resolve };
}

function fixture(initialCandidates = [], startupWarning = null, startup = {}) {
  const slots = [];
  const effects = [];
  const listeners = new Map();
  const languageChange = () => {};
  let cursor = 0;
  let dirty = false;
  let tree;
  let settings = { ...structuredClone(defaultSettings), setupComplete: true, ...startup.stored };
  let candidates = initialCandidates;
  let history = [];
  const pendingCandidates = [];
  const pendingSettings = [];
  const calls = { candidates: 0, history: 0, stop: 0, settings: 0, update: 0 };
  const hooks = {
    useState(initial) {
      const index = cursor++;
      if (!(index in slots)) slots[index] = typeof initial === "function" ? initial() : initial;
      return [slots[index], (next) => {
        const value = typeof next === "function" ? next(slots[index]) : next;
        if (!Object.is(value, slots[index])) { slots[index] = value; dirty = true; }
      }];
    },
    useRef(initial) {
      const index = cursor++;
      if (!(index in slots)) slots[index] = { current: initial };
      return slots[index];
    },
    useMemo(make) { cursor++; return make(); },
    useEffect(effect, deps) {
      const index = cursor++;
      const previous = slots[index];
      if (!previous || deps.some((value, i) => !Object.is(value, previous.deps[i]))) {
        effects.push(() => {
          previous?.cleanup?.();
          slots[index] = { deps, cleanup: effect() };
        });
      }
    },
  };
  const api = {
    defaultSettings,
    getStartupHotkeyWarning: startupWarningApi(startupWarning).getStartupHotkeyWarning,
    getAppState: async () => ({ phase: "idle", message: null }),
    getSettings: async () => { calls.settings++; if (startup.settings) await startup.settings(); return settings; },
    getModelStatus: async () => ({ state: "ready" }),
    getGpuDiagnostics: async () => { if (startup.gpuFails) throw new Error("GPU probe failed"); return {}; },
    listAudioDevices: async () => [],
    listDictionary: async () => [],
    listHistory: async () => { calls.history++; return history; },
    listDictionaryCandidates: async () => {
      calls.candidates++;
      return pendingCandidates.length ? pendingCandidates.shift().promise : candidates;
    },
    updateSettings: async (next) => {
      calls.update++;
      if (startup.updateFails) throw new Error("save failed");
      if (pendingSettings.length) await pendingSettings.shift().promise;
      settings = next;
      return next;
    },
    stopRecording: async () => { calls.stop++; },
  };
  const mocks = {
    react: hooks,
    "@tauri-apps/api/event": { listen: async (name, fn) => { listeners.set(name, fn); return () => listeners.delete(name); } },
    "@tauri-apps/api/webviewWindow": { getCurrentWebviewWindow: () => ({ label: "main" }) },
    "./api": api,
    "./i18n": { useI18n: () => ({ language: "en", t: (key) => key }), translate: (_, key) => key, translateAppMessage: (_, value) => value },
    "./components/ui": {},
  };
  for (const name of ["Dashboard", "Setup", "Settings", "Models", "History", "Dictionary", "Privacy", "Diagnostics"]) {
    mocks[`./pages/${name}Page`] = { [`${name}Page`]: `${name}Page` };
  }
  const { MainAppContent } = load("../src/App.tsx", mocks, true);
  function render() {
    cursor = 0;
    dirty = false;
    tree = MainAppContent({ onLanguageChange: languageChange });
    while (effects.length) effects.shift()();
  }
  function nodes() {
    const result = [];
    function visit(node) {
      if (Array.isArray(node)) return node.forEach(visit);
      if (!node?.props) return;
      result.push(node);
      visit(node.props.children);
    }
    visit(tree);
    return result;
  }
  async function settle() {
    if (!tree || dirty) render();
    for (let i = 0; i < 8; i++) {
      await new Promise(setImmediate);
      if (dirty) render();
    }
  }
  return {
    calls, settle, stored: () => settings,
    navigate(label) { nodes().find((node) => node.type === "button" && node.props.children === label).props.onClick(); render(); },
    props(name) { return nodes().find((node) => node.type === `${name}Page`).props; },
    emit(name, payload) { listeners.get(name)({ payload }); render(); },
    shows(name) { return nodes().some((node) => node.type === `${name}Page`); },
    event(phase) { listeners.get("app-state")({ payload: { phase, message: null } }); render(); },
    candidates(value) { candidates = value; },
    history(value) { history = value; },
    deferCandidates() { const pending = deferred(); pendingCandidates.push(pending); return pending; },
    deferSettings() { const pending = deferred(); pendingSettings.push(pending); return pending; },
  };
}

test("The main interface renders after a null native startup warning", async () => {
  const app = fixture();
  await app.settle();
  assert.ok(app.props("Dashboard"));
});

test("The main interface renders after a native startup warning string", async () => {
  const app = fixture([], "Some saved hotkeys overlap or could not be registered.");
  await app.settle();
  assert.ok(app.props("Dashboard"));
});

test("Dictionary page entry and shortcut completion load current candidates", async () => {
  const app = fixture();
  await app.settle();
  app.candidates([candidate(1)]);
  app.navigate("Dictionary");
  await app.settle();
  assert.equal(app.props("Dictionary").candidates[0].id, 1);
  app.event("recording");
  await app.settle();
  app.candidates([candidate(2)]);
  app.event("completed");
  await app.settle();
  assert.equal(app.props("Dictionary").candidates[0].id, 2);
  assert.equal(app.calls.stop, 0);
});

test("Never hides cached candidates and invalidates a pending old response", async () => {
  const app = fixture([candidate(1)]);
  await app.settle();
  app.navigate("Dictionary");
  await app.settle();
  assert.equal(app.props("Dictionary").candidates[0].id, 1);
  const old = app.deferCandidates();
  app.event("completed");
  await app.settle();
  app.navigate("Privacy");
  app.props("Privacy").onSave({ historyRetention: "never" });
  await app.settle();
  app.candidates([]);
  app.navigate("Dictionary");
  assert.equal(app.props("Dictionary").candidates.length, 0);
  await app.settle();
  old.resolve([candidate(1)]);
  await app.settle();
  assert.equal(app.props("Dictionary").candidates.length, 0);
});

test("A shorter retention reloads candidates instead of restoring the prior cache", async () => {
  const app = fixture();
  await app.settle();
  app.candidates([candidate(1), candidate(2)]);
  app.navigate("Dictionary");
  await app.settle();
  app.navigate("Privacy");
  app.props("Privacy").onSave({ historyRetention: "24_hours" });
  await app.settle();
  app.candidates([candidate(2)]);
  app.navigate("Dictionary");
  await app.settle();
  assert.deepEqual(Array.from(app.props("Dictionary").candidates, (item) => item.id), [2]);
});

test("The latest visible-page request wins over an older completion refresh", async () => {
  const app = fixture();
  await app.settle();
  const old = app.deferCandidates();
  app.navigate("Dictionary");
  await app.settle();
  app.candidates([candidate(2)]);
  app.event("completed");
  await app.settle();
  old.resolve([candidate(1)]);
  await app.settle();
  assert.equal(app.props("Dictionary").candidates[0].id, 2);
});

test("Retention refresh after persistence supersedes an optimistic pre-purge read", async () => {
  const app = fixture();
  await app.settle();
  app.candidates([candidate(1), candidate(2)]);
  app.navigate("Privacy");
  const save = app.deferSettings();
  app.props("Privacy").onSave({ historyRetention: "24_hours" });
  await app.settle();
  app.navigate("Dictionary");
  await app.settle();
  assert.equal(app.props("Dictionary").candidates.length, 2);
  app.candidates([candidate(2)]);
  save.resolve();
  await app.settle();
  assert.deepEqual(Array.from(app.props("Dictionary").candidates, (item) => item.id), [2]);
});

test("History page entry and shortcut completion refresh retained rows", async () => {
  const app = fixture();
  await app.settle();
  app.history([{ id: 1 }]);
  app.navigate("History");
  await app.settle();
  assert.equal(app.props("History").history[0].id, 1);
  app.event("recording");
  await app.settle();
  app.history([{ id: 2 }]);
  app.event("completed");
  await app.settle();
  assert.equal(app.props("History").history[0].id, 2);
});

test("No settings save or purge happens before the stored settings load", async () => {
  const load = deferred();
  const app = fixture([], null, { stored: { historyRetention: "forever" }, settings: () => load.promise });
  await app.settle();
  app.navigate("Privacy");
  assert.equal(app.props("Privacy").settingsLoaded, false);
  // The defaults (1 month) are all that is known; picking 1 year would look
  // like lengthening and skip the confirmation, then purge the stored Forever.
  app.props("Privacy").onSave({ historyRetention: "one_year" });
  await app.settle();
  assert.equal(app.calls.update, 0);
  assert.equal(app.stored().historyRetention, "forever");
  load.resolve();
  await app.settle();
  assert.equal(app.props("Privacy").settingsLoaded, true);
  assert.equal(app.props("Privacy").settings.historyRetention, "forever");
});

test("A rejected non-settings startup call still loads settings and allows saving", async () => {
  const app = fixture([], null, { stored: { historyRetention: "forever" }, gpuFails: true });
  await app.settle();
  app.navigate("History");
  assert.equal(app.props("History").settingsLoaded, true);
  assert.equal(app.props("History").settings.historyRetention, "forever");
  app.props("History").onSave({ historyRetention: "one_year" });
  await app.settle();
  assert.equal(app.calls.update, 1);
  assert.equal(app.stored().historyRetention, "one_year");
});

test("A late settings load cannot replace settings already reported by settings-changed", async () => {
  const loads = [deferred(), deferred()];
  let next = 0;
  const app = fixture([], null, { stored: { historyRetention: "forever" }, settings: () => loads[next++].promise });
  await app.settle();
  app.navigate("Privacy");
  app.props("Privacy").onSave({ historyRetention: "one_year" });
  await app.settle();
  assert.equal(next, 2, "the refused save retries the load");
  const current = { ...app.stored(), historyRetention: "one_week" };
  app.emit("settings-changed", current);
  assert.equal(app.props("Privacy").settingsLoaded, true);
  loads[1].resolve();
  loads[0].resolve();
  await app.settle();
  assert.equal(app.props("Privacy").settings.historyRetention, "one_week");
  assert.equal(app.calls.update, 0);
});

test("Settings first obtained by a retry or settings-changed still route an unfinished setup once", async () => {
  let fail = true;
  const retried = fixture([], null, { stored: { setupComplete: false }, settings: async () => { if (fail) throw new Error("not ready"); } });
  await retried.settle();
  assert.ok(retried.shows("Dashboard"));
  fail = false;
  retried.navigate("Privacy");
  retried.props("Privacy").onSave({ historyRetention: "one_year" });
  await retried.settle();
  assert.equal(retried.calls.update, 0);
  assert.ok(retried.shows("Setup"));

  const evented = fixture([], null, { stored: { setupComplete: false }, settings: () => new Promise(() => {}) });
  await evented.settle();
  evented.emit("settings-changed", { ...evented.stored() });
  await evented.settle();
  assert.ok(evented.shows("Setup"));
  evented.navigate("Status");
  evented.emit("settings-changed", { ...evented.stored() });
  await evented.settle();
  assert.ok(evented.shows("Dashboard"));
});

test("Finishing setup leaves the Setup page only after the save succeeds", async () => {
  const unloaded = fixture([], null, { settings: () => new Promise(() => {}) });
  await unloaded.settle();
  unloaded.navigate("Setup");
  unloaded.props("Setup").onFinish();
  await unloaded.settle();
  assert.ok(unloaded.shows("Setup"));
  assert.equal(unloaded.calls.update, 0);
  const startup = { stored: { setupComplete: false }, updateFails: true };
  const app = fixture([], null, startup);
  await app.settle();
  assert.ok(app.shows("Setup"));
  app.props("Setup").onFinish();
  await app.settle();
  assert.ok(app.shows("Setup"));
  startup.updateFails = false;
  app.props("Setup").onFinish();
  await app.settle();
  assert.ok(app.shows("Dashboard"));
  assert.equal(app.stored().setupComplete, true);
});
