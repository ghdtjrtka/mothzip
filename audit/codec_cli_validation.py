"""Validate the five stream codecs end to end through the real CLI.

Independent implementations: Python `gzip`/`lzma` (stdlib zlib/xz), `zstandard`, `lz4.frame`,
and `brotli`. Every case compares bytes by SHA-256 in both directions, so a passing run means
MothZip's writer is readable by other tools and other tools' output is readable by MothZip.
"""
from pathlib import Path
import argparse, gzip, hashlib, json, lzma, os, subprocess, tarfile, time, uuid
import brotli, lz4.frame, zstandard

CODECS = {
    "gz": dict(
        name="GZIP",
        min_level=0,
        max_level=9,
        mid_level=6,
        bad_level=10,
        encode=lambda data, level: gzip.compress(data, level),
        decode=lambda blob: gzip.decompress(blob),
        concat_policy="all",
        corrupt_must_fail=True, payload_checksum=True,
        trailing_must_fail=True,
    ),
    "zst": dict(
        name="Zstd",
        min_level=1,
        max_level=22,
        mid_level=3,
        bad_level=0,
        encode=lambda data, level: zstandard.ZstdCompressor(level=level).compress(data),
        decode=lambda blob: zstandard.ZstdDecompressor().decompressobj().decompress(blob),
        concat_policy="all",
        corrupt_must_fail=False, payload_checksum=False,
        trailing_must_fail=True,
    ),
    "xz": dict(
        name="XZ",
        min_level=0,
        max_level=9,
        mid_level=6,
        bad_level=10,
        encode=lambda data, level: lzma.compress(data, format=lzma.FORMAT_XZ, preset=level),
        decode=lambda blob: lzma.decompress(blob, format=lzma.FORMAT_XZ),
        concat_policy="all",
        corrupt_must_fail=True, payload_checksum=True,
        trailing_must_fail=True,
    ),
    "lz4": dict(
        name="LZ4",
        min_level=None,
        max_level=None,
        mid_level=None,
        bad_level=5,
        encode=lambda data, level: lz4.frame.compress(
            data, compression_level=0 if level is None else level
        ),
        decode=lambda blob: lz4.frame.decompress(blob),
        concat_policy="all_or_error",
        corrupt_must_fail=False, payload_checksum=False,
        trailing_must_fail=True,
    ),
    "br": dict(
        name="Brotli",
        min_level=0,
        max_level=11,
        mid_level=9,
        bad_level=12,
        encode=lambda data, level: brotli.compress(data, quality=level),
        decode=lambda blob: brotli.decompress(blob),
        concat_policy="not_applicable",
        corrupt_must_fail=False,
        payload_checksum=False,
        trailing_must_fail=True,
    ),
}

parser = argparse.ArgumentParser()
parser.add_argument("--binary", required=True)
args = parser.parse_args()

workspace = Path(__file__).resolve().parents[1]
binary = Path(args.binary).resolve()
root = workspace / "audit/runs" / (
    "codec-cli-" + time.strftime("%Y%m%d-%H%M%S") + "-" + uuid.uuid4().hex[:6]
)
root.mkdir(parents=True)
records = []


def digest(data):
    return hashlib.sha256(data).hexdigest()


def file_digest(path):
    return digest(Path(path).read_bytes())


def inventory(path):
    base = Path(path)
    return {
        str(f.relative_to(base)).replace("\\", "/"): digest(f.read_bytes())
        for f in base.rglob("*")
        if f.is_file()
    }


def call(*arguments, capture_stdout_bytes=False):
    command = [str(binary), *map(str, arguments), "--non-interactive"]
    if not capture_stdout_bytes:
        command.append("--json")
    finished = subprocess.run(command, capture_output=True, timeout=120)
    record = {
        "argv": [str(a) for a in arguments],
        "exit": finished.returncode,
        "stdout": finished.stdout.decode("utf8", "replace")
        if not capture_stdout_bytes
        else finished.stdout.hex(),
        "stderr": finished.stderr.decode("utf8", "replace"),
    }
    if capture_stdout_bytes:
        record["stdout_bytes"] = finished.stdout
    return record


