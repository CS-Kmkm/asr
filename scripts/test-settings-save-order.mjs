import { loadSource } from "./test-support/load-source.mjs";
// Run with node --test scripts/test-settings-save-order.mjs.
// Exercise MainAppContent.saveSettings with saves made while earlier ones are
// pending: sending order, acknowledgements, failures and settings-changed echoes.
// IPC and events are stubs; this does not emulate a browser or the backend.
import assert from "node:assert/strict";
import { test } from "node:test";
import { runInNewContext } from "node:vm";
import ts from "typescript";

function load(path, mocks = {}, exposeMain = false) {
 return loadSource(new URL(path, import.meta.url), {mocks, expose: exposeMain ? ['MainAppContent'] : []});
}
const { defaultSettings } = load("../src/api.ts");

function fixture(stored = {}) {
  const slots = [];
  const effects = [];
  const listeners = new Map();
  let cursor = 0;
  let dirty = false;
  let tree;
  // The backend: stored settings and the revision of their last write.
  let backend = { ...structuredClone(defaultSettings), setupComplete: true, ...stored };
  let revision = 1;
  const store = (patch) => {
    backend = { ...backend, ...structuredClone(patch) };
    revision += 1;
    return structuredClone({ settings: backend, revision });
  };
  // Each update_settings call waits until the test answers it.
  const saves = [];
  const models = [];
  const languages = [];
  const hooks = {
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
        effects.push(() => {
          previous?.cleanup?.();
          slots[index] = { deps, cleanup: effect() };
        });
      }
    },
  };
  const api = {
    defaultSettings,
    getStartupHotkeyWarning: async () => [],
    getAppState: async () => ({ phase: "idle", message: null }),
    getSettings: async () => structuredClone({ settings: backend, revision }),
    getModelStatus: async () => ({ state: "ready" }),
    getGpuDiagnostics: async () => ({}),
    listAudioDevices: async () => [],
    listDictionary: async () => [],
    listHistory: async () => [],
    listDictionaryCandidates: async () => [],
    loadModel: async (modelId, quantization) => { models.push([modelId, quantization]); return { state: "ready" }; },
    updateSettings: (patch) => new Promise((resolve, reject) => {
      const save = {
        next: structuredClone(patch),
        snapshot: null,
        // Stores the patch as the backend does, before the answer arrives;
        // returns the snapshot its settings-changed carries.
        commit() { save.snapshot ??= store(patch); return save.snapshot; },
        resolve() { resolve(structuredClone(save.commit())); },
        reject,
      };
      saves.push(save);
    }),
  };
  const mocks = {
    react: hooks,
    "@tauri-apps/api/event": { listen: async (name, fn) => { listeners.set(name, fn); return () => listeners.delete(name); } },
    "@tauri-apps/api/webviewWindow": { getCurrentWebviewWindow: () => ({ label: "main" }) },
    "./api": api,
    "./i18n": { useI18n: () => ({ language: "en", t: (key) => key }), translate: (_, key) => key, translateAppMessage: (_, value) => value },
    "./components/ui": {},
  };
  for (const name of ["Dashboard", "Setup", "Settings", "Models", "History", "Dictionary", "Privacy", "Diagnostics"]) {
    mocks[`./pages/${name}Page`] = { [`${name}Page`]: `${name}Page` };
  }
  const { MainAppContent } = load("../src/App.tsx", mocks, true);
  const onLanguageChange = (language) => languages.push(language);
  function render() {
    cursor = 0;
    dirty = false;
    tree = MainAppContent({ onLanguageChange });
    while (effects.length) effects.shift()();
  }
  function nodes() {
    const result = [];
    function visit(node) {
      if (Array.isArray(node)) return node.forEach(visit);
      if (!node?.props) return;
      result.push(node);
      visit(node.props.children);
    }
    visit(tree);
    return result;
  }
  async function settle() {
    if (!tree || dirty) render();
    for (let i = 0; i < 8; i++) {
      await new Promise(setImmediate);
      if (dirty) render();
    }
  }
  const props = (name) => nodes().find((node) => node.type === `${name}Page`).props;
  async function navigate(label) {
    await settle();
    nodes().find((node) => node.type === "button" && node.props.children === label).props.onClick();
    await settle();
  }
  return {
    saves, models, languages, settle, props, navigate,
    backend: () => backend,
    open: () => navigate("Settings"),
    // What the Settings page currently shows.
    shown: () => props("Settings").settings,
    async save(patch) { props("Settings").onSave(patch); await settle(); },
    async emit(name, payload) { listeners.get(name)({ payload: structuredClone(payload) }); await settle(); },
    // Stores a change made elsewhere (a voice translation's target cycle);
    // returns the snapshot its settings-changed carries.
    changeElsewhere: (patch) => store(patch),
    notice: () => nodes().find((node) => node.props.className === "notice-message")?.props.children,
  };
}

