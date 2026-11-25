"""Run the Phase 1 evaluation stages and retain local, attributable evidence."""
from __future__ import annotations

import argparse
from contextlib import closing
from collections import Counter
import hashlib
from importlib.metadata import PackageNotFoundError, version
import json
import os
from pathlib import Path
import platform
import re
import shutil
import signal
import sqlite3
import statistics
import subprocess
import sys
import time
import uuid
import wave

from bench_data import LOCK_SCHEMA, DataError, read_document
from bench_asr import write_sine_wav
from eval_correction import DEFAULT_DATASET as CORRECTION_DATASET
from score_asr import load_manifest
from windows_acceptance import (
    MOCK_RECEIVERS, REAL_EXTERNAL, REAL_EXTERNAL_REPEATS, REAL_NOTEPAD, REAL_RECEIVERS, WORKER_RECOVERY,
)

ROOT = Path(__file__).resolve().parents[1]
PUBLIC = ROOT / "docs" / "eval_set" / "public"
VM_MANIFEST_DIR = ROOT / "bench_data" / "manifests"
SHORT_VM_SET = "practical-ja-vm-latency-v1"
MODEL_REVISION = "0a363e9161cbc7ed1431c9597a8ceaf0c4f78fcf"
MODEL_HASHES = {
    "config.json": "b0253ea6c0d3bea6b1e19e91a02acfd3b53f4467362efcb5a3e6b16c9b3a9b7e",
    "model.bin": "e76620f83d5f5b69efd3d87e3dc180c1bd21df9fbebacfd4335e5e1efcc018da",
    "preprocessor_config.json": "7ccc62c6f2765af1f3b46c00c9b5894426835a05021c8b9c01eecb6dfb542711",
    "tokenizer.json": "297b13372ac43916285644fb9687add3cc62ee2a1adb60da3dc25cc94c1871fd",
    "vocabulary.json": "c69260f2ab26d659b7c398f9a2b2b48ed0df16c3b47d7326782fd9cba71690c1",
}
PROFILES = {
    "headless": ("headless",),
    "cuda": ("cuda",),
    "vm": ("vm",),
    "all": ("headless", "cuda", "vm"),
}
STAGES = {
    "mock-recognition": "headless",
    "streaming-fixture": "headless",
    "correction-fixtures": "headless",
    "cuda-smoke": "cuda",
    "cuda-holdout": "cuda",
    "vm-mock": "vm",
    "vm-real": "vm",
}
OUTSTANDING = {
    "product-target-and-fault-matrix": "#111",
    "three-second-product-latency-and-soak": "#111",
    "product-ux-and-correction": "#112",
    "independent-whispered-speech-evaluation": "#113",
}
SMOKE_GUARDS = ("focus-away", "cancel")
FULL_GUARDS = (*SMOKE_GUARDS, "selection-change", "minimize", WORKER_RECOVERY)


class Blocked(RuntimeError):
    """The stage lacks a prerequisite or reliable evidence."""


class CleanupUnconfirmed(Blocked):
    """A VM controller may still own the guest."""


def new_json(path, value):
    temporary = path.with_suffix(".tmp")
    with temporary.open("x", encoding="utf-8") as output:
        json.dump(value, output, ensure_ascii=False, allow_nan=False, indent=2)
        output.write("\n")
    temporary.rename(path)


def read_json(path):
    return json.loads(path.read_text(encoding="utf-8-sig"))


def sha(path):
    with path.open("rb") as source:
        return hashlib.file_digest(source, "sha256").hexdigest()


def git(*args):
    return subprocess.check_output(["git", *args], cwd=ROOT, text=True, timeout=15).strip()


def source_identity():
    files = set(git("ls-files", "-c", "-o", "--exclude-standard", "--",
                    "scripts", "asr_worker", "uv.lock", "pyproject.toml", "docs/eval_set").splitlines())
    paths = [ROOT / name for name in sorted(files)]
    hashes = {path.relative_to(ROOT).as_posix(): sha(path) if path.is_file() else None for path in paths}
    return {
        "commit": git("rev-parse", "HEAD"),
        "dirty": bool(git("status", "--porcelain")),
        "source_files_sha256": hashes,
        "host": platform.platform(),
        "python": platform.python_version(),
        "packages": installed_versions(),
    }


def installed_versions():
    versions = {}
    for name in ("faster-whisper", "ctranslate2", "av", "numpy", "psutil"):
        try:
            versions[name] = version(name)
        except PackageNotFoundError:
            versions[name] = None
    return versions


