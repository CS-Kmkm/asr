"""Keep the documented error contract synchronized without importing models."""
from __future__ import annotations

import ast
import re
from pathlib import Path


def test_all_worker_error_codes_have_a_documented_table_row() -> None:
    root = Path(__file__).resolve().parents[2]
    documented = set(re.findall(r"^\| `([a-z_]+)` \|", (root / "docs/worker_protocol.md").read_text(encoding="utf-8"), re.MULTILINE))
    implemented: set[str] = set()
    for source in (root / "asr_worker").glob("*.py"):
        for node in ast.walk(ast.parse(source.read_text(encoding="utf-8"))):
            if not isinstance(node, ast.Call) or not isinstance(node.func, ast.Name):
                continue
            index = {"BackendError": 0, "error_response": 1}.get(node.func.id)
            if index is None or len(node.args) <= index:
                continue
            code = node.args[index]
            if isinstance(code, ast.Constant) and isinstance(code.value, str):
                implemented.add(code.value)
    assert implemented, "No error constructors found; update the contract scanner"
    assert not implemented - documented, f"Undocumented worker errors: {sorted(implemented - documented)}"


def test_worker_and_application_release_versions_stay_synchronized() -> None:
    root = Path(__file__).resolve().parents[2]
    tree = ast.parse((root / "asr_worker/worker.py").read_text(encoding="utf-8"))
    worker_version = next(node.value.value for node in tree.body if isinstance(node, ast.Assign)
                          and any(isinstance(target, ast.Name) and target.id == "WORKER_VERSION" for target in node.targets)
                          and isinstance(node.value, ast.Constant))
    versions = []
    for manifest in (root / "pyproject.toml", root / "src-tauri/Cargo.toml"):
        match = re.search(r'^version\s*=\s*"([^"]+)"', manifest.read_text(encoding="utf-8"), re.MULTILINE)
        assert match, f"Missing package version in {manifest.name}"
        versions.append(match.group(1))
    assert versions == [worker_version, worker_version]
