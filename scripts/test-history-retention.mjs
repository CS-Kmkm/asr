// Run with node --test scripts/test-history-retention.mjs.
// Exercise page-level guards with isolated hooks and IPC stubs; this does not
// emulate a browser, so native constraint validation and dialogs are stubbed.
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { createRequire } from "node:module";
import { test } from "node:test";
import { runInNewContext } from "node:vm";
import ts from "typescript";

const require = createRequire(import.meta.url);
function load(relativePath, mocks = {}, runtimeWindow = {}, exposeMain = false) {
  let source = readFileSync(new URL(relativePath, import.meta.url), "utf8");
  if (exposeMain) source = source.replace("function MainAppContent(", "export function MainAppContent(");
  const output = ts.transpileModule(source, {
    compilerOptions: { module: ts.ModuleKind.CommonJS, jsx: ts.JsxEmit.ReactJSX, target: ts.ScriptTarget.ES2021 },
  }).outputText;
  const module = { exports: {} };
  runInNewContext(output, {
    module, exports: module.exports, window: runtimeWindow,
    require: (name) => name in mocks ? mocks[name] : require(name),
  });
  return module.exports;
}

const i18n = { useI18n: () => ({ language: "en", t: (key) => key }) };
const hooks = { useState: (initial) => [initial, () => {}], useRef: (current) => ({ current }), useEffect() {}, useMemo: (create) => create() };
const settle = async () => { for (let i = 0; i < 4; i++) await new Promise(setImmediate); };
function nodes(tree) {
  const found = [];
  (function visit(node) {
    if (Array.isArray(node)) return node.forEach(visit);
    if (!node || typeof node !== "object" || !node.props) return;
    found.push(node);
    visit(node.props.children);
    visit(node.props.control);
  })(tree);
  return found;
}
// `preview` is the backend count, or a function producing it (it may throw).
function retentionSelect(answer = true, preview = { historyItems: 12, recordings: 3 }) {
  const prompts = [];
  const previews = [];
  const api = {
    previewHistoryRetentionPurge: async (retention) => {
      previews.push(retention);
      return typeof preview === "function" ? preview(retention) : preview;
    },
  };
  // Records the counting state and leaves timers to the test to fire.
  const counting = [];
  const timers = [];
  const componentHooks = { ...hooks, useState: (initial) => [initial, (value) => counting.push(value)] };
  const module = load("../src/components/HistoryRetentionSelect.tsx", { react: componentHooks, "../api": api, "../i18n": i18n }, {
    confirm: (message) => { prompts.push(message); return answer; },
    setTimeout: (fn, ms) => timers.push({ fn, ms }),
    clearTimeout() {},
  });
  return { ...module, prompts, previews, counting, timers };
}
function renderSelect(element) {
  const select = element.type(element.props);
  assert.equal(select.type, "select");
  assert.equal(select.props["aria-label"], "History retention");
  return select;
}
function retentionChange(element, value) {
  renderSelect(element).props.onChange({ target: { value } });
}
const pages = {
  history: (component, settings, onSave, settingsLoaded = true) => load("../src/pages/HistoryPage.tsx", {
    react: hooks, "../components/ui": { Empty: "empty" }, "../components/HistoryRetentionSelect": component, "../i18n": { ...i18n, insertionDetailLabels: {}, insertionOutcomeLabels: {} },
  }).HistoryPage({ settings, settingsLoaded, history: [], filter: "all", onSave, onFilter() {}, onCopyItem() {}, onRetry() {}, onDelete() {}, onDeleteAll() {}, onLoadAudio() {}, onAudioError() {}, retryActive: false, onCancelRetry() {} }),
  privacy: (component, settings, onSave, settingsLoaded = true) => load("../src/pages/PrivacyPage.tsx", {
    "../components/ui": { SettingRow: "row", Toggle: "toggle" }, "../components/HistoryRetentionSelect": component, "../i18n": i18n,
  }).PrivacyPage({ settings, settingsLoaded, onSave }),
};

