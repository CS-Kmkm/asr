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

test("invalid priority is explained in the DOM and cannot be submitted", async () => {
  const { onAdd } = dictionary();
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
  fireEvent.click(screen.getByRole("button", { name: "Edit" }));
  const priority = screen.getByRole("spinbutton", { name: "Priority" }) as HTMLInputElement;
  expect(priority.value).toBe("5000000");
  expect(priority.validationMessage).toBe("");
  fireEvent.submit(priority.closest("form")!);
  await waitFor(() => expect(onUpdate).toHaveBeenCalledWith(4, expect.objectContaining({ priority: 5_000_000 })));
  fireEvent.click(screen.getByRole("button", { name: "Edit" }));
  fireEvent.change(priority, { target: { value: "5000001" } });
  fireEvent.submit(priority.closest("form")!);
  expect(onUpdate).toHaveBeenCalledTimes(1);
  expect(screen.getByRole("alert")).toBeTruthy();
});
