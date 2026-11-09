#!/usr/bin/env python3
"""Fetch licensed public corpora and build frozen, offline ASR benchmark sets."""

from __future__ import annotations

import argparse
import csv
import contextlib
import hashlib
import http.client
import io
import json
import math
import os
import re
import shutil
import sys
import tarfile
import tempfile
import urllib.error
import urllib.request
import wave
import zipfile
from collections import defaultdict
from dataclasses import dataclass
from pathlib import Path, PurePosixPath
from typing import Any, Iterable


MANIFEST_SCHEMA = "asr-bench-manifest/1"
LOCK_SCHEMA = "asr-bench-lock/1"
FLEURS_REVISION = "70bb2e84b976b7e960aa89f1c648e09c59f894dd"
DEFAULT_SEED = 20261103
DEFAULT_CACHE = Path(__file__).resolve().parents[1] / "bench_data"
PROFILE_COUNTS = {"smoke": 30, "dev": 100, "holdout": 100}
SOURCE_FIELDS = ("source", "dataset_id", "config", "split", "revision", "license", "url", "archive_sha256")
LOCK_ITEM_FIELDS = ("id", "source", "split", "pool", "dataset_id", "revision", "locale", "item_id", "group",
                    "speaker", "audio_sha256", "source_audio_sha256", "license")
SELECTION_FIELDS = ("profile", "seed", "count_per_source", "pool", "holdout_percent", "strata")


class DataError(RuntimeError):
    """Raised for a dataset, cache, or output failure."""


@dataclass(frozen=True)
class Source:
    source: str
    config: str
    dataset_id: str
    split: str
    revision: str
    language: str
    url: str
    size_hint: str
    kind: str
    tsv_url: str | None = None
    license: str = "CC-BY-4.0"

    @property
    def key(self) -> str:
        return f"{self.source}:{self.config}"

    @property
    def locale(self) -> str:
        return self.language.lower().replace("-", "_")

    def directory(self, cache: Path) -> Path:
        return cache / "downloads" / self.source / self.config

    def archive(self, cache: Path) -> Path:
        return self.directory(cache) / Path(self.url).name

    def metadata(self, archive_sha256: str) -> dict[str, Any]:
        return {
            "source": self.source, "dataset_id": self.dataset_id, "config": self.config,
            "split": self.split, "revision": self.revision, "license": self.license,
            "url": self.url, "archive_sha256": archive_sha256,
        }


# Keep the supported revisions, URLs, locales and download estimates together.
SOURCES: dict[str, Source] = {}
for _config, _language in (("ja_jp", "ja-JP"), ("en_us", "en-US")):
    _base = f"https://huggingface.co/datasets/google/fleurs/resolve/{FLEURS_REVISION}/data/{_config}"
    SOURCES[f"fleurs:{_config}"] = Source(
        "fleurs", _config, "google/fleurs", "test", FLEURS_REVISION, _language,
        f"{_base}/audio/test.tar.gz", "approximately 449 MB", kind="fleurs", tsv_url=f"{_base}/test.tsv",
    )
for _split, _size in (("test-clean", "approximately 347 MB"), ("test-other", "approximately 328 MB")):
    SOURCES[f"librispeech:{_split}"] = Source(
        "librispeech", _split, "openslr/12", _split, "openslr-12", "en-US",
        f"https://www.openslr.org/resources/12/{_split}.tar.gz", _size, kind="librispeech",
    )
DEFAULT_SOURCES = ("fleurs:ja_jp",)
SOURCES["jvnv:ver1"] = Source(
    "jvnv", "ver1", "jvnv", "all", "ver1-20231021", "ja-JP",
    "https://ss-takashi.sakura.ne.jp/corpus/jvnv/jvnv_ver1.zip",
    "approximately 1.78 GB", kind="jvnv", license="CC-BY-SA-4.0",
)


@dataclass(frozen=True)
class Candidate:
    source: str
    config: str
    item_id: str
    member: str
    reference: str
    duration: float
    group: str
    speaker: str | None = None

    @property
    def id(self) -> str:
        spec = SOURCES[f"{self.source}:{self.config}"]
        prefix = f"{self.source}-{self.config}"
        if spec.split != self.config:
            prefix += f"-{spec.split}"
        return f"{prefix}-{self.item_id}"