const profile = (patch = {}) => ({ formality: "formal", detail: "concise", guidance: "", ...patch });

test("a specific AI failure notice survives a later completion notice until the next operation", async () => {
  const app = fixture();
  await app.open();
  await app.emit("app-state", { phase: "processing", stage: "correcting", operation: "recording", operationId: 10, message: "busy", messageCode: null });
  await app.emit("status", { kind: "text_correction_failed", code: "ai_invalid_api_key", outcome: "using_transcript", message: "specific failure cause" });
  await app.emit("status", { kind: "success", message: "generic completion" });
  assert.equal(app.notice(), "specific failure cause");
  for (const kind of ["history_save_failed", "metric_save_failed", "artifact_cleanup_failed"]) {
    await app.emit("status", { kind, message: "later operational warning" });
    assert.equal(app.notice(), "specific failure cause");
  }
  await app.emit("app-state", { phase: "completed", stage: "completed", operation: null, operationId: 10, message: "done", messageCode: null });
  assert.equal(app.notice(), "specific failure cause");
  await app.emit("app-state", { phase: "recording", stage: "recording", operation: "recording", operationId: 11, message: "new operation", messageCode: null });
  await app.emit("status", { kind: "success", message: "new completion" });
  assert.equal(app.notice(), "new completion");
});

test("History Retry cancellation follows operation identity through later processing stages", async () => {
  const app = fixture();
  await app.navigate("History");
  for (const phase of ["processing", "injecting"]) {
    await app.emit("app-state", { phase, stage: "transcribing", operation: "retry", message: "changed wording", messageCode: null });
    assert.equal(app.props("History").retryActive, true);
  }
  await app.emit("app-state", { phase: "completed", stage: "completed", operation: null, message: "done", messageCode: null });
  assert.equal(app.props("History").retryActive, false);
});

test("Setup stays open after a failed Finish save and navigates only after success", async () => {
  const app = fixture({ setupComplete: false });
  await app.settle();
  app.props("Setup").onFinish();
  await app.settle();
  assert.equal(app.saves.length, 1);
  app.saves[0].reject(new Error("settings write failed"));
  await app.settle();
  assert.ok(app.props("Setup"));
  assert.match(app.notice(), /settings write failed/);
  app.props("Setup").onFinish();
  await app.settle();
  app.saves[1].resolve();
  await app.settle();
  assert.ok(app.props("Dashboard"));
});

// A select saves at once; the guidance text is committed while it is pending.
async function selectThenCommit(app) {
  await app.save({ globalStyleProfile: profile({ formality: "casual" }) });
  await app.save({ globalStyleProfile: { ...app.shown().globalStyleProfile, guidance: "Be brief" } });
}

test("saves are sent one at a time, each with the newest values of its unconfirmed keys", async () => {
  const app = fixture({ globalStyleProfile: profile() });
  await app.open();
  await selectThenCommit(app);
  // The commit waits until the select's save settles.
  assert.equal(app.saves.length, 1);
  assert.deepEqual(app.saves[0].next, { globalStyleProfile: profile({ formality: "casual" }) });
  app.saves[0].resolve();
  await app.settle();
  assert.equal(app.saves.length, 2);
  assert.deepEqual(app.saves[1].next, { globalStyleProfile: profile({ formality: "casual", guidance: "Be brief" }) });
  app.saves[1].resolve();
  await app.settle();
  assert.equal(app.saves.length, 2);
  assert.deepEqual(app.backend().globalStyleProfile, profile({ formality: "casual", guidance: "Be brief" }));
});

test("an older save's acknowledgement or echo cannot revert text committed after it", async () => {
  const app = fixture({ globalStyleProfile: profile() });
  await app.open();
  await selectThenCommit(app);
  assert.equal(app.shown().globalStyleProfile.guidance, "Be brief");
  const [select] = app.saves;
  await app.emit("settings-changed", select.commit());
  assert.equal(app.shown().globalStyleProfile.guidance, "Be brief");
  select.resolve();
  await app.settle();
  assert.equal(app.shown().globalStyleProfile.guidance, "Be brief");

  const commit = app.saves[1];
  await app.emit("settings-changed", commit.commit());
  assert.equal(app.shown().globalStyleProfile.guidance, "Be brief");
  commit.resolve();
  await app.settle();
  assert.deepEqual(app.shown().globalStyleProfile, profile({ formality: "casual", guidance: "Be brief" }));
});

