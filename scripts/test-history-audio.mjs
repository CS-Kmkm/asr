// Run with node --test scripts/test-history-audio.mjs.
// Exercise History audio loading (raw IPC body to Play/Download) with IPC and
// DOM stubs; this does not emulate a browser or the native backend.
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { createRequire } from "node:module";
import { test } from "node:test";
import { runInNewContext } from "node:vm";
import ts from "typescript";

const require = createRequire(import.meta.url);
function load(relativePath, mocks = {}, globals = {}) {
  const source = readFileSync(new URL(relativePath, import.meta.url), "utf8");
  const output = ts.transpileModule(source, {
    compilerOptions: { module: ts.ModuleKind.CommonJS, jsx: ts.JsxEmit.ReactJSX, target: ts.ScriptTarget.ES2021 },
  }).outputText;
  const module = { exports: {} };
  runInNewContext(output, {
    window: {}, ...globals, module, exports: module.exports,
    require: (name) => name in mocks ? mocks[name] : require(name),
  });
  return module.exports;
}

function audioApi(response) {
  const calls = [];
  const api = load("../src/api.ts", {
    "@tauri-apps/api/core": { invoke: async (command, args) => {
      calls.push([command, args]);
      if (response instanceof Error) throw response;
      return response;
    } },
  }, { window: { __TAURI_INTERNALS__: {} } });
  return { api, calls };
}

test("History audio arrives as raw bytes named from the History id", async () => {
  const wav = new Uint8Array([82, 73, 70, 70, 1, 2, 3]).buffer;
  const { api, calls } = audioApi(wav);
  const payload = await api.getHistoryAudio(42);
  // The arguments object comes from the module's realm.
  assert.equal(JSON.stringify(calls), JSON.stringify([["get_history_audio", { id: 42 }]]));
  assert.equal(payload.bytes, wav, "the ArrayBuffer is used as is, without copying");
  assert.equal(payload.filename, "local-voice-history-42.wav");
  assert.equal(payload.mimeType, "audio/wav");
});

test("a number-array body from the postMessage fallback becomes the same bytes", async () => {
  const { api } = audioApi([82, 73, 70, 70, 255]);
  const payload = await api.getHistoryAudio(7);
  assert.deepEqual(Array.from(new Uint8Array(payload.bytes)), [82, 73, 70, 70, 255]);
  assert.equal(payload.filename, "local-voice-history-7.wav");
});

test("a missing or oversized recording rejects", async () => {
  const { api } = audioApi(new Error("history audio is unavailable"));
  await assert.rejects(api.getHistoryAudio(3), /history audio is unavailable/);
});

const item = {
  id: 9, transcriptText: "hello", processedText: null, sourceText: null, instructionText: null,
  actionKind: null, searchSite: null, mode: "faithful", asrProvider: "mock", llmProvider: null,
  targetLanguage: null, appCategory: null, durationMs: 1000, latencyMs: null,
  createdAt: "2030-01-01T00:00:00+00:00", hasAudio: true, retryOfId: null,
  insertionResult: null, insertionDetail: null, expiresAt: null,
};

function nodes(tree) {
  const found = [];
  (function visit(node) {
    if (Array.isArray(node)) return node.forEach(visit);
    if (!node || typeof node !== "object" || !node.props) return;
    found.push(node);
    visit(node.props.children);
  })(tree);
  return found;
}

// Renders the History page once and returns its Play and Download handlers
// together with what they did to the stubbed DOM.
function historyPage(onLoadAudio) {
  const effects = { blobs: [], urls: [], revoked: [], anchors: [], timers: [], states: [], errors: 0 };
  class Blob {
    constructor(parts, options) { this.parts = parts; this.type = options?.type; effects.blobs.push(this); }
  }
  const globals = {
    Blob,
    URL: {
      createObjectURL: (blob) => { const url = `blob:${effects.urls.length}`; effects.urls.push([url, blob]); return url; },
      revokeObjectURL: (url) => effects.revoked.push(url),
    },
    document: { createElement: (tag) => { const anchor = { tag, clicks: 0, click() { this.clicks++; } }; effects.anchors.push(anchor); return anchor; } },
    window: { setTimeout: (fn, ms) => effects.timers.push({ fn, ms }) },
  };
  const { HistoryPage } = load("../src/pages/HistoryPage.tsx", {
    react: { useState: (initial) => [initial, (value) => effects.states.push(value)], useEffect() {} },
    "../components/ui": { Empty: "empty", HelpHint: "help", IconButton: "icon-button" },
    "../components/HistoryRetentionSelect": { HistoryRetentionSelect: "retention-select" },
    "../types": load("../src/types.ts"),
    "../i18n": { useI18n: () => ({ language: "en", t: (key) => key }), insertionDetailLabels: {}, insertionOutcomeLabels: {} },
  }, globals);
  const noop = () => {};
  const tree = HistoryPage({
    settings: { ...load("../src/api.ts").defaultSettings, historyRetention: "one_month", keepFailedTakes: true }, settingsLoaded: true, history: [item],
    filter: "all", onSave: noop, onFilter: noop, onCopyItem: noop, onRetry: noop, onDelete: noop,
    onDeleteAll: noop, onLoadAudio, onAudioError: () => effects.errors++, retryActive: false, onCancelRetry: noop,
  });
  const button = (label) => nodes(tree).find((node) => node.type === "icon-button" && node.props.label === label);
  return { effects, play: () => button("Play").props.onClick(), download: () => button("Download").props.onClick() };
}
const settle = async () => { for (let i = 0; i < 4; i++) await new Promise(setImmediate); };
const payload = { bytes: new Uint8Array([1, 2, 3]).buffer, filename: "local-voice-history-9.wav", mimeType: "audio/wav" };

test("Play wraps the raw bytes in a typed Blob and plays it", async () => {
  const loaded = [];
  const page = historyPage(async (id) => { loaded.push(id); return payload; });
  page.play();
  await settle();
  assert.deepEqual(loaded, [9]);
  assert.equal(page.effects.blobs.length, 1);
  assert.equal(page.effects.blobs[0].parts[0], payload.bytes);
  assert.equal(page.effects.blobs[0].type, "audio/wav");
  assert.deepEqual(page.effects.states, ["blob:0", 9]);
  assert.equal(page.effects.anchors.length, 0);
  assert.equal(page.effects.errors, 0);
});

test("Download saves the raw bytes under the recording's name", async () => {
  const page = historyPage(async () => payload);
  page.download();
  await settle();
  assert.equal(page.effects.blobs[0].parts[0], payload.bytes);
  assert.equal(page.effects.anchors.length, 1);
  assert.equal(page.effects.anchors[0].href, "blob:0");
  assert.equal(page.effects.anchors[0].download, "local-voice-history-9.wav");
  assert.equal(page.effects.anchors[0].clicks, 1);
  assert.equal(page.effects.timers.length, 1);
  page.effects.timers[0].fn();
  assert.deepEqual(page.effects.revoked, ["blob:0"]);
  assert.deepEqual(page.effects.states, []);
});

test("a recording that cannot be loaded reports the failure and plays nothing", async () => {
  for (const action of ["play", "download"]) {
    const page = historyPage(async () => { throw new Error("history audio is unavailable"); });
    page[action]();
    await settle();
    assert.equal(page.effects.errors, 1, action);
    assert.equal(page.effects.blobs.length, 0, action);
    assert.equal(page.effects.anchors.length, 0, action);
    assert.deepEqual(page.effects.states, [], action);
  }
});
