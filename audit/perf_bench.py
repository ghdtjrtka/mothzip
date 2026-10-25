#!/usr/bin/env python3
"""MothZip performance baseline bench (Windows).

Purpose: turn the "lightweight" claim into numbers that can be defended and
re-measured on the next release.

  1. throughput (MB/s) + peak RSS per format x operation,
  2. relative speed against the Python standard library (zipfile / tarfile)
     doing the same work on the same fixtures,
  3. a regression baseline: re-running this script with the same arguments
     produces comparable rows.

This script records numbers only. It deliberately writes no faster/slower
verdict; interpretation belongs to whoever reads results.json.

Usage
-----
    python audit/perf_bench.py --binary G:\\mothzip-build\\release\\mothzip.exe

Output
------
    audit/runs/perf-<YYYYmmdd-HHMMSS>-<rand>/results.json
    audit/runs/perf-<YYYYmmdd-HHMMSS>-<rand>/fixtures/...   (deterministic)

Measurement rules that matter (each is a real trap):
  * Every child process is started with ``WorkingDirectory`` = fixture folder
    and receives *relative* paths. Passing relative paths with a different
    working directory is what produced spurious exit 1 in an earlier attempt.
  * ``cat`` is invoked through ``cmd /c "... > NUL"``. A PowerShell ``>``
    redirect re-encodes the stream and corrupts binary payloads; the pre-flight
    self-check proves the bytes survive (sha256 of stream vs source).
  * Peak RSS is taken from the kernel-maintained peak working set
    (``Process.PeakWorkingSet64`` / ``psutil memory_info().peak_wset``), which
    is sampled continuously by the OS, and is cross-recorded with a 10 ms
    poll of ``Process.WorkingSet64`` (``memory_info().rss``).
  * One warm-up run per case is executed and excluded from statistics, so the
    file cache / code pages / allocator are warm for the measured runs.
  * Raw per-run millisecond values are always preserved in ``runs``.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import platform
import random
import shutil
import statistics
import subprocess
import sys
import time
from datetime import datetime
from pathlib import Path

try:
    import psutil  # type: ignore
except Exception:  # pragma: no cover - optional
    psutil = None

# ---------------------------------------------------------------------------
# Fixed bench definition
# ---------------------------------------------------------------------------

SCHEMA = "mothzip-perf-baseline/1"

SEED = 20261026
MIB = 1024 * 1024

TREE_FILES = 2000
TREE_FILE_BYTES = 4096

# level maxima accepted by the binary (probed: zip 10 -> error, zstd 23 -> error)
MAX_LEVEL = {"zip": 9, "7z": 9, "tar.gz": 9, "tar.zst": 22}

CREATE_NO_WINDOW = getattr(subprocess, "CREATE_NO_WINDOW", 0x08000000)

WORKLOAD_FIXTURE = "mixed"      # primary format matrix workload
COMPRESSIBILITY = ("random", "text", "mixed")


# ---------------------------------------------------------------------------
# small helpers
# ---------------------------------------------------------------------------

def sha256_file(path: Path, chunk: int = MIB) -> str:
    h = hashlib.sha256()
    with open(path, "rb") as fh:
        for block in iter(lambda: fh.read(chunk), b""):
            h.update(block)
    return h.hexdigest()


def dir_bytes(path: Path) -> int:
    if not path.exists():
        return 0
    if path.is_file():
        return path.stat().st_size
    total = 0
    for root, _dirs, files in os.walk(path):
        for name in files:
            try:
                total += os.path.getsize(os.path.join(root, name))
            except OSError:
                pass
    return total


def mib_per_s(nbytes: int | None, ms: float | None) -> float | None:
    if not nbytes or ms is None or ms <= 0:
        return None
    return (nbytes / MIB) / (ms / 1000.0)


def round_or_none(value, digits: int = 3):
    if value is None:
        return None
    return round(float(value), digits)


def cpu_calibration(seconds: float = 0.10) -> float:
    """Single-core speed index (integer-loop iterations/second).

    A fixed amount of pure-Python arithmetic work is timed, so a busy or
    throttled machine scores lower. Measured before and after the case list:
    a large drift means this run shared the CPU with other work, and the
    CPU-bound rows (7z, high zstd levels) are then not comparable against a
    baseline taken on an idle machine.
    """
    end = time.perf_counter() + seconds
    iterations = 0
    value = 1
    while time.perf_counter() < end:
        for _ in range(2000):
            value = (value * 1103515245 + 12345) & 0x7FFFFFFF
        iterations += 2000
    return iterations / seconds


# ---------------------------------------------------------------------------
# environment
# ---------------------------------------------------------------------------

PS_ENV_SCRIPT = r"""
$ErrorActionPreference = 'Stop'
$cpu  = Get-CimInstance Win32_Processor | Select-Object -First 1
$os   = Get-CimInstance Win32_OperatingSystem
$cs   = Get-CimInstance Win32_ComputerSystem
$dl   = '__DRIVE__'
$part = Get-Partition -DriveLetter $dl -ErrorAction SilentlyContinue
$disk = $null
if ($part) { $disk = Get-PhysicalDisk -DeviceNumber $part.DiskNumber -ErrorAction SilentlyContinue }
[pscustomobject]@{
  cpu_name     = $cpu.Name
  cpu_cores    = [int]$cpu.NumberOfCores
  cpu_logical  = [int]$cpu.NumberOfLogicalProcessors
  cpu_max_mhz  = [int]$cpu.MaxClockSpeed
  ram_bytes    = [int64]$cs.TotalPhysicalMemory
  os_caption   = $os.Caption
  os_version   = $os.Version
  os_build     = $os.BuildNumber
  disk_letter  = $dl
  disk_number  = if ($part) { [int]$part.DiskNumber } else { $null }
  disk_model   = if ($disk) { [string]$disk.FriendlyName } else { $null }
  disk_media   = if ($disk) { [string]$disk.MediaType } else { $null }
  disk_bus     = if ($disk) { [string]$disk.BusType } else { $null }
} | ConvertTo-Json -Compress
"""


def collect_environment(binary_path: Path) -> dict:
    """CPU / RAM / OS / physical disk behind the bench drive."""
    drive = (binary_path.drive or "C:").rstrip(":")[:1] or "C"
    env = {
        "captured_at": datetime.now().astimezone().isoformat(timespec="seconds"),
        "python": sys.version.split()[0],
        "platform": platform.platform(),
        "source": "powershell-cim",
    }
    try:
        script = PS_ENV_SCRIPT.replace("__DRIVE__", drive)
        proc = subprocess.run(
            ["powershell", "-NoProfile", "-NonInteractive", "-Command", script],
            capture_output=True, text=True, timeout=90,
            creationflags=CREATE_NO_WINDOW,
        )
        if proc.returncode == 0 and proc.stdout.strip():
            env.update(json.loads(proc.stdout.strip()))
            env["ram_gib"] = round(env.get("ram_bytes", 0) / (1024 ** 3), 2)
        else:
            raise RuntimeError((proc.stderr or "no output").strip()[:300])
    except Exception as exc:  # degrade honestly instead of failing the bench
        env["source"] = "fallback"
        env["source_error"] = f"{type(exc).__name__}: {exc}"
        env["cpu_name"] = platform.processor() or "unknown"
        env["cpu_logical"] = os.cpu_count()
        env["cpu_cores"] = None
        try:
            import ctypes

            class _MemStatus(ctypes.Structure):
                _fields_ = [
                    ("dwLength", ctypes.c_ulong),
                    ("dwMemoryLoad", ctypes.c_ulong),
                    ("ullTotalPhys", ctypes.c_ulonglong),
                    ("ullAvailPhys", ctypes.c_ulonglong),
                    ("ullTotalPageFile", ctypes.c_ulonglong),
                    ("ullAvailPageFile", ctypes.c_ulonglong),
                    ("ullTotalVirtual", ctypes.c_ulonglong),
                    ("ullAvailVirtual", ctypes.c_ulonglong),
                    ("ullAvailExtendedVirtual", ctypes.c_ulonglong),
                ]

            stat = _MemStatus()
            stat.dwLength = ctypes.sizeof(_MemStatus)
            ctypes.windll.kernel32.GlobalMemoryStatusEx(ctypes.byref(stat))
            env["ram_bytes"] = int(stat.ullTotalPhys)
            env["ram_gib"] = round(stat.ullTotalPhys / (1024 ** 3), 2)
        except Exception:
            env["ram_bytes"] = None
            env["ram_gib"] = None
    env["bench_drive"] = f"{drive}:"
    return env


# ---------------------------------------------------------------------------
# fixtures (deterministic)
# ---------------------------------------------------------------------------

def build_fixtures(fix_dir: Path, size_mib: int, seed: int) -> dict:
    """Create the three compressibility classes plus a directory tree.

    Everything is derived from a fixed seed, so a re-run packs identical bytes.
    """
    fix_dir.mkdir(parents=True, exist_ok=True)
    size = size_mib * MIB
    rng = random.Random(seed)
    built = {}

    # --- random: incompressible
    random_path = fix_dir / "random.bin"
    with open(random_path, "wb") as fh:
        remaining = size
        while remaining > 0:
            block = min(4 * MIB, remaining)
            fh.write(rng.randbytes(block))
            remaining -= block
    built["random"] = {"path": random_path, "bytes": size}

    # --- text: highly compressible, realistic word stream
    words = [b"the", b"quick", b"brown", b"fox", b"jumps", b"over", b"lazy",
             b"dog", b"archive", b"compress", b"stream", b"buffer", b"window"]
    text_path = fix_dir / "text.txt"
    text_body = bytearray()
    while len(text_body) < size:
        text_body += b" ".join(rng.choice(words) for _ in range(12)) + b"\n"
    text_bytes = bytes(text_body[:size])
    with open(text_path, "wb") as fh:
        fh.write(text_bytes)
    built["text"] = {"path": text_path, "bytes": size}

    # --- mixed: half text, half random, interleaved in 512 KiB chunks
    mixed_path = fix_dir / "mixed.bin"
    chunk = 512 * 1024
    mixed_rng = random.Random(seed + 1)
    with open(mixed_path, "wb") as fh:
        written = 0
        while written < size:
            fh.write(mixed_rng.randbytes(min(chunk, size - written)))
            written += min(chunk, size - written)
            if written >= size:
                break
            take = min(chunk, size - written)
            fh.write(text_bytes[written:written + take])
            written += take
    built["mixed"] = {"path": mixed_path, "bytes": size}

    # --- directory tree: many small files (a different perf profile entirely)
    tree_dir = fix_dir / "tree"
    file_rng = random.Random(seed + 2)
    tree_dir.mkdir(parents=True, exist_ok=True)
    for index in range(TREE_FILES):
        sub = tree_dir / f"d{index % 40:02d}"
        sub.mkdir(exist_ok=True)
        head = (f"file {index:05d} lorem ipsum dolor sit amet "
                f"consectetur adipiscing elit {index:05d}\n").encode()
        filler = head * (TREE_FILE_BYTES // len(head) + 1)
        # one deterministic byte per file keeps them non-identical
        tail = bytes([file_rng.randrange(256)]) * 8
        body = (filler + tail)[:TREE_FILE_BYTES]
        with open(sub / f"f{index:05d}.txt", "wb") as fh:
            fh.write(body)
    built["tree"] = {"path": tree_dir, "bytes": TREE_FILES * TREE_FILE_BYTES,
                     "files": TREE_FILES, "file_bytes": TREE_FILE_BYTES}

    for name, info in built.items():
        info["measured_bytes"] = dir_bytes(info["path"])
    return built


# ---------------------------------------------------------------------------
# measurement harness
# ---------------------------------------------------------------------------

def _working_set_ctypes(pid: int):
    """Fallback working-set read via PSAPI (same counter as WorkingSet64)."""
    try:
        import ctypes
        from ctypes import wintypes

        class PROCESS_MEMORY_COUNTERS(ctypes.Structure):
            _fields_ = [
                ("cb", wintypes.DWORD),
                ("PageFaultCount", wintypes.DWORD),
                ("PeakWorkingSetSize", ctypes.c_size_t),
                ("WorkingSetSize", ctypes.c_size_t),
                ("QuotaPeakPagedPoolUsage", ctypes.c_size_t),
                ("QuotaPagedPoolUsage", ctypes.c_size_t),
                ("QuotaPeakNonPagedPoolUsage", ctypes.c_size_t),
                ("QuotaNonPagedPoolUsage", ctypes.c_size_t),
                ("PagefileUsage", ctypes.c_size_t),
                ("PeakPagefileUsage", ctypes.c_size_t),
            ]

        PROCESS_QUERY_LIMITED_INFORMATION = 0x1000
        kernel32 = ctypes.windll.kernel32
        psapi = ctypes.windll.psapi
        handle = kernel32.OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, False, pid)
        if not handle:
            return None
        try:
            counters = PROCESS_MEMORY_COUNTERS()
            counters.cb = ctypes.sizeof(PROCESS_MEMORY_COUNTERS)
            if psapi.GetProcessMemoryInfo(handle, ctypes.byref(counters),
                                          counters.cb):
                return {
                    "rss": int(counters.WorkingSetSize),
                    "peak": int(counters.PeakWorkingSetSize),
                }
        finally:
            kernel32.CloseHandle(handle)
        return None
    except Exception:
        return None


def measure(command, cwd: Path, timeout_s: float = 900.0,
            sample_ms: float = 10.0, out_path: Path | None = None,
            err_path: Path | None = None) -> dict:
    """Run one invocation and return wall time + peak working set.

    ``command`` is either a list (direct exec) or a string (verbatim command
    line, used for the ``cmd /c ... > NUL`` cat case).
    """
    out_path = out_path or (cwd / "_bench_stdout.tmp")
    err_path = err_path or (cwd / "_bench_stderr.tmp")
    started = time.perf_counter()
    with open(out_path, "wb") as stdout_fh, open(err_path, "wb") as stderr_fh:
        proc = subprocess.Popen(
            command,
            cwd=str(cwd),                 # <- resolves the relative-path trap
            stdin=subprocess.DEVNULL,
            stdout=stdout_fh,
            stderr=stderr_fh,
            creationflags=CREATE_NO_WINDOW,
        )
        polled_peak = 0
        kernel_peak = 0
        watch = None
        if psutil is not None:
            try:
                watch = psutil.Process(proc.pid)
            except Exception:
                watch = None
        timed_out = False
        while True:
            if proc.poll() is not None:
                break
            if watch is not None:
                try:
                    info = watch.memory_info()
                    if info.rss > polled_peak:
                        polled_peak = info.rss
                    # peak_wset == Process.PeakWorkingSet64 (kernel maintained)
                    peak = getattr(info, "peak_wset", None)
                    if peak and peak > kernel_peak:
                        kernel_peak = peak
                except Exception:
                    pass
            else:
                got = _working_set_ctypes(proc.pid)
                if got:
                    polled_peak = max(polled_peak, got["rss"])
                    kernel_peak = max(kernel_peak, got["peak"])
            if time.perf_counter() - started > timeout_s:
                timed_out = True
                proc.kill()
                break
            time.sleep(sample_ms / 1000.0)
        exit_code = proc.wait()
    elapsed_ms = (time.perf_counter() - started) * 1000.0

    try:
        stdout_text = out_path.read_text("utf-8", "replace")
    except Exception:
        stdout_text = ""
    try:
        stderr_text = err_path.read_text("utf-8", "replace")
    except Exception:
        stderr_text = ""

    return {
        "ms": elapsed_ms,
        "exit": exit_code,
        "timed_out": timed_out,
        "peak_rss_mib": round((kernel_peak or polled_peak) / MIB, 2) if (kernel_peak or polled_peak) else None,
        "polled_rss_mib": round(polled_peak / MIB, 2) if polled_peak else None,
        "stdout": stdout_text,
        "stderr": stderr_text,
    }


# ---------------------------------------------------------------------------
# case execution
# ---------------------------------------------------------------------------

def run_case(bench, *, case_id, fmt, operation, fixture, level_label,
             level_value, command, cwd, input_bytes, bytes_basis,
             prepare=None, probe=None) -> dict:
    """Execute warm-up + N measured runs and aggregate them."""
    if bench.over_budget:
        return {
            "case": case_id, "format": fmt, "operation": operation,
            "fixture": fixture, "level": level_value, "level_label": level_label,
            "status": "skipped", "runs": [], "median_ms": None,
            "median_mb_s": None, "peak_rss_mib": None, "output_bytes": None,
            "error": "time budget exhausted before this case", "command": command,
        }

    runs, rss_values, polled_values, exit_codes = [], [], [], []
    failures, probes = [], []
    warmup_ms = None

    for index in range(bench.runs + 1):
        if prepare:
            prepare()
        result = measure(command, cwd=cwd, timeout_s=bench.case_timeout,
                         out_path=bench.work_dir / "_bench_stdout.tmp",
                         err_path=bench.work_dir / "_bench_stderr.tmp")
        if index == 0:
            warmup_ms = round(result["ms"], 2)
            warmup_ok = result["exit"] == 0 and not result["timed_out"]
            if not warmup_ok:
                # a failing warm-up is reported, never counted as a pass
                failures.append({
                    "phase": "warmup",
                    "exit": result["exit"],
                    "timed_out": result["timed_out"],
                    "stderr": (result["stderr"] or "").strip()[:600],
                })
            continue
        if result["timed_out"]:
            failures.append({"phase": "measured", "run": index, "error": "timeout"})
            continue
        if result["exit"] != 0:
            failures.append({
                "phase": "measured", "run": index, "exit": result["exit"],
                "stderr": (result["stderr"] or "").strip()[:600],
            })
            continue
        runs.append(round(result["ms"], 2))
        exit_codes.append(result["exit"])
        if result["peak_rss_mib"] is not None:
            rss_values.append(result["peak_rss_mib"])
        if result["polled_rss_mib"] is not None:
            polled_values.append(result["polled_rss_mib"])
        if probe:
            probes.append(probe())

    row = {
        "case": case_id,
        "format": fmt,
        "operation": operation,
        "fixture": fixture,
        "level": level_value,
        "level_label": level_label,
        "status": "ok",
        "runs": runs,
        "warmup_ms": warmup_ms,
        "median_ms": None,
        "mean_ms": None,
        "stdev_ms": None,
        "median_mb_s": None,
        "peak_rss_mib": None,
        "output_bytes": None,
        "input_bytes": input_bytes,
        "bytes_basis": bytes_basis,
        "compression_ratio": None,
        "exit_codes": exit_codes,
        "failures": failures,
        "error": None,
        "command": command,
    }

    if failures or not runs:
        row["status"] = "fail"
        row["error"] = "; ".join(
            f"{f.get('phase')}/run{f.get('run', '-')}: "
            f"exit={f.get('exit')} {f.get('error', '') or ''} "
            f"{(f.get('stderr') or '')[:200]}".strip()
            for f in failures
        ) or "no successful measured run"
        return row

    row["median_ms"] = round(statistics.median(runs), 2)
    row["mean_ms"] = round(statistics.fmean(runs), 2)
    row["stdev_ms"] = round(statistics.stdev(runs), 2) if len(runs) > 1 else 0.0
    row["median_mb_s"] = round_or_none(mib_per_s(input_bytes, row["median_ms"]))
    if rss_values:
        row["peak_rss_mib"] = max(rss_values)
        row["polled_rss_mib"] = max(polled_values) if polled_values else None

    if probes:
        numbers = [p["output_bytes"] for p in probes if p.get("output_bytes")]
        if numbers:
            row["output_bytes"] = max(numbers)
        ratio_numbers = [p["compression_ratio"] for p in probes
                         if p.get("compression_ratio")]
        if ratio_numbers:
            row["compression_ratio"] = round(max(ratio_numbers), 3)
    return row


# ---------------------------------------------------------------------------
# bench driver
# ---------------------------------------------------------------------------

class Bench:
    def __init__(self, args, binary: Path, run_dir: Path):
        self.args = args
        self.binary = binary
        self.run_dir = run_dir
        self.fixtures_dir = run_dir / "fixtures"
        self.work_dir = run_dir / "work"
        self.runs = max(3, args.runs)
        self.started = time.perf_counter()
        self.deadline_s = args.deadline
        self.case_timeout = args.case_timeout
        self.results: list[dict] = []

    # -- budget -----------------------------------------------------------
    def elapsed(self) -> float:
        return time.perf_counter() - self.started

    @property
    def over_budget(self) -> bool:
        return self.elapsed() > self.deadline_s

    # -- plumbing ---------------------------------------------------------
    def add(self, row: dict):
        self.results.append(row)
        marker = "ok  " if row["status"] == "ok" else row["status"].upper()
        speed = f'{row["median_mb_s"]:9.1f} MB/s' if row["median_mb_s"] else " " * 13
        rss = f'{row["peak_rss_mib"]:7.1f} MiB' if row["peak_rss_mib"] else " " * 11
        med = f'{row["median_ms"]:9.1f} ms' if row["median_ms"] else " " * 12
        print(f"  [{marker}] {row['case']:<34} {med} {speed} {rss}")
        if row["status"] == "fail":
            print(f"         ↳ {row['error'][:200]}")


def build_python_baseline_script(path: Path):
    """The stdlib comparison, run as its own process so RSS is comparable."""
    path.write_text(
        '''import sys, os, zipfile, tarfile
mode, src, dst = sys.argv[1], sys.argv[2], sys.argv[3]
if mode == "zip-pack":
    with zipfile.ZipFile(dst, "w", zipfile.ZIP_DEFLATED, compresslevel=6) as z:
        z.write(src, os.path.basename(src))
elif mode == "zip-unpack":
    os.makedirs(dst, exist_ok=True)
    with zipfile.ZipFile(src) as z:
        z.extractall(dst)
elif mode == "targz-pack":
    with tarfile.open(dst, "w:gz") as tf:
        tf.add(src, os.path.basename(src))
elif mode == "targz-unpack":
    os.makedirs(dst, exist_ok=True)
    with tarfile.open(src) as tf:
        tf.extractall(dst, filter="data")
else:
    raise SystemExit("unknown mode: " + mode)
''',
        encoding="utf-8",
    )


def preflight(bench: Bench, fixtures: dict) -> dict:
    """Prove the cat pipe preserves bytes before trusting it as a measurement."""
    checks = {}
    work = bench.work_dir
    work.mkdir(parents=True, exist_ok=True)
    archive = work / "preflight.zip"
    subprocess.run([str(bench.binary), "pack", str(archive), "mixed.bin"],
                   cwd=str(bench.fixtures_dir), check=True,
                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                   creationflags=CREATE_NO_WINDOW)
    streamed = work / "preflight_cat.bin"
    # archive referenced by absolute path; the agent runs from the work dir
    line = 'cmd /c ""%s" cat "%s" "mixed.bin" > "%s""' % (
        bench.binary, archive, streamed)
    proc = subprocess.run(line, cwd=str(work), capture_output=True, text=True,
                          creationflags=CREATE_NO_WINDOW)
    source_sha = sha256_file(fixtures["mixed"]["path"])
    stream_sha = sha256_file(streamed) if streamed.exists() else None
    checks["cat_byte_integrity"] = {
        "status": "pass" if (proc.returncode == 0 and source_sha == stream_sha) else "fail",
        "source_sha256": source_sha,
        "streamed_sha256": stream_sha,
        "exit": proc.returncode,
        "stderr": (proc.stderr or "").strip()[:300],
        "method": "cmd /c redirect to file (PowerShell > would re-encode)",
    }
    streamed.unlink(missing_ok=True)
    archive.unlink(missing_ok=True)

    checks["rss_method"] = {
        "primary": "kernel peak working set (Process.PeakWorkingSet64 / memory_info().peak_wset)",
        "cross_check": "10 ms poll of Process.WorkingSet64 (memory_info().rss)",
        "psutil_available": psutil is not None,
    }
    checks["python_baseline"] = {
        "available": sys.executable is not None,
        "executable": sys.executable,
        "zip_compresslevel": 6,
        "targz_compresslevel": 9,
    }
    return checks


# ---------------------------------------------------------------------------
# main
# ---------------------------------------------------------------------------

def resolve_binary(explicit: str | None) -> Path:
    candidates = []
    if explicit:
        candidates.append(Path(explicit))
    candidates.append(Path(r"G:\mothzip-build\release\mothzip.exe"))
    local = os.environ.get("LOCALAPPDATA")
    if local:
        candidates.append(Path(local) / "Programs" / "MothZip" / "mothzip.exe")
    candidates.append(Path(r"C:\Users\Administrator\AppData\Local\Programs\MothZip\mothzip.exe"))
    for cand in candidates:
        if cand.is_file():
            return cand.resolve()
    raise SystemExit("error: mothzip.exe not found (tried: %s)"
                     % ", ".join(str(c) for c in candidates))


def parse_args(argv):
    parser = argparse.ArgumentParser(
        description="MothZip performance baseline bench",
        formatter_class=argparse.ArgumentDefaultsHelpFormatter,
    )
    parser.add_argument("--binary", help="path to mothzip.exe")
    parser.add_argument("--runs", type=int, default=3,
                        help="measured repetitions per case (warm-up is extra)")
    parser.add_argument("--size-mib", type=int, default=32,
                        help="size of each large fixture in MiB")
    parser.add_argument("--deadline", type=float, default=540.0,
                        help="soft wall-clock budget in seconds")
    parser.add_argument("--case-timeout", type=float, default=600.0,
                        help="hard timeout for a single invocation")
    parser.add_argument("--seed", type=int, default=SEED)
    parser.add_argument("--out-dir", help="parent directory for the run folder")
    return parser.parse_args(argv)


def main(argv=None) -> int:
    args = parse_args(argv if argv is not None else sys.argv[1:])

    binary = resolve_binary(args.binary)
    binary_sha = sha256_file(binary)
    try:
        version_proc = subprocess.run([str(binary), "--version"],
                                      capture_output=True, text=True,
                                      creationflags=CREATE_NO_WINDOW, timeout=60)
        version = (version_proc.stdout or "").strip() or (version_proc.stderr or "").strip()
        version_exit = version_proc.returncode
    except Exception as exc:
        version, version_exit = f"unavailable: {exc}", None

    stamp = datetime.now().strftime("%Y%m%d-%H%M%S")
    suffix = os.urandom(3).hex()
    repo_audit = Path(__file__).resolve().parent
    out_parent = Path(args.out_dir) if args.out_dir else (repo_audit / "runs")
    run_dir = out_parent / f"perf-{stamp}-{suffix}"
    run_dir.mkdir(parents=True, exist_ok=False)

    print("=" * 78)
    print("MothZip performance baseline")
    print("=" * 78)
    print(f"binary    : {binary}")
    print(f"version   : {version}   (exit {version_exit})")
    print(f"sha256    : {binary_sha}")
    print(f"run dir   : {run_dir}")
    print(f"runs/case : {max(3, args.runs)} (+1 warm-up)   fixture: {args.size_mib} MiB")
    print()

    environment = collect_environment(binary)
    print(f"cpu       : {environment.get('cpu_name')}")
    print(f"cores     : {environment.get('cpu_cores')} physical / "
          f"{environment.get('cpu_logical')} logical")
    print(f"ram       : {environment.get('ram_gib')} GiB")
    print(f"os        : {environment.get('os_caption')} {environment.get('os_build')}")
    print(f"disk      : {environment.get('disk_media')} "
          f"{environment.get('disk_model')} on {environment.get('bench_drive')}")
    print()

    cal_before = cpu_calibration()
    print(f"cpu index : {cal_before:,.0f} iters/s (single-core, before)")
    print()

    print("building fixtures (deterministic, seed=%d) ..." % args.seed)
    bench = Bench(args, binary, run_dir)
    bench.work_dir.mkdir(parents=True, exist_ok=True)
    fixtures = build_fixtures(bench.fixtures_dir, args.size_mib, args.seed)
    for name in ("random", "text", "mixed", "tree"):
        print(f"  {name:<7} {fixtures[name]['measured_bytes']:>12,} bytes")
    print(f"fixture build took {bench.elapsed():.1f}s")
    print()

    validation = preflight(bench, fixtures)
    for key, value in validation.items():
        status = value.get("status", "n/a") if isinstance(value, dict) else "n/a"
        print(f"preflight {key}: {status}")
    if validation["cat_byte_integrity"]["status"] != "pass":
        print("  WARNING: cat stream did not match source bytes; cat rows are unreliable")

    fx = bench.fixtures_dir
    mixed = fixtures["mixed"]
    tree = fixtures["tree"]

    # ---------------------------------------------------------------- pack
    print("\n[1/5] pack matrix (fixture: mixed)")
    pack_rows = [
        ("zip", "default", None),
        ("zip", "max", MAX_LEVEL["zip"]),
        ("7z", "default", None),
        ("7z", "max", MAX_LEVEL["7z"]),
        ("tar", "default", None),
        ("tar.gz", "default", None),
        ("tar.gz", "max", MAX_LEVEL["tar.gz"]),
        ("tar.zst", "default", None),
        ("tar.zst", "max", MAX_LEVEL["tar.zst"]),
    ]
    for fmt, label, level in pack_rows:
        archive = fx / f"mixed__{fmt.replace('.', '_')}__{label}.{fmt}"
        cmd = [str(binary), "pack"]
        if level is not None:
            cmd += ["--level", str(level)]
        cmd += [archive.name, mixed["path"].name]
        archive.unlink(missing_ok=True)

        def prep(a=archive):
            a.unlink(missing_ok=True)

        def probe(a=archive, size=mixed["measured_bytes"]):
            got = a.stat().st_size if a.exists() else None
            return {"output_bytes": got,
                    "compression_ratio": round(size / got, 3) if got else None}

        bench.add(run_case(
            bench, case_id=f"pack/{fmt}/{label}", fmt=fmt, operation="pack",
            fixture="mixed", level_label=label, level_value=level,
            command=cmd, cwd=fx, input_bytes=mixed["measured_bytes"],
            bytes_basis="input_uncompressed", prepare=prep, probe=probe,
        ))

    # ------------------------------------------------- python baseline (pack)
    print("\n[2/5] Python stdlib baseline (fixture: mixed)")
    py_script = bench.work_dir / "py_baseline.py"
    build_python_baseline_script(py_script)
    py_archives = {}

    py_pack_cases = [
        ("zipfile-deflate", "zip-pack", "py.zip", ".zip"),
        ("tarfile-gz", "targz-pack", "py.tar.gz", ".tar.gz"),
    ]
    for label, mode, out_name, ext in py_pack_cases:
        dest = fx / out_name

        def prep(d=dest):
            d.unlink(missing_ok=True)

        def probe(d=dest, size=mixed["measured_bytes"]):
            got = d.stat().st_size if d.exists() else None
            return {"output_bytes": got,
                    "compression_ratio": round(size / got, 3) if got else None}

        bench.add(run_case(
            bench, case_id=f"pack/{label}/default", fmt=label, operation="pack",
            fixture="mixed", level_label="default", level_value=None,
            command=[sys.executable, str(py_script), mode, mixed["path"].name,
                     dest.name],
            cwd=fx, input_bytes=mixed["measured_bytes"],
            bytes_basis="input_uncompressed", prepare=prep, probe=probe,
        ))
        # keep the measured archive for the unpack leg below
        if dest.exists():
            py_archives[ext] = dest

    py_unpack_cases = [
        ("zipfile-deflate", "zip-unpack", py_archives[".zip"], "pyx"),
        ("tarfile-gz", "targz-unpack", py_archives[".tar.gz"], "pyt"),
    ]
    for label, mode, archive, out_name in py_unpack_cases:
        target = fx / out_name

        def prep(t=target):
            shutil.rmtree(t, ignore_errors=True)

        def probe(t=target):
            return {"output_bytes": dir_bytes(t)}

        bench.add(run_case(
            bench, case_id=f"unpack/{label}/default", fmt=label,
            operation="unpack", fixture="mixed", level_label="default",
            level_value=None,
            command=[sys.executable, str(py_script), mode, archive.name,
                     target.name],
            cwd=fx, input_bytes=mixed["measured_bytes"],
            bytes_basis="output_uncompressed", prepare=prep, probe=probe,
        ))

    # ------------------------------------------------------- read operations
    print("\n[3/5] read operations on default-level archives (fixture: mixed)")
    read_formats = ["zip", "7z", "tar", "tar.gz", "tar.zst"]
    cat_members = {}
    for fmt in read_formats:
        archive = fx / f"mixed__{fmt.replace('.', '_')}__default.{fmt}"
        if not archive.exists():
            subprocess.run([str(binary), "pack", archive.name, mixed["path"].name],
                           cwd=str(fx), check=True,
                           stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                           creationflags=CREATE_NO_WINDOW)
        cat_members[fmt] = (archive, mixed["path"].name)

    for fmt in read_formats:
        archive, member = cat_members[fmt]
        base = fmt.replace(".", "_")

        # unpack
        target = fx / f"ex_{base}"

        def prep(t=target):
            shutil.rmtree(t, ignore_errors=True)

        def probe(t=target):
            return {"output_bytes": dir_bytes(t)}

        bench.add(run_case(
            bench, case_id=f"unpack/{fmt}/default", fmt=fmt, operation="unpack",
            fixture="mixed", level_label="default", level_value=None,
            command=[str(binary), "unpack", archive.name, "-o", target.name],
            cwd=fx, input_bytes=mixed["measured_bytes"],
            bytes_basis="output_uncompressed", prepare=prep, probe=probe,
        ))

        # test
        bench.add(run_case(
            bench, case_id=f"test/{fmt}/default", fmt=fmt, operation="test",
            fixture="mixed", level_label="default", level_value=None,
            command=[str(binary), "test", archive.name],
            cwd=fx, input_bytes=mixed["measured_bytes"],
            bytes_basis="output_uncompressed",
            probe=lambda: {"output_bytes": mixed["measured_bytes"]},
        ))

        # peek
        bench.add(run_case(
            bench, case_id=f"peek/{fmt}/default", fmt=fmt, operation="peek",
            fixture="mixed", level_label="default", level_value=None,
            command=[str(binary), "peek", archive.name],
            cwd=fx, input_bytes=archive.stat().st_size,
            bytes_basis="archive_bytes",
            probe=lambda a=archive: {"output_bytes": a.stat().st_size},
        ))

        # cat -> NUL through cmd (PowerShell redirect would corrupt bytes)
        cat_line = 'cmd /c ""%s" cat "%s" "%s" > NUL"' % (binary, archive.name, member)
        bench.add(run_case(
            bench, case_id=f"cat/{fmt}/default", fmt=fmt, operation="cat",
            fixture="mixed", level_label="default", level_value=None,
            command=cat_line, cwd=fx,
            input_bytes=mixed["measured_bytes"],
            bytes_basis="output_uncompressed",
            probe=lambda: {"output_bytes": mixed["measured_bytes"]},
        ))

    # ------------------------------------------------ compressibility sweep
    print("\n[4/5] compressibility sweep (pack, default level)")
    for fmt in ("zip", "tar.zst"):
        for fix_name in ("random", "text"):
            fixture = fixtures[fix_name]
            archive = fx / f"{fix_name}__{fmt.replace('.', '_')}.{fmt}"
            archive.unlink(missing_ok=True)

            def prep(a=archive):
                a.unlink(missing_ok=True)

            def probe(a=archive, size=fixture["measured_bytes"]):
                got = a.stat().st_size if a.exists() else None
                return {"output_bytes": got,
                        "compression_ratio": round(size / got, 3) if got else None}

            bench.add(run_case(
                bench, case_id=f"pack-{fix_name}/{fmt}/default", fmt=fmt,
                operation="pack",
                fixture=fix_name, level_label="default", level_value=None,
                command=[str(binary), "pack", archive.name, fixture["path"].name],
                cwd=fx, input_bytes=fixture["measured_bytes"],
                bytes_basis="input_uncompressed", prepare=prep, probe=probe,
            ))

    # -------------------------------------------------------- directory tree
    print("\n[5/5] directory tree (2000 files x 4 KiB)")
    tree_pack = [("zip", None), ("7z", None), ("tar", None), ("tar.gz", None)]
    for fmt, level in tree_pack:
        archive = fx / f"tree.{fmt}"
        archive.unlink(missing_ok=True)
        cmd = [str(binary), "pack"]
        if level is not None:
            cmd += ["--level", str(level)]
        cmd += [archive.name, "tree"]

        def prep(a=archive):
            a.unlink(missing_ok=True)

        def probe(a=archive, size=tree["measured_bytes"]):
            got = a.stat().st_size if a.exists() else None
            return {"output_bytes": got,
                    "compression_ratio": round(size / got, 3) if got else None}

        bench.add(run_case(
            bench, case_id=f"pack-tree/{fmt}/default", fmt=fmt, operation="pack",
            fixture="tree", level_label="default", level_value=None,
            command=cmd, cwd=fx, input_bytes=tree["measured_bytes"],
            bytes_basis="input_uncompressed", prepare=prep, probe=probe,
        ))

    tree_zip = fx / "tree.zip"
    if tree_zip.exists():
        target = fx / "ex_tree"

        def prep(t=target):
            shutil.rmtree(t, ignore_errors=True)

        def probe(t=target):
            return {"output_bytes": dir_bytes(t)}

        bench.add(run_case(
            bench, case_id="unpack-tree/zip/default", fmt="zip",
            operation="unpack", fixture="tree", level_label="default",
            level_value=None,
            command=[str(binary), "unpack", tree_zip.name, "-o", target.name],
            cwd=fx, input_bytes=tree["measured_bytes"],
            bytes_basis="output_uncompressed", prepare=prep, probe=probe,
        ))
        bench.add(run_case(
            bench, case_id="test-tree/zip/default", fmt="zip", operation="test",
            fixture="tree", level_label="default", level_value=None,
            command=[str(binary), "test", tree_zip.name],
            cwd=fx, input_bytes=tree["measured_bytes"],
            bytes_basis="output_uncompressed",
            probe=lambda: {"output_bytes": tree["measured_bytes"]},
        ))
        bench.add(run_case(
            bench, case_id="peek-tree/zip/default", fmt="zip", operation="peek",
            fixture="tree", level_label="default", level_value=None,
            command=[str(binary), "peek", tree_zip.name],
            cwd=fx, input_bytes=tree_zip.stat().st_size,
            bytes_basis="archive_bytes",
            probe=lambda a=tree_zip: {"output_bytes": a.stat().st_size},
        ))

    # ------------------------------------------------------------- assemble
    total_s = bench.elapsed()
    cal_after = cpu_calibration()
    drift_pct = round((cal_after - cal_before) / cal_before * 100.0, 1) if cal_before else None
    print(f"\ncpu index : {cal_after:,.0f} iters/s (after), drift {drift_pct}%")
    if drift_pct is not None and abs(drift_pct) > 15.0:
        print("  NOTE: large drift -> the machine was busy during this run; "
              "treat CPU-bound rows (7z, zstd max) as load-dependent")
    ok_rows = [r for r in bench.results if r["status"] == "ok"]
    fail_rows = [r for r in bench.results if r["status"] == "fail"]
    skipped_rows = [r for r in bench.results if r["status"] == "skipped"]

    payload = {
        "schema": SCHEMA,
        "generated_at": datetime.now().astimezone().isoformat(timespec="seconds"),
        "binary": {
            "path": str(binary),
            "version": version,
            "version_exit": version_exit,
            "sha256": binary_sha,
            "size_bytes": binary.stat().st_size,
        },
        "environment": environment,
        "machine_state": {
            "cpu_index_before": round(cal_before, 0),
            "cpu_index_after": round(cal_after, 0),
            "cpu_index_drift_pct": drift_pct,
            "logical_cpus": os.cpu_count(),
            "contended": bool(drift_pct is not None and abs(drift_pct) > 15.0),
            "cpu_index_units": "single-core integer-loop iterations/second",
            "note": "abs(drift) > 15% means the run shared the CPU; "
                    "CPU-bound rows are then load-dependent",
        },
        "config": {
            "runs_per_case": bench.runs,
            "warmup_runs": 1,
            "fixture_size_mib": args.size_mib,
            "seed": args.seed,
            "workload_fixture": WORKLOAD_FIXTURE,
            "compressibility_classes": list(COMPRESSIBILITY),
            "tree_files": TREE_FILES,
            "tree_file_bytes": TREE_FILE_BYTES,
            "deadline_s": bench.deadline_s,
            "case_timeout_s": bench.case_timeout,
            "mb_definition": "1 MiB = 1048576 bytes",
            "rss_definition": "kernel peak working set (PeakWorkingSet64)",
            "statistic": "median of measured runs; warm-up excluded",
            "cat_invocation": 'cmd /c ""<exe>" cat "<archive>" "<member>" > NUL"',
            "relative_paths_with_cwd": True,
        },
        "validation": validation,
        "fixtures": {
            name: {
                "path": str(info["path"]).replace(str(run_dir) + os.sep, ""),
                "measured_bytes": info["measured_bytes"],
                **({"files": info["files"], "file_bytes": info["file_bytes"]}
                   if "files" in info else {}),
            }
            for name, info in fixtures.items()
        },
        "totals": {
            "elapsed_s": round(total_s, 1),
            "cases": len(bench.results),
            "ok": len(ok_rows),
            "failed": len(fail_rows),
            "skipped": len(skipped_rows),
        },
        "results": bench.results,
    }

    results_path = run_dir / "results.json"
    results_path.write_text(json.dumps(payload, indent=2, ensure_ascii=False),
                            encoding="utf-8")

    # ---------------------------------------------------------------- print
    print("\n" + "=" * 78)
    print("SUMMARY  (median ms | MB/s | peak RSS MiB | ratio)")
    print("=" * 78)
    print(f"{'case':<34}{'median ms':>11}{'MB/s':>10}{'RSS MiB':>10}{'ratio':>8}")
    print("-" * 78)
    for row in bench.results:
        med = f'{row["median_ms"]:.1f}' if row["median_ms"] is not None else row["status"]
        speed = f'{row["median_mb_s"]:.1f}' if row["median_mb_s"] else "-"
        rss = f'{row["peak_rss_mib"]:.1f}' if row["peak_rss_mib"] else "-"
        ratio = f'{row["compression_ratio"]:.2f}' if row["compression_ratio"] else "-"
        print(f'{row["case"]:<34}{med:>11}{speed:>10}{rss:>10}{ratio:>8}')

    print("-" * 78)
    print(f"cases: {len(ok_rows)} ok, {len(fail_rows)} failed, "
          f"{len(skipped_rows)} skipped | elapsed {total_s:.1f}s "
          f"(budget {bench.deadline_s:.0f}s)")
    print(f"results: {results_path}")
    if fail_rows:
        print("\nFAILED (not counted as passing):")
        for row in fail_rows:
            print(f'  - {row["case"]}: {row["error"][:180]}')
    return 0 if not fail_rows else 1


if __name__ == "__main__":
    sys.exit(main())
