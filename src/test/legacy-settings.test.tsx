import assert from "node:assert/strict";
import { useState } from "react";
import { act, fireEvent, render, screen, within } from "@testing-library/react";
import { beforeEach, test, vi } from "vitest";
import { SettingsPage } from "../pages/SettingsPage";
import { I18nProvider } from "../i18n";
import { defaultSettings, startMicrophoneTest, stopMicrophoneTest } from "../api";
import type { Settings } from "../types";
const events = vi.hoisted(() => new Map<string, Set<(event: { payload: unknown }) => void>>());
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn(async (name: string, handler: (event: { payload: unknown }) => void) => {
 const list = events.get(name) ?? new Set(); events.set(name, list); list.add(handler); return () => { list.delete(handler); };
}) }));
vi.mock("../api", async (original) => ({ ...await original<typeof import("../api")>(), getShortcutWarning: vi.fn(async () => []), startMicrophoneTest: vi.fn(async () => {}), stopMicrophoneTest: vi.fn(async () => {}) }));
beforeEach(() => { events.clear(); vi.clearAllMocks(); });
const settle = async () => { await act(async () => { await Promise.resolve(); }); };
const GUIDANCE = "Optional guidance", SCOPE = "app:code or category:development";
const configuredGlobal = { formality: "formal" as const, detail: "concise" as const, guidance: "" };
function pageFixture(initial: Partial<Settings> = {}, optimistic = true) {
 let settings = { ...structuredClone(defaultSettings), ...structuredClone(initial) };
 const patches: Partial<Settings>[] = [];
 let receive!: (patch: Partial<Settings>) => void;
 function Host() {
  const [shown, setShown] = useState(settings); settings = shown;
  receive = (patch) => setShown((old) => ({ ...old, ...patch }));
  return <I18nProvider language="en"><SettingsPage settings={shown} onSave={(patch) => { patches.push(structuredClone(patch)); if (optimistic) receive(patch); }} devices={[]} recording={false} /></I18nProvider>;
 }
 const view = render(<Host />);
 const queries = within(view.container);
 const input = (placeholder: string, index = 0) => queries.getAllByPlaceholderText(placeholder)[index] as HTMLInputElement;
 const byLabel = (label: string) => queries.getByRole("textbox", { name: label }) as HTMLInputElement;
 return {
 patches, get settings() { return settings; },
 receive(patch: Partial<Settings>) { act(() => receive(patch)); },
 value(placeholder: string, index = 0) { return input(placeholder,index).value; },
 type(placeholder: string,index: number,text: string) { for(let end=1;end<=text.length;end++)fireEvent.change(input(placeholder,index), {target:{value:text.slice(0,end)}}); },
 blur(placeholder: string,index = 0) { const field=input(placeholder,index);act(()=>{field.focus();field.blur();}); },
 key(placeholder: string,index: number,key: string) { const field=input(placeholder,index);act(()=>{field.focus();fireEvent.keyDown(field,{key});}); },
 select(index: number,value: string) { const selects=Array.from(view.container.querySelectorAll('select')).filter((select)=>select.querySelector('option[value="formal"]'));fireEvent.change(selects[index],{target:{value}}); },
 click(text: string) { fireEvent.click(queries.getByRole("button",{name:text})); },
 change(label: string,value: string) { fireEvent.change(byLabel(label),{target:{value}}); },
 shortcutBlur(label: string) { const field=byLabel(label);act(()=>{field.focus();field.blur();}); },
 shortcutKey(label: string,key: string) { const field=byLabel(label);act(()=>{field.focus();fireEvent.keyDown(field,{key});}); },
 shortcutValue(label: string) { return byLabel(label).value; },
 save() { fireEvent.click(queries.getByRole("button",{name:"Save shortcuts"})); },
 escWarnings() { return queries.queryAllByRole("note").length; },
 };
}
function profileFixture(initial: Partial<Settings> = {}) { return pageFixture(initial); }
function shortcutFixture(settings: Settings = structuredClone(defaultSettings)) { const page=pageFixture(settings,false);return {...page,settings,blur:page.shortcutBlur,key:page.shortcutKey,value:page.shortcutValue}; }
function microphoneFixture() {
 pageFixture();
 return {
 get calls() { return {start:vi.mocked(startMicrophoneTest).mock.calls.length,stop:vi.mocked(stopMicrophoneTest).mock.calls.length}; },
 meter: () => ({"aria-valuenow":Number(screen.getByRole("meter").getAttribute("aria-valuenow"))}),
 label: () => screen.queryByRole("button",{name:"Stop test"}) ? "Stop test" : "Start test",
 click: async () => { fireEvent.click(screen.getByRole("button",{name:screen.queryByRole("button",{name:"Stop test"})?"Stop test":"Start test"})); await settle(); },
 emit(name: string,payload?: unknown) { act(()=>{for(const handler of events.get(name)??[])handler({payload});}); },
 };
}
test("global guidance is saved once on blur instead of per keystroke", () => {
  const page = profileFixture({ globalStyleProfile: configuredGlobal });
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
  const page = profileFixture({ globalStyleProfile: { ...configuredGlobal, guidance: "Saved" } });
  page.type(GUIDANCE, 0, "Typed");
  page.key(GUIDANCE, 0, "Enter");
  assert.deepEqual(page.patches, [{ globalStyleProfile: { ...configuredGlobal, guidance: "Typed" } }]);
  page.type(GUIDANCE, 0, "Discarded");
  page.key(GUIDANCE, 0, "Escape");
  assert.equal(page.value(GUIDANCE), "Typed");
  assert.equal(page.patches.length, 1);
});