def offline_env(**overrides):
    env = {key: value for key, value in os.environ.items()
           if not key.upper().startswith(("ASR_", "CT2_", "OMP_", "MKL_", "HF_HUB_"))}
    return {**env, "ASR_OFFLINE_ONLY": "1", "HF_HUB_OFFLINE": "1", "TRANSFORMERS_OFFLINE": "1",
            "PYTHONIOENCODING": "utf-8", **overrides}


def invoke(directory, name, command, *, env=None, timeout=180):
    with (directory / f"{name}.log").open("xb") as log:
        completed = subprocess.run([str(part) for part in command], cwd=ROOT,
                                   stdin=subprocess.DEVNULL, stdout=log, stderr=subprocess.STDOUT,
                                   env=env, timeout=timeout, check=False)
    if completed.returncode in (130, -2, 0xC000013A, -1073741510):
        raise KeyboardInterrupt
    return completed.returncode


def invoke_vm(directory, command, *, timeout):
    marker = directory / "cancel.requested"
    deadline = time.monotonic() + timeout
    reason = None
    interrupted = False

    def request_interrupt(signum, frame):
        nonlocal interrupted
        interrupted = True

    previous_handler = signal.signal(signal.SIGINT, request_interrupt)
    try:
        with (directory / "vm.log").open("xb") as log:
            # Keep the controller independent of console termination so it can clean up.
            process = subprocess.Popen([str(part) for part in [*command, "-CancellationPath", marker]],
                                       cwd=ROOT, stdin=subprocess.DEVNULL, stdout=log, stderr=subprocess.STDOUT,
                                       creationflags=subprocess.CREATE_NO_WINDOW)
            while True:
                now = time.monotonic()
                if reason is None and (interrupted or now >= deadline):
                    reason = "interrupt" if interrupted else "deadline"
                    try:
                        with marker.open("a", encoding="ascii") as request:
                            request.write(f"{process.pid}\n")
                    except OSError as exc:
                        raise CleanupUnconfirmed(f"Cannot request cancellation for VM controller PID {process.pid}") from exc
                    deadline = now + 90
                try:
                    code = process.wait(timeout=1)
                    break
                except subprocess.TimeoutExpired:
                    if reason is not None and time.monotonic() >= deadline:
                        raise CleanupUnconfirmed(f"VM controller PID {process.pid} did not acknowledge cancellation")
    finally:
        signal.signal(signal.SIGINT, previous_handler)
    if reason or interrupted:
        host_report = directory / "vm" / "host.json"
        try:
            host = read_json(host_report)
        except (OSError, ValueError) as exc:
            raise CleanupUnconfirmed(f"VM controller PID {process.pid} exited without a cleanup report") from exc
        if error := host.get("cleanup_error"):
            raise CleanupUnconfirmed(f"VM cancellation cleanup failed: {error}")
        if interrupted:
            raise KeyboardInterrupt
        raise Blocked("VM host controller exceeded its overall deadline; cleanup completed")
    return code


def python_command(script, *args):
    return [sys.executable, ROOT / "scripts" / script, *args]


def validate_manifest(dataset, name):
    path = dataset / f"{name}.json"
    lock = PUBLIC / f"{name}.lock.json"
    locked = read_document(lock, LOCK_SCHEMA)
    manifest = load_manifest(path)
    if manifest["name"] != name or [item["id"] for item in manifest["items"]] != [
        item["id"] for item in locked["items"]
    ]:
        raise Blocked("manifest does not match the pinned item names and order")
    for item, expected in zip(manifest["items"], locked["items"], strict=True):
        if (any(item.get(key) != expected.get(key) for key in expected
                if key != "reference_sha256")
                or hashlib.sha256(item["reference"].encode("utf-8")).hexdigest() != expected["reference_sha256"]
                or sha(Path(item["audio_path"])) != expected["audio_sha256"]):
            raise Blocked(f"pinned audio or reference mismatch for {item['id']}")
    return {"manifest_sha256": sha(path), "lock_sha256": sha(lock), "items": len(locked["items"])}


def short_fixture(manifest_dir):
    verified = validate_manifest(manifest_dir, SHORT_VM_SET)
    fixture, = load_manifest(manifest_dir / (SHORT_VM_SET + ".json"))["items"]
    holdout = read_document(PUBLIC / "practical-ja-holdout-v1.lock.json", LOCK_SCHEMA)
    if any(item["id"] == fixture["id"] or (
        item["source"] == fixture["source"] and item["speaker"] == fixture["speaker"]
    ) for item in holdout["items"]):
        raise Blocked("the short fixture overlaps the holdout")
    try:
        with wave.open(fixture["audio_path"], "rb") as audio:
            duration_ms = audio.getnframes() * 1000 / audio.getframerate()
    except (OSError, EOFError, wave.Error) as exc:
        raise Blocked("the pinned short fixture is not a readable WAV") from exc
    if not 2750 <= duration_ms <= 3250:
        raise Blocked("the pinned short fixture duration is out of range")
    return fixture, {**verified, "audio_duration_ms": duration_ms}


