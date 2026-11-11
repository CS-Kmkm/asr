import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { expect, test, vi } from "vitest";
import { DictionaryPage } from "../pages/DictionaryPage";
import { HistoryPage } from "../pages/HistoryPage";
import { defaultSettings } from "../api";
import { I18nProvider } from "../i18n";
import type { DictionaryCandidate, HistoryItem, Settings } from "../types";

const candidate: DictionaryCandidate = { id: 7, originalSpan: "ぎっとはぶ", preferredSpan: "GitHub", confidence: 1, historyId: 3, createdAt: "", origin: "user_edit" };

function dictionary(props: Partial<Parameters<typeof DictionaryPage>[0]> = {}) {
  const handlers = { onConfirmCandidate: vi.fn(), onRejectCandidate: vi.fn(), onToggleLearning: vi.fn(), onDeleteLearned: vi.fn() };
  render(<I18nProvider language="en"><DictionaryPage entries={[]} candidates={[candidate]} onAdd={async () => true} onUpdate={async () => true}
    onDelete={vi.fn()} onImport={async () => true} learning={false} {...handlers} {...props} /></I18nProvider>);
  return handlers;
}

test("learning is an explicit opt-in with a delete action", () => {
  const { onToggleLearning, onDeleteLearned } = dictionary();
  const toggle = screen.getByRole("switch", { name: "Learn from my edits" });
  expect(toggle.getAttribute("aria-checked")).toBe("false");
  fireEvent.click(toggle);
  expect(onToggleLearning).toHaveBeenCalledExactlyOnceWith(true);
  const confirm = vi.spyOn(window, "confirm").mockReturnValueOnce(false).mockReturnValueOnce(true);
  fireEvent.click(screen.getByRole("button", { name: "Delete learned data" }));
  expect(onDeleteLearned).not.toHaveBeenCalled();
  fireEvent.click(screen.getByRole("button", { name: "Delete learned data" }));
  expect(onDeleteLearned).toHaveBeenCalledOnce();
  confirm.mockRestore();
});

test("suggestions from edits are labeled and confirm into the chosen scope", () => {
  const { onConfirmCandidate } = dictionary();
  expect(screen.getByText("From your edit")).toBeTruthy();
  const context = "ぎっとはぶ → GitHub";
  // Default scope: every mode, globally, as before.
  fireEvent.click(screen.getByRole("button", { name: `Confirm: ${context}` }));
  expect(onConfirmCandidate).toHaveBeenLastCalledWith(7);
  fireEvent.click(screen.getByRole("checkbox", { name: `Ask: ${context}` }));
  fireEvent.change(screen.getByRole("textbox", { name: `Scope: ${context}` }), { target: { value: " category:development " } });
  fireEvent.click(screen.getByRole("button", { name: `Confirm: ${context}` }));
  expect(onConfirmCandidate).toHaveBeenLastCalledWith(7, { appScope: "category:development", modes: ["dictate", "edit"] });
});

test("the last selected mode cannot be cleared", () => {
  dictionary();
  const context = "ぎっとはぶ → GitHub";
  fireEvent.click(screen.getByRole("checkbox", { name: `Ask: ${context}` }));
  fireEvent.click(screen.getByRole("checkbox", { name: `Edit: ${context}` }));
  expect((screen.getByRole("checkbox", { name: `Dictate: ${context}` }) as HTMLInputElement).disabled).toBe(true);
});

test("learned entries show their badge and limited modes", () => {
  dictionary({ candidates: [], entries: [{ id: 1, reading: "ぎっとはぶ", surface: "GitHub", aliases: [], category: null, appScope: null, priority: 0, source: "learned", createdAt: "", modes: ["dictate"] }] });
  expect(screen.getByText("Learned")).toBeTruthy();
  expect(screen.getByText("Dictate", { selector: ".entry-badge" })).toBeTruthy();
});

const item: HistoryItem = {
  id: 3, transcriptText: "変更はぎっとはぶにプッシュ", processedText: "変更はぎっとはぶにプッシュします", sourceText: null, instructionText: null,
  actionKind: null, searchSite: null, mode: "ai_corrected", asrProvider: "mock", llmProvider: "local", targetLanguage: null, appCategory: null,
  durationMs: null, latencyMs: null, createdAt: "2026-11-11T00:00:00Z", hasAudio: false, retryOfId: null, insertionResult: null, insertionDetail: null, expiresAt: null,
} as HistoryItem;

function history(settings: Partial<Settings>, onTeach = vi.fn(async () => true)) {
  render(<I18nProvider language="en"><HistoryPage settings={{ ...defaultSettings, ...settings }} settingsLoaded history={[item]} filter="all"
    onSave={vi.fn()} onFilter={vi.fn()} onCopyItem={vi.fn()} onRetry={vi.fn()} onDelete={vi.fn()} onDeleteAll={vi.fn()}
    onLoadAudio={vi.fn()} onAudioError={vi.fn()} retryActive={false} onCancelRetry={vi.fn()} onTeach={onTeach} /></I18nProvider>);
  return onTeach;
}

test("teaching a spelling is offered only with learning and History on", () => {
  history({ personalizationLearning: false });
  expect(screen.queryByText("Teach a spelling")).toBeNull();
});

test("History off hides teaching even with learning on", () => {
  history({ personalizationLearning: true, historyRetention: "never" });
  expect(screen.queryByText("Teach a spelling")).toBeNull();
});

test("an edit is sent only when the user submits a changed text", async () => {
  const onTeach = history({ personalizationLearning: true, historyRetention: "forever" });
  fireEvent.click(screen.getByText("Teach a spelling"));
  const submit = screen.getByRole("button", { name: "Suggest from my edit" }) as HTMLButtonElement;
  expect(submit.disabled).toBe(true);
  fireEvent.change(screen.getByRole("textbox", { name: "Edited text" }), { target: { value: "変更はGitHubにプッシュします" } });
  expect(onTeach).not.toHaveBeenCalled();
  fireEvent.click(submit);
  await waitFor(() => expect(onTeach).toHaveBeenCalledExactlyOnceWith(3, "変更はGitHubにプッシュします"));
});
