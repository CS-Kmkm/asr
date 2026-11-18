import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { expect, test, vi } from "vitest";
import * as api from "../api";
import { TeachStyle } from "../components/StyleLearning";
import { I18nProvider } from "../i18n";
import type { HistoryItem } from "../types";

vi.mock("../api", async importOriginal => ({
  ...await importOriginal<typeof import("../api")>(),
  proposeStyleFromEdit: vi.fn(), setLearnedStyleRule: vi.fn(),
}));
vi.mock("../components/RecentAppPicker", () => ({ RecentAppPicker: () => null }));

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