def inputs_for(args, stage):
    kind = STAGES[stage]
    if kind == "headless":
        return {}
    if kind == "vm":
        if sys.platform != "win32" or not shutil.which("pwsh"):
            raise Blocked("the VM stage requires a Windows Hyper-V host with PowerShell 7")
        if args.app_binary is None or args.app_revision is None or args.credential is None:
            raise Blocked("provide --app-binary, --app-revision and --credential")
        if not args.credential.is_file():
            raise Blocked("the PowerShell Direct credential file is unavailable")
        vm_id = subprocess.check_output([
            "pwsh", "-NoProfile", "-NonInteractive", "-Command",
            "(Get-VM -Name 'ASR-Eval' -ErrorAction Stop).Id.ToString()",
        ], cwd=ROOT, stdin=subprocess.DEVNULL, stderr=subprocess.DEVNULL, text=True, timeout=30).strip()
        if not re.fullmatch(r"[0-9a-fA-F-]{36}", vm_id):
            raise Blocked("cannot verify the identity of the ASR-Eval VM")
        inputs = {"vm_id": vm_id.lower(), "operator_supplied_app_revision": args.app_revision,
                  "app_sha256": sha(args.app_binary)}
        if stage == "vm-mock":
            inputs["mock_on_real_profile"] = args.vm_mode == "both"
        if stage == "vm-real":
            # The VM launcher verifies its guest model and fixed real fixture itself.
            inputs["model_revision"] = MODEL_REVISION
            inputs["fixture"] = args.vm_fixture
            if args.vm_repeats is not None:
                inputs["requested_real_takes"] = args.vm_repeats
            if args.vm_fixture == "short":
                fixture, verified = short_fixture(args.manifest_dir)
                inputs.update(verified)
            else:
                items = load_manifest(args.manifest_dir / "practical-ja-smoke-v1.json")["items"]
                fixture = next((item for item in items if item["id"] == "fleurs-ja_jp-test-1100824364670689611"), None)
            if fixture is None:
                raise Blocked("the VM's pinned real fixture is unavailable")
            inputs["item_id"] = fixture["id"]
            inputs["fixture_sha256"] = sha(Path(fixture["audio_path"]))
            inputs["reference_sha256"] = hashlib.sha256(fixture["reference"].encode("utf-8")).hexdigest()
        return inputs
    if args.model_dir is None:
        raise Blocked("provide --model-dir for the pinned CUDA model")
    for name, digest in MODEL_HASHES.items():
        if sha(args.model_dir / name) != digest:
            raise Blocked(f"pinned model file hash mismatch: {name}")
    name = "practical-ja-" + stage.removeprefix("cuda-") + "-v1"
    return {
        **validate_manifest(args.manifest_dir, name), "fixture": name,
        "model_revision": MODEL_REVISION, "model_files_sha256": MODEL_HASHES,
        "gpu": gpu_inventory(), "cuda_visible_devices": os.environ.get("CUDA_VISIBLE_DEVICES"),
    }


def gpu_inventory():
    executable = shutil.which("nvidia-smi")
    if not executable:
        return None
    try:
        return subprocess.check_output(
            [executable, "--query-gpu=name,uuid,driver_version", "--format=csv,noheader"],
            text=True, timeout=10, stderr=subprocess.DEVNULL,
        ).splitlines()
    except (OSError, subprocess.SubprocessError):
        return None


