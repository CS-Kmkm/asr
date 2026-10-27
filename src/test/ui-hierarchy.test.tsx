import { act, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { expect, test, vi } from "vitest";
import { HelpHint, SettingRow, Toggle } from "../components/ui";
import { CommitField } from "../components/CommitField";
import { SettingsPage, type SettingsSection } from "../pages/SettingsPage";
import { PrivacyPage } from "../pages/PrivacyPage";
import { AiCorrectionSettings } from "../components/AiCorrectionSettings";
import { I18nProvider } from "../i18n";
import { defaultSettings, startMicrophoneTest, stopMicrophoneTest } from "../api";

vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn(async () => () => {}) }));
vi.mock("../api", async original => ({ ...await original<typeof import("../api")>(), startMicrophoneTest: vi.fn(async () => {}), stopMicrophoneTest: vi.fn(async () => {}) }));

test("a concise setting has an accessible label without a dangling help reference", () => {
  const app = render(<SettingRow title="Device" control={<select><option>Default</option></select>} />);
  expect(screen.getByRole("combobox", { name: "Device" }).getAttribute("aria-describedby")).toBeNull();
  app.rerender(<SettingRow title="Device" detail="Important device constraint" control={<select><option>Default</option></select>} />);
  const reference = screen.getByRole("combobox", { name: "Device" }).getAttribute("aria-describedby");
  expect(document.getElementById(reference!)?.textContent).toBe("Important device constraint");
});

test("Japanese safety summaries remain readable beside their controls", () => {
  const app = render(<I18nProvider language="ja"><SettingsPage section="input" settings={defaultSettings} devices={[]} recording={false} onSave={vi.fn()} /></I18nProvider>);
  const experimental = screen.getByText("一部のアプリでは途中の文字だけが残ることがあります。");
  expect(experimental.closest("details, .sr-only")).toBeNull();
  app.rerender(<I18nProvider language="ja"><AiCorrectionSettings settings={{ ...defaultSettings, correctionMode: "intent_aware" }} onSave={vi.fn()} /></I18nProvider>);
  const limitation = screen.getByText(/固有名詞は未検証です/);
  expect(limitation.closest("details")).toBeNull();
  app.rerender(<I18nProvider language="ja"><PrivacyPage settings={defaultSettings} settingsLoaded onSave={vi.fn()} /></I18nProvider>);
  expect(screen.getByText(/新しい録音を削除。失敗録音の例外/).closest("details, .sr-only")).toBeNull();
  expect(screen.getByText(/履歴オフでも録音・編集対象を保持/).closest("details, .sr-only")).toBeNull();
});

test("ordinary row help is visually hidden but remains associated with its control", () => {
  render(<I18nProvider language="en"><SettingRow title="Device" detail="Choose your audio input" control={<select><option>Default</option></select>} /></I18nProvider>);
  const select = screen.getByRole("combobox", { name: "Device" });
  expect(document.getElementById(select.getAttribute("aria-describedby")!)?.className).toBe("sr-only");
  expect(screen.queryByRole("tooltip")).toBeNull();
  fireEvent.focus(screen.getByRole("button", { name: "Help: Device" }));
  expect(screen.getByRole("tooltip").textContent).toBe("Choose your audio input");
});

test("component controls receive help descriptions and retain validation descriptions", () => {
  render(<I18nProvider language="en">
    <SettingRow title="Sound" detail="Plays a local cue" control={<Toggle label="Sound" checked={false} onChange={vi.fn()} />} />
    <SettingRow title="Model" detail="Model identifier" control={<CommitField label="Model" value="old-model" validate={value => value ? null : "Enter a non-empty model ID without control characters (up to 512 bytes)."} onCommit={vi.fn()} />} />
  </I18nProvider>);
  const sound = screen.getByRole("button", { name: "Sound" });
  const model = screen.getByRole("textbox", { name: "Model" });
  expect(document.getElementById(sound.getAttribute("aria-describedby")!)?.textContent).toBe("Plays a local cue");
  expect(document.getElementById(model.getAttribute("aria-describedby")!)?.textContent).toBe("Model identifier");
  fireEvent.change(model, { target: { value: "" } });
  fireEvent.blur(model);
  const descriptions = model.getAttribute("aria-describedby")!.split(" ").map(id => document.getElementById(id)?.textContent);
  expect(descriptions).toContain("Model identifier");
  expect(descriptions).toContain(screen.getByRole("alert").textContent);
});

