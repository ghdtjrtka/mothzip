#!/usr/bin/env python3
"""Drive the real MothZip archive-list window and measure what it shows.

The harness launches ``mothzip open <fixture>`` as a separate process, finds the window through
Win32, and manipulates it only through messages a user's click would also produce. Nothing is
stubbed and the UI code under test is the shipped one.

Measured per case
-----------------
* window: class name, title, owning pid, client size and DPI;
* list: ``SendMessageW(SysListView32, LVM_GETITEMCOUNT)`` compared with the inventory computed in
  Python from the source tree the fixture was packed from;
* menu: ``GetMenu`` / ``GetSubMenu`` / ``GetMenuStringW`` / ``GetMenuItemID`` / ``GetMenuState``
  for 압축 풀기, 선택 풀기, 검사 and 닫기;
* status line: the ``Static`` child control read with ``WM_GETTEXT``, including the transient
  ``푸는 중…`` text that shows the work left the UI thread;
* extraction: every extracted file hashed with SHA-256 and compared with the fixture, plus the
  zero-byte file, the nested folder and the Korean file name;
* lifetime: ``WM_CLOSE`` must end the process with exit code 0;
* close-while-busy (``zip-busy-close``): ``MOTHZIP_UI_TEST_CLOSE_WHILE_BUSY=1`` makes the window
  request a close the moment its extraction worker starts. The refusal must appear on the status
  line (``작업 중… 완료 후 닫을 수 있습니다 [close-refused-while-busy]``), the process must stay
  alive, the extraction must still commit every file, and the normal close must still exit 0.
  A 32 MiB stored fixture keeps the worker running long enough to observe the refusal.

Deliberately out of scope (reported as unverified, never counted as a pass)
-------------------------------------------------------------------------
* the interactive folder picker (``MOTHZIP_UI_TEST_EXTRACT_DIR`` replaces it here);
* archives with more than 50 000 entries (the row cap only has a unit test);
* encrypted archives, budget-exceeded archives and high-DPI displays;
* two windows open at the same time.

Usage
-----
    python audit/gui_window_validation.py --binary <path to mothzip.exe>

Results land in ``audit/runs/gui-<timestamp>-<rand>/results.json``.
"""

from __future__ import annotations

import argparse
import ctypes
import hashlib
import json
import os
import random
import shutil
import string
import subprocess
import sys
import time
from ctypes import wintypes
from dataclasses import dataclass, field
from datetime import datetime
from pathlib import Path

# --- Win32 message and control constants (named, never inline literals) -----------------------

WM_CLOSE = 0x0010
WM_COMMAND = 0x0111
WM_GETTEXT = 0x000D
WM_KEYDOWN = 0x0100

LVM_GETITEMCOUNT = 0x1004  # LVM_FIRST + 4: the row count this harness trusts
LVM_GETNEXTITEM = 0x100C  # LVM_FIRST + 12
LVM_GETHEADER = 0x101F  # LVM_FIRST + 31
HDM_GETITEMCOUNT = 0x1200  # HDM_FIRST + 0: the header's column count
LVNI_SELECTED = 0x0002

MF_GRAYED = 0x00000001
MF_BYCOMMAND = 0x00000000
MF_BYPOSITION = 0x00000400

VK_DOWN = 0x28
VK_RETURN = 0x0D
VK_ESCAPE = 0x1B

# Scan codes published by the keyboard layout tables; `TranslateAcceleratorW` inspects them.
SCAN_DOWN = 0x50
SCAN_RETURN = 0x1C
SCAN_ESCAPE = 0x01

WINDOW_CLASS = "MothZipBrowseWindow"
LIST_VIEW_CLASS = "SysListView32"
STATUS_CLASS = "Static"

ACTION_EXTRACT_ALL = "압축 풀기"
ACTION_EXTRACT_SELECTED = "선택 풀기"
ACTION_TEST = "검사"
ACTION_CLOSE = "닫기"

TITLE_SUFFIX = "MothZip"

# The extraction target the window must use instead of the folder picker. Test-only hook; the
# shipped behaviour is always `rfd::FileDialog::pick_folder`.
TEST_DIR_ENV = "MOTHZIP_UI_TEST_EXTRACT_DIR"

# Test-only hook: the window requests a close the moment a worker starts, so the refusal path is
# observed deterministically instead of racing a fast extraction.
CLOSE_HOOK_ENV = "MOTHZIP_UI_TEST_CLOSE_WHILE_BUSY"

# The refusal text and the note the window adds while that hook is active.
BUSY_CLOSE_REFUSAL = "작업 중… 완료 후 닫을 수 있습니다"
BUSY_CLOSE_NOTE = "close-refused-while-busy"

# Staging directories the extraction creates inside the destination and removes on commit.
STAGE_PREFIX = ".mothzip_stage_"

# A stored (uncompressed) 32 MiB fixture: big enough that the worker keeps running while the
# harness polls the status line, small enough to build and extract in a couple of seconds.
LARGE_CHUNKS = 8
LARGE_CHUNK_BYTES = 4 * 1024 * 1024

# Authored layout of the window at 96 DPI, mirrored from src/ui/mod.rs so the measurement is a
# comparison rather than an assumption.
BASE_DPI = 96
BASE_CLIENT_WIDTH = 900
BASE_CLIENT_HEIGHT = 560

# Fixture source tree: Korean name, nested folder, zero-byte file and binary content.
SOURCE_TREE: dict[str, bytes] = {
    "src/empty.txt": b"",
    "src/한글 문서.txt": "모스집 목록 창 검증\n".encode("utf-8"),
    "src/sub/nested.txt": b"nested payload\n",
    "src/sub/deep/binary.bin": bytes(range(256)),
}

