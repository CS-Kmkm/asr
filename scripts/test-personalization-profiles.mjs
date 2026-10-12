// Run with node --test scripts/test-personalization-profiles.mjs.
// Exercise the SettingsPage personalization profile fields with isolated hooks
// and IPC stubs: typing edits a draft that is saved on blur or Enter, never per
// keystroke. Saves apply optimistically, as App.saveSettings does; this does not
// emulate a browser, focus handling, or the backend.
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

const GUIDANCE = "Optional guidance";
const SCOPE = "app:code or category:development";
const configuredGlobal = { formality: "formal", detail: "concise", guidance: "" };

function fixture(initial = {}) {
  let settings = { ...structuredClone(defaultSettings), ...structuredClone(initial) };
  const patches = [];
  const slots = [];
  const pendingEffects = [];
  let cursor = 0;
  const hooks = {
    useState(value) {
      const index = cursor++;
      if (!(index in slots)) slots[index] = value;
      return [slots[index], (next) => { slots[index] = typeof next === "function" ? next(slots[index]) : next; }];
    },
    useRef(value) {
      const index = cursor++;
      if (!(index in slots)) slots[index] = { current: value };
      return slots[index];
    },
    // Effects run after a render whenever a dependency changes identity.
    useEffect(effect, deps) {
      const index = cursor++;
      const previous = slots[index];
      if (!previous || deps.some((dep, at) => !Object.is(dep, previous[at]))) {
        slots[index] = deps;
        pendingEffects.push(effect);
      }
    },
  };
  const { SettingsPage } = load("../src/pages/SettingsPage.tsx", {
    react: hooks,
    "@tauri-apps/api/event": { listen: async () => () => {} },
    "../components/ui": { SettingRow: "row", Toggle: "toggle" },
    "../components/AiCorrectionSettings": { AiCorrectionSettings: "correction" },
    "../components/SettingsRepairBanner": { SettingsRepairBanner: "repair-banner" },
    "../types": registries,
    "../api": {
      getShortcutWarning: async () => [],
      startMicrophoneTest: async () => {},
      stopMicrophoneTest: async () => {},
    },
    "../i18n": { useI18n: () => ({ language: "en", t: (key) => key }), translateAppMessage: (_language, message) => message },
  });
  // Like App.saveSettings: the patch is applied before the backend answers.
  const onSave = (patch) => {
    patches.push(structuredClone(patch));
    settings = { ...settings, ...patch };
  };
  function renderOnce() {
    cursor = 0;
    const tree = SettingsPage({ settings, onSave, devices: [], recording: false });
    const nodes = [];
    function visit(node) {
      if (Array.isArray(node)) return node.forEach(visit);
      if (!node || typeof node !== "object" || !node.props) return;
      // Render nested components (PersonalizationProfiles) with the same hooks.
      if (typeof node.type === "function") return visit(node.type(node.props));
      nodes.push(node);
      visit(node.props.children);
      visit(node.props.control);
    }
    visit(tree);
    return nodes;
  }
  function render() {
    let nodes = renderOnce();
    for (let pass = 0; pendingEffects.length && pass < 5; pass += 1) {
      pendingEffects.splice(0).forEach((effect) => effect());
      nodes = renderOnce();
    }
    return nodes;
  }
  // Global guidance is the first guidance field, before the scoped rows.
  const inputs = (placeholder) => render().filter((node) => node.type === "input" && node.props.placeholder === placeholder);
  const input = (placeholder, index) => inputs(placeholder)[index].props;
  const selects = () => render().filter((node) => node.type === "select" &&
    [node.props.children].flat().some((option) => option?.props?.value === "formal"));
  return {
    patches,
    get settings() { return settings; },
    // A settings-changed event or a save acknowledgement from the backend.
    receive(patch) { settings = { ...settings, ...structuredClone(patch) }; render(); },
    value: (placeholder, index = 0) => input(placeholder, index).value,
    type(placeholder, index, text) {
      for (let end = 1; end <= text.length; end += 1) input(placeholder, index).onChange({ target: { value: text.slice(0, end) } });
    },
    blur: (placeholder, index = 0) => input(placeholder, index).onBlur?.(),
    // A key handler's blur() reaches the handlers of the same render.
    key(placeholder, index, key) {
      const props = input(placeholder, index);
      props.onKeyDown({ key, preventDefault() {}, currentTarget: { blur: () => props.onBlur?.() } });
    },
    select: (index, value) => selects()[index].props.onChange({ target: { value } }),
    click: (label) => render().find((node) => node.type === "button" && node.props.children === label).props.onClick(),
  };
}

