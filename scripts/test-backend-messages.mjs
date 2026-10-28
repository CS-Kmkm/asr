import { loadSource } from "./test-support/load-source.mjs";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";

function load(path, mocks = {}) {
 return loadSource(new URL(path, import.meta.url), {mocks});
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
    "./windows/AskAnswerPanel": {},
    "./windows/RecordingOverlay": {},
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
