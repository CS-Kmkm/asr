import { readFileSync } from "node:fs";
import { expect, test } from "vitest";
import { speechLocaleRegistry } from "../types";

test("speech locales match the Rust validation allowlist", () => {
  const rust = readFileSync("src-tauri/src/types.rs", "utf8");
  const body = /pub const SPEECH_LOCALES: \[&str; \d+\] = \[([^\]]*)\]/.exec(rust)?.[1];
  expect(body).toBeTruthy();
  const allowed = [...body!.matchAll(/"([^"]+)"/g)].map((match) => match[1]);
  expect(speechLocaleRegistry.map(({ tag }) => tag)).toEqual(allowed);
  expect(allowed).toEqual(expect.arrayContaining(["ja-JP", "ko-KR", "de-DE"]));
});
