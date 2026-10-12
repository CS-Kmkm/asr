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

function fixture(initialCandidates = [], startupWarning = null) {
  const slots = [];
  const effects = [];
  const listeners = new Map();
  const languageChange = () => {};
  let cursor = 0;
  let dirty = false;
  let tree;
  let settings = { ...structuredClone(defaultSettings), setupComplete: true };
  let candidates = initialCandidates;
  let history = [];
  const pendingCandidates = [];
  const pendingSettings = [];
  const calls = { candidates: 0, history: 0, stop: 0 };
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
    getSettings: async () => settings,
    getModelStatus: async () => ({ state: "ready" }),
    getGpuDiagnostics: async () => ({}),
    listAudioDevices: async () => [],
    listDictionary: async () => [],
    listHistory: async () => { calls.history++; return history; },
    listDictionaryCandidates: async () => {
      calls.candidates++;
      return pendingCandidates.length ? pendingCandidates.shift().promise : candidates;
    },
    updateSettings: async (next) => {
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
    calls, settle,
    navigate(label) { nodes().find((node) => node.type === "button" && node.props.children === label).props.onClick(); render(); },
    props(name) { return nodes().find((node) => node.type === `${name}Page`).props; },
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
