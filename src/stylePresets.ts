import type { StyleProfile } from "./types";

export const stylePresets = [
  { id: "email", label: "Email preset", profile: { formality: "formal", detail: "concise", guidance: "Use a clear greeting and closing only when present in the dictated text." } },
  { id: "chat", label: "Chat preset", profile: { formality: "casual", detail: "concise", guidance: "Keep a conversational tone. Preserve the speaker's meaning." } },
  { id: "technical", label: "Code / Technical preset", profile: { formality: "formal", detail: "concise", guidance: "Preserve code, commands, identifiers, file paths, and technical terms exactly." } },
  { id: "notes", label: "Notes preset", profile: { formality: "casual", detail: "detailed", guidance: "Use readable paragraphs or lists only when supported by the dictated structure. Do not add facts." } },
] as const satisfies ReadonlyArray<{ id: string; label: string; profile: StyleProfile }>;

export const profileCategories = ["browser", "email", "messaging", "development", "document", "terminal", "other"] as const;
export const categoryLabels = {
  browser: "Browser category", email: "Email category", messaging: "Messaging category",
  development: "Development category", document: "Document category", terminal: "Terminal category", other: "Other category",
} as const;
