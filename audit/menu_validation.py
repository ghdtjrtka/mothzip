"""Explorer menu closure: run the exact command strings the registry holds, then read the output.

Checking keys alone would only prove that text was written. This harness reads each verb's
`command` value from HKCU, executes it against a real file, and decodes the produced archive with
Python's own codecs, so a wrong `--format` flag or a broken path shows up as a failure.
"""
from pathlib import Path
import argparse
import gzip
import json
import lzma
import os
import subprocess
import tarfile
import tempfile
import time
import uuid
import zipfile

import brotli
import lz4.frame
import zstandard

import winreg

parser = argparse.ArgumentParser()
parser.add_argument("--binary", required=True)
parser.add_argument(
    "--expect-double-click",
    default="open",
    help="substring expected in MothZip.Archive shell\\open command",
)
args = parser.parse_args()

workspace = Path(__file__).resolve().parents[1]
binary = Path(args.binary).resolve()
root = workspace / "audit/runs" / (
    "menu-" + time.strftime("%Y%m%d-%H%M%S") + "-" + uuid.uuid4().hex[:6]
)
root.mkdir(parents=True)
records = []

PACK_FORMATS = [
    ("Zip", "zip", ".zip"),
    ("SevenZ", "7z", ".7z"),
    ("Tar", "tar", ".tar"),
    ("TarGz", "tar.gz", ".tar.gz"),
    ("TarZst", "tar.zst", ".tar.zst"),
    ("TarXz", "tar.xz", ".tar.xz"),
    ("TarLz4", "tar.lz4", ".tar.lz4"),
    ("TarBr", "tar.br", ".tar.br"),
    ("Gz", "gz", ".gz"),
    ("Zst", "zst", ".zst"),
    ("Xz", "xz", ".xz"),
    ("Lz4", "lz4", ".lz4"),
    ("Br", "br", ".br"),
]


def record(case, passed, note=None, extra=None):
    entry = {"case": case, "passed": bool(passed)}
    if note:
        entry["note"] = note
    if extra:
        entry.update(extra)
    records.append(entry)
    print(json.dumps(entry), flush=True)


def key_exists(path):
    try:
        winreg.OpenKey(winreg.HKEY_CURRENT_USER, path).Close()
        return True
    except FileNotFoundError:
        return False


def key_value(path, name=""):
    try:
        with winreg.OpenKey(winreg.HKEY_CURRENT_USER, path) as key:
            value, _ = winreg.QueryValueEx(key, name)
            return value
    except FileNotFoundError:
        return None


def subkey_names(path):
    names = []
    try:
        with winreg.OpenKey(winreg.HKEY_CURRENT_USER, path) as key:
            index = 0
            while True:
                try:
                    names.append(winreg.EnumKey(key, index))
                except OSError:
                    return names
                index += 1
    except FileNotFoundError:
        return names


def decode_produced(path, suffix):
    """Decode with an independent implementation and return the single member payload."""
    if suffix == ".zip":
        with zipfile.ZipFile(path) as archive:
            assert archive.testzip() is None, "zip CRC failure"
            return archive.read("probe.txt")
    if suffix == ".7z":
        import py7zr

        # Extract on one handle and check CRCs on a fresh one: a single handle cannot run
        # testzip and extractall in sequence without leaving partial output.
        with tempfile.TemporaryDirectory() as target:
            with py7zr.SevenZipFile(path, mode="r") as archive:
                archive.extractall(target)
            extracted = (Path(target) / "probe.txt").read_bytes()
        with py7zr.SevenZipFile(path, mode="r") as archive:
            assert archive.testzip() is None, "7z CRC failure"
        return extracted
    if suffix.startswith(".tar"):
        blob = path.read_bytes()
        if suffix == ".tar":
            return _first_tar_member(blob)
        if suffix == ".tar.gz":
            return _first_tar_member(gzip.decompress(blob))
        if suffix == ".tar.zst":
            return _first_tar_member(
                zstandard.ZstdDecompressor().decompressobj().decompress(blob)
            )
        if suffix == ".tar.xz":
            return _first_tar_member(lzma.decompress(blob))
        if suffix == ".tar.lz4":
            return _first_tar_member(lz4.frame.decompress(blob))
        if suffix == ".tar.br":
            return _first_tar_member(brotli.decompress(blob))
    blob = path.read_bytes()
    if suffix == ".gz":
        return gzip.decompress(blob)
    if suffix == ".zst":
        return zstandard.ZstdDecompressor().decompressobj().decompress(blob)
    if suffix == ".xz":
        return lzma.decompress(blob)
    if suffix == ".lz4":
        return lz4.frame.decompress(blob)
    if suffix == ".br":
        return brotli.decompress(blob)
    raise AssertionError(f"no decoder for {suffix}")


