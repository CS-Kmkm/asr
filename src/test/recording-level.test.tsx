import { act, render, screen, waitFor } from "@testing-library/react";
import { expect, test, vi } from "vitest";
import { RecordingLevel } from "../components/RecordingLevel";
import { I18nProvider } from "../i18n";

const transport = vi.hoisted(() => ({
  receive: (_event: { payload: { peak: number; rms: number } }) => {},
  stop: vi.fn(),
}));
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn(async (_name, handler) => {
  transport.receive = handler;
  return transport.stop;
}) }));

test("audio meter events update the visible meter without rendering its parent", async () => {
  let parentRenders = 0;
  function Parent() { parentRenders += 1; return <RecordingLevel />; }
  const view = render(<I18nProvider language="en"><Parent /></I18nProvider>);
  await waitFor(() => expect(screen.getByLabelText("Input level 0 percent")).toBeTruthy());
  await act(async () => { transport.receive({ payload: { peak: 0.5, rms: 0.2 } }); });
  expect(screen.getByLabelText("Input level 50 percent")).toBeTruthy();
  expect(parentRenders).toBe(1);
  view.unmount();
  expect(transport.stop).toHaveBeenCalledTimes(1);
});
