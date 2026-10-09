// Run with node --test scripts/test-frontend-safety.mjs.
// Exercise page-level guards with isolated hooks and IPC stubs; this does not
// emulate a browser, so native constraint validation and dialogs are stubbed.
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { createRequire } from "node:module";
import { test } from "node:test";
import { runInNewContext } from "node:vm";
import ts from "typescript";

const require = createRequire(import.meta.url);
function load(relativePath, mocks = {}, runtimeWindow = {}) {
  const source = readFileSync(new URL(relativePath, import.meta.url), "utf8");
  const output = ts.transpileModule(source, {
    compilerOptions: { module: ts.ModuleKind.CommonJS, jsx: ts.JsxEmit.ReactJSX, target: ts.ScriptTarget.ES2021 },
  }).outputText;
  const module = { exports: {} };
  runInNewContext(output, {
    module, exports: module.exports, window: runtimeWindow,
    require: (name) => name in mocks ? mocks[name] : require(name),
  });
  return module.exports;
}
const registries = load("../src/types.ts");

test("speech locale registry matches the Rust update_settings allowlist", () => {
  const rust = readFileSync(new URL("../src-tauri/src/types.rs", import.meta.url), "utf8");
  const body = /pub const SPEECH_LOCALES: \[&str; \d+\] = \[([^\]]*)\]/.exec(rust)?.[1];
  assert.ok(body, "SPEECH_LOCALES constant not found");
  const allowlist = [...body.matchAll(/"([^"]+)"/g)].map((match) => match[1]);
  assert.deepEqual(Array.from(registries.speechLocaleRegistry, ({ tag }) => tag), allowlist);
  for (const tag of ["ja-JP", "ko-KR", "de-DE"]) assert.ok(allowlist.includes(tag), tag);
});