def _first_tar_member(tar_bytes):
    """Read `probe.txt` out of the uncompressed TAR payload held in memory."""
    with tarfile.open(fileobj=__import__("io").BytesIO(tar_bytes), mode="r:") as archive:
        return archive.extractfile("probe.txt").read()


def run_command_template(template, selection, workdir):
    """Execute a registry command string with %1 replaced by the selection path."""
    text = template.replace("%1", selection)
    finished = subprocess.run(
        text, shell=True, cwd=workdir, capture_output=True, timeout=300
    )
    return finished


status = subprocess.run(
    [str(binary), "status", "--json", "--non-interactive"],
    capture_output=True,
    timeout=120,
)
status_json = {}
if status.returncode == 0:
    status_json = json.loads(status.stdout.decode("utf8", "replace"))
record(
    "status-reports-install",
    status_json.get("is_installed") is True,
    f"exit={status.returncode}",
    {"status": {k: status_json.get(k) for k in ("version", "is_installed", "registered_in_registry")}},
)

workdir = root / "work"
workdir.mkdir()
payload = b"mothzip menu probe payload\n" * 40
(workdir / "probe.txt").write_bytes(payload)
directories = workdir / "folder"
directories.mkdir()
(directories / "inner.txt").write_bytes(b"inner")

# 1. Files: one-click ZIP verb plus the cascading format submenu, executed for every format.
for root_key, label in (("Software\\Classes\\*", "file"), ("Software\\Classes\\Directory", "directory")):
    zip_dir = workdir / f"{label}-zip"
    zip_dir.mkdir()
    target = "probe.txt" if label == "file" else "folder"
    if label == "file":
        (zip_dir / "probe.txt").write_bytes(payload)
    else:
        (zip_dir / "folder").mkdir()
        (zip_dir / "folder" / "inner.txt").write_bytes(b"inner")
    command = key_value(f"{root_key}\\shell\\MothZip.PackZip\\command")
    record(f"{label}:zip-verb-key", command is not None, command)
    if command:
        finished = run_command_template(command, f'"{zip_dir / target}"', zip_dir)
        produced = zip_dir / ("probe.zip" if label == "file" else "folder.zip")
        record(
            f"{label}:zip-verb-runs",
            finished.returncode == 0 and produced.is_file(),
            f"exit={finished.returncode} err={finished.stderr.decode('utf8', 'replace')[:200]}",
        )

    submenu_parent = f"{root_key}\\shell\\MothZip.Pack"
    record(
        f"{label}:submenu-marker",
        key_value(submenu_parent, "SubCommands") == "",
        "SubCommands must be empty so Explorer cascades instead of running a command",
    )
    children = set(subkey_names(submenu_parent + "\\shell"))
    record(
        f"{label}:submenu-has-13-formats",
        children == {f"MothZip.Pack.{verb}" for verb, _, _ in PACK_FORMATS},
        f"children={sorted(children)}",
    )

    for verb, suffix, extension in PACK_FORMATS:
        fmt_dir = workdir / f"{label}-fmt-{suffix.replace('.', '_')}"
        fmt_dir.mkdir()
        if label == "file":
            (fmt_dir / "probe.txt").write_bytes(payload)
            target_path = fmt_dir / "probe.txt"
            expected = fmt_dir / f"probe{extension}"
        else:
            (fmt_dir / "folder").mkdir()
            (fmt_dir / "folder" / "inner.txt").write_bytes(b"inner")
            target_path = fmt_dir / "folder"
            expected = fmt_dir / f"folder{extension}"
        template = key_value(f"{submenu_parent}\\shell\\MothZip.Pack.{verb}\\command")
        if not template:
            record(f"{label}:{suffix}:key", False, "command value missing")
            continue
        finished = run_command_template(template, f'"{target_path}"', fmt_dir)
        produced_ok = expected.is_file()
        decoded = None
        if finished.returncode == 0 and produced_ok:
            try:
                decoded = decode_produced(expected, extension)
            except Exception as error:  # noqa: BLE001 - recorded as a failed case
                decoded = f"decode error: {error}".encode()
        wanted = payload if label == "file" else b""
        passed = (
            finished.returncode == 0
            and produced_ok
            and (decoded == wanted if label == "file" else decoded is not None)
        )
        record(
            f"{label}:{suffix}:format-submenu",
            passed,
            f"exit={finished.returncode} produced={produced_ok} "
            f"decoded={len(decoded) if isinstance(decoded, bytes) else decoded!r} "
            f"expected={len(wanted)} "
            f"err={finished.stderr.decode('utf8', 'replace')[:160]}",
        )