FIXTURES = (
    ("zip", "sample.zip"),
    ("7z", "sample.7z"),
    ("tarzst", "sample.tar.zst"),
)


# --- Win32 surface ----------------------------------------------------------------------------


class Win32:
    """The user32 calls the harness needs, with explicit prototypes."""

    ENUM_PROC = ctypes.WINFUNCTYPE(wintypes.BOOL, wintypes.HWND, wintypes.LPARAM)

    def __init__(self) -> None:
        self.user32 = ctypes.WinDLL("user32", use_last_error=True)
        u = self.user32
        u.EnumWindows.argtypes = [self.ENUM_PROC, wintypes.LPARAM]
        u.EnumWindows.restype = wintypes.BOOL
        u.EnumChildWindows.argtypes = [wintypes.HWND, self.ENUM_PROC, wintypes.LPARAM]
        u.EnumChildWindows.restype = wintypes.BOOL
        u.GetWindowTextW.argtypes = [wintypes.HWND, wintypes.LPWSTR, ctypes.c_int]
        u.GetWindowTextW.restype = ctypes.c_int
        u.GetClassNameW.argtypes = [wintypes.HWND, wintypes.LPWSTR, ctypes.c_int]
        u.GetClassNameW.restype = ctypes.c_int
        u.GetWindowThreadProcessId.argtypes = [wintypes.HWND, ctypes.POINTER(wintypes.DWORD)]
        u.GetWindowThreadProcessId.restype = wintypes.DWORD
        u.IsWindowVisible.argtypes = [wintypes.HWND]
        u.IsWindowVisible.restype = wintypes.BOOL
        u.GetClientRect.argtypes = [wintypes.HWND, ctypes.POINTER(wintypes.RECT)]
        u.GetClientRect.restype = wintypes.BOOL
        u.GetDpiForWindow.argtypes = [wintypes.HWND]
        u.GetDpiForWindow.restype = wintypes.UINT
        u.SendMessageW.argtypes = [wintypes.HWND, wintypes.UINT, wintypes.WPARAM, wintypes.LPARAM]
        u.SendMessageW.restype = ctypes.c_ssize_t
        u.PostMessageW.argtypes = [wintypes.HWND, wintypes.UINT, wintypes.WPARAM, wintypes.LPARAM]
        u.PostMessageW.restype = wintypes.BOOL
        u.GetMenu.argtypes = [wintypes.HWND]
        u.GetMenu.restype = wintypes.HMENU
        u.GetSubMenu.argtypes = [wintypes.HMENU, ctypes.c_int]
        u.GetSubMenu.restype = wintypes.HMENU
        u.GetMenuItemCount.argtypes = [wintypes.HMENU]
        u.GetMenuItemCount.restype = ctypes.c_int
        u.GetMenuItemID.argtypes = [wintypes.HMENU, ctypes.c_int]
        u.GetMenuItemID.restype = wintypes.UINT
        u.GetMenuStringW.argtypes = [wintypes.HMENU, wintypes.UINT, wintypes.LPWSTR, ctypes.c_int, wintypes.UINT]
        u.GetMenuStringW.restype = ctypes.c_int
        u.GetMenuState.argtypes = [wintypes.HMENU, wintypes.UINT, wintypes.UINT]
        u.GetMenuState.restype = wintypes.UINT

    def window_text(self, hwnd: int) -> str:
        buffer = ctypes.create_unicode_buffer(1024)
        self.user32.GetWindowTextW(hwnd, buffer, 1024)
        return buffer.value

    def class_name(self, hwnd: int) -> str:
        buffer = ctypes.create_unicode_buffer(256)
        self.user32.GetClassNameW(hwnd, buffer, 256)
        return buffer.value

    def window_pid(self, hwnd: int) -> int:
        pid = wintypes.DWORD()
        self.user32.GetWindowThreadProcessId(hwnd, ctypes.byref(pid))
        return int(pid.value)

    def is_visible(self, hwnd: int) -> bool:
        return bool(self.user32.IsWindowVisible(hwnd))

    def control_text(self, hwnd: int) -> tuple[str, int]:
        """Read a control's text with WM_GETTEXT, which the system marshals across processes."""
        buffer = ctypes.create_unicode_buffer(4096)
        copied = self.user32.SendMessageW(hwnd, WM_GETTEXT, 4096, ctypes.addressof(buffer))
        return buffer.value, int(copied)

    def send(self, hwnd: int, message: int, wparam: int = 0, lparam: int = 0) -> int:
        return int(self.user32.SendMessageW(hwnd, message, wparam, lparam))

    def post(self, hwnd: int, message: int, wparam: int = 0, lparam: int = 0) -> bool:
        return bool(self.user32.PostMessageW(hwnd, message, wparam, lparam))

    def top_level_windows(self) -> list[tuple[int, str, str]]:
        found: list[tuple[int, str, str]] = []

        def callback(hwnd: int, _lparam: int) -> bool:
            found.append((hwnd, self.class_name(hwnd), self.window_text(hwnd)))
            return True

        self.user32.EnumWindows(self.ENUM_PROC(callback), 0)
        return found

    def child_windows(self, hwnd: int) -> list[tuple[int, str]]:
        found: list[tuple[int, str]] = []

        def callback(child: int, _lparam: int) -> bool:
            found.append((child, self.class_name(child)))
            return True

        self.user32.EnumChildWindows(hwnd, self.ENUM_PROC(callback), 0)
        return found

    def client_size(self, hwnd: int) -> tuple[int, int]:
        rect = wintypes.RECT()
        self.user32.GetClientRect(hwnd, ctypes.byref(rect))
        return rect.right - rect.left, rect.bottom - rect.top

    def dpi(self, hwnd: int) -> int:
        return int(self.user32.GetDpiForWindow(hwnd))

    def menu_snapshot(self, hwnd: int) -> list[dict]:
        """Every popup of the window's menu bar, with ids, labels and the greyed flag."""
        menu = self.user32.GetMenu(hwnd)
        if not menu:
            return []
        popups = []
        for index in range(self.user32.GetMenuItemCount(menu)):
            sub = self.user32.GetSubMenu(menu, index)
            items = []
            if sub:
                for item in range(self.user32.GetMenuItemCount(sub)):
                    title = ctypes.create_unicode_buffer(256)
                    self.user32.GetMenuStringW(sub, item, title, 256, MF_BYPOSITION)
                    item_id = int(self.user32.GetMenuItemID(sub, item))
                    state = None
                    if item_id:
                        state = int(self.user32.GetMenuState(sub, item_id, MF_BYCOMMAND))
                    items.append(
                        {
                            "position": item,
                            "id": item_id,
                            "title": title.value,
                            "grayed": None if state in (None, 0xFFFFFFFF) else bool(state & MF_GRAYED),
                        }
                    )
            popups.append({"position": index, "items": items})
        return popups