test("an acknowledgement or backend change never overwrites guidance being edited", () => {
  const page = profileFixture({ globalStyleProfile: configuredGlobal });
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
  assert.equal(page.patches.at(-1)!.globalStyleProfile, null);
  page.click("Configure global profile");
  assert.equal(page.value(GUIDANCE), "");
  assert.equal(page.patches.length, 3);
});

test("global profile selects still save immediately", () => {
  const page = profileFixture({ globalStyleProfile: { ...configuredGlobal, guidance: "Saved" } });
  page.select(0, "casual");
  assert.deepEqual(page.patches, [{ globalStyleProfile: { ...configuredGlobal, formality: "casual", guidance: "Saved" } }]);
});

test("built-in presets apply complete editable global and scoped profiles", () => {
  const page = profileFixture({ globalStyleProfile: configuredGlobal,
    scopedStyleProfiles: [{ scope: "category:terminal", profile: configuredGlobal }] });
  fireEvent.change(screen.getByRole("combobox", { name: "Global style preset" }), { target: { value: "chat" } });
  assert.equal(page.patches.at(-1)!.globalStyleProfile!.formality, "casual");
  assert.equal(page.patches.at(-1)!.globalStyleProfile!.detail, "concise");
  assert.match(page.value(GUIDANCE), /conversational/);
  fireEvent.change(screen.getByRole("combobox", { name: "Scoped style preset 1" }), { target: { value: "technical" } });
  const profile = page.patches.at(-1)!.scopedStyleProfiles![0];
  assert.equal(profile.scope, "category:terminal");
  assert.equal(profile.profile.formality, "formal");
  assert.match(profile.profile.guidance!, /commands, identifiers/);
});

test("category picker creates a valid terminal profile without typing a scope", () => {
  const page = profileFixture();
  fireEvent.change(screen.getByRole("combobox", { name: "Add category profile" }), { target: { value: "terminal" } });
  assert.equal(page.patches.length, 1);
  assert.equal(page.patches[0].scopedStyleProfiles![0].scope, "category:terminal");
});

test("scoped profile scope and guidance are saved on blur, not per keystroke", () => {
  const page = profileFixture({ globalStyleProfile: configuredGlobal });
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
  const row = { scope: "category:email", profile: { formality: "formal" as const, detail: "concise" as const, guidance: "" } };
  const page = profileFixture({ scopedStyleProfiles: [row] });
  page.select(0, "casual");
  assert.deepEqual(page.patches, [{ scopedStyleProfiles: [{ ...row, profile: { ...row.profile, formality: "casual" } }] }]);
});

