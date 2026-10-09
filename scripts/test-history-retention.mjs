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
function load(relativePath, mocks = {}, runtimeWindow = {}) {
  const source = readFileSync(new URL(relativePath, import.meta.url), "utf8");
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
function retentionSelect(answer = true) {
  const prompts = [];
  const module = load("../src/components/HistoryRetentionSelect.tsx", { "../i18n": i18n }, {
    confirm: (message) => { prompts.push(message); return answer; },
  });
  return { ...module, prompts };
}
function retentionChange(element, value) {
  const select = element.type(element.props);
  assert.equal(select.type, "select");
  assert.equal(select.props["aria-label"], "History retention");
  select.props.onChange({ target: { value } });
}
const pages = {
  history: (component, settings, onSave) => load("../src/pages/HistoryPage.tsx", {
    react: hooks, "../components/ui": { Empty: "empty" }, "../components/HistoryRetentionSelect": component, "../i18n": { ...i18n, insertionDetailLabels: {}, insertionOutcomeLabels: {} },
  }).HistoryPage({ settings, history: [], filter: "all", onSave, onFilter() {}, onCopyItem() {}, onRetry() {}, onDelete() {}, onDeleteAll() {}, onLoadAudio() {}, onAudioError() {}, retryActive: false, onCancelRetry() {} }),
  privacy: (component, settings, onSave) => load("../src/pages/PrivacyPage.tsx", {
    "../components/ui": { SettingRow: "row", Toggle: "toggle" }, "../components/HistoryRetentionSelect": component, "../i18n": i18n,
  }).PrivacyPage({ settings, onSave }),
};

for (const [name, render] of Object.entries(pages)) {
  test(`${name} page: shortening retention waits for confirmation and a declined change is not saved`, () => {
    for (const [answer, saves] of [[false, 0], [true, 1]]) {
      const component = retentionSelect(answer);
      const patches = [];
      const tree = render(component, { historyRetention: "forever", deleteAudioAfterProcessing: true }, (patch) => patches.push(patch));
      const element = nodes(tree).find((node) => node.type === component.HistoryRetentionSelect);
      assert.ok(element, "retention select not rendered");
      assert.equal(element.props.value, "forever");
      retentionChange(element, "24_hours");
      assert.equal(component.prompts.length, 1);
      assert.match(component.prompts[0], /older than the new period will be deleted immediately/);
      assert.equal(patches.length, saves);
      if (saves) assert.equal(patches[0].historyRetention, "24_hours");
    }
  });

  test(`${name} page: lengthening retention saves without confirmation`, () => {
    const component = retentionSelect(false);
    const patches = [];
    const tree = render(component, { historyRetention: "one_week", deleteAudioAfterProcessing: true }, (patch) => patches.push(patch));
    retentionChange(nodes(tree).find((node) => node.type === component.HistoryRetentionSelect), "one_year");
    assert.equal(component.prompts.length, 0);
    assert.equal(patches.length, 1);
    assert.equal(patches[0].historyRetention, "one_year");
  });
}

test("choosing Never warns that all History is deleted", () => {
  const component = retentionSelect(false);
  const changes = [];
  retentionChange({ type: component.HistoryRetentionSelect, props: { value: "one_month", onChange: (value) => changes.push(value) } }, "never");
  assert.equal(component.prompts.length, 1);
  assert.match(component.prompts[0], /^History retention: 1 month → Never/);
  assert.match(component.prompts[0], /All History entries, saved recordings, and suggested spellings will be deleted immediately/);
  assert.equal(changes.length, 0);
});

test("retention order treats every move toward Never as shortening", () => {
  const { historyRetentionOptions, shortensHistoryRetention } = retentionSelect();
  const values = Array.from(historyRetentionOptions, ({ value }) => value);
  assert.deepEqual(values, ["never", "24_hours", "one_week", "one_month", "one_year", "forever"]);
  for (const [i, current] of values.entries()) {
    for (const [j, next] of values.entries()) assert.equal(shortensHistoryRetention(current, next), j < i, `${current} -> ${next}`);
  }
});
