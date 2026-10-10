// Run with node --test scripts/test-microphone-test-settings.mjs.
// Exercise the SettingsPage microphone-test controls with isolated hooks,
// event, and IPC stubs. This does not emulate a browser, a window, or audio.
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { createRequire } from "node:module";
import { test } from "node:test";
import { runInNewContext } from "node:vm";
import ts from "typescript";

const require = createRequire(import.meta.url);
function load(relativePath, mocks = {}) {
  const source = readFileSync(new URL(relativePath, import.meta.url), "utf8");
  const output = ts.transpileModule(source, {
    compilerOptions: { module: ts.ModuleKind.CommonJS, jsx: ts.JsxEmit.ReactJSX, target: ts.ScriptTarget.ES2021 },
  }).outputText;
  const module = { exports: {} };
  runInNewContext(output, {
    module, exports: module.exports, window: {},
    require: (name) => name in mocks ? mocks[name] : require(name),
  });
  return module.exports;
}
const { defaultSettings } = load("../src/api.ts");
const registries = load("../src/types.ts");
const settle = () => new Promise((resolve) => setImmediate(resolve));

function fixture() {
  const settings = structuredClone(defaultSettings);
  const slots = [];
  const pendingEffects = [];
  const listeners = new Map();
  const calls = { start: 0, stop: 0 };
  let cursor = 0;
  const hooks = {
    useState(initial) {
      const index = cursor++;
      if (!(index in slots)) slots[index] = initial;
      return [slots[index], (next) => { slots[index] = typeof next === "function" ? next(slots[index]) : next; }];
    },
    useRef(initial) {
      const index = cursor++;
      if (!(index in slots)) slots[index] = { current: initial };
      return slots[index];
    },
    // Effects run once, on mount, which is all the listener setup needs.
    useEffect(effect) {
      const index = cursor++;
      if (index in slots) return;
      slots[index] = true;
      pendingEffects.push(effect);
    },
  };
  const { SettingsPage } = load("../src/pages/SettingsPage.tsx", {
    react: hooks,
    "@tauri-apps/api/event": {
      listen: async (event, handler) => {
        listeners.set(event, handler);
        return () => listeners.delete(event);
      },
    },
    "../components/ui": { SettingRow: "row", Toggle: "toggle" },
    "../components/AiCorrectionSettings": { AiCorrectionSettings: "correction" },
    "../types": registries,
    "../api": {
      getShortcutWarning: async () => false,
      startMicrophoneTest: async () => { calls.start += 1; },
      stopMicrophoneTest: async () => { calls.stop += 1; },
    },
    "../i18n": { useI18n: () => ({ language: "en", t: (key) => key }), translateAppMessage: (_language, message) => message },
  });
  function render() {
    cursor = 0;
    const tree = SettingsPage({ settings, onSave: () => {}, devices: [], recording: false });
    pendingEffects.splice(0).forEach((effect) => effect());
    const nodes = [];
    function visit(node) {
      if (Array.isArray(node)) return node.forEach(visit);
      if (!node || typeof node !== "object" || !node.props) return;
      nodes.push(node);
      visit(node.props.children);
      visit(node.props.control);
    }
    visit(tree);
    return nodes;
  }
  const testButton = () => render().find((node) => node.type === "button" &&
    (node.props.children === "Start test" || node.props.children === "Stop test")).props;
  const meter = () => render().find((node) => node.props.role === "meter").props;
  return {
    calls, listeners, meter,
    label: () => testButton().children,
    click: async () => { testButton().onClick(); await settle(); },
    emit: (event, payload) => listeners.get(event)?.({ payload }),
  };
}

test("hiding the window resets a running microphone test to its idle controls", async () => {
  const page = fixture();
  page.label();
  await settle();
  await page.click();
  assert.equal(page.label(), "Stop test");
  page.emit("audio-level", { rms: 0.5, peak: 0.8 });
  assert.notEqual(page.meter()["aria-valuenow"], 0);

  page.emit("microphone-test-stopped");
  assert.equal(page.label(), "Start test");
  assert.equal(page.meter()["aria-valuenow"], 0);
  // The backend already released the device; the page must not stop it again.
  assert.equal(page.calls.stop, 0);

  await page.click();
  assert.equal(page.calls.start, 2);
  assert.equal(page.label(), "Stop test");
});

test("a stopped event without a running test leaves the controls idle", async () => {
  const page = fixture();
  page.label();
  await settle();
  page.emit("microphone-test-stopped");
  assert.equal(page.label(), "Start test");
  assert.equal(page.calls.stop, 0);
});
