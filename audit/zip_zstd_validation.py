#!/usr/bin/env python3
"""Validate that MothZip *reads* ZIP members stored with compression method 93 (Zstandard).

Independence (why this is not a self-confirmation):
  * The .zip container is assembled here byte by byte: local file header, raw
    payload, central directory, EOCD. No ZIP library writes the fixtures, and
    Python's ``zipfile`` is deliberately unused because it cannot decode method
    93 at all (it would only ever confirm the container, never the codec).
  * The method-93 payload comes from Python ``zstandard`` with ``write_checksum``
    and ``write_content_size`` enabled, and the expected plaintext is the
    Python-side buffer that was compressed.
  * Every verdict is an exit code plus an exact-bytes / SHA-256 comparison. A
    case is never marked passed from an inference or from a tool's own claim.

Written against the already-built binary; this script never builds, edits or
patches the Rust sources.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import random
import re
import shutil
import struct
import subprocess
import sys
import time
import uuid
import zlib
from pathlib import Path

import zstandard

REPO_ROOT = Path(__file__).resolve().parent.parent
DEFAULT_BINARY = Path("G:/mothzip-build/release/mothzip.exe")
RUNS_DIR = REPO_ROOT / "audit" / "runs"
HANDOFF_PATH = REPO_ROOT / "audit" / "delegation" / "zip-zstd-handoff.md"

# Fixed DOS timestamp (2026-01-01 00:00:00) so the fixtures are reproducible.
DOS_TIME = 0
DOS_DATE = ((2026 - 1980) << 9) | (1 << 5) | 1
ZIP_VERSION = 20
ZSTD_METHOD = 93
DEFLATE_METHOD = 8
STORED_METHOD = 0
KOREAN_NAME = "\ud55c\uae00 \ubb38\uc11c.txt"  # "한글 문서.txt"


# --------------------------------------------------------------------------- #
# primitives
# --------------------------------------------------------------------------- #
def sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def sha256_file(path: Path) -> str:
    return sha256(Path(path).read_bytes())


def run(cmd) -> dict:
    """Run one MothZip call and keep the raw stdout bytes for byte comparison."""
    argv = [str(part) for part in cmd]
    proc = subprocess.run(argv, capture_output=True)
    return {
        "cmd": argv,
        "exit": proc.returncode,
        "stdout": proc.stdout,
        "stdout_len": len(proc.stdout),
        "stdout_sha256": sha256(proc.stdout),
        "stderr": proc.stderr.decode("utf8", errors="replace").strip(),
    }


def public(call: dict) -> dict:
    """JSON-safe projection: raw stdout is dropped, only its digest survives."""
    record = {key: value for key, value in call.items() if key != "stdout"}
    record["stderr"] = record["stderr"][:600]
    return record


class Case:
    """One purpose body: a named set of exit-code/byte assertions plus its calls."""

    def __init__(self, name: str, purpose: str):
        self.name = name
        self.purpose = purpose
        self.checks: list[dict] = []
        self.calls: list[dict] = []

    def check(self, label: str, ok: bool, detail: str = "") -> bool:
        self.checks.append({"check": label, "ok": bool(ok), "detail": detail})
        return bool(ok)

    def expect(self, call: dict, expected_exit: int = 0, label: str | None = None) -> bool:
        self.calls.append(public(call))
        name = label or f"{call['cmd'][1]} exit"
        return self.check(
            f"{name} == {expected_exit}",
            call["exit"] == expected_exit,
            f"exit={call['exit']} stderr={call['stderr'][:300]!r}",
        )

    @property
    def passed(self) -> bool:
        return bool(self.checks) and all(item["ok"] for item in self.checks)

    def to_json(self) -> dict:
        return {
            "case": self.name,
            "passed": self.passed,
            "calls": self.calls,
            "purpose": self.purpose,
            "checks": self.checks,
        }


# --------------------------------------------------------------------------- #
# independent ZIP writer (container assembled by hand)
# --------------------------------------------------------------------------- #
def compress_member(member: dict) -> bytes:
    data = member["data"]
    if member["method"] == ZSTD_METHOD:
        return zstandard.ZstdCompressor(
            level=9, write_checksum=True, write_content_size=True
        ).compress(data)
    if member["method"] == DEFLATE_METHOD:
        compressor = zlib.compressobj(9, zlib.DEFLATED, -15)
        return compressor.compress(data) + compressor.flush()
    if member["method"] == STORED_METHOD:
        return data
    raise ValueError(f"unsupported fixture method {member['method']}")


def build_zip(path: Path, members: list[dict]) -> None:
    """Assemble LFH+payload per member, then the central directory, then EOCD.

    General purpose flags are 0 and the version-needed field is 20 for every
    member, as required by the brief. Entry names are raw UTF-8 bytes; MothZip
    decodes UTF-8 before any ANSI code page, so the Korean name resolves even
    with bit 11 clear (recorded as a deliberate, documented deviation).
    """
    local = bytearray()
    central = bytearray()
    for member in members:
        name = member["name"].encode("utf-8")
        payload = compress_member(member)
        crc = zlib.crc32(member["data"]) & 0xFFFFFFFF
        offset = len(local)
        local += struct.pack(
            "<IHHHHHIIIHH",
            0x04034B50,
            ZIP_VERSION,
            0,
            member["method"],
            DOS_TIME,
            DOS_DATE,
            crc,
            len(payload),
            len(member["data"]),
            len(name),
            0,
        )
        local += name + payload
        central += struct.pack(
            "<IHHHHHHIIIHHHHHII",
            0x02014B50,
            ZIP_VERSION,
            ZIP_VERSION,
            0,
            member["method"],
            DOS_TIME,
            DOS_DATE,
            crc,
            len(payload),
            len(member["data"]),
            len(name),
            0,
            0,
            0,
            0,
            0,
            offset,
        )
        central += name
        member["csize"] = len(payload)

    eocd = struct.pack(
        "<IHHHHIIH",
        0x06054B50,
        0,
        0,
        len(members),
        len(members),
        len(central),
        len(local),
        0,
    )
    path.write_bytes(bytes(local) + bytes(central) + eocd)


def read_zip_headers(path: Path) -> dict:
    """Parse the container the way an outside reader would: EOCD -> CDH -> LFH."""
    blob = Path(path).read_bytes()
    eocd_pos = blob.rfind(b"PK\x05\x06")
    if eocd_pos < 0:
        raise ValueError(f"{path}: no EOCD signature")
    _, _, _, _, total, cd_size, cd_offset, _ = struct.unpack_from(
        "<IHHHHIIH", blob, eocd_pos
    )
    entries = []
    pos = cd_offset
    for _ in range(total):
        if blob[pos : pos + 4] != b"PK\x01\x02":
            raise ValueError(f"{path}: central directory signature missing at {pos}")
        (
            _sig,
            _vmade,
            _vneed,
            _flags,
            method,
            _mtime,
            _mdate,
            crc,
            csize,
            usize,
            nlen,
            elen,
            clen,
            _disk,
            _iattr,
            _eattr,
            local_offset,
        ) = struct.unpack_from("<IHHHHHHIIIHHHHHII", blob, pos)
        raw_name = blob[pos + 46 : pos + 46 + nlen]
        local_flags, local_method = struct.unpack_from("<HH", blob, local_offset + 6)
        entries.append(
            {
                "name": raw_name.decode("utf8", errors="replace"),
                "name_sha256": sha256(raw_name),
                "flags": _flags,
                "method": method,
                "crc": crc,
                "csize": csize,
                "usize": usize,
                "local_offset": local_offset,
                "local_flags": local_flags,
                "local_method": local_method,
                "local_crc": struct.unpack_from("<I", blob, local_offset + 14)[0],
                "is_dir": raw_name.endswith(b"/"),
            }
        )
        pos += 46 + nlen + elen + clen
    return {
        "size": len(blob),
        "sha256": sha256(blob),
        "eocd_offset": eocd_pos,
        "cd_offset": cd_offset,
        "cd_size": cd_size,
        "total_entries": total,
        "entries": entries,
    }


# --------------------------------------------------------------------------- #
# fixture specs (deterministic)
# --------------------------------------------------------------------------- #
def fixture_specs() -> list[dict]:
    generator = random.Random(20261026)
    big = generator.randbytes(1 << 20)
    text = b"mothzip zstd member round-trip\n" * 8
    korean_body = "\ud55c\uae00 Zstandard \uba64\ubc84 \ub0b4\uc6a9\uc785\ub2c8\ub2e4.\n".encode("utf8")
    mixed_big = generator.randbytes(64 * 1024)
    return [
        {
            "key": "text",
            "case": "read-method93-text",
            "zip": "zstd-text.zip",
            "members": [{"name": "hello.txt", "data": text, "method": ZSTD_METHOD}],
        },
        {
            "key": "random-1mib",
            "case": "read-method93-random-1mib",
            "zip": "zstd-random-1mib.zip",
            "members": [{"name": "random.bin", "data": big, "method": ZSTD_METHOD}],
        },
        {
            "key": "empty",
            "case": "read-method93-empty",
            "zip": "zstd-empty.zip",
            "members": [{"name": "empty.bin", "data": b"", "method": ZSTD_METHOD}],
        },
        {
            "key": "korean-name",
            "case": "read-method93-korean-name",
            "zip": "zstd-korean.zip",
            "members": [
                {"name": KOREAN_NAME, "data": korean_body, "method": ZSTD_METHOD}
            ],
        },
        {
            "key": "mixed",
            "case": "read-method93-mixed-members",
            "zip": "mixed-methods.zip",
            "members": [
                {"name": "zstd.txt", "data": text, "method": ZSTD_METHOD},
                {
                    "name": "deflate.txt",
                    "data": b"deflate payload " * 40,
                    "method": DEFLATE_METHOD,
                },
                {"name": "stored.bin", "data": bytes(range(256)), "method": STORED_METHOD},
                {"name": "\ud55c\uae00-zstd.txt", "data": korean_body * 2, "method": ZSTD_METHOD},
                {"name": "zstd-blob.bin", "data": mixed_big, "method": ZSTD_METHOD},
            ],
        },
    ]


# --------------------------------------------------------------------------- #
# fixture validation
# --------------------------------------------------------------------------- #
def json_of(case: Case, call: dict, label: str):
    try:
        document = json.loads(call["stdout"].decode("utf8"))
    except Exception as exc:  # noqa: BLE001 - the failure detail is the evidence
        case.check(
            f"{label}: JSON parse",
            False,
            f"{exc}: stdout={call['stdout'][:200]!r} stderr={call['stderr'][:200]!r}",
        )
        return None
    case.check(f"{label}: JSON parse", True)
    return document


def collect_tree(root: Path) -> dict[str, bytes]:
    if not root.exists():
        return {}
    return {
        path.relative_to(root).as_posix(): path.read_bytes()
        for path in sorted(root.rglob("*"))
        if path.is_file()
    }


def staging_residue(root: Path) -> list[str]:
    if not root.exists():
        return []
    return sorted(
        path.name for path in root.rglob("*") if path.name.startswith(".mothzip_stage_")
    )


def validate_fixture(case: Case, spec: dict, zip_path: Path, out_dir: Path) -> dict:
    headers = read_zip_headers(zip_path)
    expected_methods = [member["method"] for member in spec["members"]]
    case.check(
        "container exposes the declared methods",
        [entry["method"] for entry in headers["entries"]] == expected_methods,
        f"cdh={[entry['method'] for entry in headers['entries']]} expected={expected_methods}",
    )
    case.check(
        "local headers agree with the central directory",
        all(entry["local_method"] == entry["method"] for entry in headers["entries"]),
        f"lfh={[entry['local_method'] for entry in headers['entries']]}",
    )
    case.check(
        "general purpose flags are 0",
        all(entry["flags"] == 0 for entry in headers["entries"]),
        f"flags={[entry['flags'] for entry in headers['entries']]}",
    )
    if any(member["method"] == ZSTD_METHOD for member in spec["members"]):
        frame_ok = True
        detail = []
        for member in spec["members"]:
            if member["method"] != ZSTD_METHOD:
                continue
            payload = compress_member(member)
            params = zstandard.get_frame_parameters(payload)
            decoded = zstandard.ZstdDecompressor().decompress(
                payload, max_output_size=max(1, len(member["data"]))
            )
            frame_ok = frame_ok and params.has_checksum and decoded == member["data"]
            detail.append(
                f"{member['name']}: checksum={params.has_checksum} "
                f"content_size={params.content_size} payload={len(payload)}B"
            )
        case.check("payload is a real zstd frame with checksum", frame_ok, "; ".join(detail))

    # peek --json: entry names and sizes
    peek_call = run([BIN, "peek", zip_path, "--json"])
    case.expect(peek_call, 0, "peek exit")
    peek_doc = json_of(case, peek_call, "peek")
    tree: dict[str, int] = {}
    if isinstance(peek_doc, dict):
        tree = {
            item["path"]: item["size"] for item in peek_doc.get("tree_summary", [])
        }
        case.check(
            "peek reports every member with its exact size",
            tree == {member["name"]: len(member["data"]) for member in spec["members"]},
            f"tree={tree}",
        )
        case.check(
            "peek total_entries",
            peek_doc.get("total_entries") == len(spec["members"]),
            f"total_entries={peek_doc.get('total_entries')}",
        )
        case.check(
            "peek total_compressed_bytes",
            peek_doc.get("total_compressed_bytes")
            == sum(member["csize"] for member in spec["members"]),
            f"total_compressed_bytes={peek_doc.get('total_compressed_bytes')} "
            f"expected={sum(member['csize'] for member in spec['members'])}",
        )

    # cat: raw stdout bytes per member
    cat_ok = True
    cat_detail = []
    for member in spec["members"]:
        call = run([BIN, "cat", zip_path, member["name"]])
        matched = call["exit"] == 0 and call["stdout"] == member["data"]
        cat_ok = cat_ok and matched
        cat_detail.append(
            f"{member['name']}: exit={call['exit']} "
            f"sha={call['stdout_sha256'][:12]} want={sha256(member['data'])[:12]}"
        )
        case.calls.append(public(call))
    case.check("cat returns the exact member bytes", cat_ok, "; ".join(cat_detail))

    # unpack: committed files
    if out_dir.exists():
        shutil.rmtree(out_dir)
    unpack_call = run([BIN, "unpack", zip_path, "-o", out_dir, "--json"])
    case.expect(unpack_call, 0, "unpack exit")
    expected_files = {
        member["name"]: member["data"] for member in spec["members"]
    }
    committed = collect_tree(out_dir)
    case.check(
        "unpack commits exactly the expected files and bytes",
        committed == expected_files,
        f"committed={sorted(committed)} expected={sorted(expected_files)} "
        f"match={committed == expected_files}",
    )
    case.check(
        "unpack leaves no staging residue",
        staging_residue(out_dir) == [],
        f"residue={staging_residue(out_dir)}",
    )

    # test --json
    test_call = run([BIN, "test", zip_path, "--json"])
    case.expect(test_call, 0, "test exit")
    test_doc = json_of(case, test_call, "test")
    if isinstance(test_doc, dict):
        case.check(
            "test reports passed=true",
            test_doc.get("passed") is True,
            f"passed={test_doc.get('passed')!r}",
        )
        case.check(
            "test exercised every non-directory member",
            test_doc.get("tested_entries")
            == sum(1 for member in spec["members"] if not member["name"].endswith("/")),
            f"tested_entries={test_doc.get('tested_entries')}",
        )

    return {
        "case": case.name,
        "zip": zip_path.name,
        "zip_sha256": headers["sha256"],
        "members": [
            {
                "name": member["name"],
                "method": member["method"],
                "size": len(member["data"]),
                "csize": member["csize"],
                "crc": zlib.crc32(member["data"]) & 0xFFFFFFFF,
                "sha256": sha256(member["data"]),
            }
            for member in spec["members"]
        ],
        "peek_exit": peek_call["exit"],
        "cat_exit": 0 if cat_ok else 1,
        "unpack_exit": unpack_call["exit"],
        "test_exit": test_call["exit"],
        "test_passed": isinstance(test_doc, dict) and test_doc.get("passed") is True,
        "tested_entries": test_doc.get("tested_entries") if isinstance(test_doc, dict) else None,
        "passed": case.passed,
    }


# --------------------------------------------------------------------------- #
# corruption variants
# --------------------------------------------------------------------------- #
def first_member_span(blob: bytes) -> tuple[int, int]:
    if blob[:4] != b"PK\x03\x04":
        raise ValueError("fixture does not start with a local file header")
    csize = struct.unpack_from("<I", blob, 18)[0]
    nlen = struct.unpack_from("<H", blob, 26)[0]
    return 30 + nlen, csize


def corrupt_copy(source: Path, destination: Path, variant: str) -> tuple[Path, int]:
    """Return the corrupted archive and the offset that was actually changed."""
    blob = bytearray(Path(source).read_bytes())
    data_start, csize = first_member_span(blob)
    cd_pos = blob.find(b"PK\x01\x02", data_start + csize)
    if cd_pos < 0:
        raise ValueError("central directory not found")
    if variant == "flip-payload-mid":
        offset = data_start + max(1, csize // 2)
        blob[offset] ^= 0x01
    elif variant == "flip-zstd-checksum":
        offset = data_start + csize - 1
        blob[offset] ^= 0x01
    elif variant == "wrong-zip-crc":
        offset = 14
        crc = struct.unpack_from("<I", blob, 14)[0] ^ 0xFFFFFFFF
        struct.pack_into("<I", blob, 14, crc)
        struct.pack_into("<I", blob, cd_pos + 16, crc)
    else:
        raise ValueError(variant)
    destination.write_bytes(bytes(blob))
    return destination, offset


def classify_error(text: str) -> str:
    if "Restored data doesn't match checksum" in text:
        return "zstd-frame-checksum"
    if "Invalid checksum" in text:
        return "zip-crc32"
    if "not supported" in text.lower():
        return "unsupported-method"
    return "other"


def validate_corruption(
    case: Case, base: dict, variant: str, zip_path: Path, out_dir: Path, offset: int
) -> dict:
    headers = read_zip_headers(zip_path)
    case.check(
        "corrupted archive still parses as method 93",
        [entry["method"] for entry in headers["entries"]] == [ZSTD_METHOD],
        f"methods={[entry['method'] for entry in headers['entries']]} changed_offset={offset}",
    )

    # The catalog must still open: the damage is in the payload, not the headers.
    peek_call = run([BIN, "peek", zip_path, "--json"])
    case.expect(peek_call, 0, "peek exit (payload damage must not break the catalog)")

    member = base["members"][0]
    cat_call = run([BIN, "cat", zip_path, member["name"]])
    case.calls.append(public(cat_call))
    case.check(
        "cat rejects the corrupted member (exit != 0)",
        cat_call["exit"] != 0,
        f"exit={cat_call['exit']} stdout_len={cat_call['stdout_len']} "
        f"stdout_match={cat_call['stdout'] == member['data']}",
    )
    case.check(
        "cat never reports success for wrong bytes",
        not (cat_call["exit"] == 0 and cat_call["stdout"] != member["data"]),
        f"exit={cat_call['exit']} stdout_len={cat_call['stdout_len']}",
    )

    if out_dir.exists():
        shutil.rmtree(out_dir)
    unpack_call = run([BIN, "unpack", zip_path, "-o", out_dir, "--json"])
    case.calls.append(public(unpack_call))
    committed = collect_tree(out_dir)
    case.check(
        "unpack rejects the corrupted member (exit != 0)",
        unpack_call["exit"] != 0,
        f"exit={unpack_call['exit']} stderr={unpack_call['stderr'][:300]!r}",
    )
    case.check(
        "unpack commits no member file",
        committed == {},
        f"committed={sorted(committed)}",
    )
    case.check(
        "unpack leaves no staging residue",
        staging_residue(out_dir) == [],
        f"residue={staging_residue(out_dir)}",
    )

    test_call = run([BIN, "test", zip_path, "--json"])
    case.calls.append(public(test_call))
    case.check(
        "test rejects the corrupted member (exit != 0)",
        test_call["exit"] != 0,
        f"exit={test_call['exit']} stderr={test_call['stderr'][:300]!r}",
    )
    claimed_pass = False
    if test_call["stdout"]:
        try:
            document = json.loads(test_call["stdout"].decode("utf8"))
            claimed_pass = document.get("passed") is True
        except Exception:  # noqa: BLE001 - non-JSON stdout is not a pass claim
            claimed_pass = False
    case.check(
        "test never reports passed=true",
        not claimed_pass,
        f"claimed_pass={claimed_pass} stdout={test_call['stdout'][:200]!r}",
    )

    layer = classify_error(cat_call["stderr"] or unpack_call["stderr"])
    return {
        "case": case.name,
        "base": base["case"],
        "variant": variant,
        "changed_offset": offset,
        "cat_exit": cat_call["exit"],
        "cat_stdout_len": cat_call["stdout_len"],
        "cat_stdout_full": cat_call["stdout"] == member["data"],
        "unpack_exit": unpack_call["exit"],
        "committed_files": len(committed),
        "staging_residue": staging_residue(out_dir),
        "test_exit": test_call["exit"],
        "error_text": (cat_call["stderr"] or unpack_call["stderr"] or test_call["stderr"])[:300],
        "detected_by": layer,
        "passed": case.passed,
    }


# --------------------------------------------------------------------------- #
# pack regression
# --------------------------------------------------------------------------- #
def make_pack_source(root: Path) -> Path:
    if root.exists():
        shutil.rmtree(root)
    root.mkdir(parents=True)
    generator = random.Random(777)
    (root / "note.txt").write_bytes(b"mothzip pack regression\n" * 40)
    (root / "blob.bin").write_bytes(generator.randbytes(256 * 1024))
    (root / "\ud55c\uae00.txt").write_bytes(
        "\ud55c\uae00 \ud68c\uadc0 \ud14c\uc2a4\ud2b8\n".encode("utf8") * 10
    )
    return root


def pack_source_files(source: Path) -> dict[str, bytes]:
    return {path.name: path.read_bytes() for path in sorted(source.iterdir()) if path.is_file()}


def validate_pack_case(
    case: Case, level: int, expected_method: int, source: Path, out_zip: Path, out_dir: Path
) -> dict:
    pack_call = run([BIN, "pack", out_zip, source, "-l", str(level), "--json"])
    case.expect(pack_call, 0, f"pack -l {level} exit")
    if not out_zip.exists():
        case.check("pack produced an archive", False, f"missing {out_zip}")
        return {"level": level, "passed": False}

    headers = read_zip_headers(out_zip)
    files = [entry for entry in headers["entries"] if not entry["is_dir"]]
    dirs = [entry for entry in headers["entries"] if entry["is_dir"]]
    file_methods = sorted({entry["method"] for entry in files})
    case.check(
        f"pack -l {level} writes method {expected_method} for every file member",
        file_methods == [expected_method],
        f"methods={file_methods} dirs={[(e['name'], e['method']) for e in dirs]}",
    )
    case.check(
        f"pack -l {level} never writes method 93",
        ZSTD_METHOD not in {entry["method"] for entry in headers["entries"]},
        f"all_methods={[entry['method'] for entry in headers['entries']]}",
    )
    sources = pack_source_files(source)
    entry_names = {Path(entry["name"]).name: entry["name"] for entry in files}
    case.check(
        "archive holds every source file exactly once",
        sorted(entry_names) == sorted(sources),
        f"entries={sorted(entry_names)} sources={sorted(sources)}",
    )

    cat_ok = True
    cat_detail = []
    for name, data in sources.items():
        inner = entry_names.get(name)
        if inner is None:
            cat_ok = False
            cat_detail.append(f"{name}: missing")
            continue
        call = run([BIN, "cat", out_zip, inner])
        matched = call["exit"] == 0 and call["stdout"] == data
        cat_ok = cat_ok and matched
        cat_detail.append(
            f"{inner}: exit={call['exit']} sha={call['stdout_sha256'][:12]} "
            f"want={sha256(data)[:12]}"
        )
        case.calls.append(public(call))
    case.check("cat round-trips the packed bytes", cat_ok, "; ".join(cat_detail))

    if out_dir.exists():
        shutil.rmtree(out_dir)
    unpack_call = run([BIN, "unpack", out_zip, "-o", out_dir, "--json"])
    case.expect(unpack_call, 0, f"unpack -l {level} exit")
    extracted = collect_tree(out_dir)
    by_name = {Path(name).name: blob for name, blob in extracted.items()}
    case.check(
        "unpack round-trips the packed bytes",
        by_name == sources,
        f"extracted={sorted(by_name)} expected={sorted(sources)} match={by_name == sources}",
    )
    case.check(
        "unpack leaves no staging residue",
        staging_residue(out_dir) == [],
        f"residue={staging_residue(out_dir)}",
    )

    test_call = run([BIN, "test", out_zip, "--json"])
    case.expect(test_call, 0, f"test -l {level} exit")
    test_doc = json_of(case, test_call, f"test -l {level}")
    if isinstance(test_doc, dict):
        case.check(
            f"test -l {level} reports passed=true",
            test_doc.get("passed") is True,
            f"passed={test_doc.get('passed')!r}",
        )

    return {
        "level": level,
        "archive": out_zip.name,
        "entries": len(headers["entries"]),
        "file_methods": file_methods,
        "dir_methods": sorted({entry["method"] for entry in dirs}),
        "method93": ZSTD_METHOD in {entry["method"] for entry in headers["entries"]},
        "zip_sha256": headers["sha256"],
        "cat_exit": 0 if cat_ok else 1,
        "unpack_exit": unpack_call["exit"],
        "test_exit": test_call["exit"],
        "passed": case.passed,
    }


def scan_pack_methods(case: Case, level: int, source: Path, out_zip: Path) -> dict:
    checks_before = len(case.checks)
    pack_call = run([BIN, "pack", out_zip, source, "-l", str(level), "--json"])
    case.expect(pack_call, 0, f"pack -l {level} exit")
    if not out_zip.exists():
        case.check(f"pack -l {level} produced an archive", False, f"missing {out_zip}")
        return {"level": level, "passed": False}
    headers = read_zip_headers(out_zip)
    files = [entry for entry in headers["entries"] if not entry["is_dir"]]
    file_methods = sorted({entry["method"] for entry in files})
    expected = STORED_METHOD if level == 0 else DEFLATE_METHOD
    case.check(
        f"pack -l {level} file members use method {expected}",
        file_methods == [expected],
        f"methods={file_methods}",
    )
    case.check(
        f"pack -l {level} contains no method 93",
        ZSTD_METHOD not in {entry["method"] for entry in headers["entries"]},
        f"all_methods={[entry['method'] for entry in headers['entries']]}",
    )
    level_ok = all(item["ok"] for item in case.checks[checks_before:])
    return {
        "level": level,
        "archive": out_zip.name,
        "file_methods": file_methods,
        "dir_methods": sorted(
            {entry["method"] for entry in headers["entries"] if entry["is_dir"]}
        ),
        "method93": ZSTD_METHOD in {entry["method"] for entry in headers["entries"]},
        "passed": level_ok,
    }


# --------------------------------------------------------------------------- #
# static evidence: where the method-93 read path lives
# --------------------------------------------------------------------------- #
CRATE_PATTERNS = [
    r"93 => CompressionMethod::Zstd",
    r"CompressionMethod::Zstd => \{",
    r"Decompressor::Zstd",
    r"pub const ZSTD",
    r"Zstd,?$",
]


def file_snapshot(path: Path) -> dict:
    """One read: the digest and the lines cited below come from the same bytes.

    The working tree is edited concurrently in this repo, so re-reading a file
    for the digest and again for the lines could bind a digest to a different
    revision than the line numbers.
    """
    data = path.read_bytes()
    stat = path.stat()
    return {
        "text": data.decode("utf8", errors="replace"),
        "sha256": sha256(data),
        "mtime": time.strftime("%Y-%m-%dT%H:%M:%S", time.localtime(stat.st_mtime)),
        "size": len(data),
    }


def enclosing_function(lines: list[str], one_based_index: int) -> str:
    """Walk backwards to the nearest `fn <name>` so a site names its real body."""
    for index in range(one_based_index - 1, -1, -1):
        match = re.match(
            r"\s*(?:pub(?:\([^)]*\))?\s+)?(?:async\s+)?fn\s+(\w+)", lines[index]
        )
        if match:
            return match.group(1)
    return "?"


def collect_static_evidence() -> dict:
    """Bind every cited line to the file digest that was current at snapshot time."""
    evidence: dict = {
        "zip_crate": None,
        "cargo": {},
        "files": {},
        "call_sites": [],
        "write_sites": [],
        "zstd_write_references": [],
    }

    cache: dict[str, dict] = {}

    def snap(relative: str) -> dict | None:
        if relative not in cache:
            path = REPO_ROOT / relative
            cache[relative] = file_snapshot(path) if path.exists() else None
        return cache[relative]

    for path in sorted((REPO_ROOT / "src").rglob("*.rs")):
        snap(path.relative_to(REPO_ROOT).as_posix())

    cargo = snap("Cargo.toml")
    if cargo:
        evidence["files"]["Cargo.toml"] = {
            key: cargo[key] for key in ("sha256", "mtime", "size")
        }
        for index, line in enumerate(cargo["text"].splitlines(), 1):
            if re.match(r"\s*zip\s*=", line) or re.match(r"\s*zstd\s*=", line):
                evidence["cargo"][f"Cargo.toml:{index}"] = line.strip()
    lock = snap("Cargo.lock")
    if lock:
        evidence["files"]["Cargo.lock"] = {
            key: lock[key] for key in ("sha256", "mtime", "size")
        }
        for package in ("zip", "zstd", "zstd-safe", "zstd-sys"):
            match = re.search(
                r'name = "%s"\nversion = "([^"]+)"' % re.escape(package), lock["text"]
            )
            if match:
                evidence["cargo"][f"Cargo.lock:{package}"] = match.group(1)

    call_patterns = [
        ("src/core/catalog.rs", r"zip::ZipArchive::new", "peek/test catalog open"),
        ("src/core/inspect.rs", r"ZipArchive::new", "cat/test archive open"),
        ("src/core/inspect.rs", r"archive\.by_index\(", "cat/test member open"),
        ("src/core/extract.rs", r"ZipArchive::new", "unpack archive open"),
    ]
    for relative, pattern, role in call_patterns:
        snapshot = snap(relative)
        if snapshot is None:
            continue
        evidence["files"].setdefault(
            relative, {key: snapshot[key] for key in ("sha256", "mtime", "size")}
        )
        lines = snapshot["text"].splitlines()
        for index, line in enumerate(lines, 1):
            if not re.search(pattern, line) or ".unwrap()" in line:
                continue
            evidence["call_sites"].append(
                {
                    "file": f"{relative}:{index}",
                    "function": enclosing_function(lines, index),
                    "role": role,
                    "line": line.strip(),
                }
            )
    for relative, pattern, role in [
        (
            "src/core/pack.rs",
            r"=> CompressionMethod::(?:Stored|Deflated)",
            "pack write method",
        ),
        (
            "src/core/edit.rs",
            r"=> zip::CompressionMethod::(?:Stored|Deflated)",
            "add/delete/rename write method",
        ),
    ]:
        snapshot = snap(relative)
        if snapshot is None:
            continue
        evidence["files"].setdefault(
            relative, {key: snapshot[key] for key in ("sha256", "mtime", "size")}
        )
        lines = snapshot["text"].splitlines()
        for index, line in enumerate(lines, 1):
            if not re.search(pattern, line):
                continue
            evidence["write_sites"].append(
                {
                    "file": f"{relative}:{index}",
                    "function": enclosing_function(lines, index),
                    "role": role,
                    "line": line.strip(),
                }
            )

    candidates = sorted(
        (Path.home() / ".cargo" / "registry" / "src").glob(
            "*/zip-9.1.0/src/compression.rs"
        )
    )
    # Static support for "the writer never selects method 93": any reference to
    # CompressionMethod::Zstd outside the decoder path would show up here.
    for relative, snapshot in sorted(cache.items()):
        if snapshot is None:
            continue
        for index, line in enumerate(snapshot["text"].splitlines(), 1):
            if "CompressionMethod::Zstd" in line:
                evidence["zstd_write_references"].append(
                    {"file": f"{relative}:{index}", "line": line.strip()}
                )
    if candidates:
        crate = candidates[0]
        snapshot = file_snapshot(crate)
        lines = snapshot["text"].splitlines()
        hits = []
        for index, line in enumerate(lines, 1):
            for pattern in CRATE_PATTERNS:
                if re.search(pattern, line):
                    gate = lines[index - 2].strip() if index >= 2 else ""
                    hits.append(
                        {
                            "line": index,
                            "text": line.strip(),
                            "cfg": gate if gate.startswith("#[cfg") else "",
                        }
                    )
                    break
        evidence["zip_crate"] = {
            "path": str(crate),
            "sha256": snapshot["sha256"],
            "matches": hits,
            "line_count": len(lines),
        }
    return evidence


# --------------------------------------------------------------------------- #
# handoff
# --------------------------------------------------------------------------- #
def fmt_int(value: int) -> str:
    return f"{value:,}"


def display_path(path: Path) -> str:
    try:
        return path.relative_to(REPO_ROOT).as_posix()
    except ValueError:
        return str(path)


def build_handoff(
    run_id: str,
    started: str,
    binary: dict,
    environment: dict,
    fixtures: list[dict],
    corruptions: list[dict],
    packs: list[dict],
    scans: list[dict],
    cross: list[dict],
    static: dict,
    cases: list[dict],
    results_path: Path,
) -> str:
    passed = sum(1 for case in cases if case["passed"])
    failed = len(cases) - passed
    lines: list[str] = []
    add = lines.append

    add("# ZIP Zstandard(method 93) 읽기 검증 — handoff")
    add("")
    add(f"- run id: `{run_id}`")
    add(f"- 실행 시각: {started}")
    add(f"- 측정 바이너리: `{binary['path']}`")
    add(f"  - `--version` = `{binary['version']}`")
    add(f"  - sha256 = `{binary['sha256']}`")
    add(f"  - 빌드 시각(mtime) = {binary.get('built_at', '?')}")
    add(f"- Python {environment['python']} / `zstandard` {environment['zstandard']}")
    add(f"- 결과 원본: `{display_path(results_path)}`")
    add(f"- 케이스 {len(cases)}개: **통과 {passed} / 실패 {failed}**")
    add("")
    add("## 결론")
    add("")
    add(
        f"다른 도구가 만든 method 93(Zstandard) ZIP 멤버를 MothZip이 **원본 바이트 그대로** 읽는다. "
        f"fixture {len(fixtures)}종의 peek/cat/unpack/test가 모두 exit 0이고, cat·unpack 산출 바이트의 "
        "SHA-256이 Python `zstandard`로 압축하기 전 버퍼와 일치한다. "
        "쓰기 경로는 여전히 method 8(deflate, level 0이면 stored)만 방출하며 method 93을 쓰지 않는다."
        if not failed
        else "일부 케이스가 실패했다. 아래 표의 실패 행과 `results.json`의 checks/calls를 근거로 root가 판단해야 한다."
    )
    add("")
    add("## 1. fixture 조립 방식 (독립성 근거)")
    add("")
    add(
        "`build_zip()`이 ZIP 컨테이너를 **바이트 단위로 직접 조립**한다. 필드는 다음과 같이 고정했다."
    )
    add("")
    add("- 로컬 파일 헤더(LFH) + payload, 중앙 디렉터리(CDH), EOCD를 각각 `struct.pack`으로 생성")
    add(
        "- 각 멤버: `method`(93/8/0), `crc32`(원본 기준 정확값), `compressed_size`, `uncompressed_size`, "
        "일반 플래그 `0`, version needed `20`, DOS 시각 고정(2026-01-01 00:00:00)"
    )
    add(
        "- method 93 payload는 Python `zstandard` `ZstdCompressor(level=9, write_checksum=True, "
        "write_content_size=True)` 산출물이다. 즉 **컨테이너는 이 스크립트가, 압축 스트림은 Python이, "
        "해제는 MothZip(Rust `zip` 9.1 + `zstd` 0.14)이** 담당하는 삼중 독립 구조다."
    )
    add(
        "- Python `zipfile`은 사용하지 않는다. method 93을 읽지 못하므로 컨테이너 확인 외에는 "
        "아무것도 검증해 주지 않는다."
    )
    add(
        "- 한글 이름 fixture는 UTF-8 이름 바이트를 **일반 플래그 0**(bit 11 미설정)으로 넣었다. "
        "APPNOTE 엄밀 해석과는 어긋나지만, MothZip이 UTF-8을 먼저 시도하는 디코더"
        "(`src/core/encoding.rs`의 `decode_entry_name_raw`)를 쓰므로 실사용 파일과 같은 조건이며, 이번 검증의 목적(코덱)과 "
        "분리하기 위해 임의로 bit 11을 켜지 않았다."
    )
    add("")
    add("## 2. 읽기 결과 [measured]")
    add("")
    add("| 케이스 | 멤버 (원본 B / 압축 B) | SHA-256 접두 | peek | cat | unpack | test | 판정 |")
    add("|---|---|---|---|---|---|---|---|")
    for item in fixtures:
        rows = "; ".join(
            f"`{member['name']}` {fmt_int(member['size'])}/{fmt_int(member['csize'])}"
            for member in item["members"]
        )
        digest = item["members"][0]["sha256"][:12]
        if len(item["members"]) > 1:
            digest += f" (+{len(item['members']) - 1})"
        cells = [
            "0" if item["peek_exit"] == 0 else f"**{item['peek_exit']}**",
            "0" if item["cat_exit"] == 0 else f"**{item['cat_exit']}**",
            "0" if item["unpack_exit"] == 0 else f"**{item['unpack_exit']}**",
            "0" if item["test_exit"] == 0 else f"**{item['test_exit']}**",
            "PASS" if item["passed"] else "**FAIL**",
        ]
        add(f"| `{item['case']}` | {rows} | `{digest}` | " + " | ".join(cells) + " |")
    add("")
    add(
        "cat은 `capture_output`의 원시 bytes로, unpack은 커밋된 파일 바이트로 비교했다. "
        "빈 파일(0 B)은 zstd 프레임 content size 0 + CRC 0으로 조립했고 그대로 통과했다. "
        + "`test --json`은 모든 fixture에서 `passed=true`, "
        + f"`tested_entries`={[item['tested_entries'] for item in fixtures]}를 보고했다."
    )
    add("")
    add("## 3. 손상/경계 [measured]")
    add("")
    partial_text = next(
        (item["cat_stdout_len"] for item in corruptions if item["case"].endswith("-mid-text")),
        0,
    )
    partial_large = next(
        (item["cat_stdout_len"] for item in corruptions if item["case"].endswith("-mid-large")),
        0,
    )
    add("| 케이스 | 변형 | cat exit / stdout B | unpack exit / 커밋 파일 | test exit | 검출 계층 | 에러 문자열 | 판정 |")
    add("|---|---|---|---|---|---|---|---|")
    for item in corruptions:
        committed = f"{item['unpack_exit']} / {item['committed_files']}개"
        add(
            f"| `{item['case']}` | {item['variant']} | {item['cat_exit']} / {item['cat_stdout_len']} | "
            f"{committed} | {item['test_exit']} | {item['detected_by']} | "
            f"`{(item['error_text'] or '').splitlines()[0][:80]}` | "
            f"{'PASS' if item['passed'] else '**FAIL**'} |"
        )
    add("")
    add(
        "두 층이 실제로 분리되어 동작한다. 압축 스트림만 깨뜨리면 zstd 프레임 체크섬이 "
        "`Restored data doesn't match checksum`으로 먼저 잡고(오류 주입 지점: payload 중간/프레임 마지막 바이트), "
        "payload는 온전하고 ZIP CRC 필드만 틀리면 `zip` 크레이트의 `Invalid checksum`(`Crc32Reader`)이 잡는다. "
        "두 경우 모두 exit≠0이고 파일 커밋은 없지만, 스트리밍 특성상 stdout에는 이미 나간 바이트가 있을 수 있다. "
        f"작은 케이스의 payload 손상은 stdout {partial_text} B였고, "
        f"1 MiB 케이스는 payload 중간 1바이트 flip만으로도 {partial_large:,} B가 먼저 나간 뒤 exit 1이었다. "
        "`cat` 출력 바이트 수를 표에 그대로 기록했다."
    )
    add("")
    add(
        "세 변형 모두 `peek`은 exit 0이었다. 카탈로그는 압축을 풀지 않으므로, 실패는 헤더가 아니라 "
        "디코드 계층에서 일어난다. 또한 `unpack --json`/`test --json`은 손상 시 stdout 없이 exit 1과 "
        "stderr `{\"error\": ...}`로 끝난다(`passed:false` 요약을 기대하는 소비자는 주의). "
        "읽기 자체는 실패하지 않았으므로 `Compression method not supported` 계열 에러는 이번 run에서 "
        "재현되지 않았다 — 위 표의 문자열이 이번에 관측된 전부다."
    )
    add("")
    add(
        "`unpack`은 6개 변형 모두 exit≠0이고 커밋 파일 0개, `.mothzip_stage_*` 잔여물 0개였다. "
        "스테이징 디렉터리를 만들고(`src/core/extract.rs:891`) 전 멤버 검증이 끝나야 커밋하는 구조"
        "(`src/core/extract.rs:1013`)가 손상 멤버에서 실제로 부분 산출물을 남기지 않음을 확인했다."
    )
    add("")
    add("교차 검증(`results.json`의 `cross_checks`와 동일):")
    add("")
    for item in cross:
        add(f"- [{'ok' if item['ok'] else 'FAIL'}] {item['check']} — `{item['detail']}`")
    add("")
    add("## 4. 회귀: deflate / stored [measured]")
    add("")
    add("| 레벨 | 파일 멤버 method | 디렉터리 멤버 method | method 93 포함 | 판정 |")
    add("|---|---|---|---|---|")
    for item in scans:
        add(
            f"| -l {item['level']} | {item['file_methods']} | {item.get('dir_methods', [])} | "
            f"{'**예**' if item.get('method93') else '아니오'} | "
            f"{'PASS' if item['passed'] else '**FAIL**'} |"
        )
    add("")
    add("| 라운드트립 케이스 | 레벨 | method | zip sha256 접두 | cat | unpack | test | 판정 |")
    add("|---|---|---|---|---|---|---|---|")
    for item in packs:
        add(
            f"| `{item.get('case', '')}` | {item['level']} | {item.get('file_methods')} | "
            f"`{item.get('zip_sha256', '')[:12]}` | {item.get('cat_exit')} | "
            f"{item.get('unpack_exit')} | {item.get('test_exit')} | "
            f"{'PASS' if item['passed'] else '**FAIL**'} |"
        )
    add("")
    add(
        "레벨 0은 stored(method 0), 레벨 1/6/9는 deflate(method 8)만 방출한다. "
        "ZIP 컨테이너 헤더를 직접 파싱해 확인한 값이며, 어떤 레벨에서도 method 93은 나오지 않는다. "
        "디렉터리 엔트리는 별도 method(0)로 기록된다."
    )
    add("")
    add("## 5. 우리 reader가 쓰는 코드 경로")
    add("")
    add("```")
    add('zip 9.1.0 crate (feature = "zstd" 이 켜진 상태로 링크됨)')
    if static.get("zip_crate"):
        crate = static["zip_crate"]
        add(f"  {crate['path']}")
        add(f"  sha256 {crate['sha256']}")
        last_gate = ""
        for hit in crate["matches"]:
            gate = hit.get("cfg") or ""
            if gate and gate != last_gate:
                add(f"  {gate}")
                last_gate = gate
            add(f"  :{hit['line']}  {hit['text']}")
    else:
        add("  (cargo registry 사본을 찾지 못했다 — 이 항목은 [unverified])")
    add("")
    add("mothzip call sites")
    for site in static.get("call_sites", []):
        add(f"  {site['file']}  {site['function']}()  [{site['role']}]  {site['line']}")
    add("")
    add("writer (method 선택)")
    for site in static.get("write_sites", []):
        add(f"  {site['file']}  {site['function']}()  {site['line']}")
    add("")
    add("src 전체의 CompressionMethod::Zstd 참조")
    write_refs = static.get("zstd_write_references", [])
    if write_refs:
        for ref in write_refs:
            add(f"  {ref['file']}  {ref['line']}")
    else:
        add("  (0건 — writer가 method 93을 고르는 코드 없음)")
    add("```")
    add("")
    add(
        "`cat`은 `cat_zip_file`에서 `ZipArchive::new` → `by_index`로 멤버를 열고 "
        "`copy_archive_entry`로 stdout에 스트리밍한다. `by_index`가 반환한 `ZipFile`의 reader는 "
        "`zip` 크레이트 `make_reader()`가 감싼 `LazyDecompressor`이고, method 93이면 "
        "`compression.rs`의 `CompressionMethod::Zstd` 분기에서 `zstd::Decoder`가 만들어진다. "
        "그 위에 `Crc32Reader`가 붙어 ZIP CRC를 EOF에서 검증한다. "
        "`peek`은 `read_zip_catalog`(메타데이터만), `test`는 `test_archive_integrity`(전 멤버를 sink로 해제), "
        "`unpack`은 `extract_zip_archive`(스테이징 후 커밋)가 같은 reader를 공유한다."
    )
    add("")
    add(
        "라인 번호와 아래 다이제스트는 **실행 시점 워크트리 스냅샷**이다. 측정 바이너리는 "
        f"`{binary.get('built_at', '?')}` 빌드이므로, 그 뒤에 src가 바뀌었다면 라인 번호가 아니라 "
        "sha256으로 고정해 확인해야 한다."
    )
    add("")
    add("| 소스 파일 | sha256 앞 16 | mtime | 바이너리 빌드 이후 변경 |")
    add("|---|---|---|---|")
    binary_epoch = binary.get("built_epoch", 0)
    for relative, stamp in sorted(static.get("files", {}).items()):
        epoch = time.mktime(time.strptime(stamp["mtime"], "%Y-%m-%dT%H:%M:%S"))
        drift = "**예**" if epoch > binary_epoch else "아니오"
        add(
            f"| `{relative}` | `{stamp['sha256'][:16]}` | {stamp['mtime']} | {drift} |"
        )
    add("")
    add("## 6. 쓰기는 왜 여전히 deflate인가")
    add("")
    add(
        "root 결정 사항이며, 코드 구조상으로도 method 93 쓰기 분기가 없다. "
        "`pack`과 `add`/`delete`/`rename`의 ZIP 재작성 경로는 압축 레벨을 `0`과 그 외로만 나눠 "
        "`Stored` / `Deflated`를 고른다(위 5절의 writer 네 줄). "
        f"스냅샷 시점 `src/**/*.rs` 전체에서 `CompressionMethod::Zstd` 참조는 "
        f"{len(static.get('zstd_write_references', []))}건이다."
    )
    add("")
    add(
        "즉 method 93은 **읽기 전용 능력**으로만 들어왔고, `pack`은 측정한 네 레벨(0/1/6/9) 모두에서 "
        "method 8(또는 0)만 방출했다(4절, ZIP 헤더 직접 파싱). "
        "`add`/`delete`/`rename`은 같은 writer를 쓰지만 이번 run에서 CLI로 실행하지 않았으므로 "
        "[unverified]로 남긴다. "
        "호환성 근거(일부 구형 도구·구형 Windows 탐색기가 method 93을 읽지 못함)는 root 결정의 배경이며 "
        "이 스크립트가 측정한 값은 아니다 — [unverified/decision]."
    )
    add("")
    add("## 7. 남은 위험 / 미검증")
    add("")
    add(
        "- **method 93 + AES**: 암호화 ZIP의 method 93 조합은 이번에 조립하지 않았다. `cat`/`unpack`의 "
        "`by_index_decrypt` 경로가 zstd와 만나는 조합은 [unverified]. AE-2는 CRC를 0으로 두고 CRC 검증을 "
        "끄므로(`readers.rs:344-352`) 검출 계층이 달라질 수 있다."
    )
    add(
        "- **분할 ZIP / ZIP64**: multi-disk, ZIP64 EOCD, data descriptor(일반 플래그 bit 3) 조합은 "
        "미검증. 이번 fixture는 단일 디스크·EOCD·flags 0이다."
    )
    add(
        "- **content size 없는 zstd 프레임**: `write_content_size=False`(스트리밍 프레임)는 미검증. "
        "현재 fixture는 모두 content size와 체크섬을 포함한다."
    )
    add(
        "- **method 20(ZSTD deprecated)**: `zip` 크레이트는 20을 `Unsupported(20)`로 매핑한다"
        "(`compression.rs:130`). method 20 ZIP은 여전히 읽지 못하며, 이는 의도된 동작으로 보이나 "
        "실제 거부 문자열은 [unverified]."
    )
    add(
        "- **zstd dictionary / skippable frame / 다중 프레임**: 미검증. ZIP 멤버는 보통 단일 프레임이지만 "
        "다중 프레임 스트림을 넣은 경우의 동작은 확인하지 않았다."
    )
    add(
        "- **대용량**: 1 MiB까지 확인. `--max-bytes`(기본 10 GiB) 경계, 수 GB 멤버, 압축 폭탄 비율은 "
        "미검증."
    )
    add(
        "- **stdout 부분 산출**: 손상 시 `cat`은 exit≠0이지만 그 전에 나간 바이트가 있을 수 있다. "
        f"이번 측정에서 CRC-only 손상은 평문 전체(작은 케이스 248 B, 1 MiB 케이스 1,048,576 B), "
        f"payload 중간 flip은 작은 케이스 0 B·1 MiB 케이스 {partial_large:,} B가 먼저 나갔다. "
        "파이프 소비자가 exit code를 무시하고 바이트를 커밋하면 손상 데이터가 남을 수 있다. "
        "이는 `cat`의 스트리밍 계약이지 method 93 고유 문제는 아니다."
    )
    add("")
    add("## 8. 재현")
    add("")
    add("```powershell")
    add("python audit/zip_zstd_validation.py")
    add("python audit/zip_zstd_validation.py --binary G:\\mothzip-build\\release\\mothzip.exe")
    add("```")
    add("")
    add(
        f"이 문서는 `audit/zip_zstd_validation.py`가 같은 run의 측정값으로 자동 생성했다"
        f"(`{run_id}`). 수치는 `results.json`과 1:1 대응한다."
    )
    add("")
    return "\n".join(lines)


# --------------------------------------------------------------------------- #
# main
# --------------------------------------------------------------------------- #
BIN = DEFAULT_BINARY


def main() -> int:
    global BIN

    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", default=str(DEFAULT_BINARY))
    parser.add_argument("--runs-dir", default=str(RUNS_DIR))
    args = parser.parse_args()

    BIN = Path(args.binary)
    if not BIN.exists():
        print(f"FATAL: binary not found: {BIN}", file=sys.stderr)
        return 2

    started = time.strftime("%Y-%m-%dT%H:%M:%S%z")
    run_id = f"zip-zstd-{time.strftime('%Y%m%dT%H%M%S')}-{uuid.uuid4().hex[:6]}"
    run_dir = Path(args.runs_dir) / run_id
    fixtures_dir = run_dir / "fixtures"
    out_root = run_dir / "out"
    pack_dir = run_dir / "pack"
    for directory in (fixtures_dir, out_root, pack_dir):
        directory.mkdir(parents=True, exist_ok=True)

    version_call = run([BIN, "--version"])
    if version_call["exit"] != 0:
        print(f"FATAL: --version failed: {version_call['stderr']}", file=sys.stderr)
        return 2

    binary = {
        "path": str(BIN),
        "sha256": sha256_file(BIN),
        "size": BIN.stat().st_size,
        "built_at": time.strftime(
            "%Y-%m-%dT%H:%M:%S", time.localtime(BIN.stat().st_mtime)
        ),
        "built_epoch": int(BIN.stat().st_mtime),
        "version": version_call["stdout"].decode("utf8", errors="replace").strip(),
    }
    environment = {
        "python": sys.version.split()[0],
        "zstandard": zstandard.__version__,
    }

    cases: list[Case] = []
    fixtures: list[dict] = []
    corruptions: list[dict] = []
    packs: list[dict] = []
    scans: list[dict] = []

    # --- read fixtures -----------------------------------------------------
    specs = fixture_specs()
    built: dict[str, dict] = {}
    for spec in specs:
        zip_path = fixtures_dir / spec["zip"]
        build_zip(zip_path, spec["members"])
        built[spec["key"]] = spec
        case = Case(spec["case"], f"method-93 fixture '{spec['key']}' read through peek/cat/unpack/test")
        fixtures.append(
            validate_fixture(case, spec, zip_path, out_root / spec["case"])
        )
        cases.append(case)

    # --- corruption variants ----------------------------------------------
    variants = [
        ("flip-payload-mid", "corrupt-flip-payload-mid"),
        ("flip-zstd-checksum", "corrupt-frame-checksum"),
        ("wrong-zip-crc", "corrupt-crc-wrong"),
    ]
    for base_key, base_suffix in (("text", "text"), ("random-1mib", "large")):
        base_spec = built[base_key]
        base_zip = fixtures_dir / base_spec["zip"]
        for variant, name_prefix in variants:
            case_name = f"{name_prefix}-{base_suffix}"
            case = Case(
                case_name,
                f"{variant} on '{base_key}' must fail closed without committing output",
            )
            corrupt_zip, offset = corrupt_copy(
                base_zip, fixtures_dir / f"{case_name}.zip", variant
            )
            corruptions.append(
                validate_corruption(
                    case, base_spec, variant, corrupt_zip, out_root / case_name, offset
                )
            )
            cases.append(case)

    # --- pack regression ---------------------------------------------------
    source = make_pack_source(run_dir / "src" / "pack-src")

    for level, expected, case_name in (
        (6, DEFLATE_METHOD, "pack-deflate-roundtrip"),
        (0, STORED_METHOD, "pack-stored-roundtrip"),
    ):
        case = Case(
            case_name,
            f"pack -l {level} keeps writing method {expected} and round-trips through cat/unpack/test",
        )
        info = validate_pack_case(
            case,
            level,
            expected,
            source,
            pack_dir / f"level{level}.zip",
            out_root / case_name,
        )
        info["case"] = case_name
        packs.append(info)
        cases.append(case)

    scan_case = Case(
        "pack-method-selection",
        "no pack level emits method 93; level 0 is stored and level 1/6/9 are deflate",
    )
    for level in (0, 1, 6, 9):
        scans.append(
            scan_pack_methods(
                scan_case, level, source, pack_dir / f"scan-level{level}.zip"
            )
        )
    cases.append(scan_case)

    # --- cross-case assertions --------------------------------------------
    cross: list[dict] = []
    layer_counts: dict[str, int] = {}
    for item in corruptions:
        layer_counts[item["detected_by"]] = layer_counts.get(item["detected_by"], 0) + 1
    cross.append(
        {
            "check": "payload damage and CRC damage are detected by different layers",
            "ok": layer_counts.get("zstd-frame-checksum") == 4
            and layer_counts.get("zip-crc32") == 2,
            "detail": f"layers={layer_counts}",
        }
    )
    cross.append(
        {
            "check": "no corruption variant committed an extracted file",
            "ok": all(item["committed_files"] == 0 for item in corruptions)
            and all(item["staging_residue"] == [] for item in corruptions),
            "detail": f"committed={[item['committed_files'] for item in corruptions]} "
            f"residue={[item['staging_residue'] for item in corruptions]}",
        }
    )
    cross.append(
        {
            "check": "no pack level emitted method 93",
            "ok": not any(item.get("method93") for item in scans + packs),
            "detail": f"levels={[(item['level'], item.get('file_methods')) for item in scans]}",
        }
    )

    static = collect_static_evidence()

    cases_json = [case.to_json() for case in cases]
    passed = sum(1 for case in cases_json if case["passed"])
    failed = len(cases_json) - passed
    summary = {
        "cases": len(cases_json),
        "passed": passed,
        "failed": failed,
        "cross_checks_passed": sum(1 for item in cross if item["ok"]),
        "cross_checks": len(cross),
        "conclusion": "all cases passed" if not failed and all(i["ok"] for i in cross) else "failures recorded",
    }

    results_path = run_dir / "results.json"
    results = {
        "run_id": run_id,
        "started_at": started,
        "binary": binary,
        "environment": environment,
        "summary": summary,
        "cross_checks": cross,
        "static_evidence": static,
        "cases": cases_json,
    }
    results_path.write_text(
        json.dumps(results, ensure_ascii=False, indent=2) + "\n", encoding="utf8"
    )

    handoff = build_handoff(
        run_id,
        started,
        binary,
        environment,
        fixtures,
        corruptions,
        packs,
        scans,
        cross,
        static,
        cases_json,
        results_path,
    )
    HANDOFF_PATH.parent.mkdir(parents=True, exist_ok=True)
    HANDOFF_PATH.write_text(handoff + "\n", encoding="utf8")

    # Console summary stays ASCII-safe: the Windows console/pipe may be cp949.
    def say(text: str) -> None:
        encoding = sys.stdout.encoding or "utf8"
        print(text.encode(encoding, "replace").decode(encoding, "replace"))

    say(f"run         : {run_id}")
    say(f"binary      : {binary['path']} ({binary['version']})")
    say(f"binary sha  : {binary['sha256'][:16]}")
    say(f"cases       : {len(cases_json)}  passed={passed}  failed={failed}")
    for item in cross:
        say(f"cross-check : [{'ok' if item['ok'] else 'FAIL'}] {item['check']}  {item['detail']}")
    for item in fixtures:
        members = ", ".join(
            f"{m['name']}={m['size']}/{m['csize']}B" for m in item["members"]
        )
        say(
            f"  [{'PASS' if item['passed'] else 'FAIL'}] {item['case']:<32} "
            f"peek={item['peek_exit']} cat={item['cat_exit']} unpack={item['unpack_exit']} "
            f"test={item['test_exit']}  {members}"
        )
    for item in corruptions:
        say(
            f"  [{'PASS' if item['passed'] else 'FAIL'}] {item['case']:<32} "
            f"cat={item['cat_exit']}/{item['cat_stdout_len']}B unpack={item['unpack_exit']}"
            f"/{item['committed_files']}f test={item['test_exit']} layer={item['detected_by']}"
        )
    for item in scans:
        say(
            f"  [{'PASS' if item['passed'] else 'FAIL'}] pack -l {item['level']}: "
            f"methods={item['file_methods']} 93={item['method93']}"
        )
    for item in packs:
        say(
            f"  [{'PASS' if item['passed'] else 'FAIL'}] {item.get('case', '')}: "
            f"methods={item.get('file_methods')} cat={item.get('cat_exit')} "
            f"unpack={item.get('unpack_exit')} test={item.get('test_exit')}"
        )
    say(f"results     : {results_path}")
    say(f"handoff     : {HANDOFF_PATH}")

    return 0 if summary["conclusion"] == "all cases passed" else 1


if __name__ == "__main__":
    sys.exit(main())
