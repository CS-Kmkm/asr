// Run with node --test scripts/test-frontend-safety.mjs.
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
const registries = load("../src/types.ts");

test("speech locale registry matches the Rust update_settings allowlist", () => {
  const rust = readFileSync(new URL("../src-tauri/src/types.rs", import.meta.url), "utf8");
  const body = /pub const SPEECH_LOCALES: \[&str; \d+\] = \[([^\]]*)\]/.exec(rust)?.[1];
  assert.ok(body, "SPEECH_LOCALES constant not found");
  const allowlist = [...body.matchAll(/"([^"]+)"/g)].map((match) => match[1]);
  assert.deepEqual(Array.from(registries.speechLocaleRegistry, ({ tag }) => tag), allowlist);
  for (const tag of ["ja-JP", "ko-KR", "de-DE"]) assert.ok(allowlist.includes(tag), tag);
});

const i18n = { useI18n: () => ({ language: "en", t: (key) => key }) };
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

function dictionaryFixture() {
  const slots = [];
  const added = [];
  const validity = [];
  let cursor = 0;
  let effects = [];
  const statefulHooks = {
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
    useMemo: (create) => create(),
    useEffect: (effect) => effects.push(effect),
  };
  const module = load("../src/pages/DictionaryPage.tsx", { react: statefulHooks, "../components/ui": { Empty: "empty" }, "../i18n": i18n });
  function render() {
    cursor = 0;
    effects = [];
    const found = nodes(module.DictionaryPage({
      entries: [], candidates: [], onAdd: async (entry) => { added.push(entry); return true; }, onUpdate: async () => true,
      onDelete() {}, onImport: async () => true, onConfirmCandidate() {}, onRejectCandidate() {},
    }));
    const priority = found.find((node) => node.type === "input" && node.props["aria-label"] === "Priority");
    (priority.props.ref ?? priority.ref).current = { setCustomValidity: (message) => validity.push(message) };
    effects.forEach((effect) => effect());
    return found;
  }
  const priorityInput = () => render().find((node) => node.type === "input" && node.props["aria-label"] === "Priority");
  return {
    module, added, validity, render, priorityInput,
    fill(priority) {
      render().filter((node) => node.type === "input" && node.props.required).forEach((node) => node.props.onChange({ target: { value: "OpenAI" } }));
      priorityInput().props.onChange({ target: { value: priority } });
    },
    submit: () => render().find((node) => node.type === "form").props.onSubmit({ preventDefault() {} }),
    alert: () => render().find((node) => node.type === "p" && node.props.role === "alert")?.props.children,
  };
}

test("dictionary priority accepts only safe whole numbers within the input range", () => {
  const { parsePriority, PRIORITY_MIN, PRIORITY_MAX } = dictionaryFixture().module;
  for (const value of ["1.5", "1e20", "", " ", "abc", "9007199254740993", String(PRIORITY_MAX + 1), String(PRIORITY_MIN - 1)]) {
    assert.equal(parsePriority(value), null, value);
  }
  for (const [value, expected] of [["0", 0], ["-5", -5], ["1e3", 1000], [String(PRIORITY_MAX), PRIORITY_MAX], [String(PRIORITY_MIN), PRIORITY_MIN]]) {
    assert.equal(parsePriority(value), expected, value);
  }
});

test("an invalid dictionary priority shows a translated error and is never submitted", async () => {
  for (const priority of ["1.5", "1e20"]) {
    const page = dictionaryFixture();
    page.fill(priority);
    assert.equal(page.alert(), "Priority must be a whole number from -1000000 to 1000000.");
    assert.equal(page.validity.at(-1), "Priority must be a whole number from -1000000 to 1000000.");
    await page.submit();
    assert.equal(page.added.length, 0);
  }
  const page = dictionaryFixture();
  const input = page.priorityInput().props;
  assert.equal(input.step, 1);
  assert.equal(input.min, page.module.PRIORITY_MIN);
  assert.equal(input.max, page.module.PRIORITY_MAX);
  page.fill("7");
  assert.equal(page.alert(), undefined);
  assert.equal(page.validity.at(-1), "");
  await page.submit();
  assert.equal(page.added.length, 1);
  assert.equal(page.added[0].priority, 7);
  assert.equal(page.added[0].surface, "OpenAI");
});

test("shortcut warning API returns the inactive shortcut list, empty in the browser fallback", async () => {
  const fallback = await load("../src/api.ts").getShortcutWarning();
  assert.ok(Array.isArray(fallback) && fallback.length === 0);
  const inactive = ["Ask Anything (Ctrl+Shift+A)"];
  const api = load("../src/api.ts", {
    "@tauri-apps/api/core": { invoke: async (command) => { assert.equal(command, "get_shortcut_warning"); return inactive; } },
  }, { __TAURI_INTERNALS__: {} });
  assert.deepEqual(await api.getShortcutWarning(), inactive);
});

async function settingsAlerts(inactive) {
  const { defaultSettings } = load("../src/api.ts");
  const slots = [];
  let cursor = 0;
  let effects = [];
  const statefulHooks = {
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
    useEffect: (effect) => effects.push(effect),
  };
  const { SettingsPage } = load("../src/pages/SettingsPage.tsx", {
    react: statefulHooks,
    "@tauri-apps/api/event": {},
    "../components/ui": { SettingRow: "row", Toggle: "toggle" },
    "../components/AiCorrectionSettings": { AiCorrectionSettings: "correction" },
    "../types": registries,
    "../api": { getShortcutWarning: async () => inactive },
    "../i18n": { ...i18n, translateAppMessage: (_language, message) => message },
  });
  const render = () => {
    cursor = 0;
    effects = [];
    return nodes(SettingsPage({ settings: structuredClone(defaultSettings), onSave() {}, devices: [], recording: false }));
  };
  render();
  // Run only the startup-warning fetch; the other effects need native IPC.
  effects.filter((effect) => String(effect).includes("getShortcutWarning")).forEach((effect) => effect());
  await new Promise((resolve) => setImmediate(resolve));
  return render().filter((node) => node.type === "p" && node.props.role === "alert").map((node) => [].concat(node.props.children).join(""));
}

test("Settings shows the startup shortcut warning only when some shortcut is inactive", async () => {
  assert.deepEqual(await settingsAlerts([]), []);
  const [warning, ...rest] = await settingsAlerts(["Voice Translate (Ctrl+Shift+Y)", "Ask Anything (Ctrl+Shift+A)"]);
  assert.equal(rest.length, 0);
  assert.match(warning, /^Some saved shortcuts could not be activated at startup\./);
  assert.match(warning, /Inactive shortcuts: Voice Translate \(Ctrl\+Shift\+Y\), Ask Anything \(Ctrl\+Shift\+A\)$/);
});