def run_headless(stage, directory):
    artifact = directory / "data.json"
    fixture = directory / "manifest.json"
    if stage == "streaming-fixture":
        wav = directory / "streaming-test.wav"
        write_sine_wav(wav, seconds=.3, sample_rate=16000, freq_hz=400)
        new_json(fixture, {"schema": "asr-bench-manifest/1", "name": "synthetic-streaming", "sources": [],
                           "items": [{"id": wav.stem, "audio_path": wav.name, "reference": "mock",
                                      "language": "ja-JP", "source": "synthetic"}]})
    commands = {
        "mock-recognition": python_command("bench_asr.py", "--smoke", "--repeat", "1", "--cold-runs", "0",
                                           "--json", "--output", artifact),
        "streaming-fixture": python_command("bench_streaming.py", "--manifest", fixture, "--output", artifact,
                                            "--adapter-cmd", *python_command("bench_streaming.py", "--mock-adapter")),
        "correction-fixtures": python_command("eval_correction.py", "run", "--provider", "fixtures", "--out", artifact),
    }
    code = invoke(directory, stage, commands[stage], env=offline_env())
    if code != 0:
        return "FAIL", {}, f"fixture command exited with code {code}"
    result = read_json(artifact)
    if stage in ("mock-recognition", "streaming-fixture"):
        samples = result["warm"]["samples"]
        expected = "mock transcription" if stage == "mock-recognition" else "mock"
        if len(samples) != 1 or samples[0]["text"] != expected:
            return "FAIL", {}, "fixture transcription does not match"
    else:
        expected = {(case["id"], fixture["id"]): sorted(set(fixture["expect_failures"]))
                    for case in read_json(CORRECTION_DATASET)["cases"]
                    for fixture in [{"id": "reference", "expect_failures": []}, *case["fixtures"]]}
        actual = {(row["case"], row["output_id"]): row["score"]["failures"] for row in result["rows"]}
        if (result["fixture_mismatches"] or result["provider"] != "fixtures"
                or len(result["rows"]) != len(expected) or actual != expected):
            return "FAIL", {}, "correction fixture evidence does not match"
    return "PASS", {"evidence": "data.json"}, None


def run_cuda(args, directory, inputs):
    manifest = args.manifest_dir / (inputs["fixture"] + ".json")
    bench = directory / "bench.json"
    score = directory / "score.json"
    repeat = 1 if args.suite == "smoke" else 2
    code = invoke(directory, "bench", python_command(
        "bench_asr.py", "--backend", "faster-whisper", "--whisper-profile", "standard",
        "--manifest", manifest, "--repeat", repeat, "--cold-runs", "0", "--quantization", "4bit",
        "--model-revision", MODEL_REVISION, "--record-device", "--record-resources", "--json", "--output", bench,
    ), env=offline_env(ASR_DEVICE="cuda", ASR_MODEL_ID=str(args.model_dir)), timeout=1800)
    if code != 0:
        return "FAIL", {}, f"CUDA benchmark exited with code {code} after preflight"
    result = read_json(bench)
    config = result["config"]
    expected_config = {"backend": "faster-whisper", "whisper_decode_profile": "standard",
                       "device_policy": "cuda", "quantization": "4bit", "request_shape": "production",
                       "model": f"faster-whisper:{args.model_dir}", "model_revision": MODEL_REVISION}
    if any(config.get(key) != value for key, value in expected_config.items()):
        return "BLOCKED", {}, "reported CUDA benchmark configuration does not match the plan"
    samples = result["warm"]["samples"]
    expected = Counter({item["audio_path"]: repeat for item in load_manifest(manifest)["items"]})
    actual = Counter(str(Path(sample["audio"]).resolve()) for sample in samples)
    if (len(expected) != inputs["items"] or actual != expected
            or any((sample["device"], sample["compute_type"]) != ("cuda", "int8_float16")
                                  for sample in samples)):
        return "BLOCKED", {}, "measured backend, device or sample coverage does not match the plan"
    if invoke(directory, "score", python_command(
        "score_asr.py", "--bench", bench, "--manifest", manifest, "--json", "--output", score,
        "--csv", directory / "score.csv",
    ), env=offline_env()) != 0:
        return "BLOCKED", {}, "recognition scoring did not complete"
    scored = read_json(score)
    if scored["overall"]["files"] != inputs["items"] or scored["missing_items"] or scored["unmatched_audio"]:
        return "BLOCKED", {}, "the scorer did not cover the full fixed set"
    return "PASS", cuda_metrics(result, scored), None


def cuda_metrics(bench, score):
    overall = score["overall"]
    return {
        "recognition": {"utterances": overall["files"], "requests": len(bench["warm"]["samples"]),
                        "raw_cer": overall["raw"]["cer"], "normalized_cer": overall["normalized"]["cer"],
                        "worker_round_trip_ms": bench["warm"]["wall_ms"]},
        "evidence": "score.json",
    }


