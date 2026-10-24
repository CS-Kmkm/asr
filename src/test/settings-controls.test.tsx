import { act, fireEvent, render, renderHook, screen } from "@testing-library/react";
import { afterEach, beforeEach, expect, test, vi } from "vitest";
import { CommitField, GainSlider, validateEnvironmentName, validateModel } from "../components/CommitField";
import { normalizeShortcutText, ShortcutField, shortcutFromEvent } from "../components/ShortcutField";
import { AiCorrectionSettings } from "../components/AiCorrectionSettings";
import { SettingsPage } from "../pages/SettingsPage";
import { I18nProvider } from "../i18n";
import { defaultSettings, setShortcutCapture } from "../api";
import { useNoticeQueue } from "../hooks/useNoticeQueue";

vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn(async () => () => {}) }));
vi.mock("../api", async (original) => ({ ...await original<typeof import("../api")>(), setShortcutCapture: vi.fn(async () => {}) }));
beforeEach(() => { vi.clearAllMocks(); vi.mocked(setShortcutCapture).mockResolvedValue(undefined); });
afterEach(() => vi.useRealTimers());

test("hold to talk is off by default and each mode saves its own choice", () => {
  const save = vi.fn();
  render(<I18nProvider language="en"><SettingsPage settings={defaultSettings} onSave={save} devices={[]} recording={false} /></I18nProvider>);
  const dictate = screen.getByRole("button", { name: "Dictation shortcuts: Hold to talk" });
  expect(dictate.getAttribute("aria-pressed")).toBe("false");
  fireEvent.click(dictate);
  expect(save).toHaveBeenCalledWith({ holdToTalk: { dictate: true, translate: false, ask: false, edit: false } });
});

test("free text stays a draft, rejects invalid commits, and saves Enter exactly once", () => {
  const save = vi.fn();
  render(<I18nProvider language="ja"><CommitField label="Model" value="old-model" onCommit={save} validate={validateModel} /></I18nProvider>);
  const input = screen.getByRole("textbox", { name: "Model" });
  fireEvent.focus(input);
  fireEvent.change(input, { target: { value: "" } });
  expect(save).not.toHaveBeenCalled();
  fireEvent.blur(input);
  expect(screen.getByRole("alert").textContent).toContain("モデル ID");
  expect((input as HTMLInputElement).value).toBe("");
  fireEvent.focus(input);
  fireEvent.change(input, { target: { value: "new-model" } });
  fireEvent.keyDown(input, { key: "Enter" });
  expect(save.mock.calls).toEqual([["new-model"]]);
});

test("IME Enter never commits and composition finishing after blur commits its final value", () => {
  const save = vi.fn();
  render(<CommitField label="Guidance" value="" onCommit={save} />);
  const input = screen.getByRole("textbox", { name: "Guidance" });
  fireEvent.compositionStart(input);
  fireEvent.change(input, { target: { value: "にほん" } });
  fireEvent.keyDown(input, { key: "Enter", isComposing: true, keyCode: 229 });
  fireEvent.blur(input);
  expect(save).not.toHaveBeenCalled();
  fireEvent.change(input, { target: { value: "日本" } });
  fireEvent.compositionEnd(input);
  expect(save.mock.calls).toEqual([["日本"]]);
});

test("an unrelated saved snapshot cannot discard an incomplete environment-name draft", () => {
  const view = render(<CommitField label="Environment" value="OPENAI_API_KEY" onCommit={vi.fn()} validate={validateEnvironmentName} />);
  const input = screen.getByRole("textbox", { name: "Environment" });
  fireEvent.change(input, { target: { value: "INVALID-" } });
  view.rerender(<CommitField label="Environment" value="GEMINI_API_KEY" onCommit={vi.fn()} validate={validateEnvironmentName} />);
  expect((input as HTMLInputElement).value).toBe("INVALID-");
  fireEvent.blur(input);
  expect(screen.getByRole("alert")).toBeTruthy();
});

test("AI model, key variable and guidance fields never save per keystroke", () => {
  const save = vi.fn();
  render(<I18nProvider language="en"><AiCorrectionSettings settings={defaultSettings} onSave={save} /></I18nProvider>);
  for (const [name, value, key] of [["OpenAI model", "new-model", "openaiCorrectionModel"], ["OpenAI API key environment variable", "NEW_KEY", "openaiApiKeyEnvVar"], ["Additional style and tone guidance", "New guidance", "correctionInstruction"]]) {
    const input = screen.getByRole("textbox", { name });
    save.mockClear();
    fireEvent.change(input, { target: { value } });
    expect(save).not.toHaveBeenCalled();
    fireEvent.blur(input);
    expect(save.mock.calls).toEqual([[{ [key]: value }]]);
  }
});

test("gain commits only once after dragging and once after a keyboard adjustment", () => {
  const save = vi.fn();
  render(<GainSlider label="Gain" value={100} onCommit={save} />);
  const slider = screen.getByRole("slider", { name: "Gain" });
  fireEvent.change(slider, { target: { value: "150" } });
  fireEvent.change(slider, { target: { value: "200" } });
  expect(save).not.toHaveBeenCalled();
  fireEvent.pointerUp(slider); fireEvent.blur(slider);
  expect(save.mock.calls).toEqual([[200]]);
  fireEvent.change(slider, { target: { value: "205" } }); fireEvent.keyUp(slider, { key: "ArrowRight" });
  expect(save.mock.calls).toEqual([[200], [205]]);
});