for (const [name, render] of Object.entries(pages)) {
  test(`${name} page: shortening retention shows the counts and waits for confirmation`, async () => {
    for (const [answer, saves] of [[false, 0], [true, 1]]) {
      const component = retentionSelect(answer);
      const patches = [];
      const tree = render(component, { historyRetention: "forever", deleteAudioAfterProcessing: true }, (patch) => patches.push(patch));
      const element = nodes(tree).find((node) => node.type === component.HistoryRetentionSelect);
      assert.ok(element, "retention select not rendered");
      assert.equal(element.props.value, "forever");
      retentionChange(element, "24_hours");
      assert.equal(patches.length, 0, "nothing is saved before the confirmation");
      await settle();
      assert.deepEqual(component.previews, ["24_hours"]);
      assert.equal(component.prompts.length, 1);
      assert.match(component.prompts[0], /History entries to delete: 12\nSaved recordings to delete: 3/);
      assert.match(component.prompts[0], /older than the new period will be deleted immediately/);
      assert.equal(patches.length, saves);
      if (saves) assert.equal(patches[0].historyRetention, "24_hours");
    }
  });

  test(`${name} page: lengthening retention saves without counting or confirmation`, () => {
    const component = retentionSelect(false);
    const patches = [];
    const tree = render(component, { historyRetention: "one_week", deleteAudioAfterProcessing: true }, (patch) => patches.push(patch));
    retentionChange(nodes(tree).find((node) => node.type === component.HistoryRetentionSelect), "one_year");
    assert.equal(component.prompts.length, 0);
    assert.equal(component.previews.length, 0);
    assert.equal(patches.length, 1);
    assert.equal(patches[0].historyRetention, "one_year");
  });

  test(`${name} page: the retention select stays disabled until settings are loaded`, () => {
    const component = retentionSelect();
    for (const loaded of [false, true]) {
      const tree = render(component, { historyRetention: "one_month", deleteAudioAfterProcessing: true }, () => {}, loaded);
      const element = nodes(tree).find((node) => node.type === component.HistoryRetentionSelect);
      assert.equal(element.type(element.props).props.disabled, !loaded);
    }
  });
}

test("choosing Never warns that all History is deleted", async () => {
  const component = retentionSelect(false, { historyItems: 40, recordings: 0 });
  const changes = [];
  retentionChange({ type: component.HistoryRetentionSelect, props: { value: "one_month", onChange: (value) => changes.push(value) } }, "never");
  await settle();
  assert.equal(component.prompts.length, 1);
  assert.match(component.prompts[0], /^History retention: 1 month → Never/);
  assert.match(component.prompts[0], /History entries to delete: 40\nSaved recordings to delete: 0/);
  assert.match(component.prompts[0], /All History entries, saved recordings, and suggested spellings will be deleted immediately/);
  assert.equal(changes.length, 0);
});

test("a shortening that deletes nothing is still confirmed", async () => {
  const component = retentionSelect(false, { historyItems: 0, recordings: 0 });
  const changes = [];
  retentionChange({ type: component.HistoryRetentionSelect, props: { value: "forever", onChange: (value) => changes.push(value) } }, "one_year");
  await settle();
  assert.equal(component.prompts.length, 1);
  assert.match(component.prompts[0], /History entries to delete: 0/);
  assert.equal(changes.length, 0);
});

test("a failed count falls back to the generic warning and still asks before saving", async () => {
  for (const [answer, saves] of [[false, 0], [true, 1]]) {
    const component = retentionSelect(answer, () => { throw new Error("database is locked"); });
    const changes = [];
    retentionChange({ type: component.HistoryRetentionSelect, props: { value: "one_year", onChange: (value) => changes.push(value) } }, "one_week");
    await settle();
    assert.equal(component.prompts.length, 1);
    assert.doesNotMatch(component.prompts[0], /to delete:/);
    assert.equal(component.prompts[0], "History retention: 1 year → 1 week\n\nHistory entries, saved recordings, and suggested spellings older than the new period will be deleted immediately. This cannot be undone. Continue?");
    assert.deepEqual(changes, saves ? ["one_week"] : []);
  }
});