test("an older save's echo delivered after every save settled is ignored", async () => {
  const app = fixture({ globalStyleProfile: profile() });
  await app.open();
  await selectThenCommit(app);
  const [select] = app.saves;
  const olderEcho = select.commit();
  select.resolve();
  await app.settle();
  const commit = app.saves[1];
  await app.emit("settings-changed", commit.commit());
  commit.resolve();
  await app.settle();
  // The select's settings-changed arrives last, with a lower revision.
  await app.emit("settings-changed", olderEcho);
  assert.deepEqual(app.shown().globalStyleProfile, profile({ formality: "casual", guidance: "Be brief" }));
});

test("an older save's failure reports its error and leaves the queued save to decide", async () => {
  const app = fixture({ globalStyleProfile: profile() });
  await app.open();
  await selectThenCommit(app);
  app.saves[0].reject(new Error("save failed"));
  await app.settle();
  assert.equal(app.notice(), "Error: save failed");
  assert.deepEqual(app.shown().globalStyleProfile, profile({ formality: "casual", guidance: "Be brief" }));
  // The queued save still sends the failed change along with its own.
  assert.deepEqual(app.saves[1].next.globalStyleProfile, profile({ formality: "casual", guidance: "Be brief" }));
  app.saves[1].resolve();
  await app.settle();
  assert.deepEqual(app.shown().globalStyleProfile, profile({ formality: "casual", guidance: "Be brief" }));
});

test("a failed save's keys are sent again by the next queued save", async () => {
  const app = fixture();
  await app.open();
  await app.save({ interactionSounds: true });
  await app.save({ theme: "dark" });
  app.saves[0].reject(new Error("save failed"));
  await app.settle();
  assert.deepEqual(app.saves[1].next, { interactionSounds: true, theme: "dark" });
});

test("when every queued save fails, the settings return to the last confirmed ones", async () => {
  const app = fixture({ globalStyleProfile: profile() });
  await app.open();
  await selectThenCommit(app);
  app.saves[0].reject(new Error("first save failed"));
  await app.settle();
  app.saves[1].reject(new Error("second save failed"));
  await app.settle();
  assert.equal(app.notice(), "Error: second save failed");
  assert.deepEqual(app.shown().globalStyleProfile, profile());
});

test("a failed latest save returns to what the previous save confirmed", async () => {
  const app = fixture({ globalStyleProfile: profile() });
  await app.open();
  await selectThenCommit(app);
  app.saves[0].resolve();
  await app.settle();
  app.saves[1].reject(new Error("save failed"));
  await app.settle();
  assert.equal(app.notice(), "Error: save failed");
  assert.deepEqual(app.shown().globalStyleProfile, profile({ formality: "casual" }));
});

test("a failed save keeps unsaved Setup choices and reverts only what it changed", async () => {
  const app = fixture({ setupComplete: false, microphoneId: null });
  await app.settle();
  const setup = app.props("Setup");
  setup.onSettingsChange({ ...setup.settings, microphoneId: "usb-mic" });
  await app.settle();
  app.props("Setup").onFinish();
  await app.settle();
  assert.deepEqual(app.saves[0].next, { microphoneId: "usb-mic", setupComplete: true });
  app.saves[0].reject(new Error("save failed"));
  await app.settle();
  assert.equal(app.props("Setup").settings.microphoneId, "usb-mic");
  assert.equal(app.props("Setup").settings.setupComplete, false);
  // The kept choice goes with the next attempt.
  app.props("Setup").onFinish();
  await app.settle();
  assert.deepEqual(app.saves[1].next, { microphoneId: "usb-mic", setupComplete: true });
  app.saves[1].resolve();
  await app.settle();
  assert.equal(app.backend().microphoneId, "usb-mic");
  assert.ok(app.props("Dashboard"));
});

