import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { expect, test, vi } from "vitest";
import { DictionaryPage, parsePriority, PRIORITY_MAX, PRIORITY_MIN } from "../pages/DictionaryPage";
import { I18nProvider } from "../i18n";
import type { DictionaryEntry } from "../types";

function dictionary(entries: DictionaryEntry[] = []) {
  const onAdd = vi.fn(async () => true);
  const onUpdate = vi.fn(async () => true);
  render(<I18nProvider language="en"><DictionaryPage entries={entries} candidates={[]} onAdd={onAdd} onUpdate={onUpdate}
    onDelete={vi.fn()} onImport={async () => true} onConfirmCandidate={vi.fn()} onRejectCandidate={vi.fn()} /></I18nProvider>);
  return { onAdd, onUpdate };
}

test("dictionary priority accepts only safe whole numbers in its range", () => {
  for (const invalid of ["", " ", "1.5", "NaN", "Infinity", "9007199254740992", "1000001", "-1000001"]) expect(parsePriority(invalid)).toBeNull();
  expect(parsePriority(String(PRIORITY_MIN))).toBe(PRIORITY_MIN);
  expect(parsePriority(String(PRIORITY_MAX))).toBe(PRIORITY_MAX);
  expect(parsePriority(" 7 ")).toBe(7);
});

test("alias formatting help remains available when editing a populated field", () => {
  dictionary([{ id: 1, reading: "term", surface: "Term", aliases: ["existing alias"], category: null, appScope: null, priority: 0, source: "manual", createdAt: "" }]);
  fireEvent.click(screen.getByRole("button", { name: /^Edit:/ }));
  expect(screen.getByRole("textbox", { name: "Aliases" })).toHaveProperty("value", "existing alias");
  fireEvent.click(screen.getByRole("button", { name: "Help: Aliases" }));
  expect(screen.getByRole("tooltip").textContent).toBe("One alias per line");
});

test("candidate actions distinguish different originals with the same preferred spelling", () => {
  const confirm = vi.fn(), reject = vi.fn();
  render(<I18nProvider language="en"><DictionaryPage entries={[]} candidates={["first spelling", "second spelling"].map((originalSpan, index) => ({ id: index + 1, originalSpan, preferredSpan: "Term", confidence: 1, historyId: null, createdAt: "" }))} onAdd={async () => true} onUpdate={async () => true} onDelete={vi.fn()} onImport={async () => true} onConfirmCandidate={confirm} onRejectCandidate={reject} /></I18nProvider>);
  fireEvent.click(screen.getByRole("button", { name: "Confirm: second spelling → Term" }));
  expect(confirm).toHaveBeenCalledExactlyOnceWith(2);
  fireEvent.click(screen.getByRole("button", { name: "Reject: first spelling → Term" }));
  expect(reject).toHaveBeenCalledExactlyOnceWith(1);
});

test("switching the edited entry moves focus to the updated editor", () => {
  dictionary([1, 2].map(id => ({ id, reading: `term-${id}`, surface: `Term ${id}`, aliases: [], category: null,
    appScope: null, priority: 0, source: "manual" as const, createdAt: "" })));
  const edit = screen.getAllByRole("button", { name: /^Edit:/ });
  expect(edit[0].getAttribute("aria-label")).toBe("Edit: Term 1 (term-1)");
  expect(edit[1].getAttribute("aria-label")).toBe("Edit: Term 2 (term-2)");
  fireEvent.click(edit[0]);
  expect(document.activeElement).toBe(screen.getByRole("textbox", { name: "Reading" }));
  edit[1].focus();
  fireEvent.click(edit[1]);
  expect(screen.getByRole("textbox", { name: "Reading" })).toHaveProperty("value", "term-2");
  expect(document.activeElement).toBe(screen.getByRole("textbox", { name: "Reading" }));
});

test("invalid priority is explained in the DOM and cannot be submitted", async () => {
  const { onAdd } = dictionary();
  fireEvent.click(screen.getByRole("button", { name: "Add entry" }));
  fireEvent.change(screen.getByRole("textbox", { name: "Reading" }), { target: { value: "open ai" } });
  fireEvent.change(screen.getByRole("textbox", { name: "Surface" }), { target: { value: "OpenAI" } });
  const priority = screen.getByRole("spinbutton", { name: "Priority" }) as HTMLInputElement;
  fireEvent.change(priority, { target: { value: "1.5" } });
  expect(screen.getByRole("alert").textContent).toBe("Priority must be a whole number from -1000000 to 1000000.");
  expect(priority.validationMessage).toBe(screen.getByRole("alert").textContent);
  fireEvent.submit(priority.closest("form")!);
  expect(onAdd).not.toHaveBeenCalled();
  fireEvent.change(priority, { target: { value: "7" } });
  fireEvent.submit(priority.closest("form")!);
  await waitFor(() => expect(onAdd).toHaveBeenCalledWith(expect.objectContaining({ priority: 7, surface: "OpenAI" })));
});

test("editing preserves an imported priority but rejects a newly out-of-range value", async () => {
  const { onUpdate } = dictionary([{ id: 4, reading: "open ai", surface: "OpenAI", aliases: [], category: null,
    appScope: null, priority: 5_000_000, source: "manual", createdAt: "" }]);
  fireEvent.click(screen.getByRole("button", { name: /^Edit:/ }));
  const priority = screen.getByRole("spinbutton", { name: "Priority" }) as HTMLInputElement;
  expect(priority.value).toBe("5000000");
  expect(priority.validationMessage).toBe("");
  fireEvent.submit(priority.closest("form")!);
  await waitFor(() => expect(onUpdate).toHaveBeenCalledWith(4, expect.objectContaining({ priority: 5_000_000 })));
  fireEvent.click(screen.getByRole("button", { name: /^Edit:/ }));
  fireEvent.change(priority, { target: { value: "5000001" } });
  fireEvent.submit(priority.closest("form")!);
  expect(onUpdate).toHaveBeenCalledTimes(1);
  expect(screen.getByRole("alert")).toBeTruthy();
});

test("entry rows show only non-default metadata and candidates name each spelling once", () => {
  render(<I18nProvider language="en"><DictionaryPage entries={[
    { id: 1, reading: "manual", surface: "Manual", aliases: [], category: null, appScope: null, priority: 0, source: "manual", createdAt: "" },
    { id: 2, reading: "learned", surface: "Learned", aliases: [], category: null, appScope: "app:code", priority: 5, source: "auto", createdAt: "" },
  ]} candidates={[{ id: 3, originalSpan: "open ai", preferredSpan: "OpenAI", confidence: 1, historyId: null, createdAt: "" }]}
    onAdd={async () => true} onUpdate={async () => true} onDelete={vi.fn()} onImport={async () => true} onConfirmCandidate={vi.fn()} onRejectCandidate={vi.fn()} /></I18nProvider>);
  const [manual, learned] = ["Manual", "Learned"].map(name => screen.getByText(name).closest(".setting-row")!);
  expect(manual.querySelectorAll(".entry-badge")).toHaveLength(0);
  expect([...learned.querySelectorAll(".entry-badge")].map(badge => badge.textContent)).toEqual(["Auto-added", "priority 5", "scope: app:code"]);
  expect(screen.getAllByText("OpenAI")).toHaveLength(1);
  expect(screen.getByText("open ai").closest(".candidate-spans")?.textContent).toBe("open ai → OpenAI");
});