def safe_name(value: str) -> str:
    if not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9_.-]*", value) or value.endswith("."):
        raise DataError(f"unsafe name: {value!r}; use letters, digits, dots, underscores or hyphens")
    if value.split(".")[0].upper() in {"CON", "PRN", "AUX", "NUL", *(f"COM{i}" for i in range(10)), *(f"LPT{i}" for i in range(10))}:
        raise DataError(f"reserved Windows filename: {value!r}")
    return value


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def parse_fleurs_tsv(text: str, config: str, source: str = "fleurs") -> list[Candidate]:
    """Parse the headerless FLEURS TSV, preserving raw transcription exactly."""
    items = []
    seen = set()
    skipped = 0
    for line, row in enumerate(csv.reader(io.StringIO(text), delimiter="\t", quoting=csv.QUOTE_NONE), 1):
        if not row:
            continue
        if len(row) != 7:
            raise DataError(f"FLEURS TSV line {line}: expected 7 columns")
        group, filename, reference, _, _, sample_count, _ = row
        if PurePosixPath(filename).name != filename or "\\" in filename or not filename.endswith(".wav"):
            raise DataError(f"FLEURS TSV line {line}: invalid audio filename")
        item_id = safe_name(filename[:-4])
        try:
            samples = int(sample_count)
        except ValueError as exc:
            raise DataError(f"FLEURS TSV line {line}: invalid num_samples") from exc
        if samples <= 0 or not group or item_id in seen:
            raise DataError(f"FLEURS TSV line {line}: empty/duplicate item or invalid num_samples")
        seen.add(item_id)
        if not reference.strip():
            skipped += 1
            continue
        items.append(Candidate(source, config, item_id, f"test/{filename}", reference, samples / 16000, group))
    if skipped:
        print(f"[bench_data] warning: FLEURS {config}: skipped {skipped} row(s) with empty raw_transcription", file=sys.stderr)
    return items


def parse_transcripts(text: str) -> dict[str, str]:
    """Parse LibriSpeech trans.txt without normalizing the reference text."""
    result = {}
    for line, value in enumerate(text.splitlines(), 1):
        if not value.strip():
            continue
        match = re.fullmatch(r"(\d+-\d+-\d+)[ \t](.+)", value)
        if match is None or not match[2].strip() or match[1] in result:
            raise DataError(f"LibriSpeech transcript line {line}: invalid or duplicate utterance")
        result[match[1]] = match[2]
    return result


def assign_pool(source: str, group: str, holdout_percent: int = 50) -> str:
    if not 0 <= holdout_percent <= 100:
        raise DataError("holdout_percent must be between 0 and 100")
    value = int(hashlib.sha256(f"{source}:{group}".encode("utf-8")).hexdigest(), 16)
    return "holdout" if value % 100 < holdout_percent else "dev"


def dedupe_groups(items: Iterable[Candidate]) -> list[Candidate]:
    """Keep the lexicographically first recording of each FLEURS sentence."""
    groups = {}
    for item in sorted(items, key=lambda item: item.id):
        groups.setdefault((item.source, item.config, item.group), item)
    return list(groups.values())


def seeded_hash(seed: int, key: str) -> str:
    return hashlib.sha256(f"{seed}:{key}".encode("utf-8")).hexdigest()


