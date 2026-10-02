// Run with node --test scripts/test-shortcut-settings.mjs.
// Exercise the SettingsPage event contract with isolated hooks and IPC stubs.
// This covers patch submission; it does not emulate a browser or native hotkeys.
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

function fixture(settings = structuredClone(defaultSettings)) {
  const patches = [];
  const slots = [];
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
    useEffect() {},
  };
  const { SettingsPage } = load("../src/pages/SettingsPage.tsx", {
    react: hooks,
    "@tauri-apps/api/event": {},
    "../components/ui": { SettingRow: "row", Toggle: "toggle" },
    "../components/AiCorrectionSettings": { AiCorrectionSettings: "correction" },
    "../types": registries,
    "../api": {},
    "../i18n": { useI18n: () => ({ language: "en", t: (key) => key }) },
  });
  function render() {
    cursor = 0;
    const tree = SettingsPage({ settings, onSave: (patch) => patches.push(structuredClone(patch)), devices: [], recording: false });
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
  function input(label) {
    return render().find((node) => node.type === "input" && node.props["aria-label"] === label).props;
  }
  return {
    settings, patches,
    change: (label, value) => input(label).onChange({ target: { value } }),
    blur: (label) => input(label).onBlur?.(),
    key: (label, key) => input(label).onKeyDown({ key, preventDefault() {}, currentTarget: { blur() { input(label).onBlur?.(); } } }),
    value: (label) => input(label).value,
    save: () => render().find((node) => node.type === "button" && node.props.children === "Save shortcuts").props.onClick(),
  };
}
const voice = "Voice Translate shortcuts 1";
const selected = "Selected-text translation hotkey";

test("swapping selected-text and voice chords submits one combined patch without blur writes", () => {
  const page = fixture();
  page.change(selected, page.settings.shortcuts.translate[0]);
  page.blur(selected);
  assert.equal(page.patches.length, 0);
  page.change(voice, page.settings.translationHotkey);
  page.blur(voice);
  page.save();
  assert.equal(page.patches.length, 1);
  assert.equal(page.patches[0].translationHotkey, page.settings.shortcuts.translate[0]);
  assert.equal(page.patches[0].shortcuts.translate[0], page.settings.translationHotkey);
});

test("selected-text-only and voice-only edits both use the common Save action", () => {
  for (const label of [selected, voice]) {
    const page = fixture();
    page.change(label, "  Ctrl+Alt+J  ");
    page.save();
    assert.equal(page.patches.length, 1);
    assert.equal(page.patches[0].translationHotkey, label === selected ? "Ctrl+Alt+J" : page.settings.translationHotkey);
    assert.equal(page.patches[0].shortcuts.translate[0], label === voice ? "Ctrl+Alt+J" : page.settings.shortcuts.translate[0]);
  }
});

test("Enter submits a valid combined swap only once", () => {
  const page = fixture();
  page.change(voice, page.settings.translationHotkey);
  page.change(selected, page.settings.shortcuts.translate[0]);
  page.key(selected, "Enter");
  assert.equal(page.patches.length, 1);
  assert.equal(page.patches[0].shortcuts.translate[0], page.settings.translationHotkey);
});

test("an incomplete swap or empty selected-text chord cannot be persisted", () => {
  const page = fixture();
  page.change(selected, page.settings.shortcuts.translate[0]);
  page.save();
  page.change(selected, " ");
  page.key(selected, "Enter");
  assert.equal(page.patches.length, 0);
});

test("Escape restores selected-text draft without saving, and unchanged Save is a no-op", () => {
  const page = fixture();
  page.change(selected, "Ctrl+Alt+J");
  page.key(selected, "Escape");
  assert.equal(page.value(selected), page.settings.translationHotkey);
  page.save();
  assert.equal(page.patches.length, 0);
});