test("Edit and Ask retain concise visible send warnings", () => {
  render(<I18nProvider language="en"><SettingsPage section="shortcuts" settings={defaultSettings} devices={[]} recording={false} onSave={vi.fn()} /></I18nProvider>);
  for (const phrase of ["Uses provider even with correction off: selected text and instruction. ASR may send audio.", "Uses provider even with correction off: instruction and any used selection. ASR may send audio."]) {
    expect(screen.getByText(phrase).closest("details, .sr-only")).toBeNull();
  }
});

test("help supports hover, focus, click and Escape without changing the setting", () => {
  render(<I18nProvider language="en"><HelpHint label="Audio">Audio is measured locally.</HelpHint><button>Elsewhere</button></I18nProvider>);
  const help = screen.getByRole("button", { name: "Help: Audio" });
  const wrapper = help.closest(".help-hint")!;
  fireEvent.mouseEnter(wrapper);
  expect(screen.getByRole("tooltip").textContent).toContain("measured locally");
  fireEvent.mouseLeave(help, { relatedTarget: screen.getByRole("tooltip") });
  expect(screen.getByRole("tooltip")).toBeTruthy();
  fireEvent.mouseLeave(wrapper);
  expect(screen.queryByRole("tooltip")).toBeNull();
  act(() => help.focus());
  expect(screen.getByRole("tooltip")).toBeTruthy();
  fireEvent.keyDown(document, { key: "Escape" });
  expect(screen.queryByRole("tooltip")).toBeNull();
  expect(document.activeElement).toBe(help);
  fireEvent.click(help);
  expect(screen.getByRole("tooltip")).toBeTruthy();
  fireEvent.pointerDown(screen.getByRole("button", { name: "Elsewhere" }));
  expect(screen.queryByRole("tooltip")).toBeNull();
});

test.each(["scroll", "resize"])("open help closes on viewport %s", event => {
  render(<I18nProvider language="en"><HelpHint label="Audio">Audio help</HelpHint></I18nProvider>);
  fireEvent.click(screen.getByRole("button", { name: "Help: Audio" }));
  expect(screen.getByRole("tooltip")).toBeTruthy();
  fireEvent(window, new Event(event));
  expect(screen.queryByRole("tooltip")).toBeNull();
});

test("long combined help can receive keyboard focus and scroll without dismissal", () => {
  render(<I18nProvider language="en"><SettingRow title="Audio" detail="Brief explanation" help={"Detailed explanation\n".repeat(50)} control={<select><option>Default</option></select>} /></I18nProvider>);
  expect(screen.queryByText("Details")).toBeNull();
  const trigger = screen.getByRole("button", { name: "Help: Audio" });
  act(() => trigger.focus());
  const content = screen.getByRole("tooltip").querySelector<HTMLElement>(".help-tooltip-content")!;
  expect(content.textContent).toContain("Brief explanation\n\nDetailed explanation");
  act(() => content.focus());
  fireEvent.scroll(content);
  expect(screen.getByRole("tooltip")).toBeTruthy();
  fireEvent.keyDown(content, { key: "Escape" });
  expect(screen.queryByRole("tooltip")).toBeNull();
  expect(document.activeElement).toBe(trigger);
});

test.each([false, true])("leaving input settings stops a microphone test, including pending start (%s)", async pending => {
  vi.clearAllMocks();
  let started!: () => void;
  if (pending) vi.mocked(startMicrophoneTest).mockImplementationOnce(() => new Promise<void>(resolve => { started = resolve; }));
  const view = (section: SettingsSection) => <I18nProvider language="en"><SettingsPage section={section} settings={defaultSettings} devices={[]} recording={false} onSave={vi.fn()} /></I18nProvider>;
  const app = render(view("input"));
  fireEvent.click(screen.getByRole("button", { name: "Start test" }));
  await waitFor(() => expect(startMicrophoneTest).toHaveBeenCalled());
  app.rerender(view("general"));
  if (pending) await act(async () => started());
  await waitFor(() => expect(stopMicrophoneTest).toHaveBeenCalled());
  expect(screen.queryByRole("button", { name: "Stop test" })).toBeNull();
});
