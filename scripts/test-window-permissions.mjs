import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";

const read = (path) => readFileSync(new URL(`../${path}`, import.meta.url), "utf8");
const capability = (name) => JSON.parse(read(`src-tauri/capabilities/${name}.json`));

test("auxiliary windows cannot invoke settings, History or autostart commands", () => {
  const main = capability("default");
  assert.deepEqual(main.windows, ["main"]);
  assert.ok(main.permissions.includes("main-commands"));
  for (const [name, allowed] of [
    ["recording-overlay", ["core:event:allow-listen", "core:event:allow-unlisten", "allow-get-ui-preferences", "allow-cancel-recording", "allow-cycle-voice-translation-target"]],
    ["ask-answer", ["core:event:allow-listen", "core:event:allow-unlisten", "allow-get-ui-preferences", "allow-get-ask-answer", "allow-dismiss-ask-answer", "allow-copy-to-clipboard"]],
  ]) {
    const config = capability(name);
    assert.deepEqual(config.windows, [name]);
    assert.deepEqual(config.permissions.toSorted(), allowed.toSorted());
  }
  for (const name of ["default", "recording-overlay", "ask-answer"]) {
    assert.ok(capability(name).permissions.every((permission) => !permission.startsWith("autostart:")));
  }
});

test("every IPC handler is covered by the application manifest and main permission", () => {
  const source = read("src-tauri/src/lib.rs");
  const handler = /\.invoke_handler\(tauri::generate_handler!\[([\s\S]*?)\]\)/.exec(source)?.[1];
  assert.ok(handler);
  const names = [...handler.matchAll(/commands(?:::[a-z_]+)+/g)].map((match) => match[0].split("::").at(-1)).sort();
  for (const path of ["src-tauri/src/command_manifest.rs", "src-tauri/permissions/main.toml"]) {
    const declared = [...read(path).matchAll(/^\s*"([a-z_]+)",/gm)].map((match) => match[1]).sort();
    assert.deepEqual(declared, names, path);
  }
  assert.match(read("src-tauri/build.rs"), /app_manifest\(tauri_build::AppManifest::new\(\)\.commands\(APP_COMMANDS\)\)/);
});