test("global guidance is saved once on blur instead of per keystroke", () => {
  const page = fixture({ globalStyleProfile: configuredGlobal });
  page.type(GUIDANCE, 0, "Be brief");
  assert.equal(page.patches.length, 0);
  assert.equal(page.value(GUIDANCE), "Be brief");
  page.blur(GUIDANCE);
  assert.deepEqual(page.patches, [{ globalStyleProfile: { ...configuredGlobal, guidance: "Be brief" } }]);
  // An unchanged field saves nothing on a later blur.
  page.blur(GUIDANCE);
  assert.equal(page.patches.length, 1);
});

test("Enter saves global guidance once and Escape restores it without saving", () => {
  const page = fixture({ globalStyleProfile: { ...configuredGlobal, guidance: "Saved" } });
  page.type(GUIDANCE, 0, "Typed");
  page.key(GUIDANCE, 0, "Enter");
  assert.deepEqual(page.patches, [{ globalStyleProfile: { ...configuredGlobal, guidance: "Typed" } }]);
  page.type(GUIDANCE, 0, "Discarded");
  page.key(GUIDANCE, 0, "Escape");
  assert.equal(page.value(GUIDANCE), "Typed");
  assert.equal(page.patches.length, 1);
});

test("an acknowledgement or backend change never overwrites guidance being edited", () => {
  const page = fixture({ globalStyleProfile: configuredGlobal });
  page.type(GUIDANCE, 0, "abc");
  page.blur(GUIDANCE);
  page.type(GUIDANCE, 0, "abcd");
  // The acknowledgement of the earlier save arrives while typing continues.
  page.receive({ globalStyleProfile: { ...configuredGlobal, guidance: "abc" } });
  assert.equal(page.value(GUIDANCE), "abcd");
  page.receive({ interactionSounds: !page.settings.interactionSounds });
  assert.equal(page.value(GUIDANCE), "abcd");
  page.receive({ globalStyleProfile: { ...configuredGlobal, guidance: "elsewhere" } });
  assert.equal(page.value(GUIDANCE), "abcd");
  page.key(GUIDANCE, 0, "Escape");
  assert.equal(page.value(GUIDANCE), "elsewhere");
  // With no unsaved edit, a backend change replaces the shown text.
  page.receive({ globalStyleProfile: { ...configuredGlobal, guidance: "later" } });
  assert.equal(page.value(GUIDANCE), "later");
  page.click("Clear");
  assert.equal(page.patches.at(-1).globalStyleProfile, null);
  page.click("Configure global profile");
  assert.equal(page.value(GUIDANCE), "");
  assert.equal(page.patches.length, 3);
});

test("global profile selects still save immediately", () => {
  const page = fixture({ globalStyleProfile: { ...configuredGlobal, guidance: "Saved" } });
  page.select(0, "casual");
  assert.deepEqual(page.patches, [{ globalStyleProfile: { ...configuredGlobal, formality: "casual", guidance: "Saved" } }]);
});

test("scoped profile scope and guidance are saved on blur, not per keystroke", () => {
  const page = fixture({ globalStyleProfile: configuredGlobal });
  page.click("Add scoped profile");
  // An empty scope is invalid, so the new row is only a draft.
  assert.equal(page.patches.length, 0);
  page.type(SCOPE, 0, "app:code");
  assert.equal(page.patches.length, 0);
  page.blur(SCOPE, 0);
  const row = { scope: "app:code", profile: { formality: "formal", detail: "concise", guidance: "" } };
  assert.deepEqual(page.patches, [{ scopedStyleProfiles: [row] }]);

  // The scoped row's guidance field follows the global one.
  page.type(GUIDANCE, 1, "Use code terms");
  assert.equal(page.patches.length, 1);
  page.key(GUIDANCE, 1, "Enter");
  const guided = { ...row, profile: { ...row.profile, guidance: "Use code terms" } };
  assert.deepEqual(page.patches.at(-1), { scopedStyleProfiles: [guided] });
  page.blur(GUIDANCE, 1);
  assert.equal(page.patches.length, 2);

  // An invalid scope is kept as a draft and never saved.
  page.type(SCOPE, 0, "app:");
  page.blur(SCOPE, 0);
  assert.equal(page.patches.length, 2);
  assert.equal(page.value(SCOPE, 0), "app:");
});

test("scoped profile selects still save immediately", () => {
  const row = { scope: "category:email", profile: { formality: "formal", detail: "concise", guidance: "" } };
  const page = fixture({ scopedStyleProfiles: [row] });
  page.select(0, "casual");
  assert.deepEqual(page.patches, [{ scopedStyleProfiles: [{ ...row, profile: { ...row.profile, formality: "casual" } }] }]);
});