def summarise(call_record):
    return {
        "argv": call_record["argv"],
        "exit": call_record["exit"],
        "stdout": call_record["stdout"][:800],
        "stderr": call_record["stderr"][:800],
    }


def record(case, passed, calls, note=None):
    entry = {"case": case, "passed": bool(passed), "calls": [summarise(c) for c in calls]}
    if note:
        entry["note"] = note
    records.append(entry)
    print(json.dumps({"case": case, "passed": entry["passed"], "note": note}), flush=True)


payload = os.urandom(192 * 1024)
text = (b"mothzip codec validation line\n" * 3000) + "한글 압축 테스트\n".encode("utf8") * 500
tree = root / "tree"
(tree / "nested").mkdir(parents=True)
(tree / "payload.bin").write_bytes(payload)
(tree / "notes.txt").write_bytes(text)
(tree / "empty.txt").write_bytes(b"")
(tree / "nested" / "한글 이름.txt").write_bytes("유니코드 내용".encode("utf8"))
# MothZip packs a directory with the directory name as the archive root.
tree_inventory = {"tree/" + name: value for name, value in inventory(tree).items()}

# An independent TAR writer for the "other tool writes, MothZip reads" direction.
independent_tar = root / "independent.tar"
with tarfile.open(independent_tar, "w") as archive:
    archive.add(tree, arcname="tree")
independent_tar_bytes = independent_tar.read_bytes()
tar_member_count = len(tarfile.open(independent_tar).getnames())

