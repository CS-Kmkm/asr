// Run with node --test scripts/test-settings-save-order.mjs.
// Exercise MainAppContent.saveSettings with saves made while earlier ones are
// pending: sending order, acknowledgements, failures and settings-changed echoes.
// IPC and events are stubs; this does not emulate a browser or the backend.
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { createRequire } from "node:module";
import { test } from "node:test";
import { runInNewContext } from "node:vm";
import ts from "typescript";

const require = createRequire(import.meta.url);
function load(path, mocks = {}, exposeMain = false) {
  let source = readFileSync(new URL(path, import.meta.url), "utf8");
  if (exposeMain) source = source.replace("function MainAppContent(", "export function MainAppContent(");
  const output = ts.transpileModule(source, {
    compilerOptions: { module: ts.ModuleKind.CommonJS, jsx: ts.JsxEmit.ReactJSX },
  }).outputText;
  const module = { exports: {} };
  runInNewContext(output, {
    module, exports: module.exports, window: {},
    require: (name) => name in mocks ? mocks[name] : require(name),
  });
  return module.exports;
}
const { defaultSettings } = load("../src/api.ts");

function fixture(stored = {}) {
  const slots = [];
  const effects = [];
  const listeners = new Map();
  let cursor = 0;
  let dirty = false;
  let tree;
  const settings = { ...structuredClone(defaultSettings), setupComplete: true, ...stored };
  // Each update_settings call waits until the test answers it.
  const saves = [];
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
    getStartupHotkeyWarning: async () => [],
    getAppState: async () => ({ phase: "idle", message: null }),
    getSettings: async () => settings,
    getModelStatus: async () => ({ state: "ready" }),
    getGpuDiagnostics: async () => ({}),
    listAudioDevices: async () => [],
    listDictionary: async () => [],
    listHistory: async () => [],
    listDictionaryCandidates: async () => [],
    updateSettings: (next) => new Promise((resolve, reject) => {
      saves.push({ next: structuredClone(next), resolve: () => resolve(structuredClone(next)), reject });
    }),
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
  const onLanguageChange = () => {};
  function render() {
    cursor = 0;
    dirty = false;
    tree = MainAppContent({ onLanguageChange });
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
  const props = (name) => nodes().find((node) => node.type === `${name}Page`).props;
  return {
    saves, settle, props,
    async open() {
      await settle();
      nodes().find((node) => node.type === "button" && node.props.children === "Settings").props.onClick();
      await settle();
    },
    // What the Settings page currently shows.
    shown: () => props("Settings").settings,
    async save(patch) { props("Settings").onSave(patch); await settle(); },
    async emit(name, payload) { listeners.get(name)({ payload: structuredClone(payload) }); await settle(); },
    notice: () => nodes().find((node) => node.props.className === "notice-message")?.props.children,
  };
}

const profile = (patch = {}) => ({ formality: "formal", detail: "concise", guidance: "", ...patch });

// A select saves at once; the guidance text is committed while it is pending.
async function selectThenCommit(app) {
  await app.save({ globalStyleProfile: profile({ formality: "casual" }) });
  await app.save({ globalStyleProfile: { ...app.shown().globalStyleProfile, guidance: "Be brief" } });
}

test("saves are sent one at a time, each with the newest settings", async () => {
  const app = fixture({ globalStyleProfile: profile() });
  await app.open();
  await selectThenCommit(app);
  // The commit waits until the select's save settles.
  assert.equal(app.saves.length, 1);
  assert.deepEqual(app.saves[0].next.globalStyleProfile, profile({ formality: "casual" }));
  app.saves[0].resolve();
  await app.settle();
  assert.equal(app.saves.length, 2);
  assert.deepEqual(app.saves[1].next.globalStyleProfile, profile({ formality: "casual", guidance: "Be brief" }));
  app.saves[1].resolve();
  await app.settle();
  assert.equal(app.saves.length, 2);
});

test("an older save's acknowledgement or echo cannot revert text committed after it", async () => {
  const app = fixture({ globalStyleProfile: profile() });
  await app.open();
  await selectThenCommit(app);
  assert.equal(app.shown().globalStyleProfile.guidance, "Be brief");
  const [select] = app.saves;
  await app.emit("settings-changed", select.next);
  assert.equal(app.shown().globalStyleProfile.guidance, "Be brief");
  select.resolve();
  await app.settle();
  assert.equal(app.shown().globalStyleProfile.guidance, "Be brief");

  const commit = app.saves[1];
  await app.emit("settings-changed", commit.next);
  assert.equal(app.shown().globalStyleProfile.guidance, "Be brief");
  commit.resolve();
  await app.settle();
  assert.deepEqual(app.shown().globalStyleProfile, profile({ formality: "casual", guidance: "Be brief" }));
});

test("an older save's failure reports its error and leaves the queued save to decide", async () => {
  const app = fixture({ globalStyleProfile: profile() });
  await app.open();
  await selectThenCommit(app);
  app.saves[0].reject(new Error("save failed"));
  await app.settle();
  assert.equal(app.notice(), "Error: save failed");
  assert.deepEqual(app.shown().globalStyleProfile, profile({ formality: "casual", guidance: "Be brief" }));
  // The queued save still sends the failed change along with its own.
  assert.deepEqual(app.saves[1].next.globalStyleProfile, profile({ formality: "casual", guidance: "Be brief" }));
  app.saves[1].resolve();
  await app.settle();
  assert.deepEqual(app.shown().globalStyleProfile, profile({ formality: "casual", guidance: "Be brief" }));
});

test("when every queued save fails, the settings return to the last confirmed ones", async () => {
  const app = fixture({ globalStyleProfile: profile() });
  await app.open();
  await selectThenCommit(app);
  app.saves[0].reject(new Error("first save failed"));
  await app.settle();
  app.saves[1].reject(new Error("second save failed"));
  await app.settle();
  assert.equal(app.notice(), "Error: second save failed");
  assert.deepEqual(app.shown().globalStyleProfile, profile());
});

test("a failed latest save returns to what the previous save confirmed", async () => {
  const app = fixture({ globalStyleProfile: profile() });
  await app.open();
  await selectThenCommit(app);
  app.saves[0].resolve();
  await app.settle();
  app.saves[1].reject(new Error("save failed"));
  await app.settle();
  assert.equal(app.notice(), "Error: save failed");
  assert.deepEqual(app.shown().globalStyleProfile, profile({ formality: "casual" }));
});

test("a failed save keeps unsaved Setup choices and reverts only what it changed", async () => {
  const app = fixture({ setupComplete: false, microphoneId: null });
  await app.settle();
  const setup = app.props("Setup");
  setup.onSettingsChange({ ...setup.settings, microphoneId: "usb-mic" });
  await app.settle();
  app.props("Setup").onFinish();
  await app.settle();
  assert.equal(app.saves[0].next.microphoneId, "usb-mic");
  app.saves[0].reject(new Error("save failed"));
  await app.settle();
  assert.equal(app.props("Setup").settings.microphoneId, "usb-mic");
  assert.equal(app.props("Setup").settings.setupComplete, false);
});

test("settings-changed applies again once no save is queued or in flight", async () => {
  const app = fixture({ globalStyleProfile: profile() });
  await app.open();
  await app.save({ interactionSounds: !app.shown().interactionSounds });
  const elsewhere = { ...app.shown(), translationTargetLanguage: "ja" };
  await app.emit("settings-changed", elsewhere);
  assert.notEqual(app.shown().translationTargetLanguage, "ja");
  app.saves[0].resolve();
  await app.settle();
  // A change made outside this window, such as the tray or a hotkey.
  await app.emit("settings-changed", elsewhere);
  assert.equal(app.shown().translationTargetLanguage, "ja");
});