def vm_metrics(guest, requested, *, real=None, external=False):
    cases = guest.get("cases", [])
    if real is None:
        real = any(case["scenario"] in REAL_RECEIVERS for case in cases)
    insertion = []
    numbers = []
    guards = {}
    recognition = []
    for case in cases:
        if case["scenario"] in FULL_GUARDS:
            guards[case["scenario"]] = case["status"]
            continue
        for index, trial in enumerate(case.get("trials", [case]), start=1):
            acceptance = trial.get("acceptance", {}).get("results", [{}])[0]
            insertion.append({
                "scenario": case["scenario"], "status": trial["status"], "reason": trial.get("reason"),
                "audio_duration_ms": acceptance.get("audio_duration_ms"),
                "app_outcome": acceptance.get("app_outcome"),
                "ready_ms": (acceptance.get("app_spans_ms") or {}).get("record_ready"),
                "stop_to_observed_ms": acceptance.get("stop_to_observed_insertion_ms"),
            })
            if proof := trial.get("real_worker_proof"):
                raw, normalized = proof.get("cer_raw"), proof.get("cer_normalized")
                if all(isinstance(score, dict) and isinstance(score.get("cer"), (int, float))
                       for score in (raw, normalized)):
                    recognition.append({"raw_cer": raw, "normalized_cer": normalized})
            if (case["scenario"] == REAL_NOTEPAD and index > 1 and trial["status"] == "PASS"
                    and (observed := acceptance.get("stop_to_observed_insertion_ms")) is not None):
                numbers.append(observed)
    confirmed = sum(take["status"] == "PASS" for take in insertion)
    warm = {}
    if real:
        warm = {"warm_stop_planned": max(0, requested - 1), "warm_stop_samples": len(numbers),
                "warm_stop_median_ms": statistics.median(numbers) if numbers else None,
                "warm_stop_p95_ms": statistics.quantiles(numbers, n=20, method="inclusive")[-1]
                if len(numbers) > 1 else None}
    targets = {REAL_NOTEPAD: requested, **dict.fromkeys(REAL_EXTERNAL, REAL_EXTERNAL_REPEATS)} if external else {}
    passed_by_target = Counter(take["scenario"] for take in insertion if take["status"] == "PASS")
    return {
        "insertion": {"planned": sum(targets.values()) if external else requested,
                      "reported": len(insertion), "confirmed": confirmed, "trials": insertion,
                      "by_target": {name: {"planned": count, "confirmed": passed_by_target[name]}
                                    for name, count in targets.items()}, **warm},
        "guard_outcomes": guards,
        "recognition": recognition,
        "evidence": "vm/host.json",
    }


def planned_takes(stage, suite, fixture="original", override=None):
    if stage != "vm-real":
        return len(MOCK_RECEIVERS) if suite == "full" else 2
    if override is not None:
        return override
    if suite == "smoke":
        return 1
    return 10 if fixture == "short" else 6


def run_vm(args, stage, directory, inputs):
    repeats = planned_takes(stage, args.suite, args.vm_fixture, inputs.get("requested_real_takes"))
    real = stage == "vm-real"
    external = real and args.suite == "full"
    flags = (["-RealModelNotepad", "-RealRepeats", str(repeats), "-RealFixture", args.vm_fixture]
             if real else ["-ExpandedMatrix" if args.suite == "full" else "-IncludeNotepad"])
    if external:
        flags.append("-RealApps")
    total_takes = repeats + len(REAL_EXTERNAL) * REAL_EXTERNAL_REPEATS if external else repeats
    if inputs.get("mock_on_real_profile"):
        flags.append("-MockOnRealProfile")
    vm = directory / "vm"
    code = invoke_vm(directory, [
        "pwsh", "-NoProfile", "-File", ROOT / "scripts" / "run_vm_insertion_smoke.ps1",
        "-AppBinary", args.app_binary, "-AppRevision", args.app_revision,
        "-ManagementCredentialPath", args.credential, "-OutputDirectory", vm,
        "-ManifestDirectory", args.manifest_dir, *flags,
    ], timeout=190 + 160 * total_takes if real else (1065 if args.suite == "full" else 500))
    try:
        host = read_json(vm / "host.json")
    except (OSError, ValueError) as exc:
        raise CleanupUnconfirmed("VM controller exited without a readable cleanup report") from exc
    if error := host.get("cleanup_error"):
        raise CleanupUnconfirmed(f"VM controller cleanup failed: {error}")
    status = {0: "PASS", 1: "FAIL", 2: "BLOCKED"}.get(code)
    if status is None or host["status"] != status:
        raise Blocked("VM exit code and host report are inconsistent")
    if host.get("vm_id", "").lower() != inputs["vm_id"]:
        raise Blocked("VM host report identity does not match preflight")
    guest = read_json(vm / "result.json") if (vm / "result.json").is_file() else {}
    if guest and not real and guest.get("mock_on_real_profile") != inputs["mock_on_real_profile"]:
        raise Blocked("VM result does not match the selected mock worker mode")
    metrics = vm_metrics(guest, repeats, real=real, external=external)
    if real and (guest or host.get("input_item_id")):
        if (host.get("input_item_id") != inputs["item_id"]
                or host.get("input_audio_sha256", "").lower() != inputs["fixture_sha256"]
                or host.get("input_reference_sha256", "").lower() != inputs["reference_sha256"]):
            raise Blocked("VM result does not match the pinned fixture")
        if args.vm_fixture == "short" and any(
            abs(trial["audio_duration_ms"] - inputs["audio_duration_ms"]) > .1
            for trial in metrics["insertion"]["trials"] if trial["audio_duration_ms"] is not None
        ):
            raise Blocked("measured short fixture duration is out of range")
    if status == "PASS" and (guest.get("status") != "PASS"
                             or metrics["insertion"]["confirmed"] != total_takes
                             or any(target["confirmed"] != target["planned"]
                                    for target in metrics["insertion"]["by_target"].values())
                             or (stage == "vm-mock" and metrics["guard_outcomes"] != dict.fromkeys(
                                 FULL_GUARDS if args.suite == "full" else SMOKE_GUARDS, "PASS"))
                             or (real and len(metrics["recognition"]) != total_takes)):
        raise Blocked("VM success report does not contain all expected evidence")
    return status, metrics, host.get("reason")


