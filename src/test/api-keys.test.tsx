import { act, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { beforeEach, expect, test, vi } from "vitest";
import { ApiKeySettings } from "../components/ApiKeySettings";
import { defaultSettings, getProviderCredentials, updateProviderCredential, getSettings, updateSettings } from "../api";
import { useSettings } from "../hooks/useSettings";
import { I18nProvider } from "../i18n";
import type { CredentialStatus, SettingsSnapshot } from "../types";

vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn(async () => () => {}) }));
vi.mock("../api", async (original) => ({ ...await original<typeof import("../api")>(), getProviderCredentials: vi.fn(), updateProviderCredential: vi.fn(), getSettings: vi.fn(), updateSettings: vi.fn() }));
const statuses: CredentialStatus[] = ["openai", "gemini", "speech"].map((provider) => ({ provider, alias: provider === "gemini" ? "GEMINI_API_KEY" : "OPENAI_API_KEY", source: "missing", storeAvailable: true })) as CredentialStatus[];
beforeEach(() => { vi.mocked(getProviderCredentials).mockReset().mockResolvedValue(statuses); vi.mocked(updateProviderCredential).mockReset(); });
function show() { return render(<I18nProvider language="ja"><ApiKeySettings settings={defaultSettings} /></I18nProvider>); }

test("key drafts are masked and only presence returns after an explicit save", async () => {
  vi.mocked(updateProviderCredential).mockResolvedValue({ provider: "openai", alias: "OPENAI_API_KEY", source: "credential_manager", storeAvailable: true });
  show();
  const input = screen.getByLabelText("OpenAIのAPIキー") as HTMLInputElement;
  await waitFor(() => expect(input.disabled).toBe(false));
  expect(input.type).toBe("password");
  expect(input.value).toBe("");
  fireEvent.change(input, { target: { value: "synthetic-private-key" } });
  expect(updateProviderCredential).not.toHaveBeenCalled();
  fireEvent.click(within(input.closest("form")!).getByRole("button", { name: "キーを保存" }));
  await screen.findByText("APIキーを保存しました");
  expect(updateProviderCredential).toHaveBeenCalledWith("openai", defaultSettings.openaiApiKeyEnvVar, "synthetic-private-key");
  expect(input.value).toBe("");
  expect(document.body.textContent).not.toContain("synthetic-private-key");
});

test("deletion removes only the saved credential and reports the environment fallback", async () => {
  vi.mocked(getProviderCredentials).mockResolvedValueOnce([{ provider: "openai", alias: "OPENAI_API_KEY", source: "credential_manager", storeAvailable: true }, ...statuses.slice(1)]);
  vi.mocked(updateProviderCredential).mockResolvedValue({ provider: "openai", alias: "OPENAI_API_KEY", source: "environment", storeAvailable: true });
  show();
  const form = screen.getByLabelText("OpenAIのAPIキー").closest("form")!;
  const remove = within(form).getByRole("button", { name: "保存済みのキーを削除" });
  await waitFor(() => expect((remove as HTMLButtonElement).disabled).toBe(false));
  fireEvent.click(remove);
  await screen.findByText("保存済みのキーを削除しました。環境変数・.envのキーが使われる場合があります。");
  expect(updateProviderCredential).toHaveBeenCalledWith("openai", defaultSettings.openaiApiKeyEnvVar, null);
});

test("a completed stale save cannot clear the draft for a different alias", async () => {
  let finish!: (value: CredentialStatus) => void;
  vi.mocked(updateProviderCredential).mockImplementation(() => new Promise((resolve) => { finish = resolve; }));
  const view = show();
  const input = screen.getByLabelText("OpenAIのAPIキー") as HTMLInputElement;
  await waitFor(() => expect(input.disabled).toBe(false));
  fireEvent.change(input, { target: { value: "old-key" } });
  fireEvent.click(within(input.closest("form")!).getByRole("button", { name: "キーを保存" }));
  vi.mocked(getProviderCredentials).mockResolvedValue(statuses.map((status) => status.provider === "openai" ? { ...status, alias: "OTHER_KEY" } : status));
  view.rerender(<I18nProvider language="ja"><ApiKeySettings settings={{ ...defaultSettings, openaiApiKeyEnvVar: "OTHER_KEY" }} /></I18nProvider>);
  await waitFor(() => expect(input.disabled).toBe(false));
  fireEvent.change(input, { target: { value: "new-key" } });
  await act(async () => { finish({ provider: "openai", alias: "OPENAI_API_KEY", source: "credential_manager", storeAvailable: true }); });
  expect(input.value).toBe("new-key");
  expect(screen.queryByText("APIキーを保存しました")).toBeNull();
});

test("an optimistic alias cannot display old key presence and confirmed settings refresh it", async () => {
  let persisted = { ...defaultSettings, setupComplete: true };
  let commit!: () => void;
  vi.mocked(getSettings).mockResolvedValue({ settings: persisted, revision: 1 });
  vi.mocked(updateSettings).mockImplementation((patch) => new Promise<SettingsSnapshot>((resolve) => {
    commit = () => { persisted = { ...persisted, ...patch }; resolve({ settings: persisted, revision: 2 }); };
  }));
  vi.mocked(getProviderCredentials).mockImplementation(async () => statuses.map((status) => status.provider === "openai" ? {
    ...status, alias: persisted.openaiApiKeyEnvVar,
    source: persisted.openaiApiKeyEnvVar === "OPENAI_API_KEY" ? "credential_manager" : "missing",
  } : status));
  function Harness() {
    const model = useSettings({ onLanguageChange: () => {}, onSetupNeeded: () => {}, onNotReady: () => {}, onError: () => {} });
    return <><button disabled={!model.settingsLoaded} onClick={() => void model.queueSettingsSave({ openaiApiKeyEnvVar: "NEW_PROVIDER_KEY" })}>Change alias</button><ApiKeySettings settings={model.settings} /></>;
  }
  render(<I18nProvider language="ja"><Harness /></I18nProvider>);
  const input = screen.getByLabelText("OpenAIのAPIキー") as HTMLInputElement;
  await waitFor(() => expect((screen.getByText("Change alias") as HTMLButtonElement).disabled).toBe(false));
  await screen.findByText(/OPENAI_API_KEY — 資格情報マネージャーに保存済み/);
  fireEvent.click(screen.getByText("Change alias"));
  await waitFor(() => expect(updateSettings).toHaveBeenCalledOnce());
  await screen.findByText(/NEW_PROVIDER_KEY — 未確認/);
  expect(input.disabled).toBe(true);
  expect(within(input.closest("form")!).queryByText(/資格情報マネージャーに保存済み/)).toBeNull();
  await act(async () => { commit(); });
  await screen.findByText(/NEW_PROVIDER_KEY — 未設定/);
  await waitFor(() => expect(input.disabled).toBe(false));
});
