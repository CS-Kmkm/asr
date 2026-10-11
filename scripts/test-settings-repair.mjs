// Run with node --test scripts/test-settings-repair.mjs.
// Exercise SettingsRepairBanner with isolated hooks and IPC stubs. This covers
// the pull-on-mount status and the explicit save; it does not emulate a browser.
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

const flush = () => new Promise((resolve) => setImmediate(resolve));

function fixture({ status, persist }) {
  const slots = [];
  const effects = [];
  let cursor = 0;
  const hooks = {
    useState(initial) {
      const index = cursor++;
      if (!(index in slots)) slots[index] = initial;
      return [slots[index], (next) => { slots[index] = typeof next === "function" ? next(slots[index]) : next; }];
    },
    // Effects run on mount and whenever a dependency changes identity.
    useEffect(effect, deps) {
      const index = cursor++;
      const previous = slots[index];
      if (!previous || deps.some((dep, at) => dep !== previous[at])) { slots[index] = deps; effects.push(effect); }
    },
  };
  const calls = { status: 0, persist: 0 };
  const { SettingsRepairBanner } = load("../src/components/SettingsRepairBanner.tsx", {
    react: hooks,
    "../api": {
      getSettingsRepairStatus: () => { calls.status += 1; return status(); },
      persistRepairedSettings: () => { calls.persist += 1; return persist(); },
    },
    "../i18n": { useI18n: () => ({ language: "en", t: (key) => key }) },
  });
  let settings = {};
  function render(nextSettings = settings) {
    settings = nextSettings;
    cursor = 0;
    const tree = SettingsRepairBanner({ settings });
    while (effects.length) effects.shift()();
    const nodes = [];
    function visit(node) {
      if (Array.isArray(node)) return node.forEach(visit);
      if (!node || typeof node !== "object" || !node.props) return;
      nodes.push(node);
      visit(node.props.children);
    }
    visit(tree);
    const text = [];
    function collect(node) {
      if (Array.isArray(node)) return node.forEach(collect);
      if (typeof node === "string") return text.push(node);
      if (node && typeof node === "object" && node.props) collect(node.props.children);
    }
    collect(tree);
    return { tree, nodes, text: text.join(" ") };
  }
  return { render, calls };
}

test("banner fetches the repair status on mount and lists the fields", async () => {
  const { render, calls } = fixture({
    status: async () => ["historyRetention", "shortcuts.ask"],
    persist: async () => [],
  });
  assert.equal(render().tree, null);
  await flush();
  const { text } = render();
  assert.equal(calls.status, 1);
  assert.match(text, /fallback values for now/);
  assert.match(text, /historyRetention/);
  assert.match(text, /shortcuts\.ask/);
});

test("a settings change re-fetches the status, so fixed fields disappear", async () => {
  const replies = [["historyRetention", "theme"], ["theme"]];
  const { render, calls } = fixture({ status: async () => replies.shift() ?? [], persist: async () => [] });
  render();
  await flush();
  assert.match(render().text, /historyRetention/);
  // Saving through the form or a settings-changed event yields new settings.
  render({ historyRetention: "one_week" });
  await flush();
  const { text } = render();
  assert.equal(calls.status, 2);
  assert.doesNotMatch(text, /historyRetention/);
  assert.match(text, /theme/);
});

test("the whole unreadable row is named in words", async () => {
  const { render } = fixture({ status: async () => ["unreadable", "autoStart"], persist: async () => [] });
  render();
  await flush();
  assert.match(render().text, /All settings \(the stored settings could not be read\)/);
});

test("saving the current values persists them and hides the banner", async () => {
  const { render, calls } = fixture({ status: async () => ["historyRetention"], persist: async () => [] });
  render();
  await flush();
  const button = render().nodes.find((node) => node.type === "button");
  button.props.onClick();
  await flush();
  assert.equal(calls.persist, 1);
  assert.equal(render().tree, null);
});

test("a failed save keeps the banner and reports the failure", async () => {
  const { render } = fixture({ status: async () => ["historyRetention"], persist: async () => { throw new Error("busy"); } });
  render();
  await flush();
  render().nodes.find((node) => node.type === "button").props.onClick();
  await flush();
  const { text, nodes } = render();
  assert.match(text, /could not be saved/);
  assert.equal(nodes.find((node) => node.type === "button").props.disabled, false);
});

test("no banner when every stored setting is readable", async () => {
  const { render } = fixture({ status: async () => [], persist: async () => [] });
  render();
  await flush();
  assert.equal(render().tree, null);
});
