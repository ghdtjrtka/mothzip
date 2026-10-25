"""Measure the peak working set of `mothzip cat <rar> <member>`.

The RAR cat body (`inspect::cat_rar_file`) is the subject: this harness reports,
per member size, the peak resident memory of the child process that emits those
bytes, so a claim about "streaming" or "member-sized" memory is a measurement
rather than an inference.

What is measured
- peak working set of the CLI child process (Windows psapi GetProcessMemoryInfo,
  PROCESS_MEMORY_COUNTERS_EX.PeakWorkingSetSize, polled while the child is alive;
  the counter is monotonic so the last successful read is the run's peak)
- output SHA-256 vs the expected bytes of the member
- `test <archive>` exit code, so a memory change cannot silently break decoding

Where the cases come from
- --manifest generated.json : fixtures written by an external generator that
  records each member's size and SHA-256 in the manifest
- --reference reference.json : the repo's RAR reference manifests; the expected
  bytes are the `oracle/<entry>` files next to each archive
- --case rar::member::expected_file : one explicit case

No third-party packages: psapi/kernel32 are reached through ctypes.

Usage
  python audit/rar_cat_memory.py --binary <mothzip.exe> \
      --reference audit/runs/rar-reference-20261025-201124/reference.json \
      --manifest <tmp>/fixtures/manifest.json --repeat 3 --json <out>.json
"""
from pathlib import Path
import argparse
import ctypes
import ctypes.wintypes as wintypes
import hashlib
import json
import os
import subprocess
import sys
import tempfile
import time

PROCESS_QUERY_LIMITED_INFORMATION = 0x1000
PROCESS_VM_READ = 0x0010


class PROCESS_MEMORY_COUNTERS_EX(ctypes.Structure):
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
        ("PrivateUsage", ctypes.c_size_t),
    ]


if os.name != "nt":
    raise SystemExit("rar_cat_memory.py measures Windows working sets only")

kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
psapi = ctypes.WinDLL("psapi", use_last_error=True)
kernel32.OpenProcess.argtypes = [wintypes.DWORD, wintypes.BOOL, wintypes.DWORD]
kernel32.OpenProcess.restype = wintypes.HANDLE
kernel32.CloseHandle.argtypes = [wintypes.HANDLE]
psapi.GetProcessMemoryInfo.argtypes = [
    wintypes.HANDLE,
    ctypes.POINTER(PROCESS_MEMORY_COUNTERS_EX),
    wintypes.DWORD,
]
psapi.GetProcessMemoryInfo.restype = wintypes.BOOL


def read_peak_working_set(handle, counters):
    """Return the process's peak working set in bytes, or None when unreadable."""
    counters.cb = ctypes.sizeof(PROCESS_MEMORY_COUNTERS_EX)
    if not psapi.GetProcessMemoryInfo(handle, ctypes.byref(counters), counters.cb):
        return None
    return int(counters.PeakWorkingSetSize)


def run_with_peak_rss(command, stdout_path, deadline_s):
    """Run command, polling the child's peak working set until it exits.

    Returns dict(exit_code, peak_bytes, samples, seconds, stderr).
    """
    counters = PROCESS_MEMORY_COUNTERS_EX()
    peak = 0
    samples = 0
    started = time.perf_counter()
    with open(stdout_path, "wb") as out_file:
        with tempfile.TemporaryFile() as err_file:
            proc = subprocess.Popen(
                command,
                stdout=out_file,
                stderr=err_file,
                stdin=subprocess.DEVNULL,
            )
            handle = kernel32.OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_VM_READ,
                False,
                proc.pid,
            )
            killed = False
            try:
                while True:
                    if handle:
                        observed = read_peak_working_set(handle, counters)
                        if observed is not None:
                            samples += 1
                            if observed > peak:
                                peak = observed
                    if proc.poll() is not None:
                        break
                    if time.perf_counter() - started > deadline_s:
                        proc.kill()
                        killed = True
                        break
                exit_code = proc.wait()
                # A terminated process object keeps its counters; this covers a
                # child that exited between two sessions of the polling loop.
                if handle:
                    observed = read_peak_working_set(handle, counters)
                    if observed is not None:
                        samples += 1
                        if observed > peak:
                            peak = observed
            finally:
                if handle:
                    kernel32.CloseHandle(handle)
            err_file.seek(0)
            stderr = err_file.read().decode("utf8", "replace")
    return {
        "exit_code": exit_code,
        "peak_bytes": peak if samples else None,
        "samples": samples,
        "seconds": round(time.perf_counter() - started, 3),
        "killed": killed,
        "stderr": stderr.strip()[:400],
    }


