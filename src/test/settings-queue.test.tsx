import assert from "node:assert/strict";
import { act, renderHook, waitFor } from "@testing-library/react";
import { beforeEach, test, vi } from "vitest";
import { useSettings } from "../hooks/useSettings";
import { defaultSettings, getSettings, updateSettings } from "../api";
import type { Settings, SettingsSnapshot, StyleProfile } from "../types";
const subscriptions = vi.hoisted(() => new Set<(event: { payload: SettingsSnapshot }) => void>());
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn(async (_name, handler) => { subscriptions.add(handler);return()=>subscriptions.delete(handler); }) }));
vi.mock("../api", async(original)=>({...await original<typeof import("../api")>(),getSettings:vi.fn(),updateSettings:vi.fn()}));
beforeEach(()=>{vi.clearAllMocks();subscriptions.clear();});
const profile = (patch: Partial<StyleProfile> = {}): StyleProfile => ({ formality: "formal", detail: "concise", guidance: "", ...patch });
function queueFixture(stored: Partial<Settings> = {}) {
 let backend = { ...structuredClone(defaultSettings), setupComplete: true, ...stored }, revision=1, notice: string|undefined;
 const languages: string[]=[];
 type Save = { next:Partial<Settings>; snapshot:SettingsSnapshot|null;commit:()=>SettingsSnapshot;resolve:()=>void;reject:(error:Error)=>void };
 const saves:Save[]=[];
 const store=(patch:Partial<Settings>)=>{backend={...backend,...structuredClone(patch)};return structuredClone({settings:backend,revision:++revision});};
 vi.mocked(getSettings).mockImplementation(async()=>structuredClone({settings:backend,revision}));
 vi.mocked(updateSettings).mockImplementation((patch)=>new Promise((resolve,reject)=>{
  const save:Save={next:structuredClone(patch),snapshot:null,commit(){return save.snapshot??=store(patch);},resolve(){resolve(structuredClone(save.commit()));},reject};saves.push(save);
 }));
 const {result}=renderHook(()=>useSettings({onLanguageChange:(value)=>languages.push(value),onSetupNeeded:()=>{},onNotReady:()=>{},onError:(error)=>{notice=String(error);}}));
 const settle=async()=>{await act(async()=>{for(let i=0;i<8;i++)await Promise.resolve();});};
 const save=(patch:Partial<Settings>)=>{void result.current.queueSettingsSave(patch).then(outcome=>{if(!outcome.saved)notice=String(outcome.error);});};
 return {saves,languages,settle,backend:()=>backend,shown:()=>result.current.settings,notice:()=>notice,
 open:async()=>{await waitFor(()=>assert.equal(result.current.settingsLoaded,true));},
 save:async(patch:Partial<Settings>)=>{act(()=>save(patch));await settle();},
 emit:async(_name:string,payload:SettingsSnapshot)=>{await act(async()=>{for(const fn of subscriptions)fn({payload});});await settle();},
 changeElsewhere:store,
 };
}
// A select saves at once; the guidance text is committed while it is pending.
async function selectThenCommit(app: ReturnType<typeof queueFixture>) {
  await app.save({ globalStyleProfile: profile({ formality: "casual" }) });
  await app.save({ globalStyleProfile: { ...app.shown().globalStyleProfile!, guidance: "Be brief" } });
}

test("saves are sent one at a time, each with the newest values of its unconfirmed keys", async () => {
  const app = queueFixture({ globalStyleProfile: profile() });
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
  const app = queueFixture({ globalStyleProfile: profile() });
  await app.open();
  await selectThenCommit(app);
  assert.equal(app.shown().globalStyleProfile!.guidance, "Be brief");
  const [select] = app.saves;
  await app.emit("settings-changed", select.commit());
  assert.equal(app.shown().globalStyleProfile!.guidance, "Be brief");
  select.resolve();
  await app.settle();
  assert.equal(app.shown().globalStyleProfile!.guidance, "Be brief");

  const commit = app.saves[1];
  await app.emit("settings-changed", commit.commit());
  assert.equal(app.shown().globalStyleProfile!.guidance, "Be brief");
  commit.resolve();
  await app.settle();
  assert.deepEqual(app.shown().globalStyleProfile, profile({ formality: "casual", guidance: "Be brief" }));
});

test("an older save's echo delivered after every save settled is ignored", async () => {
  const app = queueFixture({ globalStyleProfile: profile() });
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
  const app = queueFixture({ globalStyleProfile: profile() });
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
  const app = queueFixture();
  await app.open();
  await app.save({ interactionSounds: true });
  await app.save({ theme: "dark" });
  app.saves[0].reject(new Error("save failed"));
  await app.settle();
  assert.deepEqual(app.saves[1].next, { interactionSounds: true, theme: "dark" });
});

test("when every queued save fails, the settings return to the last confirmed ones", async () => {
  const app = queueFixture({ globalStyleProfile: profile() });
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
  const app = queueFixture({ globalStyleProfile: profile() });
  await app.open();
  await selectThenCommit(app);
  app.saves[0].resolve();
  await app.settle();
  app.saves[1].reject(new Error("save failed"));
  await app.settle();
  assert.equal(app.notice(), "Error: save failed");
  assert.deepEqual(app.shown().globalStyleProfile, profile({ formality: "casual" }));
});

test("a change made elsewhere during a pending save is shown and kept by the queued save", async () => {
  const app = queueFixture({ globalStyleProfile: profile(), translationTargetLanguage: "en" });
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
  const app = queueFixture({ interactionSounds: false });
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
  const app = queueFixture();
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
  assert.equal(app.notice(), undefined);
  assert.equal(app.shown().clipboardRestore, false);
});

test("only the latest save's acknowledgement switches the UI language", async () => {
  const app = queueFixture({ uiLanguage: "ja" });
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