def execute(args, root, stage, inputs):
    directory = root / stage / uuid.uuid4().hex
    directory.mkdir(parents=True)
    if args.verbose:
        print(f"{stage}: starting", flush=True)
    evidence = {}
    unsafe_vm = False
    try:
        kind = STAGES[stage]
        if kind == "headless":
            status, metrics, reason = run_headless(stage, directory)
        elif kind == "cuda":
            status, metrics, reason = run_cuda(args, directory, inputs)
        else:
            status, metrics, reason = run_vm(args, stage, directory, inputs)
        if status == "PASS":
            evidence = evidence_hashes(stage, directory)
    except KeyboardInterrupt:
        status, metrics, reason = "INTERRUPTED", {}, "operator interrupted the stage"
    except CleanupUnconfirmed as exc:
        status, metrics, reason = "BLOCKED", {}, str(exc)
        unsafe_vm = True
    except (Blocked, OSError, ValueError, KeyError, TypeError, IndexError, DataError, subprocess.SubprocessError) as exc:
        status, metrics, reason = "BLOCKED", {}, str(exc)
    result = {"status": status, "metrics": metrics, "reason": reason, "inputs": inputs, "evidence_sha256": evidence,
              "directory": directory.relative_to(root).as_posix()}
    if unsafe_vm:
        result["vm_cleanup_unconfirmed"] = True
    new_json(directory / "stage.json", result)
    if args.verbose:
        print(f"{stage}: {status}", flush=True)
    return result


def evidence_hashes(stage, directory):
    files = {
        "headless": ("data.json",),
        "cuda": ("bench.json", "score.json", "score.csv"),
        "vm": ("vm/host.json", "vm/result.json"),
    }[STAGES[stage]]
    return {name: sha(directory / name) for name in files}


def matching_pass(root, stage, inputs, suite):
    for directory in sorted((root / stage).iterdir()) if (root / stage).is_dir() else ():
        path = directory / "stage.json"
        if path.is_file():
            record = read_json(path)
            if record["status"] == "PASS" and record["inputs"] == inputs:
                try:
                    matches = evidence_hashes(stage, directory) == record.get("evidence_sha256")
                except FileNotFoundError:
                    matches = False
                if matches:
                    kind = STAGES[stage]
                    if kind == "vm":
                        metrics = vm_metrics(read_json(directory / "vm/result.json"),
                                             planned_takes(stage, suite, inputs.get("fixture", "original"),
                                                           inputs.get("requested_real_takes")),
                                             real=stage == "vm-real",
                                             external=stage == "vm-real" and suite == "full")
                    elif kind == "cuda":
                        metrics = cuda_metrics(read_json(directory / "bench.json"), read_json(directory / "score.json"))
                    else:
                        metrics = {"evidence": "data.json"}
                    if record["metrics"] == metrics:
                        return {**record, "directory": directory.relative_to(root).as_posix()}
    return None


def planned(args, stage):
    return (STAGES[stage] in PROFILES[args.profile]
            and (args.suite == "full" or stage != "cuda-holdout")
            and (STAGES[stage] != "vm" or args.vm_mode in ("both", stage.removeprefix("vm-"))))