test("further changes while a count is pending do not open a second confirmation", async () => {
  let release;
  const pending = new Promise((resolve) => { release = resolve; });
  const component = retentionSelect(true, () => pending);
  const changes = [];
  const select = renderSelect({ type: component.HistoryRetentionSelect, props: { value: "forever", onChange: (value) => changes.push(value) } });
  select.props.onChange({ target: { value: "one_year" } });
  select.props.onChange({ target: { value: "one_month" } });
  release({ historyItems: 1, recordings: 1 });
  await settle();
  assert.deepEqual(component.previews, ["one_year"]);
  assert.equal(component.prompts.length, 1);
  assert.deepEqual(changes, ["one_year"]);
  assert.deepEqual(component.counting, [true, false]);
});

test("a count that never arrives times out to the generic warning and frees the select", async () => {
  const component = retentionSelect(true, () => new Promise(() => {}));
  const changes = [];
  const select = renderSelect({ type: component.HistoryRetentionSelect, props: { value: "forever", onChange: (value) => changes.push(value) } });
  select.props.onChange({ target: { value: "one_year" } });
  await settle();
  assert.equal(component.prompts.length, 0);
  assert.deepEqual(component.counting, [true]);
  assert.equal(component.timers.length, 1);
  assert.equal(component.timers[0].ms, 3000);
  component.timers[0].fn();
  await settle();
  assert.deepEqual(component.counting, [true, false]);
  assert.equal(component.prompts.length, 1);
  assert.doesNotMatch(component.prompts[0], /to delete:/);
  assert.match(component.prompts[0], /older than the new period will be deleted immediately/);
  assert.deepEqual(changes, ["one_year"]);
  // The stuck call does not block a later shortening.
  select.props.onChange({ target: { value: "one_month" } });
  await settle();
  component.timers[1].fn();
  await settle();
  assert.equal(component.prompts.length, 2);
  assert.deepEqual(changes, ["one_year", "one_month"]);
});

// Renders MainAppContent with the real History page and retention select, so
// the select's displayed value is checked through App's save path.
async function appWithStoredRetention(historyRetention, updateSettings, preview) {
  const slots = [];
  const effects = [];
  let cursor = 0;
  let dirty = false;
  let tree;
  const appHooks = {
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
        effects.push(() => { previous?.cleanup?.(); slots[index] = { deps, cleanup: effect() }; });
      }
    },
  };
  const { defaultSettings } = load("../src/api.ts");
  const stored = { ...structuredClone(defaultSettings), setupComplete: true, historyRetention };
  const calls = { update: 0, saved: [] };
  const api = {
    defaultSettings,
    getStartupHotkeyWarning: async () => [],
    getAppState: async () => ({ phase: "idle", message: null }),
    getSettings: async () => stored,
    getModelStatus: async () => ({ state: "ready" }),
    getGpuDiagnostics: async () => ({}),
    listAudioDevices: async () => [],
    listDictionary: async () => [],
    listHistory: async () => [],
    listDictionaryCandidates: async () => [],
    updateSettings: async (next) => { calls.update++; calls.saved.push(next); return updateSettings(next); },
  };
  const component = retentionSelect(true, preview);
  const historyModule = load("../src/pages/HistoryPage.tsx", {
    react: hooks, "../components/ui": { Empty: "empty" }, "../components/HistoryRetentionSelect": component, "../i18n": { ...i18n, insertionDetailLabels: {}, insertionOutcomeLabels: {} },
  });
  const mocks = {
    react: appHooks,
    "@tauri-apps/api/event": { listen: async () => () => {} },
    "@tauri-apps/api/webviewWindow": { getCurrentWebviewWindow: () => ({ label: "main" }) },
    "./api": api,
    "./i18n": { ...i18n, translate: (_, key) => key, translateAppMessage: (_, value) => value },
    "./components/ui": {},
    "./pages/HistoryPage": historyModule,
  };
  for (const name of ["Dashboard", "Setup", "Settings", "Models", "Dictionary", "Privacy", "Diagnostics"]) {
    mocks[`./pages/${name}Page`] = { [`${name}Page`]: `${name}Page` };
  }
  const { MainAppContent } = load("../src/App.tsx", mocks, {}, true);
  const render = () => {
    cursor = 0;
    dirty = false;
    tree = MainAppContent({ onLanguageChange() {} });
    while (effects.length) effects.shift()();
  };
  const settleApp = async () => {
    if (!tree || dirty) render();
    for (let i = 0; i < 8; i++) {
      await new Promise(setImmediate);
      if (dirty) render();
    }
  };
  const all = () => nodes(tree);
  const navigate = (label) => { all().find((node) => node.type === "button" && node.props.children === label).props.onClick(); render(); };
  await settleApp();
  navigate("History");
  return {
    calls, component, navigate, settle: settleApp,
    props: (name) => all().find((node) => node.type === `${name}Page`).props,
    // The retention select element as the History page currently renders it.
    retentionSelect() {
      const page = all().find((node) => node.type === historyModule.HistoryPage);
      assert.ok(page, "History page not rendered");
      return nodes(page.type(page.props)).find((node) => node.type === component.HistoryRetentionSelect);
    },
  };
}