test("a change made elsewhere during a pending save is shown and kept by the queued save", async () => {
  const app = fixture({ globalStyleProfile: profile(), translationTargetLanguage: "en" });
  await app.open();
  const sounds = !app.shown().interactionSounds;
  await app.save({ interactionSounds: sounds });
  await app.save({ globalStyleProfile: profile({ formality: "casual" }) });
  // A voice translation cycles the target while both saves are pending.
  await app.emit("settings-changed", app.changeElsewhere({ translationTargetLanguage: "ja" }));
  assert.equal(app.shown().translationTargetLanguage, "ja");
  assert.equal(app.shown().interactionSounds, sounds);
  assert.deepEqual(app.shown().globalStyleProfile, profile({ formality: "casual" }));
  app.saves[0].resolve();
  await app.settle();
  assert.deepEqual(app.saves[1].next, { globalStyleProfile: profile({ formality: "casual" }) });
  app.saves[1].resolve();
  await app.settle();
  assert.equal(app.backend().translationTargetLanguage, "ja");
  assert.equal(app.backend().interactionSounds, sounds);
  assert.equal(app.shown().translationTargetLanguage, "ja");
  assert.deepEqual(app.shown().globalStyleProfile, profile({ formality: "casual" }));
});

test("a key changed again while its save is in flight stays unconfirmed and is sent again", async () => {
  const app = fixture({ interactionSounds: false });
  await app.open();
  await app.save({ interactionSounds: true });
  await app.save({ interactionSounds: false });
  app.saves[0].resolve();
  await app.settle();
  // The first acknowledgement stores true, but false is still what was chosen.
  assert.equal(app.shown().interactionSounds, false);
  assert.deepEqual(app.saves[1].next, { interactionSounds: false });
  app.saves[1].resolve();
  await app.settle();
  assert.equal(app.backend().interactionSounds, false);
  assert.equal(app.shown().interactionSounds, false);
});

test("a save whose keys an earlier save already sent makes no call", async () => {
  const app = fixture();
  await app.open();
  await app.save({ interactionSounds: true });
  await app.save({ theme: "dark" });
  await app.save({ clipboardRestore: false });
  app.saves[0].resolve();
  await app.settle();
  // The second save sends both queued changes; the third has nothing left.
  assert.deepEqual(app.saves[1].next, { theme: "dark", clipboardRestore: false });
  app.saves[1].resolve();
  await app.settle();
  assert.equal(app.saves.length, 2);
  assert.equal(app.notice(), "Settings saved locally.");
  assert.equal(app.shown().clipboardRestore, false);
});

test("only the latest save's acknowledgement switches the UI language", async () => {
  const app = fixture({ uiLanguage: "ja" });
  await app.open();
  assert.deepEqual(app.languages, ["ja"]);
  await app.save({ uiLanguage: "en" });
  await app.save({ theme: "dark" });
  app.saves[0].resolve();
  await app.settle();
  assert.deepEqual(app.languages, ["ja"]);
  app.saves[1].resolve();
  await app.settle();
  assert.deepEqual(app.languages, ["ja", "en"]);
});

test("a model configuration is saved after the saves queued before it, then loaded", async () => {
  const app = fixture();
  await app.open();
  await app.save({ interactionSounds: true });
  await app.navigate("Models");
  const configuration = {
    asrBackend: "vibevoice",
    modelId: "org/model",
    modelQuantization: "8bit",
    apiBaseUrl: defaultSettings.apiBaseUrl,
    apiKeyEnvVar: defaultSettings.apiKeyEnvVar,
  };
  app.props("Models").onConfigureModel(configuration);
  await app.settle();
  assert.equal(app.saves.length, 1);
  app.saves[0].resolve();
  await app.settle();
  assert.deepEqual(app.saves[1].next, configuration);
  assert.deepEqual(app.models, []);
  app.saves[1].resolve();
  await app.settle();
  assert.deepEqual(app.models, [["org/model", "8bit"]]);
  assert.equal(app.notice(), "Model loaded and ready.");
  assert.equal(app.backend().interactionSounds, true);
});

test("a custom model is saved after the saves queued before it", async () => {
  const app = fixture();
  await app.open();
  await app.save({ interactionSounds: true });
  await app.navigate("Models");
  const custom = { asrBackend: "faster-whisper", modelId: "org/custom" };
  let saved;
  void app.props("Models").onSaveCustomModel(custom).then((result) => { saved = result; });
  await app.settle();
  assert.equal(app.saves.length, 1);
  app.saves[0].resolve();
  await app.settle();
  assert.deepEqual(app.saves[1].next, { asrBackend: "faster-whisper", modelId: "org/custom", customModels: [custom] });
  app.saves[1].resolve();
  await app.settle();
  assert.equal(saved, true);
  assert.equal(app.notice(), "Custom model saved locally.");
  assert.deepEqual(app.backend().customModels, [custom]);
  assert.equal(app.backend().interactionSounds, true);
});
