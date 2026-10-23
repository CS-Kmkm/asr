import { loadSource } from "./test-support/load-source.mjs";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";

function load(path, mocks = {}, globals = {}, expose = []) {
 return loadSource(new URL(path, import.meta.url), {mocks, globals, expose});
}
const i18n = load("../src/i18n.tsx");
const { defaultSettings } = load("../src/api.ts");
const catalogue = JSON.parse(readFileSync(new URL("../src/backend-messages.json", import.meta.url), "utf8"));

test("browser entry loads without asking Tauri for a webview window", () => {
  const mocks = {
    "@tauri-apps/api/webviewWindow": { getCurrentWebviewWindow() { throw new Error("Tauri is unavailable in this browser"); } },
    "./api": { defaultSettings },
    "./i18n": i18n,
    "./components/ui": {},
  };
  for (const page of ["Dashboard", "Setup", "Settings", "Models", "History", "Dictionary", "Privacy", "Diagnostics"]) mocks[`./pages/${page}Page`] = {};
  assert.doesNotThrow(() => load("../src/App.tsx", mocks));
});

test("backend codes select Japanese messages independently of English fallback text", () => {
  const codes = new Set();
  for (const [en, entry] of Object.entries(catalogue)) {
    const code = entry.code;
    assert.ok(en && entry.ja && code, code);
    assert.ok(!codes.has(code), code);
    codes.add(code);
    const fallback = "changed fallback wording";
    assert.equal(i18n.translateAppMessage("ja", fallback, code), en.includes("{") ? fallback : entry.ja, code);
    assert.equal(i18n.translateAppMessage("en", fallback, code), en.includes("{") ? fallback : en, code);
  }
  assert.match(i18n.translateAppMessage("ja", "private provider body", "ai_invalid_api_key", "using_transcript"), /APIキー.*元の文字起こし/);
  assert.equal(i18n.translateAppMessage("ja", "Autostart update failed: unavailable", "autostart_update_failed"), "自動起動の更新に失敗しました：unavailable");
});

function nodes(tree) {
  const found = [];
  const visit = (node) => {
    if (Array.isArray(node)) return node.forEach(visit);
    if (!node?.props) return;
    found.push(node);
    visit(node.props.children);
  };
  visit(tree);
  return found;
}
const text = (node) => Array.isArray(node) ? node.map(text).join("") : node?.props ? text(node.props.children) : typeof node === "string" ? node : "";

function fixture(component, label) {
  const slots = [];
  const handlers = new Map();
  const copies = [];
  let cursor = 0;
  let effects = [];
  let tree;
  const hooks = {
    useState(initial) { const index = cursor++; if (!(index in slots)) slots[index] = typeof initial === "function" ? initial() : initial;
      return [slots[index], (value) => { slots[index] = typeof value === "function" ? value(slots[index]) : value; }]; },
    useRef(initial) { const index = cursor++; if (!(index in slots)) slots[index] = { current: initial }; return slots[index]; },
    useEffect(effect) { const index = cursor++; if (!(index in slots)) { slots[index] = true; effects.push(effect); } },
    useMemo(make) { cursor++; return make(); },
  };
  const mocks = {
    react: hooks,
    "@tauri-apps/api/event": { listen: async (name, callback) => { handlers.set(name, callback); return () => {}; } },
    "@tauri-apps/api/webviewWindow": { getCurrentWebviewWindow: () => ({ label }) },
    "./api": { defaultSettings, getAskAnswer: async () => null, copyToClipboard: async (value) => copies.push(value) },
    "./i18n": { ...i18n, useI18n: () => ({ language: "ja", t: (key) => i18n.translate("ja", key) }) },
    "./components/ui": {},
  };
  for (const page of ["Dashboard", "Setup", "Settings", "Models", "History", "Dictionary", "Privacy", "Diagnostics"]) mocks[`./pages/${page}Page`] = { [`${page}Page`]: page };
  const exports = load("../src/App.tsx", mocks, {
    document: { body: { classList: { add() {} } } },
    window: { setTimeout: () => 1, clearTimeout() {} },
  }, [component]);
  const render = () => { cursor = 0; tree = exports[component](); for (const effect of effects.splice(0)) effect(); };
  return { copies, render, nodes: () => nodes(tree), async emit(name, payload) { handlers.get(name)({ payload }); await Promise.resolve(); render(); } };
}

test("generated Ask answers matching app messages and prefixes stay verbatim when displayed and copied", async () => {
  const panel = fixture("AskAnswerPanel", "ask-answer");
  panel.render();
  await Promise.resolve();
  let operationId = 0;
  for (const payload of ["Dictation cancelled.", "Microphone test failed. Keep this generated answer unchanged.", "Ask needs clarification."]) {
    await panel.emit("ask-answer", { operationId: ++operationId, payload, kind: "generated" });
    const answer = panel.nodes().find((node) => node.props.className === "ask-answer-text");
    assert.equal(text(answer), payload);
    const copy = panel.nodes().find((node) => node.type === "button" && text(node) === i18n.translate("ja", "Copy"));
    await copy.props.onClick();
    assert.equal(panel.copies.at(-1), payload);
  }
  await panel.emit("ask-answer", { operationId: ++operationId, payload: "Dictation cancelled.", kind: "notice" });
  assert.equal(text(panel.nodes().find((node) => node.props.className === "ask-answer-text")), i18n.translateAppMessage("ja", "Dictation cancelled."));
});

test("overlay labels follow typed stages even when English fallback wording changes", async () => {
  const overlay = fixture("RecordingOverlay", "recording-overlay");
  overlay.render();
  for (const [stage, key] of [["preparing_model", "Preparing speech model"], ["finishing_audio", "Finishing audio"], ["transcribing", "Transcribing"]]) {
    await overlay.emit("app-state", { phase: "processing", stage, operation: "recording", message: "arbitrary changed wording", messageCode: null });
    const label = overlay.nodes().find((node) => node.props.className === "recording-overlay-label");
    assert.equal(text(label), i18n.translate("ja", key));
  }
});

test("voice translation retains its transcript draft while selected-text translation clears it", async () => {
  const overlay = fixture("RecordingOverlay", "recording-overlay");
  overlay.render();
  await overlay.emit("voice-mode", { mode: "translate", targetLanguage: "en" });
  await overlay.emit("correction-preview", { stage: "draft", text: "spoken transcript" });
  await overlay.emit("app-state", { phase: "processing", stage: "translating", operation: "recording", message: "changed wording", messageCode: null });
  assert.ok(overlay.nodes().some((node) => node.props.className?.includes("correction-preview") && text(node) === "spoken transcript"));
  await overlay.emit("app-state", { phase: "processing", stage: "translating", operation: "selected_text", message: "changed wording", messageCode: null });
  assert.ok(!overlay.nodes().some((node) => node.props.className?.includes("correction-preview") && text(node) === "spoken transcript"));
});
