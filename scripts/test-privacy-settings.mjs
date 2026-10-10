// Run with node --test scripts/test-privacy-settings.mjs.
// Exercise the failed-take retention toggle and History-off listing with
// isolated hooks; this does not emulate a browser or the native backend.
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
const { translate } = load("../src/i18n.tsx");
const i18n = { useI18n: () => ({ language: "en", t: (key) => key }) };
const hooks = {
  useState: (initial) => [typeof initial === "function" ? initial() : initial, () => {}],
  useEffect() {},
};

function nodes(tree) {
  const found = [];
  function visit(node) {
    if (Array.isArray(node)) return node.forEach(visit);
    if (!node || typeof node !== "object" || !node.props) return;
    found.push(node);
    visit(node.props.children);
    visit(node.props.control);
  }
  visit(tree);
  return found;
}
function text(node) {
  if (typeof node === "string" || typeof node === "number") return String(node);
  if (Array.isArray(node)) return node.map(text).join("");
  return node?.props ? text(node.props.children) : "";
}

const KEEP_TITLE = "Keep failed recordings for 24 hours";

function privacyRows(settings) {
  const patches = [];
  const { PrivacyPage } = load("../src/pages/PrivacyPage.tsx", {
    "../components/ui": { SettingRow: "row", Toggle: "toggle" },
    "../i18n": i18n,
  });
  const rows = nodes(PrivacyPage({ settings, onSave: (patch) => patches.push(structuredClone(patch)) }))
    .filter((node) => node.type === "row");
  return { rows, patches };
}

test("failed takes are kept for Retry by default", () => {
  assert.equal(defaultSettings.keepFailedTakes, true);
});

test("the Privacy page explains and toggles failed-take retention next to History retention", () => {
  const { rows, patches } = privacyRows({ ...structuredClone(defaultSettings), historyRetention: "never" });
  const titles = rows.map((row) => row.props.title);
  assert.deepEqual(titles, ["Delete audio after processing", "History retention", KEEP_TITLE]);
  const keep = rows[2];
  assert.match(keep.props.detail, /24 hours/);
  assert.match(keep.props.detail, /History retention is Never/);
  assert.match(keep.props.detail, /Delete audio after processing is on/);
  assert.match(rows[0].props.detail, /except failed recordings kept by the setting below/);
  assert.equal(keep.props.control.props.checked, true);
  keep.props.control.props.onChange(false);
  assert.deepEqual(patches, [{ keepFailedTakes: false }]);

  const off = privacyRows({ ...structuredClone(defaultSettings), keepFailedTakes: false });
  assert.equal(off.rows[2].props.control.props.checked, false);
});

test("the failed-take retention copy is translated", () => {
  for (const key of [
    KEEP_TITLE,
    privacyRows(structuredClone(defaultSettings)).rows[2].props.detail,
    "Transcription failed. The recording is kept in History for 24 hours, where you can retry it.",
    "History is off. Failed recordings are kept here for 24 hours so you can retry them.",
  ]) {
    assert.equal(translate("en", key), key);
    assert.notEqual(translate("ja", key), key, key);
  }
});

function historyPage(settings, history) {
  const { HistoryPage } = load("../src/pages/HistoryPage.tsx", {
    react: hooks,
    "../components/ui": { Empty: "empty" },
    "../i18n": { ...i18n, insertionDetailLabels: {}, insertionOutcomeLabels: {} },
  });
  const noop = () => {};
  return nodes(HistoryPage({
    settings, history, filter: "all", onSave: noop, onFilter: noop, onCopyItem: noop,
    onRetry: noop, onDelete: noop, onDeleteAll: noop, onLoadAudio: async () => ({}),
    onAudioError: noop, retryActive: false, onCancelRetry: noop,
  }));
}

const failedTake = {
  id: 7, transcriptText: "", processedText: null, sourceText: null, instructionText: null,
  actionKind: null, searchSite: null, mode: "faithful", asrProvider: "mock", llmProvider: null,
  targetLanguage: null, appCategory: null, durationMs: 1000, latencyMs: null,
  createdAt: "2030-01-01T00:00:00+00:00", hasAudio: true, retryOfId: null,
  insertionResult: "transcription_failed", insertionDetail: null,
  expiresAt: "2030-01-02T00:00:00+00:00",
};

test("History off still lists failed takes kept for Retry", () => {
  const settings = { ...structuredClone(defaultSettings), historyRetention: "never" };
  const empty = historyPage(settings, []);
  assert.equal(empty.find((node) => node.type === "empty").props.title, "History is disabled");

  const listed = historyPage(settings, [failedTake]);
  assert.equal(listed.some((node) => node.type === "empty"), false);
  assert.ok(listed.some((node) => text(node) === "History is off. Failed recordings are kept here for 24 hours so you can retry them."));
  assert.ok(listed.some((node) => node.type === "small" && text(node).startsWith("Deleted automatically:")));
  const retry = listed.find((node) => node.type === "button" && text(node) === "Retry");
  assert.equal(retry.props.disabled, false);
});