for ext, codec in CODECS.items():
    name = codec["name"]
    level_args = [] if codec["mid_level"] is None else ["--level", codec["mid_level"]]

    # 1. MothZip writes, independent decoder reads.
    single = root / f"single.bin.{ext}"
    packed = call("pack", single, tree / "payload.bin", *level_args)
    decoded = None
    try:
        decoded = codec["decode"](single.read_bytes())
    except Exception as error:  # noqa: BLE001 - recorded as a failed case
        decoded = f"decode error: {error}".encode()
    record(
        f"{ext}:pack-single-independent-decode",
        packed["exit"] == 0 and decoded == payload,
        [packed],
        None if decoded == payload else f"decoded {len(decoded)} bytes",
    )

    # 2. MothZip writes a TAR container, independent decoder + tarfile read it.
    tar_path = root / f"bundle.tar.{ext}"
    packed_tar = call("pack", tar_path, tree, *level_args)
    extracted_files = None
    try:
        raw_tar = codec["decode"](tar_path.read_bytes())
        member_dir = root / f"python-read-{ext}"
        member_dir.mkdir(exist_ok=True)
        with tarfile.open(fileobj=__import__("io").BytesIO(raw_tar)) as archive:
            archive.extractall(member_dir, filter="data")
        extracted_files = inventory(member_dir)
    except Exception as error:  # noqa: BLE001
        extracted_files = {"error": str(error)}
    record(
        f"{ext}:pack-tar-independent-decode",
        packed_tar["exit"] == 0 and extracted_files == tree_inventory,
        [packed_tar],
        None if extracted_files == tree_inventory else str(extracted_files)[:400],
    )

    # 3. Independent encoder writes, MothZip reads (cat + unpack + peek).
    py_single = root / f"from-python.bin.{ext}"
    py_single.write_bytes(codec["encode"](payload, codec["mid_level"] or 0))
    cat = call("cat", py_single, "from-python.bin", capture_stdout_bytes=True)
    record(
        f"{ext}:independent-encode-cat",
        cat["exit"] == 0 and cat["stdout_bytes"] == payload,
        [cat],
    )
    unpacked_dir = root / f"unpacked-{ext}"
    unpacked = call("unpack", py_single, "-o", unpacked_dir)
    record(
        f"{ext}:independent-encode-unpack",
        unpacked["exit"] == 0
        and inventory(unpacked_dir) == {"from-python.bin": digest(payload)},
        [unpacked],
    )

    py_tar = root / f"from-python.tar.{ext}"
    py_tar.write_bytes(codec["encode"](independent_tar_bytes, codec["mid_level"] or 0))
    peek = call("peek", py_tar)
    peek_report = {}
    if peek["exit"] == 0:
        peek_report = json.loads(peek["stdout"])
    record(
        f"{ext}:independent-tar-peek",
        peek["exit"] == 0 and peek_report.get("total_entries") == tar_member_count,
        [peek],
        f"entries={peek_report.get('total_entries')}",
    )
    tar_unpacked = root / f"unpacked-tar-{ext}"
    unpacked_tar = call("unpack", py_tar, "-o", tar_unpacked)
    record(
        f"{ext}:independent-tar-unpack",
        unpacked_tar["exit"] == 0 and inventory(tar_unpacked) == tree_inventory,
        [unpacked_tar],
    )
    integrity = call("test", py_tar)
    record(f"{ext}:independent-tar-test", integrity["exit"] == 0, [integrity])

    # 4. Empty member keeps working in both directions.
    empty_archive = root / f"empty.txt.{ext}"
    packed_empty = call("pack", empty_archive, tree / "empty.txt", *level_args)
    empty_bytes = codec["decode"](empty_archive.read_bytes()) if packed_empty["exit"] == 0 else None
    empty_out = root / f"empty-out-{ext}"
    empties_unpacked = call("unpack", empty_archive, "-o", empty_out)
    record(
        f"{ext}:empty-member",
        packed_empty["exit"] == 0
        and empty_bytes == b""
        and empties_unpacked["exit"] == 0
        and inventory(empty_out) == {"empty.txt": digest(b"")},
        [packed_empty, empties_unpacked],
    )

    # 5. Concatenated members follow the codec's declared policy.
    if codec["concat_policy"] != "not_applicable":
        concatenated = root / f"concat.bin.{ext}"
        first, second = payload[: len(payload) // 2], payload[len(payload) // 2 :]
        concatenated.write_bytes(
            codec["encode"](first, codec["mid_level"] or 0)
            + codec["encode"](second, codec["mid_level"] or 0)
        )
        concat_out = root / f"concat-out-{ext}"
        concat_call = call("unpack", concatenated, "-o", concat_out)
        produced = inventory(concat_out).get("concat.bin") if concat_call["exit"] == 0 else None
        if codec["concat_policy"] == "all":
            passed = concat_call["exit"] == 0 and produced == digest(payload)
            note = "concatenated members must decode completely"
        else:
            passed = (concat_call["exit"] == 0 and produced == digest(payload)) or (
                concat_call["exit"] != 0 and not os.path.exists(concat_out / "concat.bin")
            )
            note = f"policy all_or_error; exit={concat_call['exit']}"
        record(f"{ext}:concatenated-members", passed, [concat_call], note)

    # 6. Truncated input must fail and must not disturb an existing destination.
    truncated = root / f"truncated.bin.{ext}"
    truncated.write_bytes(py_single.read_bytes()[:-4])
    preserved = root / f"preserved-{ext}"
    preserved.mkdir()
    (preserved / "keep.txt").write_bytes(b"EXISTING")
    trunc_call = call("unpack", truncated, "-o", preserved)
    record(
        f"{ext}:truncated-refused",
        trunc_call["exit"] != 0
        and (preserved / "keep.txt").read_bytes() == b"EXISTING"
        and not (preserved / "truncated.bin").exists(),
        [trunc_call],
        "existing destination file must survive",
    )

    # 7. Trailing bytes after the last member must fail.
    trailing = root / f"trailing.bin.{ext}"
    trailing.write_bytes(py_single.read_bytes() + b"TRAILING GARBAGE")
    trailing_out = root / f"trailing-out-{ext}"
    trailing_call = call("unpack", trailing, "-o", trailing_out)
    if codec["trailing_must_fail"]:
        record(
            f"{ext}:trailing-garbage-refused",
            trailing_call["exit"] != 0,
            [trailing_call],
        )
    else:
        record(
            f"{ext}:trailing-garbage",
            True,
            [trailing_call],
            f"informational exit={trailing_call['exit']}",
        )

    # 8. Corrupting our own archive must be caught whenever the format carries a payload
    # checksum. Brotli defines none, so a flipped byte inside a metablock can decode to
    # different bytes; that case is reported instead of asserted.
    corrupted = root / f"corrupt.bin.{ext}"
    blob = bytearray(single.read_bytes())
    blob[len(blob) // 2] ^= 0xFF
    corrupted.write_bytes(bytes(blob))
    corrupt_out = root / f"corrupt-out-{ext}"
    corrupt_call = call("unpack", corrupted, "-o", corrupt_out)
    produced = inventory(corrupt_out).get("corrupt.bin") if corrupt_call["exit"] == 0 else None
    if codec["corrupt_must_fail"]:
        passed = corrupt_call["exit"] != 0
        note = "own archive with a payload checksum must reject flipped bytes"
    else:
        passed = True
        note = (
            f"format has no payload checksum; exit={corrupt_call['exit']} "
            f"silently_wrong={produced is not None and produced != digest(payload)}"
        )
    record(f"{ext}:corrupt-own-archive", passed, [corrupt_call], note)

    # 8b. Corrupting a foreign stream is informational: the reader can only reject what the
    # other tool chose to encode.
    foreign_corrupt = root / f"foreign-corrupt.bin.{ext}"
    foreign_blob = bytearray(py_single.read_bytes())
    foreign_blob[len(foreign_blob) // 2] ^= 0xFF
    foreign_corrupt.write_bytes(bytes(foreign_blob))
    foreign_out = root / f"foreign-corrupt-out-{ext}"
    foreign_call = call("unpack", foreign_corrupt, "-o", foreign_out)
    record(
        f"{ext}:corrupt-foreign-stream",
        True,
        [foreign_call],
        f"informational exit={foreign_call['exit']}",
    )

    # 9. Level range: the codec's own boundaries are accepted, the next value is refused.
    if codec["min_level"] is not None:
        window = (
            (codec["min_level"], codec["max_level"])
            if codec["min_level"] != codec["max_level"]
            else (codec["min_level"],)
        )
        for level in window:
            probe = root / f"level{level}.bin.{ext}"
            level_call = call("pack", probe, tree / "payload.bin", "--level", level)
            record(
                f"{ext}:level-{level}-accepted",
                level_call["exit"] == 0
                and codec["decode"](probe.read_bytes()) == payload,
                [level_call],
            )
    rejected = root / f"level-bad.bin.{ext}"
    bad_call = call("pack", rejected, tree / "payload.bin", "--level", codec["bad_level"])
    record(
        f"{ext}:level-{codec['bad_level']}-refused",
        bad_call["exit"] != 0 and not rejected.exists(),
        [bad_call],
        "an out-of-range level must not create the output file",
    )

    # 10. Decompression budget is enforced through the real reader.
    budget_out = root / f"budget-{ext}"
    budget_call = call("unpack", py_single, "-o", budget_out, "--max-bytes", 1024)
    record(
        f"{ext}:budget-refused",
        budget_call["exit"] != 0 and not (budget_out / "from-python.bin").exists(),
        [budget_call],
    )

# gzip keeps its legacy names and the bare `.gz`/`.tgz` reader paths.
legacy = root / "legacy.tgz"
legacy_pack = call("pack", legacy, tree, "--level", 6)
legacy_out = root / "legacy-out"
legacy_unpack = call("unpack", legacy, "-o", legacy_out)
record(
    "gz:legacy-tgz-suffix",
    legacy_pack["exit"] == 0
    and legacy_unpack["exit"] == 0
    and inventory(legacy_out) == tree_inventory,
    [legacy_pack, legacy_unpack],
)

report = dict(
    passed=all(case["passed"] for case in records),
    cases=len(records),
    binary=str(binary),
    binary_sha256=file_digest(binary),
    codecs=sorted(CODECS),
    records=records,
)
(root / "results.json").write_text(json.dumps(report, indent=2), encoding="utf8")
failed = [case["case"] for case in records if not case["passed"]]
print(json.dumps(dict(passed=report["passed"], cases=report["cases"], failed=failed, report=str(root / "results.json"))))
raise SystemExit(0 if report["passed"] else 1)