test("a failed save of a confirmed shorter retention restores the displayed value", async () => {
  const app = await appWithStoredRetention("forever", async () => { throw new Error("save failed"); });
  const element = app.retentionSelect();
  assert.equal(renderSelect(element).props.value, "forever");
  retentionChange(element, "24_hours");
  await app.settle();
  assert.equal(app.component.prompts.length, 1);
  assert.equal(app.calls.update, 1);
  assert.equal(renderSelect(app.retentionSelect()).props.value, "forever");
});

test("a successful save of a confirmed shorter retention shows the new value", async () => {
  const app = await appWithStoredRetention("forever", async (next) => next);
  retentionChange(app.retentionSelect(), "24_hours");
  await app.settle();
  assert.equal(app.calls.update, 1);
  assert.equal(renderSelect(app.retentionSelect()).props.value, "24_hours");
});

test("confirming after a slow count keeps settings changed while it was pending", async () => {
  let release;
  const pending = new Promise((resolve) => { release = resolve; });
  const app = await appWithStoredRetention("forever", async (next) => next, () => pending);
  retentionChange(app.retentionSelect(), "24_hours");
  await app.settle();
  assert.equal(app.calls.update, 0);
  // Another setting changes elsewhere while the count is pending.
  app.navigate("Privacy");
  const audio = app.props("Privacy").settings.deleteAudioAfterProcessing;
  app.props("Privacy").onSave({ deleteAudioAfterProcessing: !audio });
  await app.settle();
  assert.equal(app.calls.update, 1);
  release({ historyItems: 2, recordings: 1 });
  await app.settle();
  assert.equal(app.component.prompts.length, 1);
  assert.equal(app.calls.update, 2);
  assert.equal(app.calls.saved[1].historyRetention, "24_hours");
  assert.equal(app.calls.saved[1].deleteAudioAfterProcessing, !audio);
  assert.equal(app.props("Privacy").settings.deleteAudioAfterProcessing, !audio);
});

test("retention order treats every move toward Never as shortening", () => {
  const { historyRetentionOptions, shortensHistoryRetention } = retentionSelect();
  const values = Array.from(historyRetentionOptions, ({ value }) => value);
  assert.deepEqual(values, ["never", "24_hours", "one_week", "one_month", "one_year", "forever"]);
  for (const [i, current] of values.entries()) {
    for (const [j, next] of values.entries()) assert.equal(shortensHistoryRetention(current, next), j < i, `${current} -> ${next}`);
  }
});