const voice = "Voice Translate shortcuts 1";
const selected = "Selected-text translation hotkey";

test("swapping selected-text and voice chords submits one combined patch without blur writes", () => {
  const page = shortcutFixture();
  page.change(selected, page.settings.shortcuts.translate[0]);
  page.blur(selected);
  assert.equal(page.patches.length, 0);
  page.change(voice, page.settings.translationHotkey);
  page.blur(voice);
  page.save();
  assert.equal(page.patches.length, 1);
  assert.equal(page.patches[0].translationHotkey, page.settings.shortcuts.translate[0]);
  assert.equal(page.patches[0].shortcuts!.translate[0], page.settings.translationHotkey);
});

test("selected-text-only and voice-only edits both use the common Save action", () => {
  for (const label of [selected, voice]) {
    const page = shortcutFixture();
    page.change(label, "  Ctrl+Alt+J  ");
    page.save();
    assert.equal(page.patches.length, 1);
    assert.equal(page.patches[0].translationHotkey, label === selected ? "Ctrl+Alt+J" : page.settings.translationHotkey);
    assert.equal(page.patches[0].shortcuts!.translate[0], label === voice ? "Ctrl+Alt+J" : page.settings.shortcuts.translate[0]);
  }
});

test("Enter submits a valid combined swap only once", () => {
  const page = shortcutFixture();
  page.change(voice, page.settings.translationHotkey);
  page.change(selected, page.settings.shortcuts.translate[0]);
  page.key(selected, "Enter");
  assert.equal(page.patches.length, 1);
  assert.equal(page.patches[0].shortcuts!.translate[0], page.settings.translationHotkey);
});

test("an incomplete swap or empty selected-text chord cannot be persisted", () => {
  const page = shortcutFixture();
  page.change(selected, page.settings.shortcuts.translate[0]);
  page.save();
  page.change(selected, " ");
  page.key(selected, "Enter");
  assert.equal(page.patches.length, 0);
});

test("Escape restores selected-text draft without saving, and unchanged Save is a no-op", () => {
  const page = shortcutFixture();
  page.change(selected, "Ctrl+Alt+J");
  page.key(selected, "Escape");
  assert.equal(page.value(selected), page.settings.translationHotkey);
  page.save();
  assert.equal(page.patches.length, 0);
});

test("A route on Esc warns that the overlay's x button becomes the cancel path", () => {
  const plain = shortcutFixture();
  assert.equal(plain.escWarnings(), 0);
  const settings = structuredClone(defaultSettings);
  settings.shortcuts.ask = ["Escape"];
  settings.translationHotkey = " esc ";
  const esc = shortcutFixture(settings);
  assert.equal(esc.escWarnings(), 2);
  // Esc with a modifier is a different chord and does not shadow cancel.
  esc.change("Ask Anything shortcuts 1", "Ctrl+Escape");
  assert.equal(esc.escWarnings(), 1);
});

test("hiding the window resets a running microphone test to its idle controls", async () => {
  const page = microphoneFixture();
  page.label();
  await settle();
  await page.click();
  assert.equal(page.label(), "Stop test");
  page.emit("audio-level", { rms: 0.5, peak: 0.8 });
  assert.notEqual(page.meter()["aria-valuenow"], 0);

  page.emit("microphone-test-stopped");
  assert.equal(page.label(), "Start test");
  assert.equal(page.meter()["aria-valuenow"], 0);
  // The backend already released the device; the page must not stop it again.
  assert.equal(page.calls.stop, 0);

  await page.click();
  assert.equal(page.calls.start, 2);
  assert.equal(page.label(), "Stop test");
});

test("a stopped event without a running test leaves the controls idle", async () => {
  const page = microphoneFixture();
  page.label();
  await settle();
  page.emit("microphone-test-stopped");
  assert.equal(page.label(), "Start test");
  assert.equal(page.calls.stop, 0);
});
