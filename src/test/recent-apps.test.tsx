import { act, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { beforeEach, expect, test, vi } from "vitest";
import { RecentAppPicker } from "../components/RecentAppPicker";
import { I18nProvider } from "../i18n";
import { clearRecentApps, listRecentApps } from "../api";
import type { RecentApp } from "../types";
import { listen } from "@tauri-apps/api/event";

vi.mock("../api", () => ({ clearRecentApps: vi.fn(), listRecentApps: vi.fn() }));
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn() }));
beforeEach(() => { vi.mocked(listen).mockReset().mockResolvedValue(() => {}); vi.mocked(clearRecentApps).mockReset().mockResolvedValue(); vi.mocked(listRecentApps).mockReset().mockResolvedValue([]); });
function show(scopes: string[] = []) {
  const choose = vi.fn();
  render(<I18nProvider language="en"><RecentAppPicker scopes={scopes} onChoose={choose} /></I18nProvider>);
  return choose;
}

test("recent app and category choices create scopes and existing scopes are disabled", async () => {
  vi.mocked(listRecentApps).mockResolvedValue([{ appKey: "code", category: "development" }, { appKey: "pwsh", category: "terminal" }]);
  const choose = show(["app:code"]);
  expect(await screen.findByRole("option", { name: "code · Development" })).toHaveProperty("disabled", true);
  fireEvent.change(screen.getByRole("combobox", { name: "Recent apps" }), { target: { value: "pwsh" } });
  expect(choose).toHaveBeenLastCalledWith("app:pwsh");
  fireEvent.change(screen.getByRole("combobox", { name: "Add category profile" }), { target: { value: "terminal" } });
  expect(choose).toHaveBeenLastCalledWith("category:terminal");
});

test("initial list waits for asynchronous listener registration before taking its snapshot", async () => {
  let registered!: (unlisten: () => void) => void;
  vi.mocked(listen).mockReturnValueOnce(new Promise((resolve) => { registered = resolve; }));
  show();
  expect(listRecentApps).not.toHaveBeenCalled();
  // An observation before the listener exists will be included in the
  // reconciliation snapshot rather than relying on a lost event.
  vi.mocked(listRecentApps).mockResolvedValue([{ appKey: "pwsh", category: "terminal" }]);
  await act(async () => { registered(() => {}); });
  expect(await screen.findByRole("option", { name: "pwsh · Terminal" })).toBeTruthy();
  expect(listRecentApps).toHaveBeenCalledOnce();
});

test("clear ignores an older list response and reads the current native list", async () => {
  let oldRead!: (items: RecentApp[]) => void;
  vi.mocked(listRecentApps).mockReturnValueOnce(new Promise((resolve) => { oldRead = resolve; }));
  show();
  await waitFor(() => expect(listRecentApps).toHaveBeenCalledTimes(1));
  fireEvent.click(screen.getByRole("button", { name: "Clear recent apps" }));
  await waitFor(() => expect(listRecentApps).toHaveBeenCalledTimes(2));
  await act(async () => { oldRead([{ appKey: "stale", category: "other" }]); });
  expect(screen.queryByRole("option", { name: "stale · Other" })).toBeNull();
  expect(clearRecentApps).toHaveBeenCalledOnce();
});

test("failed list has a localized retry without exposing the native error", async () => {
  vi.mocked(listRecentApps).mockRejectedValueOnce(new Error("private-path-sentinel"));
  show();
  expect(await screen.findByRole("alert")).toHaveProperty("textContent", expect.stringContaining("Recent apps could not be loaded or cleared."));
  expect(screen.queryByText("private-path-sentinel")).toBeNull();
  vi.mocked(listRecentApps).mockResolvedValue([{ appKey: "pwsh", category: "terminal" }]);
  fireEvent.click(screen.getByRole("button", { name: "Refresh" }));
  expect(await screen.findByRole("option", { name: "pwsh · Terminal" })).toBeTruthy();
  expect(screen.queryByRole("alert")).toBeNull();
});