# --- Fixtures ---------------------------------------------------------------------------------


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1 << 20), b""):
            digest.update(chunk)
    return digest.hexdigest()


def build_source_tree(fixtures: Path) -> None:
    for relative, payload in SOURCE_TREE.items():
        target = fixtures / relative
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_bytes(payload)


def expected_inventory(fixtures: Path) -> tuple[list[str], dict[str, str]]:
    """Entry names and file hashes as the archive must contain them, computed from disk.

    Names are relative to the directory the pack command is given, which is why the fixture tree
    starts at ``src/``: `mothzip pack` names members relative to each source's parent.
    """
    root = fixtures / "src"
    entries = ["src/"]
    hashes: dict[str, str] = {}
    for path in sorted(root.rglob("*")):
        relative = path.relative_to(fixtures).as_posix()
        if path.is_dir():
            entries.append(relative + "/")
        else:
            entries.append(relative)
            hashes[relative] = sha256_file(path)
    return entries, hashes


def build_large_tree(fixtures: Path) -> tuple[list[str], dict[str, str]]:
    """A stored 32 MiB fixture tree so one extraction keeps the worker busy while we poll."""
    entries = ["large/"]
    hashes: dict[str, str] = {}
    block = bytes(range(256)) * 4096  # 1 MiB, deterministic across runs
    for index in range(LARGE_CHUNKS):
        relative = f"large/chunk{index:02d}.bin"
        target = fixtures / relative
        target.parent.mkdir(parents=True, exist_ok=True)
        rotated = block[index * 4096 :] + block[: index * 4096]
        payload = (rotated * (LARGE_CHUNK_BYTES // len(rotated) + 1))[:LARGE_CHUNK_BYTES]
        target.write_bytes(payload)
        entries.append(relative)
        hashes[relative] = hashlib.sha256(payload).hexdigest()
    return entries, hashes


def run_pack(binary: Path, fixtures: Path, archive: Path, source: str, level: int | None = None) -> dict:
    command = [str(binary), "pack", str(archive), source]
    if level is not None:
        command += ["-l", str(level)]
    completed = subprocess.run(
        command,
        cwd=str(fixtures),
        capture_output=True,
        text=True,
        encoding="utf-8",
        errors="replace",
        timeout=180,
    )
    return {
        "command": " ".join(command),
        "returncode": completed.returncode,
        "stdout": completed.stdout.strip(),
        "stderr": completed.stderr.strip(),
    }


# --- Case execution ---------------------------------------------------------------------------


@dataclass
class Case:
    name: str
    archive_name: str
    selected_only: bool = False
    expect_load_error: bool = False
    keyboard: bool = False
    busy_close: bool = False


@dataclass
class CaseRun:
    case: Case
    checks: list[dict] = field(default_factory=list)
    measurements: dict = field(default_factory=dict)
    transitions: list[str] = field(default_factory=list)
    notes: list[str] = field(default_factory=list)

    def check(self, name: str, ok: bool, detail: object = "") -> bool:
        self.checks.append({"name": name, "ok": bool(ok), "detail": detail})
        return bool(ok)

    @property
    def passed(self) -> bool:
        return bool(self.checks) and all(check["ok"] for check in self.checks)

    def as_dict(self) -> dict:
        return {
            "case": self.case.name,
            "archive": self.case.archive_name,
            "status": "pass" if self.passed else "fail",
            "checks": self.checks,
            "measurements": self.measurements,
            "status_line_transitions": self.transitions,
            "notes": self.notes,
        }


class Harness:
    def __init__(self, binary: Path, run_dir: Path, timeout: float) -> None:
        self.binary = binary
        self.run_dir = run_dir
        self.timeout = timeout
        self.win32 = Win32()
        self.fixtures = run_dir / "fixtures"
        self.work = run_dir / "work"
        self.pack_log: dict[str, dict] = {}
        self.expected_entries: list[str] = []
        self.expected_files: dict[str, str] = {}
        self.large_entries: list[str] = []
        self.large_files: dict[str, str] = {}

    # -- fixture setup ------------------------------------------------------------------------

    def prepare(self) -> None:
        self.fixtures.mkdir(parents=True, exist_ok=True)
        self.work.mkdir(parents=True, exist_ok=True)
        build_source_tree(self.fixtures)
        self.expected_entries, self.expected_files = expected_inventory(self.fixtures)
        for _key, archive_name in FIXTURES:
            archive = self.fixtures / archive_name
            if archive.exists():
                archive.unlink()
            result = run_pack(self.binary, self.fixtures, archive, "src")
            self.pack_log[archive_name] = result
            if result["returncode"] != 0 or not archive.is_file():
                raise RuntimeError(f"pack failed for {archive_name}: {result}")
        # The close-while-busy case needs a job that outlives the harness's first status poll.
        # Stored (level 0) keeps the fixture at ~32 MiB so the worker runs for a measurable time.
        self.large_entries, self.large_files = build_large_tree(self.fixtures)
        large_archive = self.fixtures / "large.zip"
        if large_archive.exists():
            large_archive.unlink()
        result = run_pack(self.binary, self.fixtures, large_archive, "large", level=0)
        self.pack_log["large.zip"] = result
        if result["returncode"] != 0 or not large_archive.is_file():
            raise RuntimeError(f"pack failed for large.zip: {result}")
        if large_archive.stat().st_size < 20 * 1024 * 1024:
            raise RuntimeError(
                f"large.zip is not stored: {large_archive.stat().st_size} bytes, expected > 20 MiB"
            )
        # A truncated ZIP keeps the local-file magic, so the format is recognised and the central
        # directory read is what fails: exactly the "corrupt archive" path the window must survive.
        source = (self.fixtures / "sample.zip").read_bytes()
        (self.fixtures / "corrupt.zip").write_bytes(source[:64])

    # -- window interaction -------------------------------------------------------------------

    def find_window(self, process: subprocess.Popen, archive_name: str) -> tuple[int, dict]:
        """Locate the window by title, then record how it was found."""
        deadline = time.monotonic() + self.timeout
        seen: list[str] = []
        while time.monotonic() < deadline:
            candidates = [
                (hwnd, cls, title)
                for hwnd, cls, title in self.win32.top_level_windows()
                if self.win32.window_pid(hwnd) == process.pid
            ]
            seen = [f"{cls}:{title!r}" for _hwnd, cls, title in candidates]
            by_title = [
                hwnd
                for hwnd, _cls, title in candidates
                if archive_name in title and title.endswith(TITLE_SUFFIX)
            ]
            if len(by_title) == 1:
                return by_title[0], self.describe_window(by_title[0], "title")
            by_class = [hwnd for hwnd, cls, _title in candidates if cls == WINDOW_CLASS]
            if len(by_class) == 1:
                return by_class[0], self.describe_window(by_class[0], "class")
            time.sleep(0.05)
        raise RuntimeError(
            f"no single window titled like {archive_name!r} appeared within {self.timeout}s; "
            f"windows of pid {process.pid}: {seen}"
        )

    def describe_window(self, hwnd: int, found_by: str) -> dict:
        return {
            "found_by": found_by,
            "class": self.win32.class_name(hwnd),
            "title": self.win32.window_text(hwnd),
            "pid": self.win32.window_pid(hwnd),
        }

    def children(self, hwnd: int) -> tuple[int | None, int | None, list[str]]:
        listing = self.win32.child_windows(hwnd)
        names = [cls for _child, cls in listing]
        list_view = next((child for child, cls in listing if cls == LIST_VIEW_CLASS), None)
        status = next((child for child, cls in listing if cls == STATUS_CLASS), None)
        return list_view, status, names

    def status_text(self, status: int | None) -> str:
        if status is None:
            return ""
        text, _copied = self.win32.control_text(status)
        return text

    def wait_ready(self, hwnd: int) -> None:
        """Wait until the window finished building.

        The handle is enumerable while ``WM_CREATE`` is still running, so measuring right after
        ``EnumWindows`` would read a half-built window. Visible + both children + a reported
        catalog + a client size that stopped moving is the point where the app is idle.
        """
        deadline = time.monotonic() + self.timeout
        previous = None
        while time.monotonic() < deadline:
            list_view, status, _ = self.children(hwnd)
            size = self.win32.client_size(hwnd)
            built = (
                self.win32.is_visible(hwnd)
                and list_view is not None
                and status is not None
                and self.status_text(status) != ""
            )
            if built and size == previous:
                return
            previous = size
            time.sleep(0.05)
        raise RuntimeError(f"window did not finish building within {self.timeout}s (client size {previous})")

    def wait_for_status(
        self,
        status: int | None,
        *,
        contains: tuple[str, ...] = (),
        transitions: list[str] | None = None,
    ) -> str:
        deadline = time.monotonic() + self.timeout
        last = self.status_text(status)
        if transitions is not None:
            transitions.append(last)
        while time.monotonic() < deadline:
            current = self.status_text(status)
            if current != last:
                last = current
                if transitions is not None:
                    transitions.append(current)
            if contains and any(marker in current for marker in contains):
                return current
            time.sleep(0.02)
        return last

    def trigger(self, hwnd: int, command_id: int) -> None:
        """Send the command id a menu selection or accelerator would send."""
        if not self.win32.post(hwnd, WM_COMMAND, command_id, 0):
            raise RuntimeError(f"PostMessageW(WM_COMMAND {command_id}) failed")

    def press(self, hwnd: int, virtual_key: int, scan_code: int) -> bool:
        """Post a WM_KEYDOWN shaped like a real key press so the accelerator table accepts it."""
        lparam = 1 | (scan_code << 16)  # repeat count 1, scan code in bits 16..23
        return self.win32.post(hwnd, WM_KEYDOWN, virtual_key, lparam)

    # -- the case itself ----------------------------------------------------------------------

    def run_case(self, case: Case) -> CaseRun:
        run = CaseRun(case=case)
        archive = self.fixtures / case.archive_name
        out_dir = self.work / case.name / "out"
        work_dir = self.work / case.name
        if work_dir.exists():
            shutil.rmtree(work_dir)
        work_dir.mkdir(parents=True)

        run.measurements["archive"] = str(archive)
        run.measurements["archive_bytes"] = archive.stat().st_size
        run.measurements["archive_sha256"] = sha256_file(archive)
        run.measurements["extract_dir"] = str(out_dir)
        expected_entries = self.large_entries if case.busy_close else self.expected_entries
        expected_files = self.large_files if case.busy_close else self.expected_files

        environment = dict(os.environ)
        environment[TEST_DIR_ENV] = str(out_dir)
        if case.busy_close:
            environment[CLOSE_HOOK_ENV] = "1"
        stdout_path = work_dir / "stdout.txt"
        stderr_path = work_dir / "stderr.txt"
        process: subprocess.Popen | None = None
        with stdout_path.open("wb") as stdout, stderr_path.open("wb") as stderr:
            process = subprocess.Popen(
                [str(self.binary), "open", str(archive)],
                cwd=str(work_dir),
                env=environment,
                stdout=stdout,
                stderr=stderr,
            )
        try:
            hwnd, window_info = self.find_window(process, case.archive_name)
            self.wait_ready(hwnd)
            run.measurements["window"] = window_info
            run.check("window.class", window_info["class"] == WINDOW_CLASS, window_info["class"])
            run.check(
                "window.title",
                window_info["title"] == f"{case.archive_name} — {TITLE_SUFFIX}",
                window_info["title"],
            )
            run.check("window.pid", window_info["pid"] == process.pid, window_info)

            width, height = self.win32.client_size(hwnd)
            dpi = self.win32.dpi(hwnd) or BASE_DPI
            expected_width = round(BASE_CLIENT_WIDTH * dpi / BASE_DPI)
            expected_height = round(BASE_CLIENT_HEIGHT * dpi / BASE_DPI)
            run.measurements["dpi"] = dpi
            run.measurements["client_size"] = [width, height]
            run.measurements["expected_client_size"] = [expected_width, expected_height]
            run.check(
                "window.client_size_follows_dpi",
                abs(width - expected_width) <= 2 and abs(height - expected_height) <= 2,
                [width, height, expected_width, expected_height],
            )

            list_view, status, child_classes = self.children(hwnd)
            run.measurements["child_classes"] = child_classes
            run.check("window.has_list_view", list_view is not None, child_classes)
            run.check("window.has_status_line", status is not None, child_classes)
            if list_view is None:
                raise RuntimeError("the window has no SysListView32 child to measure")

            rows = self.win32.send(list_view, LVM_GETITEMCOUNT)
            run.measurements["lvm_item_count"] = rows
            run.measurements["expected_entry_count"] = len(expected_entries)
            if case.expect_load_error:
                run.check("list.empty_after_load_error", rows == 0, rows)
            else:
                run.check(
                    "list.item_count_equals_catalog",
                    rows == len(expected_entries),
                    {"measured": rows, "expected": len(expected_entries)},
                )

            header = self.win32.send(list_view, LVM_GETHEADER)
            columns = self.win32.send(header, HDM_GETITEMCOUNT) if header else 0
            run.measurements["column_count"] = columns
            run.check("list.column_count_is_five", columns == 5, columns)

            # Opening the window must not unpack anything: the destination only appears once the
            # extract action runs.
            run.check("open.writes_nothing", not out_dir.exists(), str(out_dir))

            initial_status = self.status_text(status)
            run.measurements["initial_status"] = initial_status
            run.transitions.append(initial_status)
            if case.expect_load_error:
                run.check("status.reports_load_error", "읽지 못했" in initial_status, initial_status)
            else:
                run.check(
                    "status.reports_entry_count",
                    initial_status == f"{len(expected_entries)}개 항목",
                    initial_status,
                )

            menu = self.win32.menu_snapshot(hwnd)
            run.measurements["menu"] = menu
            labels = {item["title"]: item for popup in menu for item in popup["items"] if item["title"]}
            run.check(
                "menu.has_all_actions",
                all(
                    label in labels
                    for label in (ACTION_EXTRACT_ALL, ACTION_EXTRACT_SELECTED, ACTION_TEST, ACTION_CLOSE)
                ),
                sorted(labels),
            )
            run.check(
                "menu.command_ids_are_distinct",
                len({item["id"] for item in labels.values()}) == len(labels),
                {label: item["id"] for label, item in labels.items()},
            )
            action_labels = (ACTION_EXTRACT_ALL, ACTION_EXTRACT_SELECTED, ACTION_TEST)
            action_states = {label: labels[label]["grayed"] for label in action_labels if label in labels}
            if case.expect_load_error:
                run.check(
                    "menu.actions_disabled_after_load_error",
                    all(state is True for state in action_states.values()),
                    action_states,
                )
                self.run_load_failure_actions(
                    run, process, hwnd, list_view, status, labels, out_dir
                )
            else:
                run.check(
                    "menu.actions_enabled_after_load",
                    all(state is False for state in action_states.values()),
                    action_states,
                )
                if case.busy_close:
                    self.run_busy_close(
                        run,
                        process,
                        hwnd,
                        status,
                        labels,
                        out_dir,
                        expected_files,
                    )
                else:
                    self.run_actions(
                        run, hwnd, list_view, status, labels, case, out_dir, expected_files
                    )

            if case.keyboard:
                # Esc is the accelerator for 닫기, so the same window command is exercised.
                if not self.press(hwnd, VK_ESCAPE, SCAN_ESCAPE):
                    raise RuntimeError("PostMessageW(WM_KEYDOWN VK_ESCAPE) failed")
                run.measurements["close_method"] = "escape-accelerator"
            else:
                if not self.win32.post(hwnd, WM_CLOSE):
                    raise RuntimeError("PostMessageW(WM_CLOSE) failed")
                run.measurements["close_method"] = "WM_CLOSE"
            try:
                exit_code = process.wait(timeout=self.timeout)
            except subprocess.TimeoutExpired:
                process.kill()
                exit_code = None
            run.measurements["exit_code"] = exit_code
            run.check("process.exit_code_zero", exit_code == 0, exit_code)
            run.measurements["stdout"] = stdout_path.read_text(encoding="utf-8", errors="replace").strip()
            run.measurements["stderr"] = stderr_path.read_text(encoding="utf-8", errors="replace").strip()
        except Exception as error:  # the record must survive any failure
            run.check("case.completed_without_error", False, f"{type(error).__name__}: {error}")
        finally:
            if process is not None and process.poll() is None:
                process.kill()
                process.wait(timeout=10)
        return run

    def run_load_failure_actions(
        self,
        run: CaseRun,
        process: subprocess.Popen,
        hwnd: int,
        list_view: int,
        status: int | None,
        labels: dict[str, dict],
        out_dir: Path,
    ) -> None:
        """A catalog that failed to load must ignore every route to an action.

        Enter is the accelerator for 압축 풀기, and the three menu command ids are what a click or
        any other accelerator sends; all of them are posted to the real window. A started job would
        change the status line, and an extraction would create the destination, so both are
        measured instead of assumed.
        """
        before = self.status_text(status)
        run.measurements["status_before_ignored_actions"] = before
        run.check(
            "load_failure.starts_from_load_error", "읽지 못했" in before, before
        )
        if not self.press(hwnd, VK_RETURN, SCAN_RETURN):
            raise RuntimeError("PostMessageW(WM_KEYDOWN VK_RETURN) failed")
        for label in (ACTION_EXTRACT_ALL, ACTION_EXTRACT_SELECTED, ACTION_TEST):
            self.trigger(hwnd, labels[label]["id"])
        time.sleep(0.4)
        after = self.status_text(status)
        run.measurements["status_after_ignored_actions"] = after
        run.check(
            "load_failure.status_unchanged",
            after == before,
            {"before": before, "after": after},
        )
        run.check("load_failure.no_extraction_output", not out_dir.exists(), str(out_dir))
        run.check("load_failure.process_alive", process.poll() is None, process.poll())
        # The list still holds no rows, so a selected extraction has nothing to resolve either.
        run.check(
            "load_failure.list_still_empty",
            self.win32.send(list_view, LVM_GETITEMCOUNT) == 0,
            self.win32.send(list_view, LVM_GETITEMCOUNT),
        )

    def run_busy_close(
        self,
        run: CaseRun,
        process: subprocess.Popen,
        hwnd: int,
        status: int | None,
        labels: dict[str, dict],
        out_dir: Path,
        expected_files: dict[str, str],
    ) -> None:
        """Start one extraction and measure the refusal of a close request while it runs.

        The window's test hook sends WM_CLOSE synchronously as soon as the worker starts, so the
        refusal does not race the job. The status line is polled for the hook's note; at that moment
        the process must still be alive. The job then has to commit every file, and the case closes
        normally, which also proves the close path still exits 0 once the worker is gone.
        """
        before = self.status_text(status)
        run.measurements["status_before_extract"] = before
        run.check("busy_close.ready_before_action", before.endswith("개 항목"), before)
        self.trigger(hwnd, labels[ACTION_EXTRACT_ALL]["id"])
        run.measurements["extract_trigger"] = "menu-command-with-close-hook"

        marker_status = None
        marker_while_running = False
        alive_at_marker = None
        last = before
        deadline = time.monotonic() + self.timeout
        while time.monotonic() < deadline:
            current = self.status_text(status)
            if current != last:
                last = current
                run.transitions.append(current)
            if marker_status is None and BUSY_CLOSE_NOTE in current:
                marker_status = current
                alive_at_marker = process.poll() is None
                marker_while_running = (
                    "압축 풀기 완료" not in current and "압축 풀기 실패" not in current
                )
            if "압축 풀기 완료" in current or "압축 풀기 실패" in current:
                break
            if process.poll() is not None:
                break
            time.sleep(0.005)

        run.measurements["busy_close_refusal_status"] = marker_status
        run.measurements["busy_close_refusal_seen_while_job_running"] = marker_while_running
        run.measurements["busy_close_process_alive_at_refusal"] = alive_at_marker
        run.measurements["status_after_busy_extract"] = last

        run.check(
            "busy_close.refusal_status_contains_note",
            marker_status is not None and BUSY_CLOSE_NOTE in marker_status,
            marker_status,
        )
        run.check(
            "busy_close.process_alive_at_refusal",
            alive_at_marker is True,
            {"alive": alive_at_marker, "status": marker_status},
        )
        run.check(
            "busy_close.completion_after_refusal",
            "압축 풀기 완료" in last,
            last,
        )
        run.check(
            "busy_close.note_survives_completion",
            BUSY_CLOSE_NOTE in last,
            last,
        )
        run.check("busy_close.process_alive_before_normal_close", process.poll() is None, process.poll())
        if not marker_while_running:
            run.notes.append(
                "the refusal note was first read from the completion status: the 32 MiB extraction "
                "finished before the first poll, so the transient busy text was not sampled"
            )

        extracted = self.compare_extraction(out_dir, run, expected_files)
        run.check("extract.no_missing_files", not extracted["missing"], extracted["missing"])
        run.check(
            "extract.no_unexpected_files", not extracted["unexpected"], extracted["unexpected"]
        )
        run.check(
            "extract.sha256_matches_source",
            not extracted["sha256_mismatch"],
            extracted["sha256_mismatch"],
        )
        staging = (
            sorted(path.name for path in out_dir.rglob(f"{STAGE_PREFIX}*"))
            if out_dir.is_dir()
            else []
        )
        run.check("busy_close.no_staging_residue", not staging, staging)
        run.measurements["extracted_bytes"] = sum(
            item["size"] for item in extracted["files"].values()
        )

    def run_actions(
        self,
        run: CaseRun,
        hwnd: int,
        list_view: int,
        status: int | None,
        labels: dict[str, dict],
        case: Case,
        out_dir: Path,
        expected_files: dict[str, str],
    ) -> None:
        before_test = self.status_text(status)
        self.trigger(hwnd, labels[ACTION_TEST]["id"])
        after_test = self.wait_for_status(
            status,
            contains=("검사 통과", "검사 실패"),
            transitions=run.transitions,
        )
        run.measurements["status_after_test"] = after_test
        run.check("integrity_action.reaches_a_verdict", after_test != before_test, after_test)
        run.check("integrity_action.passes", "검사 통과" in after_test, after_test)

        if case.selected_only:
            for _ in range(2):
                self.press(list_view, VK_DOWN, SCAN_DOWN)
                time.sleep(0.05)
            selected = []
            start = -1
            while True:
                index = self.win32.send(list_view, LVM_GETNEXTITEM, start, LVNI_SELECTED)
                if index < 0:
                    break
                selected.append(index)
                start = index
            run.measurements["selected_rows"] = selected
            run.check("selection.keyboard_selection_applied", len(selected) == 1, selected)
            command = labels[ACTION_EXTRACT_SELECTED]["id"]
        else:
            command = labels[ACTION_EXTRACT_ALL]["id"]

        if case.keyboard:
            # Enter is translated by the window's accelerator table into the whole-archive command.
            if not self.press(hwnd, VK_RETURN, SCAN_RETURN):
                raise RuntimeError("PostMessageW(WM_KEYDOWN VK_RETURN) failed")
            run.measurements["extract_trigger"] = "enter-accelerator"
        else:
            self.trigger(hwnd, command)
            run.measurements["extract_trigger"] = "menu-command"
        # A synchronous query right after the command measures whether the UI thread returned to
        # its message loop instead of unpacking inline; recorded, not asserted, because a tiny
        # archive can finish before the query is sent.
        query_started = time.perf_counter()
        self.win32.send(list_view, LVM_GETITEMCOUNT)
        run.measurements["ui_thread_query_ms_during_extract"] = round(
            (time.perf_counter() - query_started) * 1000, 2
        )
        after_extract = self.wait_for_status(
            status,
            contains=("압축 풀기 완료", "압축 풀기 실패"),
            transitions=run.transitions,
        )
        run.measurements["status_after_extract"] = after_extract
        run.check("extract_action.succeeded", "압축 풀기 완료" in after_extract, after_extract)

        busy_seen = any(label == "푸는 중…" for label in run.transitions)
        run.measurements["busy_status_observed"] = busy_seen
        if not busy_seen:
            run.notes.append(
                "status line never showed 푸는 중…: the job finished before the first poll, so the "
                "worker-thread hand-off is evidenced by the completion text only"
            )

        extracted = self.compare_extraction(out_dir, run, expected_files)
        if case.selected_only:
            members = [name for name in expected_files if name in extracted["files"]]
            run.check(
                "extract.selected_writes_exactly_one_member",
                len(extracted["files"]) == 1 and len(members) == 1,
                {"extracted": sorted(extracted["files"]), "matching_members": members},
            )
            if len(extracted["files"]) == 1:
                member = next(iter(extracted["files"]))
                run.measurements["selected_member"] = member
                run.check(
                    "extract.selected_member_matches_source",
                    expected_files.get(member) == extracted["files"][member]["sha256"],
                    member,
                )
        else:
            run.check("extract.no_missing_files", not extracted["missing"], extracted["missing"])
            run.check("extract.no_unexpected_files", not extracted["unexpected"], extracted["unexpected"])
            run.check("extract.sha256_matches_source", not extracted["sha256_mismatch"], extracted["sha256_mismatch"])
            zero = extracted["files"].get("src/empty.txt")
            run.check("extract.zero_byte_file_created", zero is not None and zero["size"] == 0, zero)
            run.check(
                "extract.nested_folder_preserved",
                (out_dir / "src" / "sub" / "deep" / "binary.bin").is_file(),
                str(out_dir / "src" / "sub" / "deep" / "binary.bin"),
            )
            run.check(
                "extract.hangul_name_preserved",
                (out_dir / "src" / "한글 문서.txt").is_file(),
                sorted(extracted["files"]),
            )

    def compare_extraction(self, out_dir: Path, run: CaseRun, expected_files: dict[str, str]) -> dict:
        files: dict[str, dict] = {}
        if out_dir.exists():
            for path in sorted(out_dir.rglob("*")):
                if path.is_file():
                    files[path.relative_to(out_dir).as_posix()] = {
                        "size": path.stat().st_size,
                        "sha256": sha256_file(path),
                    }
        missing = sorted(set(expected_files) - set(files))
        unexpected = sorted(set(files) - set(expected_files))
        mismatched = {
            name: {"expected": expected_files[name], "actual": files[name]["sha256"]}
            for name in expected_files
            if name in files and files[name]["sha256"] != expected_files[name]
        }
        run.measurements["selected_files" if run.case.selected_only else "extracted_files"] = files
        return {
            "files": files,
            "missing": missing,
            "unexpected": unexpected,
            "sha256_mismatch": mismatched,
        }


# --- Reporting --------------------------------------------------------------------------------


def new_run_dir(runs_dir: Path) -> Path:
    stamp = datetime.now().strftime("%Y%m%d-%H%M%S")
    suffix = "".join(random.choice(string.ascii_lowercase + string.digits) for _ in range(5))
    run_dir = runs_dir / f"gui-{stamp}-{suffix}"
    run_dir.mkdir(parents=True, exist_ok=False)
    return run_dir


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description="Validate the MothZip archive-list window on real archives.")
    parser.add_argument("--binary", required=True, help="path to the mothzip executable under test")
    parser.add_argument(
        "--runs-dir",
        default=str(Path(__file__).resolve().parent / "runs"),
        help="where the gui-<timestamp>-<rand> run directory is created",
    )
    parser.add_argument("--timeout", type=float, default=90.0, help="seconds to wait for one window or job")
    parser.add_argument("--case", action="append", dest="cases", help="run only the named case (repeatable)")
    args = parser.parse_args(argv[1:])

    if sys.platform != "win32":
        print("this harness drives Win32 windows and only runs on Windows", file=sys.stderr)
        return 2

    binary = Path(args.binary).resolve()
    if not binary.is_file():
        print(f"binary not found: {binary}", file=sys.stderr)
        return 2

    run_dir = new_run_dir(Path(args.runs_dir).resolve())
    harness = Harness(binary, run_dir, args.timeout)
    cases = [
        Case(name="zip-all", archive_name="sample.zip"),
        Case(name="7z-all", archive_name="sample.7z"),
        Case(name="tarzst-all", archive_name="sample.tar.zst"),
        Case(name="zip-selected", archive_name="sample.zip", selected_only=True),
        Case(name="zip-enter-key", archive_name="sample.zip", keyboard=True),
        Case(name="zip-corrupt", archive_name="corrupt.zip", expect_load_error=True),
        Case(name="zip-busy-close", archive_name="large.zip", busy_close=True),
    ]
    if args.cases:
        known = {case.name for case in cases}
        unknown = sorted(set(args.cases) - known)
        if unknown:
            print(f"unknown case(s): {unknown}; known: {sorted(known)}", file=sys.stderr)
            return 2
        cases = [case for case in cases if case.name in set(args.cases)]

    print(f"mothzip binary : {binary}")
    print(f"run directory  : {run_dir}")
    started = time.time()
    setup_error = None
    try:
        harness.prepare()
    except Exception as error:  # recorded, never swallowed
        setup_error = f"{type(error).__name__}: {error}"

    results: list[dict] = []
    if setup_error is None:
        large_size = (harness.fixtures / "large.zip").stat().st_size
        print(f"fixtures       : {len(harness.expected_entries)} entries, {len(harness.expected_files)} files")
        print(f"busy-close     : {len(harness.large_entries)} entries, large.zip = {large_size} bytes")
        for case in cases:
            run = harness.run_case(case)
            results.append(run.as_dict())
            mark = "PASS" if run.passed else "FAIL"
            print(f"  [{mark}] {case.name}")
            for check in run.checks:
                if not check["ok"]:
                    print(f"         ! {check['name']}: {check['detail']}")
    else:
        print(f"fixture setup failed: {setup_error}", file=sys.stderr)

    passed = sum(1 for result in results if result["status"] == "pass")
    failed = len(results) - passed
    summary = {
        "cases": len(results),
        "passed": passed,
        "failed": failed,
        "expected_cases": len(cases),
        "setup_error": setup_error,
        "duration_seconds": round(time.time() - started, 2),
    }
    payload = {
        "run_id": run_dir.name,
        "timestamp": datetime.now().isoformat(timespec="seconds"),
        "binary": str(binary),
        "binary_sha256": sha256_file(binary),
        "python": sys.version.split()[0],
        "expected_entries": harness.expected_entries,
        "expected_files": harness.expected_files,
        "busy_close_expected_entries": harness.large_entries,
        "busy_close_expected_files": harness.large_files,
        "pack_log": harness.pack_log,
        "unverified": [
            "interactive folder picker (MOTHZIP_UI_TEST_EXTRACT_DIR replaces it here)",
            "archives above the 50000 row cap only have a unit test of the status text",
            "encrypted and budget-exceeded archives are not driven through the window",
            "high DPI layouts are only measured at this host's DPI",
            "two windows open at the same time",
            "the busy-close fixture is stored (level 0); compressed archive timing is not measured",
        ],
        "summary": summary,
        "cases": results,
    }
    results_path = run_dir / "results.json"
    results_path.write_text(json.dumps(payload, ensure_ascii=False, indent=2), encoding="utf-8")

    print()
    print(f"cases={summary['cases']} passed={passed} failed={failed}")
    if setup_error:
        print(f"setup: {setup_error}")
    print(f"results: {results_path}")
    return 0 if failed == 0 and setup_error is None and results else 1


if __name__ == "__main__":
    sys.exit(main(sys.argv))