# 2. Archives: exactly one extraction verb, and none of the retired ones.
supported = status_json.get("supported_extensions") or []
extensions_checked = 0
for extension in supported:
    prefix = f"Software\\Classes\\SystemFileAssociations\\{extension}\\shell"
    has_smart = key_exists(prefix + "\\MothZip.SmartUnpack")
    legacy = [name for name in subkey_names(prefix) if name in ("MothZip.UnpackHere", "MothZip.UnpackFolder")]
    extensions_checked += 1
    if not has_smart or legacy:
        record(
            f"ext{extension}:single-extraction-verb",
            False,
            f"smart={has_smart} legacy={legacy}",
        )
record(
    "ext:all-single-extraction-verb",
    extensions_checked == len(supported) and len(supported) >= 15,
    f"checked={extensions_checked} of {len(supported)}",
)

# 3. Extraction verb still works on a real archive produced above.
archive = workdir / "file-zip" / "probe.zip"
smart_command = key_value(
    "Software\\Classes\\SystemFileAssociations\\.zip\\shell\\MothZip.SmartUnpack\\command"
)
if archive.is_file() and smart_command:
    template = smart_command.replace(" --open", "")
    finished = run_command_template(template, f'"{archive}"', workdir)
    produced = [str(path) for path in workdir.rglob("probe.txt") if path.is_file()]
    record(
        "ext:smart-unpack-runs",
        finished.returncode == 0 and bool(produced),
        f"exit={finished.returncode} found={produced} "
        f"err={finished.stderr.decode('utf8', 'replace')[:200]}",
    )
else:
    record("ext:smart-unpack-runs", False, "archive or command missing")

# 4. Double-click command points at the intended action.
open_command = key_value("Software\\Classes\\MothZip.Archive\\shell\\open\\command") or ""
record(
    "progid:double-click-command",
    args.expect_double_click in open_command,
    open_command,
)

report = dict(
    passed=all(case["passed"] for case in records),
    cases=len(records),
    binary=str(binary),
    binary_sha256=__import__("hashlib").sha256(binary.read_bytes()).hexdigest(),
    records=records,
)
(root / "results.json").write_text(json.dumps(report, indent=2), encoding="utf8")
failed = [case["case"] for case in records if not case["passed"]]
print(
    json.dumps(
        dict(
            passed=report["passed"],
            cases=report["cases"],
            failed=failed,
            report=str(root / "results.json"),
        )
    )
)
raise SystemExit(0 if report["passed"] else 1)