def sha256_file(path):
    digest = hashlib.sha256()
    with open(path, "rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def manifest_cases(manifest_paths):
    cases = []
    for manifest_path in manifest_paths:
        data = json.loads(Path(manifest_path).read_text(encoding="utf8"))
        for entry in data["fixtures"]:
            cases.append(
                {
                    "name": "generated-" + entry["case"],
                    "archive": entry["archive"],
                    "member": entry["member"],
                    "expected_size": entry["size"],
                    "expected_sha256": entry["sha256"],
                    "expected_source": "generator manifest sha256",
                    "password": entry.get("password"),
                }
            )
    return cases


def reference_cases(reference_paths):
    cases = []
    for reference_path in reference_paths:
        source = json.loads(Path(reference_path).read_text(encoding="utf8"))
        for case in source["cases"]:
            if case.get("reference_extract_exit") != 0 or case.get("reference_test_exit") != 0:
                continue
            archive = Path(case["archive"])
            oracle_dir = archive.parent / "oracle"
            for entry, digest in case.get("files", {}).items():
                oracle = oracle_dir / entry
                if not oracle.is_file():
                    continue
                cases.append(
                    {
                        "name": "reference-" + archive.parent.name + "-" + oracle.name,
                        "archive": str(archive),
                        "member": entry,
                        "expected_size": oracle.stat().st_size,
                        "expected_sha256": digest,
                        "expected_source": "oracle file " + str(oracle),
                        "password": case.get("password"),
                    }
                )
    return cases


def explicit_cases(specs):
    cases = []
    for spec in specs:
        archive, member, oracle = spec.split("::")
        oracle_path = Path(oracle)
        cases.append(
            {
                "name": "explicit-" + Path(archive).stem + "-" + Path(member).name,
                "archive": archive,
                "member": member,
                "expected_size": oracle_path.stat().st_size,
                "expected_sha256": sha256_file(oracle_path),
                "expected_source": "oracle file " + str(oracle_path),
                "password": None,
            }
        )
    return cases


def measure(binary, case, repeats, deadline_s):
    archive = case["archive"]
    member = case["member"]
    password = case.get("password")
    command = [str(binary), "cat", archive, member, "--json"]
    if password:
        command += ["-p", password]
    test_command = [str(binary), "test", archive]
    if password:
        test_command += ["-p", password]

    runs = []
    with tempfile.TemporaryDirectory() as scratch:
        stdout_path = Path(scratch) / "cat.bin"
        for _ in range(repeats):
            run = run_with_peak_rss(command, stdout_path, deadline_s)
            run["output_bytes"] = stdout_path.stat().st_size if stdout_path.exists() else 0
            run["output_sha256"] = sha256_file(stdout_path) if stdout_path.exists() else None
            runs.append(run)

    peaks = [r["peak_bytes"] for r in runs if r["peak_bytes"]]
    peak = max(peaks) if peaks else None
    exit_ok = all(r["exit_code"] == 0 for r in runs)
    digest_ok = all(r["output_sha256"] == case["expected_sha256"] for r in runs)
    size_ok = all(r["output_bytes"] == case["expected_size"] for r in runs)
    test = subprocess.run(test_command, capture_output=True, timeout=deadline_s)
    return {
        "name": case["name"],
        "archive": archive,
        "archive_bytes": Path(archive).stat().st_size,
        "member": member,
        "member_bytes": case["expected_size"],
        "expected_sha256": case["expected_sha256"],
        "expected_source": case["expected_source"],
        "peak_bytes": peak,
        "peak_bytes_runs": peaks,
        "peak_over_member": (round(peak / case["expected_size"], 3) if peak and case["expected_size"] else None),
        "peak_minus_member": (peak - case["expected_size"]) if peak else None,
        "samples_min": min(r["samples"] for r in runs),
        "cat_exit_ok": exit_ok,
        "output_sha256_match": digest_ok,
        "output_size_match": size_ok,
        "test_exit_code": test.returncode,
        "seconds_max": max(r["seconds"] for r in runs),
        "stderr": next((r["stderr"] for r in runs if r["stderr"]), ""),
    }


def human_table(results):
    rows = [
        ("case", "member B", "peak WS B", "peak/member", "cat", "sha", "test"),
    ]
    for r in results:
        rows.append(
            (
                r["name"],
                str(r["member_bytes"]),
                "-" if r["peak_bytes"] is None else str(r["peak_bytes"]),
                "-" if r["peak_over_member"] is None else f'{r["peak_over_member"]:.2f}x',
                "ok" if r["cat_exit_ok"] else "FAIL",
                "ok" if r["output_sha256_match"] else "FAIL",
                "ok" if r["test_exit_code"] == 0 else f'FAIL({r["test_exit_code"]})',
            )
        )
    widths = [max(len(row[i]) for row in rows) for i in range(len(rows[0]))]
    lines = []
    for index, row in enumerate(rows):
        lines.append("  ".join(cell.ljust(widths[i]) for i, cell in enumerate(row)))
        if index == 0:
            lines.append("  ".join("-" * widths[i] for i in range(len(widths))))
    return "\n".join(lines)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", required=True)
    parser.add_argument("--manifest", action="append", default=[])
    parser.add_argument("--reference", action="append", default=[])
    parser.add_argument("--case", action="append", default=[], help="rar::member::oracle_file")
    parser.add_argument("--repeat", type=int, default=3)
    parser.add_argument("--deadline", type=float, default=180.0)
    parser.add_argument("--label", default="")
    parser.add_argument("--json", dest="json_out")
    parser.add_argument("--only", default="", help="substring filter on case name")
    args = parser.parse_args()

    binary = Path(args.binary).resolve()
    if not binary.is_file():
        raise SystemExit(f"binary not found: {binary}")

    cases = (
        manifest_cases(args.manifest)
        + reference_cases(args.reference)
        + explicit_cases(args.case)
    )
    if args.only:
        cases = [c for c in cases if args.only in c["name"]]
    if not cases:
        raise SystemExit("no cases selected")

    results = []
    for case in cases:
        result = measure(binary, case, max(1, args.repeat), args.deadline)
        results.append(result)
        print(
            f'measured {result["name"]}: member={result["member_bytes"]} '
            f'peak={result["peak_bytes"]} test_exit={result["test_exit_code"]}',
            flush=True,
        )

    report = {
        "binary": str(binary),
        "binary_sha256": sha256_file(binary),
        "label": args.label,
        "repeat": args.repeat,
        "results": results,
        "all_outputs_match": all(r["output_sha256_match"] and r["cat_exit_ok"] for r in results),
        "all_tests_pass": all(r["test_exit_code"] == 0 for r in results),
    }
    print()
    print(human_table(results))
    if args.json_out:
        out_path = Path(args.json_out)
        out_path.parent.mkdir(parents=True, exist_ok=True)
        out_path.write_text(json.dumps(report, ensure_ascii=False, indent=2), encoding="utf8")
        print()
        print("RAR_CAT_MEMORY_JSON=" + str(out_path))
    ok = report["all_outputs_match"] and report["all_tests_pass"]
    print("RAR_CAT_MEMORY=" + ("PASS" if ok else "FAIL"))
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
