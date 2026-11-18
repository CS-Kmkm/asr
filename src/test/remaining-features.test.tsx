import { act, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { beforeEach, expect, test, vi } from "vitest";
import { defaultSettings } from "../api";
import * as api from "../api";
import { TeachStyle } from "../components/StyleLearning";
import { LocalServerSettings } from "../components/LocalServerSettings";
import { I18nProvider } from "../i18n";
import type { HistoryItem, LocalServerStatus } from "../types";

vi.mock("../api", async importOriginal => ({
  ...await importOriginal<typeof import("../api")>(),
  proposeStyleFromEdit: vi.fn(), setLearnedStyleRule: vi.fn(),
  getLocalServerStatus: vi.fn(), startLocalServer: vi.fn(), stopLocalServer: vi.fn(),
}));
vi.mock("../components/RecentAppPicker", () => ({ RecentAppPicker: () => null }));

beforeEach(() => {
  vi.mocked(api.getLocalServerStatus).mockResolvedValue({ state: "stopped", pid: null, readyMs: null });
  vi.mocked(api.stopLocalServer).mockResolvedValue(undefined);
});

test("edited text is submitted once and only confirmed fixed codes are persisted", async () => {
  vi.mocked(api.proposeStyleFromEdit).mockResolvedValue(["formal"]);
  const item = { id: 41, transcriptText: "記録だ。", processedText: null, appCategory: null } as HistoryItem;
  render(<I18nProvider language="en"><TeachStyle item={item} /></I18nProvider>);
  fireEvent.change(screen.getByRole("textbox", { name: "Style edit" }), { target: { value: "記録です。" } });
  expect(api.proposeStyleFromEdit).not.toHaveBeenCalled();
  fireEvent.click(screen.getByRole("button", { name: "Suggest style preferences" }));
  await screen.findByRole("button", { name: "Confirm: Prefer polite endings" });
  expect(api.setLearnedStyleRule).not.toHaveBeenCalled();
  expect(api.proposeStyleFromEdit).toHaveBeenCalledWith(41, "記録です。");
  fireEvent.click(screen.getByRole("button", { name: "Confirm: Prefer polite endings" }));
  await waitFor(() => expect(api.setLearnedStyleRule).toHaveBeenCalledWith({ preference: "formal", scope: "global", modes: ["dictate"] }, true));
});

test("stop remains available during server startup and ignores its late response", async () => {
  let finish!: (value: LocalServerStatus) => void;
  vi.mocked(api.startLocalServer).mockReturnValue(new Promise(resolve => { finish = resolve; }));
  render(<I18nProvider language="en"><LocalServerSettings settings={defaultSettings} onSave={vi.fn()} recording={false} /></I18nProvider>);
  await waitFor(() => expect(api.getLocalServerStatus).toHaveBeenCalled());
  fireEvent.click(screen.getByRole("button", { name: "Start server" }));
  const stop = screen.getByRole("button", { name: "Stop server" }) as HTMLButtonElement;
  expect(stop.disabled).toBe(false);
  fireEvent.click(stop);
  await waitFor(() => expect(api.stopLocalServer).toHaveBeenCalled());
  await act(async () => { finish({ state: "ready", pid: 123, readyMs: 42 }); });
  expect(screen.getByRole("status").textContent).toBe("Server stopped");
});