def run_plan(args, root):
    results = {}
    interrupted = False
    unsafe_vm = False
    for stage in STAGES:
        if not planned(args, stage):
            results[stage] = {"status": "NOT_SELECTED"}
        elif interrupted:
            results[stage] = {"status": "NOT_RUN", "reason": "a prior stage was interrupted"}
        elif unsafe_vm and STAGES[stage] == "vm":
            results[stage] = {"status": "NOT_RUN", "reason": "guest cleanup was not confirmed"}
        else:
            try:
                inputs = inputs_for(args, stage)
                saved = matching_pass(root, stage, inputs, args.suite) if args.resume else None
                results[stage] = {**saved, "reused": True} if saved else execute(args, root, stage, inputs)
            except KeyboardInterrupt:
                results[stage] = {"status": "INTERRUPTED", "reason": "operator interrupted preflight"}
            except (Blocked, OSError, ValueError, KeyError, TypeError, DataError, subprocess.SubprocessError) as exc:
                results[stage] = {"status": "BLOCKED", "reason": str(exc)}
            interrupted = results[stage]["status"] == "INTERRUPTED"
            unsafe_vm |= results[stage].get("vm_cleanup_unconfirmed", False)
    statuses = {result["status"] for result in results.values()}
    return results, next((state for state in ("INTERRUPTED", "FAIL", "BLOCKED") if state in statuses), "PASS")


def markdown(report):
    selected = {name: row for name, row in report["stages"].items() if row["status"] != "NOT_SELECTED"}
    passed = sum(row["status"] == "PASS" for row in selected.values())
    lines = ["# Evaluation", "", f"**{report['status']} · {passed}/{len(selected)} stages passed**", "",
             "| Stage | Status | Key measurements |", "| --- | --- | --- |"]
    for stage, row in selected.items():
        metrics = row.get("metrics", {})
        detail = []
        if "insertion" in metrics:
            insertion = metrics["insertion"]
            detail.append(f"confirmed insertion {insertion['confirmed']}/{insertion['planned']}")
            targets = insertion.get("by_target", {})
            if targets:
                names = {"real-notepad": "Notepad", "real-vscode": "VS Code", "real-chrome": "Chrome"}
                detail.append(", ".join(f"{names[name]} {target['confirmed']}/{target['planned']}"
                                        for name, target in targets.items()))
            for name, state in metrics["guard_outcomes"].items():
                label = "focus-away + minimize" if name == "minimize" else name
                detail.append(f"{label} {state}")
            detail.extend(f"{trial['scenario']} {trial['status']}" + (
                f" ({trial['reason']})" if trial["reason"] else "")
                for trial in insertion["trials"] if trial["status"] != "PASS")
            unverified = list(dict.fromkeys(trial["scenario"] for trial in insertion["trials"]
                                            if trial["status"] == "PASS" and trial.get("app_outcome") == "paste_unverified"))
            if unverified:
                detail.append("app reported paste_unverified: " + ", ".join(unverified))
            if insertion.get("warm_stop_planned"):
                p50, p95 = (insertion[key] for key in ("warm_stop_median_ms", "warm_stop_p95_ms"))
                samples = f"{insertion['warm_stop_samples']}/{insertion['warm_stop_planned']}"
                values = "/".join(f"{value:.0f}" if value is not None else "—" for value in (p50, p95))
                label = "Notepad warm" if targets else "warm"
                detail.append(f"{label} stop-to-insertion p50/p95 {values} ms (n={samples})")
            elif insertion["trials"]:
                first = insertion["trials"][0]["stop_to_observed_ms"]
                if first is not None:
                    detail.append(f"first stop-to-insertion {first:.0f} ms")
        recognition = metrics.get("recognition")
        if isinstance(recognition, dict):
            detail.append(f"recognition {recognition['utterances']} utterances, "
                          f"raw CER {recognition['raw_cer']['cer']:.2%}, "
                          f"normalized CER {recognition['normalized_cer']['cer']:.2%}")
            if round_trip := recognition.get("worker_round_trip_ms"):
                detail.append(f"worker p50/p95 {round_trip['p50']:.0f}/{round_trip['p95']:.0f} ms")
        elif recognition:
            raw = statistics.median(row["raw_cer"]["cer"] for row in recognition)
            normalized = statistics.median(row["normalized_cer"]["cer"] for row in recognition)
            detail.append(f"trial median raw/normalized CER {raw:.2%}/{normalized:.2%} (n={len(recognition)})")
        if reason := row.get("reason"):
            detail.append(str(reason).replace("|", "/").replace("\n", " "))
        if row.get("reused"):
            detail.append("reused")
        if evidence := metrics.get("evidence"):
            detail.append(f"[evidence](../{row['directory']}/{evidence})")
        lines.append(f"| {stage} | {row['status']} | {', '.join(detail)} |")
    lines.extend(("", "[Machine-readable results](summary.json)"))
    return "\n".join(lines) + "\n"