def stratified_order(items: Iterable[Candidate], seed: int, offset: int = 0) -> list[Candidate]:
    """Order duration tertiles by seeded SHA256, then interleave the tertiles."""
    if seed < 0:
        raise DataError("seed must be nonnegative")
    ordered = sorted(items, key=lambda item: (item.duration, item.id))
    strata: list[list[Candidate]] = [[], [], []]
    for index, item in enumerate(ordered):
        strata[min(2, index * 3 // len(ordered))].append(item)
    for stratum in strata:
        stratum.sort(key=lambda item: (seeded_hash(seed, item.id), item.id))
    strata = strata[offset % 3:] + strata[:offset % 3]
    return [stratum[index] for index in range(max(map(len, strata))) for stratum in strata if index < len(stratum)]


def select_items(items: list[Candidate], count: int, seed: int, pool: str, holdout_percent: int = 50,
                 *, kind: str = "fleurs") -> list[Candidate]:
    if count <= 0 or pool not in {"dev", "holdout"}:
        raise DataError("selection requires a positive count and pool dev or holdout")
    if seed < 0:
        raise DataError("seed must be nonnegative")
    assign_pool("", "", holdout_percent)  # Validate the percentage even for an empty candidate list.
    eligible = [item for item in items if assign_pool(item.source, item.group, holdout_percent) == pool]
    if kind in {"librispeech", "jvnv"}:
        speakers: dict[str, list[Candidate]] = defaultdict(list)
        for item in eligible:
            speakers[item.group].append(item)
        groups = sorted(speakers, key=lambda group: (seeded_hash(seed, group), group))
        # Rotate the starting tertile: small sets must not take only each
        # speaker's shortest recordings on the first round.
        orders = [stratified_order(speakers[group], seed, index % 3) for index, group in enumerate(groups)]
        ordered = [order[index] for index in range(max(map(len, orders), default=0)) for order in orders if index < len(order)]
    elif kind == "fleurs":
        ordered = stratified_order(dedupe_groups(eligible), seed)
    else:
        raise DataError(f"unsupported source kind: {kind}")
    if len(ordered) < count:
        raise DataError(f"requested {count} items from {pool}, but only {len(ordered)} eligible items are available")
    return sorted(ordered[:count], key=lambda item: item.id)


def _publish_new(temporary: Path, destination: Path) -> None:
    """Atomically publish a completed file, without replacing any existing file."""
    if os.name == "nt":
        os.rename(temporary, destination)  # Windows rename fails if the target exists.
    else:
        os.link(temporary, destination)
        # The temporary directory owns this link and removes it on exit.


def write_json_temporary(path: Path, value: dict[str, Any]) -> None:
    with path.open("x", encoding="utf-8", newline="\n") as stream:
        stream.write(json.dumps(value, ensure_ascii=False, indent=2) + "\n")


def write_json_new(path: Path, value: dict[str, Any]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix=".bench-data-", dir=path.parent) as tmp:
        temporary = Path(tmp) / "output.json"
        write_json_temporary(temporary, value)
        _publish_new(temporary, path)


def read_json(path: Path) -> dict[str, Any]:
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, ValueError) as exc:
        raise DataError(f"cannot read JSON {path}: {exc}") from exc
    if not isinstance(value, dict):
        raise DataError(f"expected a JSON object: {path}")
    return value


def cache_record(path: Path) -> Path:
    return path.with_name(path.name + ".json")


def require_fields(value: Any, fields: dict[str, type | tuple[type, ...]], context: str) -> dict[str, Any]:
    if not isinstance(value, dict):
        raise DataError(f"expected an object in {context}")
    for field, expected in fields.items():
        if field not in value:
            raise DataError(f"missing required field {field!r} in {context}")
        allowed = expected if isinstance(expected, tuple) else (expected,)
        if type(value[field]) not in allowed:
            raise DataError(f"invalid type for field {field!r} in {context}")
    return value


def validate_hash(value: str, field: str, context: str) -> None:
    if not re.fullmatch(r"[0-9a-f]{64}", value):
        raise DataError(f"invalid {field} in {context}")


def verify_cached(path: Path, url: str) -> str:
    record_path = cache_record(path)
    if path.exists() != record_path.exists():
        raise DataError(f"incomplete cache pair; delete these exact files to recover: {path.resolve()} and {record_path.resolve()}")
    if not path.exists():
        raise DataError(f"cached file not found: {path}; run fetch first")
    record = require_fields(read_json(record_path), {"url": str, "sha256": str, "size_bytes": int}, str(record_path))
    validate_hash(record["sha256"], "sha256", str(record_path))
    if record["size_bytes"] < 0:
        raise DataError(f"invalid size_bytes in {record_path}")
    actual = sha256_file(path)
    if record["url"] != url or record["sha256"] != actual or record["size_bytes"] != path.stat().st_size:
        raise DataError(f"cache SHA256/metadata mismatch: {path}; use a new cache directory to fetch again")
    return actual


def download_new(url: str, destination: Path) -> None:
    record_path = cache_record(destination)
    if destination.exists() or record_path.exists():
        verify_cached(destination, url)
        print(f"[bench_data] verified cached file: {destination}", file=sys.stderr)
        return
    destination.parent.mkdir(parents=True, exist_ok=True)
    request = urllib.request.Request(url, headers={"User-Agent": "asr-bench-data/1"})
    with tempfile.TemporaryDirectory(prefix=".download-", dir=destination.parent) as tmp:
        temporary = Path(tmp) / "download"
        digest = hashlib.sha256()
        received = 0
        with urllib.request.urlopen(request, timeout=60) as response, temporary.open("xb") as stream:
            length = response.headers.get("Content-Length")
            total = int(length) if length is not None else None
            print(f"[bench_data] size: {total if total is not None else 'unknown'} bytes; downloading {url}", file=sys.stderr, flush=True)
            last_progress = 0
            while True:
                block = response.read(1024 * 1024)
                if not block:
                    break
                stream.write(block)
                digest.update(block)
                received += len(block)
                if received - last_progress >= 8 * 1024 * 1024:
                    print(f"[bench_data] {received}/{total if total is not None else '?'} bytes", file=sys.stderr, flush=True)
                    last_progress = received
        if total is not None and received != total:
            raise DataError(f"incomplete download: {url}: expected {total} bytes, received {received}")
        temporary_record = Path(tmp) / "record.json"
        write_json_temporary(temporary_record, {"url": url, "sha256": digest.hexdigest(), "size_bytes": received})
        published = False
        try:
            _publish_new(temporary, destination)
            published = True
            _publish_new(temporary_record, record_path)
        except BaseException:
            if published:
                destination.unlink()
            raise
        print(f"[bench_data] complete: {received} bytes; SHA256 {digest.hexdigest()}", file=sys.stderr)


def source_downloads(spec: Source) -> list[tuple[str, str, str]]:
    archive = (spec.url, Path(spec.url).name, spec.size_hint)
    if spec.kind == "fleurs":
        if not spec.tsv_url:
            raise DataError(f"missing tsv_url for source {spec.key}")
        return [archive, (spec.tsv_url, "test.tsv", "unknown (TSV; server size printed before transfer)")]
    if spec.kind in {"librispeech", "jvnv"}:
        return [archive]
    raise DataError(f"unsupported source kind: {spec.kind}")


def fetch(cache: Path, sources: list[Source], accept_license: bool) -> None:
    for spec in sources:
        print(f"[bench_data] {spec.dataset_id} ({spec.config}), {spec.license}", file=sys.stderr, flush=True)
        for url, _, size in source_downloads(spec):
            print(f"  {url}\n  size: {size}", file=sys.stderr, flush=True)
    if not accept_license:
        raise DataError("fetch requires --accept-license after reviewing the dataset licenses and attribution")
    for spec in sources:
        for url, filename, _ in source_downloads(spec):
            download_new(url, spec.directory(cache) / filename)


def flac_duration(header: bytes) -> float:
    """Read FLAC STREAMINFO; no decoding or whole-member buffering is needed."""
    if len(header) < 42 or header[:4] != b"fLaC" or header[4] & 0x7F != 0 or int.from_bytes(header[5:8], "big") != 34:
        raise DataError("invalid FLAC STREAMINFO header")
    packed = int.from_bytes(header[18:26], "big")
    sample_rate = packed >> 44
    samples = packed & ((1 << 36) - 1)
    if not sample_rate or not samples:
        raise DataError("FLAC has no sample rate or total sample count")
    return samples / sample_rate


def load_candidates(cache: Path, spec: Source) -> list[Candidate]:
    if spec.kind == "jvnv":
        return load_jvnv_candidates(spec.archive(cache), spec)
    if spec.kind == "fleurs":
        if not spec.tsv_url:
            raise DataError(f"missing tsv_url for source {spec.key}")
        tsv = spec.directory(cache) / "test.tsv"
        verify_cached(tsv, spec.tsv_url)
        return parse_fleurs_tsv(tsv.read_text(encoding="utf-8"), spec.config, spec.source)
    if spec.kind != "librispeech":
        raise DataError(f"unsupported source kind: {spec.kind}")
    references: dict[str, str] = {}
    audio: dict[str, tuple[str, float, str]] = {}
    prefix = f"LibriSpeech/{spec.split}/"
    with tarfile.open(spec.archive(cache), "r|*") as archive:
        for member in archive:
            name = member.name.removeprefix("./")
            if not member.isfile() or not name.startswith(prefix):
                continue
            if name.endswith(".trans.txt"):
                stream = archive.extractfile(member)
                assert stream is not None
                parsed = parse_transcripts(stream.read().decode("utf-8"))
                if references.keys() & parsed.keys():
                    raise DataError("duplicate LibriSpeech utterance in transcript files")
                references.update(parsed)
            elif name.endswith(".flac"):
                path = PurePosixPath(name)
                item_id = path.stem
                if not re.fullmatch(r"\d+-\d+-\d+", item_id) or path.parent.name != item_id.split("-")[1] or path.parent.parent.name != item_id.split("-")[0]:
                    raise DataError(f"invalid LibriSpeech member: {name}")
                if item_id in audio:
                    raise DataError(f"duplicate LibriSpeech audio: {item_id}")
                stream = archive.extractfile(member)
                assert stream is not None
                audio[item_id] = (name, flac_duration(stream.read(42)), item_id.split("-")[0])
    missing = references.keys() - audio.keys()
    if missing:
        raise DataError(f"missing LibriSpeech audio item: {min(missing)}")
    missing = audio.keys() - references.keys()
    if missing:
        raise DataError(f"missing LibriSpeech transcript item: {min(missing)}")
    return [Candidate(spec.source, spec.config, item_id, member, references[item_id], duration, speaker, speaker)
            for item_id, (member, duration, speaker) in sorted(audio.items())]


def load_jvnv_candidates(path: Path, spec: Source) -> list[Candidate]:
    """Use regular-session scripts verbatim; free-session NV wording varies."""
    with zipfile.ZipFile(path) as archive:
        names = archive.namelist()
        if len(names) != len(set(names)):
            raise DataError("duplicate JVNV archive member")
        references = {}
        text = archive.read("jvnv_v1/transcription.csv").decode("utf-8-sig")
        for line, row in enumerate(csv.reader(io.StringIO(text), delimiter="|"), 1):
            if len(row) != 3 or not row[2].strip() or row[0] in references:
                raise DataError(f"JVNV transcript line {line}: invalid/duplicate script")
            references[row[0]] = row[2]
        items = []
        for name in sorted(names):
            match = re.fullmatch(r"jvnv_v1/(F[12]|M[12])/([a-z]+)/regular/((F[12]|M[12])_([a-z]+)_regular_\d+)\.wav", name)
            if match is None:
                continue
            speaker, emotion, item_id, file_speaker, file_emotion = match.groups()
            script = item_id.split("_", 1)[1]
            if speaker != file_speaker or emotion != file_emotion or script not in references:
                raise DataError(f"JVNV audio/script mismatch: {name}")
            with archive.open(name) as stream, wave.open(stream, "rb") as audio:
                duration = audio.getnframes() / audio.getframerate()
            if duration <= 0:
                raise DataError(f"JVNV empty audio: {name}")
            items.append(Candidate(spec.source, spec.config, item_id, name, references[script],
                                   duration, speaker, speaker))
        if not items:
            raise DataError("no JVNV regular-session audio found")
        return items


@contextlib.contextmanager
def archive_members(path: Path):
    """Expose file streams without extracting untrusted archive paths."""
    if path.suffix == ".zip":
        with zipfile.ZipFile(path) as archive:
            def entries():
                for info in archive.infolist():
                    if not info.is_dir():
                        with archive.open(info) as stream:
                            yield info.filename, stream
            yield entries()
    else:
        with tarfile.open(path, "r|*") as archive:
            def entries():
                for info in archive:
                    if info.isfile():
                        with archive.extractfile(info) as stream:
                            yield info.name.removeprefix("./"), stream
            yield entries()


def convert_audio(data: bytes, destination: Path) -> float:
    """Decode one selected member and resample to mono 16 kHz signed 16-bit WAV."""
    try:
        import av
    except ImportError as exc:
        raise DataError("PyAV (av) is required; run this script in the project's uv environment") from exc

    samples = 0
    try:
        with av.open(io.BytesIO(data)) as container:
            if not container.streams.audio:
                raise DataError(f"no audio stream in selected member for {destination.name}")
            with wave.open(str(destination), "wb") as output:
                output.setnchannels(1)
                output.setsampwidth(2)
                output.setframerate(16000)
                resampler = av.AudioResampler(format="s16", layout="mono", rate=16000)
                for frame in container.decode(audio=0):
                    for converted in resampler.resample(frame):
                        output.writeframesraw(bytes(converted.planes[0])[:converted.samples * 2])
                        samples += converted.samples
                for converted in resampler.resample(None):
                    output.writeframesraw(bytes(converted.planes[0])[:converted.samples * 2])
                    samples += converted.samples
    except av.error.FFmpegError as exc:
        raise DataError(f"cannot decode/resample selected audio: {exc}") from exc
    if not samples:
        raise DataError("decoded audio is empty")
    return samples / 16000


def read_document(path: Path, schema: str) -> dict[str, Any]:
    document = require_fields(read_json(path), {
        "schema": str, "name": str, "created_by": str, "items": list, "sources": list, "selection": dict,
    }, str(path))
    if document["schema"] != schema:
        raise DataError(f"unsupported schema in {path}: {document['schema']!r}")
    safe_name(document["name"])
    items = document["items"]
    if not items:
        raise DataError(f"items must be a nonempty list: {path}")
    seen = set()
    item_types: dict[str, type | tuple[type, ...]] = {field: str for field in LOCK_ITEM_FIELDS}
    item_types["speaker"] = (str, type(None))
    if schema == MANIFEST_SCHEMA:
        item_types.update({"audio_path": str, "reference": str, "language": str, "duration": (int, float), "provenance": str})
    for index, value in enumerate(items):
        context = f"{path} items[{index}]"
        item = require_fields(value, item_types, context)
        safe_name(item["id"])
        if item["id"] in seen:
            raise DataError(f"duplicate item id: {item['id']}")
        seen.add(item["id"])
        for field in ("audio_sha256", "source_audio_sha256"):
            validate_hash(item[field], field, context)
        if "reference_sha256" in item:
            if not isinstance(item["reference_sha256"], str):
                raise DataError(f"invalid reference_sha256 in {context}")
            validate_hash(item["reference_sha256"], "reference_sha256", context)
        if item["pool"] not in {"dev", "holdout"}:
            raise DataError(f"invalid pool in {context}")
        if schema == MANIFEST_SCHEMA and (not math.isfinite(item["duration"]) or item["duration"] <= 0):
            raise DataError(f"invalid duration in {context}")
    if not document["sources"]:
        raise DataError(f"sources must be a nonempty list: {path}")
    for index, metadata in enumerate(document["sources"]):
        context = f"{path} sources[{index}]"
        require_fields(metadata, {field: str for field in SOURCE_FIELDS}, context)
        validate_hash(metadata["archive_sha256"], "archive_sha256", context)
    selection = require_fields(document["selection"], {
        "profile": str, "seed": int, "count_per_source": int, "pool": str, "holdout_percent": int, "strata": str,
    }, f"{path} selection")
    profile = selection["profile"]
    if profile not in PROFILE_COUNTS or selection["pool"] != ("holdout" if profile == "holdout" else "dev") or selection["strata"] != "duration-tertile":
        raise DataError(f"invalid selection profile/pool/strata: {path}")
    if selection["seed"] < 0:
        raise DataError(f"selection seed must be nonnegative: {path}")
    if selection["count_per_source"] <= 0 or not 0 <= selection["holdout_percent"] <= 100:
        raise DataError(f"invalid selection count/holdout_percent: {path}")
    return document


def document_sources(document: dict[str, Any]) -> list[Source]:
    require_fields(document, {"sources": list}, "document")
    result = []
    for metadata in document["sources"]:
        require_fields(metadata, {field: str for field in SOURCE_FIELDS}, "source metadata")
        key = f"{metadata['source']}:{metadata['config']}"
        if key not in SOURCES:
            raise DataError(f"unsupported locked source: {key}")
        spec = SOURCES[key]
        expected = spec.metadata("")
        if any(metadata[field] != expected[field] for field in expected if field != "archive_sha256"):
            raise DataError(f"source metadata/revision mismatch: {key}")
        if spec in result:
            raise DataError(f"duplicate source: {key}")
        result.append(spec)
    return result


def lock_manifest(manifest: Path, output: Path) -> None:
    if output.exists():
        raise DataError(f"output already exists: {output}")
    document = read_document(manifest, MANIFEST_SCHEMA)
    document_sources(document)
    # Whitelist every field: arbitrary manifest extras must not leak text/paths.
    lock = {
        "schema": LOCK_SCHEMA, "name": safe_name(document["name"]),
        "created_by": "scripts/bench_data.py",
        "selection": {key: document["selection"][key] for key in SELECTION_FIELDS},
        "sources": [{key: metadata[key] for key in SOURCE_FIELDS} for metadata in document["sources"]],
        "items": [{**{key: item[key] for key in LOCK_ITEM_FIELDS},
                   "reference_sha256": hashlib.sha256(item["reference"].encode("utf-8")).hexdigest()}
                  for item in document["items"]],
    }
    write_json_new(output, lock)


def build(cache: Path, sources: list[Source], name: str, selection: dict[str, Any], locked: dict[str, Any] | None = None) -> Path:
    safe_name(name)
    manifest = cache / "manifests" / f"{name}.json"
    audio_root = cache / "audio" / name
    if manifest.exists() or audio_root.exists():
        raise DataError(f"output already exists for {name!r}; choose a new --out name")
    metadata = []
    selected = []
    for spec in sources:
        archive_hash = verify_cached(spec.archive(cache), spec.url)
        if locked is not None:
            locked_metadata = next(metadata for metadata in locked["sources"]
                                   if (metadata["source"], metadata["config"]) == (spec.source, spec.config))
            if locked_metadata["archive_sha256"] != archive_hash:
                print(f"[bench_data] notice: archive_sha256 differs for {spec.key}: "
                      f"lock {locked_metadata['archive_sha256']}, local {archive_hash}; verifying selected source audio", file=sys.stderr)
        candidates = load_candidates(cache, spec)
        if locked is None:
            chosen = select_items(candidates, selection["count_per_source"], selection["seed"], selection["pool"],
                                  selection["holdout_percent"], kind=spec.kind)
        else:
            index = {item.id: item for item in candidates}
            chosen = []
            for entry in locked["items"]:
                if entry["id"] not in index:
                    if (entry["source"], entry["split"], entry["locale"]) == (spec.source, spec.split, spec.locale):
                        raise DataError(f"missing locked item: {entry['id']}")
                    continue
                item = index[entry["id"]]
                expected = {"source": spec.source, "locale": spec.locale,
                            "item_id": item.item_id, "group": item.group, "speaker": item.speaker,
                            "split": spec.split, "revision": spec.revision, "dataset_id": spec.dataset_id,
                            "license": spec.license,
                            "pool": assign_pool(item.source, item.group, selection["holdout_percent"])}
                if any(entry[field] != value for field, value in expected.items()) or expected["pool"] != selection["pool"]:
                    raise DataError(f"locked item metadata/pool mismatch: {entry['id']}")
                if "reference_sha256" in entry and hashlib.sha256(item.reference.encode("utf-8")).hexdigest() != entry["reference_sha256"]:
                    raise DataError(f"reference_sha256 mismatch: {entry['id']}")
                chosen.append(item)
        metadata.append(spec.metadata(archive_hash))
        selected.extend(chosen)
    if locked is not None:
        missing = {item["id"] for item in locked["items"]} - {item.id for item in selected}
        if missing:
            raise DataError(f"missing locked item: {min(missing)}")
    if not selected:
        raise DataError("empty source selection")
    if locked is not None:
        locked_ids = [entry["id"] for entry in locked["items"]]
        index = {item.id: item for item in selected}
        selected = [index[item_id] for item_id in locked_ids]
    expected_hashes = {entry["id"]: entry["source_audio_sha256"] for entry in locked["items"]} if locked else {}
    audio_root.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix=".build-", dir=audio_root.parent) as tmp:
        staged = Path(tmp) / "audio"
        staged.mkdir()
        built = {}
        for spec in sources:
            wanted = {item.member: item for item in selected if item.source == spec.source and item.config == spec.config}
            with archive_members(spec.archive(cache)) as members:
                for member_name, stream in members:
                    if member_name not in wanted:
                        continue
                    candidate = wanted[member_name]
                    if candidate.id in built:
                        raise DataError(f"duplicate/non-file selected archive member: {member_name}")
                    data = stream.read()
                    original_hash = hashlib.sha256(data).hexdigest()
                    if candidate.id in expected_hashes and original_hash != expected_hashes[candidate.id]:
                        raise DataError(f"source_audio_sha256 mismatch: {candidate.id}")
                    relative = Path(spec.source) / spec.config / f"{candidate.id}.wav"
                    target = staged / relative
                    target.parent.mkdir(parents=True, exist_ok=True)
                    duration = convert_audio(data, target)
                    tolerance = 0.05 if spec.kind == "fleurs" else 1 / 16000
                    if abs(duration - candidate.duration) > tolerance + 1e-9:
                        raise DataError(f"duration mismatch: {candidate.id}: metadata {candidate.duration:.6f}s, decoded {duration:.6f}s")
                    built[candidate.id] = {
                        "id": candidate.id,
                        "audio_path": (Path("..") / "audio" / name / relative).as_posix(),
                        "reference": candidate.reference, "language": spec.language, "duration": duration,
                        "source": spec.source, "split": spec.split, "pool": selection["pool"],
                        "dataset_id": spec.dataset_id, "revision": spec.revision, "locale": spec.locale,
                        "item_id": candidate.item_id, "group": candidate.group, "speaker": candidate.speaker,
                        "audio_sha256": sha256_file(target), "source_audio_sha256": original_hash,
                        "provenance": f"{spec.url}#{candidate.member}", "license": spec.license,
                    }
            missing = {item.id for item in wanted.values()} - built.keys()
            if missing:
                raise DataError(f"missing selected archive item: {min(missing)}")
        document = {"schema": MANIFEST_SCHEMA, "name": name, "created_by": "scripts/bench_data.py",
                    "selection": selection, "sources": metadata, "items": [built[item.id] for item in selected]}
        # Prepare both outputs before publishing either. Roll back only paths
        # created by this run if publication fails or is interrupted.
        manifest.parent.mkdir(parents=True, exist_ok=True)
        staged_manifest = Path(tmp) / "manifest.json"
        write_json_temporary(staged_manifest, document)
        if audio_root.exists():
            raise DataError(f"audio output already exists: {audio_root}")
        published = False
        try:
            os.rename(staged, audio_root)
            published = True
            _publish_new(staged_manifest, manifest)
        except BaseException:
            if published:
                shutil.rmtree(audio_root)
            raise
    return manifest


def parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--cache-dir", type=Path, default=DEFAULT_CACHE)
    subparsers = parser.add_subparsers(dest="command", required=True)
    fetch_parser = subparsers.add_parser("fetch", help="explicitly download licensed dataset archives")
    build_parser = subparsers.add_parser("build", help="build a frozen subset from verified cached archives")
    lock_parser = subparsers.add_parser("lock", help="write a committable lock without references or audio paths")
    for subparser in (fetch_parser, build_parser, lock_parser):
        subparser.add_argument("--cache-dir", type=Path, default=argparse.SUPPRESS)
    for subparser in (fetch_parser, build_parser):
        subparser.add_argument("--source", action="append", choices=sorted(SOURCES), help="repeatable dataset:config")
    fetch_parser.add_argument("--accept-license", action="store_true")
    build_parser.add_argument("--profile", choices=PROFILE_COUNTS)
    build_parser.add_argument("--count-per-source", type=int)
    build_parser.add_argument("--seed", type=int)
    build_parser.add_argument("--holdout-percent", type=int)
    build_parser.add_argument("--out", required=True, help="new manifest name (without .json)")
    build_parser.add_argument("--lock", type=Path)
    lock_parser.add_argument("--manifest", type=Path, required=True)
    lock_parser.add_argument("--output", type=Path, required=True, help="new lock JSON file; never overwrite")
    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    args = parse_args(argv)
    try:
        if args.command == "lock":
            lock_manifest(args.manifest, args.output)
            print(args.output)
            return 0
        keys = args.source or list(DEFAULT_SOURCES)
        if len(set(keys)) != len(keys):
            raise DataError("duplicate --source")
        sources = [SOURCES[key] for key in sorted(keys)]
        if args.command == "fetch":
            fetch(args.cache_dir, sources, args.accept_license)
            return 0
        locked = None
        if args.lock:
            if any(getattr(args, key) is not None for key in ("source", "profile", "count_per_source", "seed", "holdout_percent")):
                raise DataError("--lock cannot be combined with source or selection overrides")
            locked = read_document(args.lock, LOCK_SCHEMA)
            sources = document_sources(locked)
            selection = locked["selection"]
        else:
            profile = args.profile or "smoke"
            count = args.count_per_source if args.count_per_source is not None else PROFILE_COUNTS[profile]
            percent = args.holdout_percent if args.holdout_percent is not None else 50
            if count <= 0 or not 0 <= percent <= 100:
                raise DataError("--count-per-source must be positive; --holdout-percent must be between 0 and 100")
            if args.seed is not None and args.seed < 0:
                raise DataError("--seed must be nonnegative")
            selection = {"profile": profile, "seed": args.seed if args.seed is not None else DEFAULT_SEED,
                         "count_per_source": count, "pool": "holdout" if profile == "holdout" else "dev",
                         "holdout_percent": percent, "strata": "duration-tertile"}
        print(build(args.cache_dir, sources, args.out, selection, locked))
        return 0
    except (DataError, OSError, ValueError, tarfile.TarError, zipfile.BadZipFile, KeyError,
            urllib.error.URLError, http.client.HTTPException) as exc:
        print(f"[bench_data] error: {exc}", file=sys.stderr)
        return 1
    except KeyboardInterrupt:
        print("[bench_data] interrupted", file=sys.stderr)
        return 130


if __name__ == "__main__":
    raise SystemExit(main())
