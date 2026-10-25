#!/usr/bin/env python3
"""Measure the MothZip *edit CLI surface* end to end against the installed binary.

The measured artifact is a fixed install of ``mothzip.exe`` 1.1.0, not a fresh build.
Nothing is compiled here: every call drives the real executable, and every PASS is
grounded in the process exit code plus SHA-256 of actual archive / member bytes.

Covered contract:
  * ZIP and 7z add / delete / rename, including directory subtrees
  * original-archive preservation on every expected failure (SHA-256 unchanged)
  * explicit rejection of RAR / TAR / TAR.GZ / .zst / split 7z / SFX
  * encrypted ZIP and 7z add/delete/rename through --password-stdin, wrong and
    missing passwords, and the no-prompt guarantee of --non-interactive
  * --json success on stdout, failure as {"error": ...} on stderr, exit codes 0/1/2
  * hostile rename destinations (traversal, drive/ADS colon, DOS device, empty,
    trailing slash) with a filesystem snapshot of the whole run directory

Usage:
    python audit/edit_cli_validation.py [--binary PATH]

Writes ``results.json`` under ``audit/runs/edit-cli-<timestamp>-<rand>/``.
Exit code: 0 when every case passes, 1 when any case fails, 2 on setup failure.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import random
import shutil
import subprocess
import sys
import time
import uuid
from pathlib import Path

EXPECTED_BINARY_SHA256 = "17c8dd334c1de189623d0bc506580f2340bf4827b1f46c0dd7be47ad3f0fb162"
DEFAULT_BINARY = Path(
    r"C:\Users\Administrator\AppData\Local\Programs\MothZip\mothzip.exe"
)
PASSWORD = "edit-cli-pass"
WRONG_PASSWORD = "edit-cli-wrong"
CALL_TIMEOUT_SECONDS = 120
PROMPT_BUDGET_MS = 15_000

WORKSPACE = Path(__file__).resolve().parents[1]
STAMP = time.strftime("%Y%m%d-%H%M%S")
RUN = WORKSPACE / "audit" / "runs" / f"edit-cli-{STAMP}-{uuid.uuid4().hex[:6]}"
FIXTURES = RUN / "fixtures"
CASES_DIR = RUN / "cases"
HOSTILE_DIR = RUN / "hostile"

BINARY = DEFAULT_BINARY
SETUP_CALLS: list[dict] = []
CASES: list[dict] = []
FACTS: dict = {}

NEW_BYTES = b"NEW-FILE-CONTENT\n"
REPLACED_BYTES = b"AAA-replaced-by-add\n"
PAYLOAD = random.Random(20261026).randbytes(200_000)

BASE_ITEMS = {
    "src/a.txt": b"AAA-base\n",
    "src/b.txt": b"BBB-base\n",
    "src/dir/c.txt": b"CCC-base\n",
    "src/dir/payload.bin": PAYLOAD,
}
FLAT_ITEMS = {
    "a.txt": b"AAA-base\n",
    "b.txt": b"BBB-base\n",
}


# --------------------------------------------------------------------------- calls
class Call:
    def __init__(
        self,
        argv: list[str],
        exit_code: int | None,
        out_bytes: bytes,
        err_bytes: bytes,
        elapsed_ms: int,
        timed_out: bool,
        raw: bool,
        cwd: str | None,
    ) -> None:
        self.argv = argv
        self.exit = exit_code
        self.out_bytes = out_bytes
        self.err_bytes = err_bytes
        self.elapsed_ms = elapsed_ms
        self.timed_out = timed_out
        self.raw = raw
        self.cwd = cwd
        self.out_text = out_bytes.decode("utf8", "replace")
        self.err_text = err_bytes.decode("utf8", "replace")

    def summary(self) -> dict:
        if self.raw:
            stdout = (
                f"<{len(self.out_bytes)} bytes "
                f"sha256={hashlib.sha256(self.out_bytes).hexdigest()}>"
            )
        else:
            stdout = self.out_text[:4000]
        entry = {
            "argv": self.argv,
            "exit": self.exit,
            "stdout": stdout,
            "stderr": self.err_text[:2000],
            "elapsed_ms": self.elapsed_ms,
        }
        if self.timed_out:
            entry["timed_out"] = True
        if self.raw:
            entry["stdout_sha256"] = hashlib.sha256(self.out_bytes).hexdigest()
            entry["stdout_bytes"] = len(self.out_bytes)
        return entry


def execute(
    args: list[object],
    *,
    stdin_data: bytes | None = None,
    raw: bool = False,
    cwd: Path | None = None,
    timeout: int = CALL_TIMEOUT_SECONDS,
) -> Call:
    argv = [str(BINARY), *[str(arg) for arg in args]]
    started = time.perf_counter()
    try:
        if stdin_data is not None:
            finished = subprocess.run(
                argv,
                capture_output=True,
                timeout=timeout,
                input=stdin_data,
                cwd=str(cwd) if cwd else None,
            )
        else:
            finished = subprocess.run(
                argv,
                capture_output=True,
                timeout=timeout,
                stdin=subprocess.DEVNULL,
                cwd=str(cwd) if cwd else None,
            )
        return Call(
            argv,
            finished.returncode,
            finished.stdout,
            finished.stderr,
            int((time.perf_counter() - started) * 1000),
            False,
            raw,
            str(cwd) if cwd else None,
        )
    except subprocess.TimeoutExpired as expired:
        return Call(
            argv,
            None,
            expired.stdout or b"",
            expired.stderr or b"",
            int((time.perf_counter() - started) * 1000),
            True,
            raw,
            str(cwd) if cwd else None,
        )


class Case:
    def __init__(self, name: str) -> None:
        self.name = name
        self.calls: list[Call] = []
        self.facts: dict = {}
        self.note: str | None = None

    def run(self, *args: object, **kwargs) -> Call:
        call = execute(list(args), **kwargs)
        self.calls.append(call)
        return call

    def fact(self, key: str, value: object) -> None:
        self.facts[key] = value


# ----------------------------------------------------------------------- utilities
def require(condition: bool, message: str) -> None:
    if not condition:
        raise AssertionError(message)


def sha256_file(path: Path) -> str:
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def expect_exit(call: Call, expected: int, label: str) -> None:
    detail = f"stderr={call.err_text[:400]!r}"
    if call.timed_out:
        raise AssertionError(f"{label}: timed out after {call.elapsed_ms} ms")
    require(call.exit == expected, f"{label}: expected exit {expected}, got {call.exit}; {detail}")


def expect_success(call: Call, label: str) -> None:
    expect_exit(call, 0, label)


def error_object(call: Call, label: str) -> dict:
    require(call.exit not in (0, None), f"{label}: expected a nonzero exit, got {call.exit}")
    require(call.exit == 1, f"{label}: processing failure must exit 1, got {call.exit}")
    require(call.err_text.strip() != "", f"{label}: failure wrote nothing to stderr")
    try:
        payload = json.loads(call.err_text)
    except json.JSONDecodeError as error:
        raise AssertionError(
            f"{label}: stderr is not a JSON object ({error}); stderr={call.err_text[:400]!r}"
        ) from error
    require(isinstance(payload, dict), f"{label}: stderr JSON is not an object: {payload!r}")
    require(
        isinstance(payload.get("error"), str) and payload["error"] != "",
        f"{label}: stderr JSON has no non-empty 'error' string: {payload!r}",
    )
    return payload


def expect_rejected(call: Call, label: str) -> str:
    payload = error_object(call, label)
    return payload["error"]


def expect_preserved(path: Path, before_sha: str, label: str) -> None:
    after_sha = sha256_file(path)
    require(
        after_sha == before_sha,
        f"{label}: original archive changed on failure ({before_sha} -> {after_sha})",
    )


def snapshot_files(base: Path) -> set[str]:
    return {str(p.relative_to(base)) for p in base.rglob("*") if p.is_file()}


def catalog(case: Case, archive: Path, password: str | None = None) -> dict[str, bool]:
    args: list[object] = [
        "peek",
        archive,
        "--depth",
        "10",
        "--limit",
        "1000",
        "--json",
        "--non-interactive",
    ]
    stdin_data = None
    if password is not None:
        args.append("--password-stdin")
        stdin_data = password.encode("utf8") + b"\n"
    call = case.run(*args, stdin_data=stdin_data)
    expect_success(call, f"peek {archive.name}")
    try:
        payload = json.loads(call.out_text)
    except json.JSONDecodeError as error:
        raise AssertionError(f"peek {archive.name}: stdout is not JSON: {error}") from error
    require(
        int(payload.get("truncated_items", 0)) == 0,
        f"peek {archive.name}: tree was truncated: {payload.get('truncated_items')}",
    )
    members: dict[str, bool] = {}
    for entry in payload.get("tree_summary", []):
        members[str(entry["path"]).rstrip("/")] = bool(entry["is_dir"])
    total = int(payload.get("total_entries", -1))
    require(
        len(members) == total,
        f"peek {archive.name}: tree lists {len(members)} of {total} entries",
    )
    return members


def cat_bytes(
    case: Case, archive: Path, member: str, password: str | None = None
) -> bytes:
    args: list[object] = ["cat", archive, member, "--non-interactive"]
    stdin_data = None
    if password is not None:
        args.append("--password-stdin")
        stdin_data = password.encode("utf8") + b"\n"
    call = case.run(*args, stdin_data=stdin_data, raw=True)
    expect_success(call, f"cat {archive.name}:{member}")
    return call.out_bytes


def test_archive(case: Case, archive: Path, password: str | None = None) -> dict:
    args: list[object] = ["test", archive, "--json", "--non-interactive"]
    stdin_data = None
    if password is not None:
        args.append("--password-stdin")
        stdin_data = password.encode("utf8") + b"\n"
    call = case.run(*args, stdin_data=stdin_data)
    expect_success(call, f"test {archive.name}")
    payload = json.loads(call.out_text)
    require(payload.get("passed") is True, f"test {archive.name}: passed={payload.get('passed')}")
    return payload


def fresh_copy(case_name: str, fixture_name: str, *, hostile: bool = False) -> Path:
    target_dir = (HOSTILE_DIR if hostile else CASES_DIR) / case_name
    target_dir.mkdir(parents=True, exist_ok=True)
    source = FIXTURES / fixture_name
    destination = target_dir / fixture_name
    shutil.copy2(source, destination)
    return destination


def fresh_split(case_name: str) -> tuple[Path, list[Path]]:
    target_dir = CASES_DIR / case_name
    target_dir.mkdir(parents=True, exist_ok=True)
    copied: list[Path] = []
    for volume in sorted(FIXTURES.glob("split.7z.*")):
        destination = target_dir / volume.name
        shutil.copy2(volume, destination)
        copied.append(destination)
    return target_dir / "split.7z.001", copied


def record_setup(step: str, call: Call) -> None:
    entry = call.summary()
    entry["step"] = step
    SETUP_CALLS.append(entry)


def setup_pack(
    step: str,
    output: Path,
    sources: list[Path],
    *,
    password: str | None = None,
    volume_bytes: int | None = None,
) -> None:
    args: list[object] = ["pack", output, *sources]
    if password is not None:
        args += ["-p", password]
    if volume_bytes is not None:
        args += ["--volume-bytes", str(volume_bytes)]
    args += ["--json", "--non-interactive"]
    call = execute(args)
    record_setup(step, call)
    expect_success(call, f"setup pack {step}")
    expected_output = output if volume_bytes is None else Path(str(output) + ".001")
    require(expected_output.exists(), f"setup pack {step}: output {expected_output} missing")


# ------------------------------------------------------------------------ fixtures
def build_fixtures() -> None:
    FIXTURES.mkdir(parents=True, exist_ok=True)
    CASES_DIR.mkdir(parents=True, exist_ok=True)
    HOSTILE_DIR.mkdir(parents=True, exist_ok=True)

    src = FIXTURES / "src"
    (src / "dir").mkdir(parents=True, exist_ok=True)
    for member, content in BASE_ITEMS.items():
        (FIXTURES / member).parent.mkdir(parents=True, exist_ok=True)
        (FIXTURES / member).write_bytes(content)
    require((FIXTURES / "src" / "dir" / "payload.bin").stat().st_size == len(PAYLOAD), "payload fixture")

    flat = FIXTURES / "flat"
    flat.mkdir(parents=True, exist_ok=True)
    for member, content in FLAT_ITEMS.items():
        (flat / member).write_bytes(content)

    sources = FIXTURES / "sources"
    (sources / "mod").mkdir(parents=True, exist_ok=True)
    (sources / "new.txt").write_bytes(NEW_BYTES)
    (sources / "mod" / "a.txt").write_bytes(REPLACED_BYTES)

    setup_pack("pack-base-zip", FIXTURES / "base.zip", [src])
    setup_pack("pack-base-7z", FIXTURES / "base.7z", [src])
    setup_pack("pack-flat-zip", FIXTURES / "flat.zip", [flat / "a.txt", flat / "b.txt"])
    setup_pack("pack-encrypted-zip", FIXTURES / "enc.zip", [src], password=PASSWORD)
    setup_pack("pack-encrypted-7z", FIXTURES / "enc.7z", [src], password=PASSWORD)
    setup_pack("pack-tar", FIXTURES / "t.tar", [src])
    setup_pack("pack-targz", FIXTURES / "t.tar.gz", [src])
    setup_pack("pack-zst", FIXTURES / "s.zst", [src / "a.txt"])
    setup_pack("pack-sfx-input", FIXTURES / "sfx-input.zip", [src / "a.txt"])

    sfx = execute(["sfx", FIXTURES / "app.exe", FIXTURES / "sfx-input.zip", "--json", "--non-interactive"])
    record_setup("create-sfx", sfx)
    expect_success(sfx, "setup sfx")

    split_output = FIXTURES / "split.7z"
    setup_pack("pack-split-7z", split_output, [src], volume_bytes=65_536)
    volumes = sorted(FIXTURES.glob("split.7z.*"))
    require(len(volumes) >= 2, f"setup split: expected >=2 volumes, got {[v.name for v in volumes]}")
    require((FIXTURES / "split.7z.002").exists(), "setup split: missing .002 sibling")

    rar_candidates = [
        path
        for path in (WORKSPACE / "audit" / "runs").glob("rar-reference-*/**/*.rar")
        if path.is_file() and 0 < path.stat().st_size < 100_000
    ]
    require(bool(rar_candidates), "setup rar: no reference .rar fixture found under audit/runs")
    rar_source = min(rar_candidates, key=lambda path: path.stat().st_size)
    shutil.copy2(rar_source, FIXTURES / "fixture.rar")
    rar_peek = execute(["peek", FIXTURES / "fixture.rar", "--json", "--non-interactive"])
    record_setup("peek-rar-fixture", rar_peek)
    expect_success(rar_peek, "setup peek rar")
    rar_payload = json.loads(rar_peek.out_text)
    rar_members = [str(e["path"]).rstrip("/") for e in rar_payload.get("tree_summary", []) if not e["is_dir"]]
    require(bool(rar_members), "setup rar: fixture has no file member")
    FACTS["rar_fixture"] = {
        "source": str(rar_source),
        "sha256": sha256_file(FIXTURES / "fixture.rar"),
        "first_file_member": rar_members[0],
    }

    # Sanity-check that fixtures are readable before measuring edit behavior.
    for fixture in ("base.zip", "base.7z", "flat.zip", "enc.zip", "enc.7z"):
        peek = execute(["peek", FIXTURES / fixture, "--json", "--non-interactive"]
                       + (["--password-stdin"] if fixture.startswith("enc.") else []),
                       stdin_data=(PASSWORD.encode("utf8") + b"\n") if fixture.startswith("enc.") else None)
        record_setup(f"peek-{fixture}", peek)
        expect_success(peek, f"setup peek {fixture}")


# --------------------------------------------------------------------- case bodies
def exercise_add(case: Case, fixture: str) -> None:
    archive = fresh_copy(case.name, fixture)
    before = sha256_file(archive)
    pre = catalog(case, archive)
    new_source = FIXTURES / "sources" / "new.txt"
    call = case.run("add", archive, new_source, "--json", "--non-interactive")
    expect_success(call, "add")
    payload = json.loads(call.out_text)
    require("new.txt" in payload.get("added", []), f"add: 'new.txt' not in added={payload.get('added')}")
    after = sha256_file(archive)
    require(after != before, "add: archive SHA-256 did not change")
    post = catalog(case, archive)
    require(post == {**pre, "new.txt": False}, f"add: member set changed unexpectedly: {post}")
    require(cat_bytes(case, archive, "new.txt") == NEW_BYTES, "add: new member bytes differ")
    for member in BASE_ITEMS:
        require(cat_bytes(case, archive, member) == BASE_ITEMS[member], f"add: untouched {member} changed")
    case.fact("sha256_before", before)
    case.fact("sha256_after", after)


def exercise_delete_file(case: Case, fixture: str) -> None:
    archive = fresh_copy(case.name, fixture)
    before = sha256_file(archive)
    pre = catalog(case, archive)
    call = case.run("delete", archive, "src/a.txt", "--json", "--non-interactive")
    expect_success(call, "delete")
    payload = json.loads(call.out_text)
    require("src/a.txt" in payload.get("deleted", []), f"delete: not in deleted={payload.get('deleted')}")
    require(sha256_file(archive) != before, "delete: archive SHA-256 did not change")
    post = catalog(case, archive)
    expected = {name: is_dir for name, is_dir in pre.items() if name != "src/a.txt"}
    require(post == expected, f"delete: member set mismatch: {post}")
    for member in ("src/b.txt", "src/dir/c.txt", "src/dir/payload.bin"):
        require(cat_bytes(case, archive, member) == BASE_ITEMS[member], f"delete: {member} changed")
    case.fact("sha256_before", before)
    case.fact("sha256_after", sha256_file(archive))


def exercise_rename_file(case: Case, fixture: str) -> None:
    archive = fresh_copy(case.name, fixture)
    before = sha256_file(archive)
    pre = catalog(case, archive)
    call = case.run("rename", archive, "src/a.txt", "top.txt", "--json", "--non-interactive")
    expect_success(call, "rename")
    payload = json.loads(call.out_text)
    require(["src/a.txt", "top.txt"] in payload.get("renamed", []), f"rename: report={payload.get('renamed')}")
    require(sha256_file(archive) != before, "rename: archive SHA-256 did not change")
    post = catalog(case, archive)
    expected = {name: is_dir for name, is_dir in pre.items() if name != "src/a.txt"}
    expected["top.txt"] = False
    require(post == expected, f"rename: member set mismatch: {post}")
    require(cat_bytes(case, archive, "top.txt") == BASE_ITEMS["src/a.txt"], "rename: moved bytes differ")
    for member in ("src/b.txt", "src/dir/c.txt", "src/dir/payload.bin"):
        require(cat_bytes(case, archive, member) == BASE_ITEMS[member], f"rename: {member} changed")
    case.fact("sha256_before", before)
    case.fact("sha256_after", sha256_file(archive))


def exercise_add_overwrite(case: Case) -> None:
    archive = fresh_copy(case.name, "flat.zip")
    before = sha256_file(archive)
    require(cat_bytes(case, archive, "a.txt") == FLAT_ITEMS["a.txt"], "overwrite: precondition a.txt")
    call = case.run(
        "add", archive, FIXTURES / "sources" / "mod" / "a.txt", "--json", "--non-interactive"
    )
    expect_success(call, "add-overwrite")
    payload = json.loads(call.out_text)
    require("a.txt" in payload.get("added", []), f"overwrite: added={payload.get('added')}")
    require(sha256_file(archive) != before, "overwrite: archive SHA-256 did not change")
    post = catalog(case, archive)
    require(post == {"a.txt": False, "b.txt": False}, f"overwrite: member set={post}")
    require(cat_bytes(case, archive, "a.txt") == REPLACED_BYTES, "overwrite: replaced bytes differ")
    require(cat_bytes(case, archive, "b.txt") == FLAT_ITEMS["b.txt"], "overwrite: b.txt changed")
    case.note = (
        "same-name add is accepted and replaces the member; report lists it in added and "
        f"deleted={payload.get('deleted')}"
    )
    case.fact("sha256_before", before)
    case.fact("sha256_after", sha256_file(archive))


def exercise_delete_dir(case: Case, fixture: str) -> None:
    archive = fresh_copy(case.name, fixture)
    before = sha256_file(archive)
    catalog(case, archive)
    call = case.run("delete", archive, "src", "--json", "--non-interactive")
    expect_success(call, "delete-dir")
    require(sha256_file(archive) != before, "delete-dir: archive SHA-256 did not change")
    post = catalog(case, archive)
    require(post == {}, f"delete-dir: expected empty archive, got {post}")
    test_archive(case, archive)
    case.fact("sha256_before", before)
    case.fact("sha256_after", sha256_file(archive))


def exercise_rename_dir(case: Case, fixture: str) -> None:
    archive = fresh_copy(case.name, fixture)
    before = sha256_file(archive)
    catalog(case, archive)
    call = case.run("rename", archive, "src", "dst", "--json", "--non-interactive")
    expect_success(call, "rename-dir")
    require(sha256_file(archive) != before, "rename-dir: archive SHA-256 did not change")
    post = catalog(case, archive)
    expected = {
        name.replace("src", "dst", 1): is_dir for name, is_dir in
        {
            "src": True,
            "src/a.txt": False,
            "src/b.txt": False,
            "src/dir": True,
            "src/dir/c.txt": False,
            "src/dir/payload.bin": False,
        }.items()
    }
    require(post == expected, f"rename-dir: member set mismatch: {post}")
    for old, new in (
        ("src/a.txt", "dst/a.txt"),
        ("src/b.txt", "dst/b.txt"),
        ("src/dir/c.txt", "dst/dir/c.txt"),
        ("src/dir/payload.bin", "dst/dir/payload.bin"),
    ):
        require(cat_bytes(case, archive, new) == BASE_ITEMS[old], f"rename-dir: {new} bytes differ")
    test_archive(case, archive)
    case.fact("sha256_before", before)
    case.fact("sha256_after", sha256_file(archive))


def exercise_delete_missing(case: Case, fixture: str = "base.zip") -> None:
    archive = fresh_copy(case.name, fixture)
    before = sha256_file(archive)
    pre = catalog(case, archive)
    call = case.run("delete", archive, "nope.txt", "--json", "--non-interactive")
    message = expect_rejected(call, "delete-missing")
    expect_preserved(archive, before, "delete-missing")
    require(catalog(case, archive) == pre, "delete-missing: member set changed on failure")
    case.note = message


def exercise_rename_missing(case: Case, fixture: str = "base.zip") -> None:
    archive = fresh_copy(case.name, fixture)
    before = sha256_file(archive)
    pre = catalog(case, archive)
    call = case.run("rename", archive, "nope.txt", "x.txt", "--json", "--non-interactive")
    message = expect_rejected(call, "rename-missing")
    expect_preserved(archive, before, "rename-missing")
    require(catalog(case, archive) == pre, "rename-missing: member set changed on failure")
    case.note = message


def exercise_add_missing_source(case: Case, fixture: str = "base.zip") -> None:
    archive = fresh_copy(case.name, fixture)
    before = sha256_file(archive)
    pre = catalog(case, archive)
    missing = FIXTURES / "missing-source.txt"
    require(not missing.exists(), "add-missing-source: fixture unexpectedly exists")
    call = case.run("add", archive, missing, "--json", "--non-interactive")
    message = expect_rejected(call, "add-missing-source")
    expect_preserved(archive, before, "add-missing-source")
    require(catalog(case, archive) == pre, "add-missing-source: member set changed on failure")
    case.note = message


def exercise_rename_collision(case: Case, fixture: str = "base.zip") -> None:
    archive = fresh_copy(case.name, fixture)
    before = sha256_file(archive)
    pre = catalog(case, archive)
    call = case.run(
        "rename", archive, "src/a.txt", "src/dir/c.txt", "--json", "--non-interactive"
    )
    message = expect_rejected(call, "rename-collision")
    expect_preserved(archive, before, "rename-collision")
    require(catalog(case, archive) == pre, "rename-collision: member set changed on failure")
    case.note = message


def exercise_max_bytes_add(case: Case, fixture: str = "flat.zip") -> None:
    archive = fresh_copy(case.name, fixture)
    before = sha256_file(archive)
    pre = catalog(case, archive)
    call = case.run(
        "add", archive, FIXTURES / "sources" / "new.txt", "--max-bytes", "8",
        "--json", "--non-interactive",
    )
    message = expect_rejected(call, "max-bytes-add")
    require("budget" in message.lower(), f"max-bytes-add: wrong failure: {message!r}")
    expect_preserved(archive, before, "max-bytes-add")
    require(catalog(case, archive) == pre, "max-bytes-add: member set changed on failure")
    case.note = message


def exercise_max_bytes_delete(case: Case, fixture: str = "flat.zip", member: str = "a.txt") -> None:
    archive = fresh_copy(case.name, fixture)
    before = sha256_file(archive)
    pre = catalog(case, archive)
    call = case.run(
        "delete", archive, member, "--max-bytes", "2", "--json", "--non-interactive"
    )
    message = expect_rejected(call, "max-bytes-delete")
    require("budget" in message.lower(), f"max-bytes-delete: wrong failure: {message!r}")
    expect_preserved(archive, before, "max-bytes-delete")
    require(catalog(case, archive) == pre, "max-bytes-delete: member set changed on failure")
    case.note = message


def exercise_format_rejections(case: Case, fixture: str, member: str) -> None:
    archive = fresh_copy(case.name, fixture)
    messages = []
    for label, args in (
        ("add", ["add", archive, FIXTURES / "sources" / "new.txt", "--json", "--non-interactive"]),
        ("delete", ["delete", archive, member, "--json", "--non-interactive"]),
        ("rename", ["rename", archive, member, "renamed-by-test", "--json", "--non-interactive"]),
    ):
        before = sha256_file(archive)
        call = case.run(*args)
        messages.append(f"{label}: {expect_rejected(call, label)}")
        expect_preserved(archive, before, label)
    case.note = " | ".join(messages)


def exercise_split_rejection(case: Case) -> None:
    archive, volumes = fresh_split(case.name)
    hashes = {volume.name: sha256_file(volume) for volume in volumes}
    paths_before = snapshot_files(archive.parent)
    messages = []
    for label, args in (
        ("add", ["add", archive, FIXTURES / "sources" / "new.txt", "--json", "--non-interactive"]),
        ("delete", ["delete", archive, "src/a.txt", "--json", "--non-interactive"]),
        ("rename", ["rename", archive, "src/a.txt", "renamed.txt", "--json", "--non-interactive"]),
    ):
        call = case.run(*args)
        messages.append(f"{label}: {expect_rejected(call, label)}")
        for volume in volumes:
            require(
                sha256_file(volume) == hashes[volume.name],
                f"split {label}: volume {volume.name} changed on rejection",
            )
    require(snapshot_files(archive.parent) == paths_before, "split: rejection created new files")
    case.note = " | ".join(messages)


def exercise_sfx_rejection(case: Case) -> None:
    archive = fresh_copy(case.name, "app.exe")
    before = sha256_file(archive)
    messages = []
    for label, args in (
        ("add", ["add", archive, FIXTURES / "sources" / "new.txt", "--json", "--non-interactive"]),
        ("delete", ["delete", archive, "a.txt", "--json", "--non-interactive"]),
        ("rename", ["rename", archive, "a.txt", "renamed.txt", "--json", "--non-interactive"]),
    ):
        call = case.run(*args)
        messages.append(f"{label}: {expect_rejected(call, label)}")
        expect_preserved(archive, before, f"sfx {label}")
    case.note = " | ".join(messages)


def password_stdin(case: Case, *args: object) -> Call:
    return case.run(*args, stdin_data=PASSWORD.encode("utf8") + b"\n")


def exercise_password_add(case: Case, fixture: str) -> None:
    archive = fresh_copy(case.name, fixture)
    before = sha256_file(archive)
    catalog(case, archive, PASSWORD)
    call = password_stdin(
        case, "add", archive, FIXTURES / "sources" / "new.txt", "--password-stdin",
        "--non-interactive", "--json",
    )
    expect_success(call, "password add")
    require(sha256_file(archive) != before, "password add: archive SHA-256 did not change")
    require(cat_bytes(case, archive, "new.txt", PASSWORD) == NEW_BYTES, "password add: bytes differ")
    no_password = case.run("cat", archive, "new.txt", "--non-interactive", raw=True)
    require(no_password.exit != 0, "password add: added member is readable without a password")
    case.fact("sha256_before", before)
    case.fact("sha256_after", sha256_file(archive))


def exercise_password_delete(case: Case, fixture: str) -> None:
    archive = fresh_copy(case.name, fixture)
    before = sha256_file(archive)
    catalog(case, archive, PASSWORD)
    call = password_stdin(
        case, "delete", archive, "src/a.txt", "--password-stdin", "--non-interactive", "--json"
    )
    expect_success(call, "password delete")
    require(sha256_file(archive) != before, "password delete: archive SHA-256 did not change")
    require(cat_bytes(case, archive, "src/b.txt", PASSWORD) == BASE_ITEMS["src/b.txt"], "password delete: retained bytes differ")
    no_password = case.run("cat", archive, "src/b.txt", "--non-interactive", raw=True)
    require(no_password.exit != 0, "password delete: retained member lost its encryption")
    test_archive(case, archive, PASSWORD)
    case.fact("sha256_before", before)
    case.fact("sha256_after", sha256_file(archive))


def exercise_password_rename(case: Case) -> None:
    archive = fresh_copy(case.name, "enc.zip")
    before = sha256_file(archive)
    catalog(case, archive, PASSWORD)
    call = password_stdin(
        case, "rename", archive, "src/a.txt", "top.txt", "--password-stdin",
        "--non-interactive", "--json",
    )
    expect_success(call, "password rename")
    require(sha256_file(archive) != before, "password rename: archive SHA-256 did not change")
    require(cat_bytes(case, archive, "top.txt", PASSWORD) == BASE_ITEMS["src/a.txt"], "password rename: bytes differ")
    case.fact("sha256_before", before)
    case.fact("sha256_after", sha256_file(archive))


def exercise_password_missing(case: Case, fixture: str) -> None:
    archive = fresh_copy(case.name, fixture)
    operations = (
        ("add", ["add", archive, FIXTURES / "sources" / "new.txt", "--non-interactive", "--json"]),
        ("delete", ["delete", archive, "src/a.txt", "--non-interactive", "--json"]),
        ("rename", ["rename", archive, "src/a.txt", "top.txt", "--non-interactive", "--json"]),
    )
    messages = []
    timings = []
    for label, args in operations:
        before = sha256_file(archive)
        call = case.run(*args)
        message = expect_rejected(call, f"password missing ({label})")
        expect_preserved(archive, before, f"password missing ({label})")
        require(
            call.out_text == "",
            f"password missing ({label}): failure wrote to stdout: {call.out_text[:200]!r}",
        )
        require(
            call.elapsed_ms < PROMPT_BUDGET_MS,
            f"password missing ({label}): no explicit failure within {PROMPT_BUDGET_MS} ms",
        )
        require(
            "password" in message.lower(),
            f"password missing ({label}): error does not name the cause: {message!r}",
        )
        messages.append(f"{label}: {message}")
        timings.append(call.elapsed_ms)
    case.note = f"{' | '.join(messages)} (took {timings} ms each, stdout empty)"


def exercise_password_wrong(case: Case, fixture: str) -> None:
    archive = fresh_copy(case.name, fixture)
    before = sha256_file(archive)
    call = case.run(
        "delete", archive, "src/a.txt", "--password-stdin", "--non-interactive", "--json",
        stdin_data=WRONG_PASSWORD.encode("utf8") + b"\n",
    )
    message = expect_rejected(call, "wrong password")
    expect_preserved(archive, before, "wrong password")
    case.note = message


def exercise_contract_json_success(case: Case) -> None:
    archive = fresh_copy(case.name, "base.zip")
    before = sha256_file(archive)
    call = case.run("delete", archive, "src/a.txt", "--json", "--non-interactive")
    expect_success(call, "json success")
    payload = json.loads(call.out_text)
    require(isinstance(payload, dict), f"json success: stdout is not an object: {payload!r}")
    require(call.err_text == "", f"json success: stderr not empty: {call.err_text[:200]!r}")
    require(sha256_file(archive) != before, "json success: archive unchanged")
    global_call = case.run("--json", "peek", archive, "--non-interactive")
    expect_success(global_call, "json success (global --json position)")
    json.loads(global_call.out_text)
    case.fact("sha256_after", sha256_file(archive))


def exercise_contract_json_failure(case: Case) -> None:
    archive = fresh_copy(case.name, "base.zip")
    before = sha256_file(archive)
    call = case.run("delete", archive, "nope.txt", "--json", "--non-interactive")
    message = expect_rejected(call, "json failure")
    expect_preserved(archive, before, "json failure")
    require(call.out_text == "", f"json failure: stdout not empty: {call.out_text[:200]!r}")
    case.note = message


def exercise_contract_exit_2(case: Case) -> None:
    archive = fresh_copy(case.name, "base.zip")
    call = case.run("rename", archive, "src/a.txt", "--json", "--non-interactive")
    expect_exit(call, 2, "bad command line")
    payload = json.loads(call.err_text)
    require(isinstance(payload.get("error"), str), f"exit 2: stderr JSON={payload!r}")
    require(call.out_text == "", f"exit 2: stdout not empty: {call.out_text[:200]!r}")
    case.note = payload["error"].splitlines()[0]


def exercise_contract_non_interactive(case: Case) -> None:
    archive = fresh_copy(case.name, "enc.zip")
    before = sha256_file(archive)
    call = case.run("delete", archive, "src/a.txt", "--non-interactive", "--json")
    message = expect_rejected(call, "non-interactive")
    expect_preserved(archive, before, "non-interactive")
    require(call.elapsed_ms < PROMPT_BUDGET_MS, f"non-interactive: took {call.elapsed_ms} ms")
    require(call.out_text == "", f"non-interactive: prompt text on stdout: {call.out_text[:200]!r}")
    require("password" in message.lower(), f"non-interactive: error does not name password: {message!r}")
    case.note = f"explicit failure in {call.elapsed_ms} ms without prompt: {message}"


def exercise_hostile_rename(case: Case, target: str) -> None:
    archive = fresh_copy(case.name, "base.zip", hostile=True)
    before = sha256_file(archive)
    pre = catalog(case, archive)
    paths_before = snapshot_files(RUN)
    external = Path("C:/abs.txt")
    external_existed = external.exists()
    call = case.run(
        "rename", archive, "src/a.txt", target, "--json", "--non-interactive", cwd=HOSTILE_DIR
    )
    new_paths = sorted(snapshot_files(RUN) - paths_before)
    case.fact("new_paths", new_paths)
    require(not new_paths, f"hostile {target!r}: created files outside the archive: {new_paths}")
    if not external_existed:
        require(not external.exists(), f"hostile {target!r}: created {external}")
    if target == "trail/":
        if call.exit == 0:
            require(sha256_file(archive) != before, "trailing slash: accepted but archive unchanged")
            post = catalog(case, archive)
            require(post.get("trail") is False, f"trailing slash: expected file member 'trail', got {post}")
            require(cat_bytes(case, archive, "trail") == BASE_ITEMS["src/a.txt"], "trailing slash: bytes differ")
            case.note = "accepted and normalized: 'trail/' became the file member 'trail'"
        else:
            message = expect_rejected(call, "trailing slash")
            expect_preserved(archive, before, "trailing slash")
            require(catalog(case, archive) == pre, "trailing slash: members changed")
            case.note = f"rejected: {message}"
        return
    message = expect_rejected(call, f"hostile rename {target!r}")
    expect_preserved(archive, before, f"hostile rename {target!r}")
    require(catalog(case, archive) == pre, f"hostile {target!r}: members changed")
    case.note = message


# -------------------------------------------------------------------------- driver
def case_list() -> list[tuple[str, object]]:
    rar_member = FACTS["rar_fixture"]["first_file_member"]
    return [
        ("zip/add-new", lambda c: exercise_add(c, "base.zip")),
        ("zip/delete-file", lambda c: exercise_delete_file(c, "base.zip")),
        ("zip/rename-file", lambda c: exercise_rename_file(c, "base.zip")),
        ("zip/add-overwrite-same-name", exercise_add_overwrite),
        ("zip/delete-dir-subtree", lambda c: exercise_delete_dir(c, "base.zip")),
        ("zip/rename-dir-subtree", lambda c: exercise_rename_dir(c, "base.zip")),
        ("7z/add-new", lambda c: exercise_add(c, "base.7z")),
        ("7z/delete-file", lambda c: exercise_delete_file(c, "base.7z")),
        ("7z/rename-file", lambda c: exercise_rename_file(c, "base.7z")),
        ("7z/delete-dir-subtree", lambda c: exercise_delete_dir(c, "base.7z")),
        ("7z/rename-dir-subtree", lambda c: exercise_rename_dir(c, "base.7z")),
        ("zip/delete-missing-member", exercise_delete_missing),
        ("zip/rename-missing-member", exercise_rename_missing),
        ("add/missing-source", exercise_add_missing_source),
        ("zip/rename-collision", exercise_rename_collision),
        ("7z/delete-missing-member", lambda c: exercise_delete_missing(c, "base.7z")),
        ("7z/rename-missing-member", lambda c: exercise_rename_missing(c, "base.7z")),
        ("7z/add-missing-source", lambda c: exercise_add_missing_source(c, "base.7z")),
        ("7z/rename-collision", lambda c: exercise_rename_collision(c, "base.7z")),
        ("7z/max-bytes-delete", lambda c: exercise_max_bytes_delete(c, "base.7z", "src/a.txt")),
        ("7z/max-bytes-add", lambda c: exercise_max_bytes_add(c, "base.7z")),
        ("zip/max-bytes-add", exercise_max_bytes_add),
        ("zip/max-bytes-delete", exercise_max_bytes_delete),
        ("format/rar-rejects-edit", lambda c: exercise_format_rejections(c, "fixture.rar", rar_member)),
        ("format/tar-rejects-edit", lambda c: exercise_format_rejections(c, "t.tar", "src/a.txt")),
        ("format/targz-rejects-edit", lambda c: exercise_format_rejections(c, "t.tar.gz", "src/a.txt")),
        ("format/zst-rejects-edit", lambda c: exercise_format_rejections(c, "s.zst", "a.txt")),
        ("format/split-7z-rejects-edit", exercise_split_rejection),
        ("format/sfx-rejects-edit", exercise_sfx_rejection),
        ("password/zip-add", lambda c: exercise_password_add(c, "enc.zip")),
        ("password/zip-delete", lambda c: exercise_password_delete(c, "enc.zip")),
        ("password/zip-rename", exercise_password_rename),
        ("password/zip-no-password", lambda c: exercise_password_missing(c, "enc.zip")),
        ("password/zip-wrong-password", lambda c: exercise_password_wrong(c, "enc.zip")),
        ("password/7z-delete", lambda c: exercise_password_delete(c, "enc.7z")),
        ("password/7z-wrong-password", lambda c: exercise_password_wrong(c, "enc.7z")),
        ("password/7z-no-password", lambda c: exercise_password_missing(c, "enc.7z")),
        ("contract/json-success-stdout", exercise_contract_json_success),
        ("contract/json-failure-stderr", exercise_contract_json_failure),
        ("contract/bad-command-line-exit-2", exercise_contract_exit_2),
        ("contract/non-interactive-no-prompt", exercise_contract_non_interactive),
        ("hostile/backslash-dotdot", lambda c: exercise_hostile_rename(c, r"..\escape.txt")),
        ("hostile/absolute-drive", lambda c: exercise_hostile_rename(c, r"C:\abs.txt")),
        ("hostile/dos-device-con", lambda c: exercise_hostile_rename(c, "con")),
        ("hostile/nested-dotdot", lambda c: exercise_hostile_rename(c, "a/b/../../evil.txt")),
        ("hostile/empty-string", lambda c: exercise_hostile_rename(c, "")),
        ("hostile/trailing-slash", lambda c: exercise_hostile_rename(c, "trail/")),
        ("hostile/ads-colon", lambda c: exercise_hostile_rename(c, "file:stream")),
    ]


def run_case(name: str, body) -> dict:
    case = Case(name)
    error: str | None = None
    try:
        body(case)
    except AssertionError as failure:
        error = str(failure)
    except Exception as failure:  # noqa: BLE001 - recorded, never swallowed
        error = f"{type(failure).__name__}: {failure}"
    passed = error is None
    entry = {
        "case": name,
        "passed": passed,
        "calls": [call.summary() for call in case.calls],
    }
    if case.note:
        entry["note"] = case.note
    if case.facts:
        entry["facts"] = case.facts
    if error is not None:
        entry["failure"] = error
    CASES.append(entry)
    print(json.dumps({"case": name, "passed": passed, "failure": error}), flush=True)
    return entry


def main() -> int:
    global BINARY
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--binary",
        default=str(DEFAULT_BINARY),
        help=f"installed mothzip.exe under measurement (default: {DEFAULT_BINARY})",
    )
    arguments = parser.parse_args()
    BINARY = Path(arguments.binary).resolve()

    if not BINARY.is_file():
        print(f"binary not found: {BINARY}", file=sys.stderr)
        return 2

    actual_sha = sha256_file(BINARY)
    FACTS["binary"] = str(BINARY)
    FACTS["binary_sha256"] = actual_sha
    FACTS["binary_sha256_expected"] = EXPECTED_BINARY_SHA256
    FACTS["binary_matches_expected"] = actual_sha == EXPECTED_BINARY_SHA256
    if actual_sha != EXPECTED_BINARY_SHA256:
        print(
            f"WARNING: {BINARY} sha256={actual_sha} does not match expected "
            f"{EXPECTED_BINARY_SHA256}; measurements describe this other binary.",
            file=sys.stderr,
        )

    RUN.mkdir(parents=True, exist_ok=False)

    version = execute(["--version"])
    record_setup("binary-version", version)

    try:
        build_fixtures()
    except Exception as failure:  # noqa: BLE001 - setup failure must leave a report
        report = {
            "run": str(RUN),
            "binary": str(BINARY),
            "binary_sha256": actual_sha,
            "binary_sha256_expected": EXPECTED_BINARY_SHA256,
            "binary_matches_expected": actual_sha == EXPECTED_BINARY_SHA256,
            "fatal": f"fixture setup failed: {type(failure).__name__}: {failure}",
            "setup": SETUP_CALLS,
            "summary": {"cases": 0, "passed": 0, "failed": 0},
            "cases": [],
        }
        (RUN / "results.json").write_text(json.dumps(report, indent=2), encoding="utf8")
        print(report["fatal"], file=sys.stderr)
        return 2

    for name, body in case_list():
        run_case(name, body)

    passed = sum(1 for case in CASES if case["passed"])
    failed = len(CASES) - passed
    report = {
        "run": str(RUN),
        "binary": str(BINARY),
        "binary_version": version.out_text.strip(),
        "binary_sha256": actual_sha,
        "binary_sha256_expected": EXPECTED_BINARY_SHA256,
        "binary_matches_expected": actual_sha == EXPECTED_BINARY_SHA256,
        "password_fixture": PASSWORD,
        "fixture_root": str(FIXTURES),
        "fixture_facts": FACTS,
        "setup": SETUP_CALLS,
        "summary": {"cases": len(CASES), "passed": passed, "failed": failed},
        "cases": CASES,
    }
    results_path = RUN / "results.json"
    results_path.write_text(json.dumps(report, indent=2), encoding="utf8")
    print(
        json.dumps(
            {
                "cases": len(CASES),
                "passed": passed,
                "failed": failed,
                "failed_cases": [case["case"] for case in CASES if not case["passed"]],
                "results": str(results_path),
                "binary_sha256": actual_sha,
                "binary_matches_expected": actual_sha == EXPECTED_BINARY_SHA256,
            }
        )
    )
    return 0 if failed == 0 else 1


if __name__ == "__main__":
    sys.exit(main())