def parse_args(argv):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--suite", choices=("smoke", "full"), default="smoke")
    parser.add_argument("--profile", choices=PROFILES, default="headless")
    parser.add_argument("--vm-mode", choices=("both", "mock", "real"), default="both")
    parser.add_argument("--vm-fixture", choices=("original", "short"), default="original")
    parser.add_argument("--vm-repeats", type=int, choices=range(2, 81), metavar="2–80",
                        help="repeat the real-model Notepad VM case in the same session")
    parser.add_argument("--output", type=lambda value: Path(value).resolve(),
                        help="result directory (default: a new run under .ai/evaluation)")
    parser.add_argument("--resume", action="store_true")
    parser.add_argument("--verbose", action="store_true", help="show live per-stage progress")
    parser.add_argument("--settings", type=lambda value: Path(value).resolve(),
                        help="local paths for CUDA and VM (default: .ai/evaluation.local.json if present)")
    for name in ("manifest-dir", "model-dir", "app-binary", "credential"):
        parser.add_argument("--" + name, type=lambda value: Path(value).resolve())
    parser.add_argument("--app-revision")
    args = parser.parse_args(argv)
    default_settings = ROOT / ".ai" / "evaluation.local.json"
    settings_path = args.settings
    if settings_path is None and args.profile != "headless" and default_settings.is_file():
        settings_path = default_settings
    if settings_path is not None:
        allowed = {"manifest_dir", "model_dir", "app_binary", "credential", "app_revision"}
        try:
            settings = read_json(settings_path)
            if (not isinstance(settings, dict) or settings.get("schema") != "asr-evaluation-settings/1"
                    or settings.keys() - allowed - {"schema"}):
                raise ValueError("invalid local settings schema or keys")
            for key in allowed.intersection(settings):
                value = settings[key]
                if not isinstance(value, str) or not value.strip():
                    raise ValueError(f"{key} must be a nonempty string")
                if getattr(args, key) is None:
                    setattr(args, key, value if key == "app_revision" else (ROOT / value).resolve())
        except (OSError, ValueError) as exc:
            parser.error(f"could not load local settings: {exc}")
    args.manifest_dir = args.manifest_dir or VM_MANIFEST_DIR
    if args.resume and args.output is None:
        parser.error("--resume requires --output for the existing run")
    if ((args.vm_fixture == "short" or args.vm_repeats is not None)
            and ("vm" not in PROFILES[args.profile] or args.vm_mode == "mock")):
        parser.error("--vm-fixture short and --vm-repeats require a real-model VM stage")
    if args.app_revision is not None and not re.fullmatch("[0-9a-f]{40}", args.app_revision):
        parser.error("--app-revision must be a 40-character Git revision")
    return args


def main(argv=None):
    args = parse_args(argv)
    root = args.output or ROOT / ".ai" / "evaluation" / uuid.uuid4().hex
    try:
        if not args.resume:
            root.mkdir(parents=True)
        elif not (root / "run.json").is_file():
            raise Blocked("--resume requires an initialized evaluation run")
        configuration = {key: str(value) for key, value in vars(args).items()
                         if key not in {"resume", "output", "verbose", "settings"}}
        identity = {"schema": "asr-evaluation-plan/1", "suite": args.suite, "profile": args.profile,
                    "configuration_sha256": hashlib.sha256(json.dumps(configuration, sort_keys=True).encode()).hexdigest(),
                    "source": source_identity()}
        with closing(sqlite3.connect(root / "lease.sqlite3", timeout=0)) as lease:
            lease.execute("BEGIN IMMEDIATE")
            if args.resume:
                previous = read_json(root / "run.json")
                # Dirty is descriptive. Unrelated working-tree changes need not invalidate results.
                previous["source"]["dirty"] = identity["source"]["dirty"]
                if previous != identity:
                    raise Blocked("resume requires the same evaluation configuration and source")
            else:
                new_json(root / "run.json", identity)
            results, status = run_plan(args, root)
            directory = root / ("report-" + uuid.uuid4().hex)
            directory.mkdir()
            report = {"schema": "asr-evaluation-report/1", "status": status, "stages": results,
                      "plan": identity, "pending_coverage": OUTSTANDING}
            rendered = markdown(report)
            new_json(directory / "summary.json", report)
            with (directory / "summary.md").open("x", encoding="utf-8") as output:
                output.write(rendered)
            print(f"{status}  {directory / 'summary.md'}")
            return {"PASS": 0, "FAIL": 1, "BLOCKED": 2, "INTERRUPTED": 130}[status]
    except KeyboardInterrupt:
        return 130
    except (Blocked, OSError, ValueError, KeyError, TypeError, sqlite3.Error, subprocess.SubprocessError) as exc:
        print(f"BLOCKED: {exc}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
