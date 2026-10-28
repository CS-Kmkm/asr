// Compatibility loader for legacy node:test cases while they migrate to RTL.
// Relative imports share the entry's mocks, so extracting a hook does not
// require inlining its implementation or emulating a second React instance.
import { existsSync, readFileSync } from "node:fs";
import { createRequire } from "node:module";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { runInNewContext } from "node:vm";
import ts from "typescript";

export function loadSource(entryUrl, { mocks = {}, globals = {}, expose = [] } = {}) {
  const entry = fileURLToPath(entryUrl);
  const identity = (path) => path.replace(/\.(tsx?|m?js)$/, "");
  const relativeMocks = new Map(Object.entries(mocks).filter(([name]) => name.startsWith("."))
    .map(([name, value]) => [identity(resolve(dirname(entry), name)), value]));
  const cache = new Map();
  function load(path) {
    if (cache.has(path)) return cache.get(path).exports;
    if (path.endsWith(".json")) return JSON.parse(readFileSync(path, "utf8"));
    let source = readFileSync(path, "utf8");
    if (path === entry) for (const name of expose) source = source.replace(`function ${name}(`, `export function ${name}(`);
    const output = ts.transpileModule(source, { compilerOptions: {
      module: ts.ModuleKind.CommonJS, jsx: ts.JsxEmit.ReactJSX,
      esModuleInterop: true, target: ts.ScriptTarget.ES2021,
    } }).outputText;
    const module = { exports: {} };
    cache.set(path, module);
    const nativeRequire = createRequire(path);
    runInNewContext(output, { module, exports: module.exports, window: {}, ...globals, require: (name) => {
      if (!name.startsWith(".")) return name in mocks ? mocks[name] : nativeRequire(name);
      const absolute = resolve(dirname(path), name);
      if (relativeMocks.has(identity(absolute))) return relativeMocks.get(identity(absolute));
      const local = [absolute, `${absolute}.ts`, `${absolute}.tsx`, `${absolute}.json`].find((candidate) => existsSync(candidate));
      return local ? load(local) : nativeRequire(name);
    } });
    return module.exports;
  }
  return load(entry);
}