test("recorder normalizes physical keys and legacy aliases", async () => {
  expect(shortcutFromEvent({ code: "KeyA", ctrlKey: true, altKey: false, shiftKey: true, metaKey: false })).toBe("Ctrl+Shift+A");
  expect(normalizeShortcutText("shift+control+KeyA")).toBe("Ctrl+Shift+A");
  expect(normalizeShortcutText("Ctrl+Ctrl+A")).toBeNull();
  expect(normalizeShortcutText("unfinished")).toBeNull();
  const change = vi.fn();
  render(<I18nProvider language="en"><ShortcutField label="Test shortcut" value="Ctrl+T" onChange={change} /></I18nProvider>);
  await act(async () => { fireEvent.click(screen.getByRole("button", { name: "Record shortcut" })); });
  fireEvent.keyDown(screen.getByRole("textbox", { name: "Test shortcut" }), { key: "a", code: "KeyA", ctrlKey: true, shiftKey: true });
  expect(change.mock.calls).toEqual([["Ctrl+Shift+A"]]);
});

test("missing microphone remains selectable and settings controls have accessible names", () => {
  const refresh = vi.fn();
  render(<I18nProvider language="en"><SettingsPage settings={{ ...defaultSettings, microphoneId: "disconnected-usb" }} onSave={vi.fn()} devices={[]} recording={false} onRefreshDevices={refresh} /></I18nProvider>);
  const microphone = screen.getByRole("combobox", { name: "Microphone" }) as HTMLSelectElement;
  expect(microphone.value).toBe("disconnected-usb");
  expect(screen.getByRole("option", { name: "Unavailable device" })).toBeTruthy();
  fireEvent.click(screen.getByRole("button", { name: "Refresh microphones" }));
  expect(refresh).toHaveBeenCalledOnce();
  expect(screen.getByRole("button", { name: "Automatic gain" })).toBeTruthy();
  expect(screen.getByRole("combobox", { name: "Language" })).toBeTruthy();
  expect(document.documentElement.lang).toBe("en");
});

test("errors survive later success; queued success expires after dismissal", () => {
  vi.useFakeTimers();
  const { result } = renderHook(() => useNoticeQueue());
  act(() => { result.current.push({ message: "failed", severity: "error" }); result.current.push({ message: "copied", severity: "success" }); });
  act(() => { vi.advanceTimersByTime(10000); });
  expect(result.current.notice?.message).toBe("failed");
  act(() => result.current.dismiss());
  expect(result.current.notice?.message).toBe("copied");
  act(() => { vi.advanceTimersByTime(4000); });
  expect(result.current.notice).toBeNull();
});

test("a recorder unmounted during activation releases the late native lease", async () => {
  let complete!: () => void;
  vi.mocked(setShortcutCapture).mockImplementationOnce(() => new Promise<void>((resolve) => { complete = resolve; }));
  const view = render(<I18nProvider language="en"><ShortcutField label="Shortcut" value="Ctrl+T" onChange={vi.fn()} /></I18nProvider>);
  fireEvent.click(screen.getByRole("button", { name: "Record shortcut" }));
  const id = vi.mocked(setShortcutCapture).mock.calls[0][0];
  view.unmount();
  await act(async () => complete());
  expect(vi.mocked(setShortcutCapture).mock.calls).toEqual([[id, true], [id, false], [id, false]]);
});

test("grouped selects and profile guidance have distinct accessible names", () => {
  render(<I18nProvider language="en"><SettingsPage settings={{ ...defaultSettings, globalStyleProfile: { formality: "formal", detail: "concise", guidance: "" } }} onSave={vi.fn()} devices={[]} recording={false} /></I18nProvider>);
  for (const name of ["Current translation target", "Language to add", "Global profile formality", "Global profile detail"]) expect(screen.getByRole("combobox", { name })).toBeTruthy();
  expect(screen.getByRole("textbox", { name: "Global profile guidance" })).toBeTruthy();
});

test("overlapping recorder starts keep only the latest activation when IPC completes in reverse", async () => {
  const activations: Array<{ id: number; resolve: () => void }> = [];
  vi.mocked(setShortcutCapture).mockImplementation((id, active) => active ? new Promise<void>((resolve) => activations.push({ id, resolve })) : Promise.resolve());
  const change = vi.fn();
  render(<I18nProvider language="en"><ShortcutField label="Shortcut" value="Ctrl+T" onChange={change} /></I18nProvider>);
  fireEvent.click(screen.getByRole("button", { name: "Record shortcut" }));
  fireEvent.click(screen.getByRole("button", { name: "Record shortcut" }));
  expect(activations).toHaveLength(2);
  expect(activations[1].id).toBeGreaterThan(activations[0].id);
  await act(async () => activations[1].resolve());
  await act(async () => activations[0].resolve());
  fireEvent.keyDown(screen.getByRole("textbox", { name: "Shortcut" }), { key: "a", code: "KeyA", ctrlKey: true });
  expect(change.mock.calls).toEqual([["Ctrl+A"]]);
  expect(vi.mocked(setShortcutCapture).mock.calls.at(-1)).toEqual([activations[1].id, false]);
});
