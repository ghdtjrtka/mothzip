# Mothzip CLI contract for scripts, CI, and agents

**Version 1.2.3** (Windows 10/11 x64, MIT). This page documents what a program can rely on when it drives `mothzip.exe`: the flags that exist, the JSON on each stream, how failure is signalled, and which integrity guarantees do **not** exist. It is a contract page, not a tutorial.

Every command line in sections 1–8 was executed against the shipped build on Windows before it was written; output blocks are real captures, not illustrations. Measured limits are marked **measured**; figures quoted from this repository's perf runs are marked as such, and the CLI additions at the end were run against the same build.

## 1. Install and place the binary

Run `Install.cmd` from the release ZIP, or the `install` subcommand, to install per user at `%LOCALAPPDATA%\Programs\MothZip` with PATH, Explorer actions, and a Start Menu shortcut; for CI, portable use is simpler — copy `mothzip.exe` to a directory you control and invoke it by absolute path. There is no account, no background service, no network access, and no bundled or external archiver process, so a pinned copy is a self-contained step. `mothzip.exe --version` prints `mothzip 1.2.1`, and `mothzip status --json` reports install state, paths, and version for inventory scripts. Open a new terminal after a PATH-changing install.

## 2. Command and flag contract

Global options are accepted before or after the subcommand.

| Option | Effect |
| --- | --- |
| `--json` | Structured result on stdout; error objects on stderr. |
| `--non-interactive` | No prompts, no desktop windows. |
| `-q`, `--quiet` | Suppresses non-essential output. |
| `-p`, `--password <PW>` | Password for encrypted archives. Visible in the process command line. |
| `--password-stdin` | Reads one password line from stdin; never prompts. Conflicts with `-p`. |
| `--max-bytes <N>` | Decompressed-byte budget per operation (default 10 GiB; minimum 1). |
| `-h`, `--help` / `-V`, `--version` | Usage / version. |

| Command | Arguments and options | Result |
| --- | --- | --- |
| `unpack <ARCHIVE>` | `-o/--out <DIR>`, `--to-folder`, `--member <PATH>` (repeatable), `--flat`, `--open` | Extraction summary JSON |
| `smart-unpack <ARCHIVE>` | same as `unpack` | Extraction summary JSON; `smart_adjusted` reports folder wrapping |
| `pack <OUTPUT> <SOURCES>...` | `-l/--level <0-22>`, `--format <fmt>`, `--volume-bytes <N>` (7z only), `--to-stdout`, `--from-stdin`, `--7z-dict <MiB>`, `--reproducible` | Archive summary JSON; no summary with `--to-stdout` |
| `pack-auto <SOURCES>...` | `--format <fmt>` (default `zip`) | Archive summary JSON; writes beside the selection |
| `open <ARCHIVE>` | none | Browsing window; exit 1 under `--non-interactive` |
| `peek <ARCHIVE>` | `--depth <N>` (default 2), `--limit <N>` (default 50) | Structure JSON; bounded display only |
| `test <ARCHIVE>` (alias `verify`) | none | Integrity JSON |
| `cat <ARCHIVE> <PATH>` | none | **Raw bytes on stdout** |
| `sfx <OUTPUT.exe> <INPUT.zip>` | none | SFX summary JSON |
| `add <ARCHIVE> <SOURCES>...` | `-l/--level <0-9>` | Edit summary JSON |
| `delete <ARCHIVE> <MEMBERS>...` | none | Edit summary JSON |
| `rename <ARCHIVE> <FROM> <TO>` | none | Edit summary JSON |
| `install` | `--dir <DIR>`, `--no-path`, `--no-shortcut` | Machine state |
| `uninstall` | `--keep-path` | Machine state |
| `register` / `unregister` | `register --no-shortcut` | Machine state |
| `status` | none | Install-state JSON |

`install`, `uninstall`, `register`, and `unregister` change per-user Windows state, so this page verifies their flag surface but not their output shapes; `status --json` is the read-only way to inspect the result (section 1).

Rules that matter to callers:

- `--open` conflicts with both `--json` and `--non-interactive`; either combination is rejected as an invalid command line (exit 2) before any work starts.
- A mistyped subcommand is **not** a usage error. A first token that is not a known command is treated as an archive path for implicit smart extraction, so `mothzip frobnicate` exits 1 with a path error. Validate command names in your wrapper instead of relying on exit 2.
- `cat` always writes payload bytes, with or without `--json`. With `--json` the bytes are unchanged; there is no JSON wrapper for `cat`.
- `--from-stdin` compresses the bytes on stdin into a **single-stream** archive (`gz`, `zst`, `xz`, `lz4`, `br`); a container is refused with exit 1 (`--from-stdin writes a single compressed stream; ZIP needs entry names, so pack it from files instead`). It is as atomic as an archive built from files: the level is validated and the bytes are staged beside the destination before the destination is replaced, so a failed run leaves an existing output byte-identical (measured: a 14-byte `keep.gz` stayed 14 bytes when `--level 22` was rejected). Options that cannot apply to a pipe are refused as argument errors (exit 2) instead of being ignored: `--password`, `--reproducible`, `--7z-dict`, `--volume-bytes`, and any positional `SOURCES`. It composes with `--to-stdout`, so `cat file | mothzip pack out.zst --from-stdin --format zst --to-stdout > copy.zst` never touches the disk (that path is the only non-atomic one: stdout cannot be rolled back).
- `--to-stdout` still requires the positional `OUTPUT`; that path only selects the format (`mothzip pack out.zip src --to-stdout > out.zip`). It conflicts with `--json` and `--volume-bytes` (exit 2, 0 bytes on stdout), and 7z is refused with exit 1 because it rewrites its signature header and needs a seekable output. There is no staging or commit step, so a failure can follow bytes already written; a downstream pipe that closes early is a normal success (exit 0). `--format` forces the container instead of deriving it from the output suffix. `-` is not a stdin convention here: `pack - …` creates a file literally named `-`, so use `--from-stdin` when the bytes come from a pipe.

### Exit codes

| Code | Meaning |
| --- | --- |
| **0** | Success. |
| **1** | Processing failure: unreadable or corrupt input, budget exceeded, unsupported operation, rejected edit. |
| **2** | Invalid command line: unknown flag, missing argument, out-of-range value, conflicting flags. |

Exit 2 is produced by the argument parser and is always a caller bug — never retry it. Exit 1 may be retried only after the cause is addressed (different budget, password, or input).

## 3. JSON shapes

`pack` (stdout, exit 0):

```json
{
  "archive_path": "bundle.zip",
  "total_files": 3,
  "total_uncompressed_bytes": 35,
  "compressed_bytes": 555,
  "compression_ratio": "0.1x",
  "volumes": [],
  "volume_sizes": []
}
```

`volumes` and `volume_sizes` are parallel arrays and are empty for a single-file output; a split 7z run lists `OUTPUT.7z.001` … with each part's size. `compression_ratio` is a display string, so branch on `total_uncompressed_bytes` and `compressed_bytes` instead — for a 35-byte payload in a 555-byte ZIP it printed `"0.1x"`, because container overhead dominates at that size.

`peek` (stdout, exit 0) — `total_entries` counts directories as entries, `total_files` in `pack` does not:

```json
{
  "archive_name": "bundle.zip",
  "total_entries": 5,
  "total_compressed_bytes": 41,
  "total_uncompressed_bytes": 35,
  "compression_ratio": "0.9x",
  "has_single_root": true,
  "root_name": "src",
  "extension_distribution": { ".json": 1, ".md": 1, ".txt": 1 },
  "tree_summary": [
    { "path": "src/", "is_dir": true, "size": 0 },
    { "path": "src/data.json", "is_dir": false, "size": 14 },
    { "path": "src/docs/", "is_dir": true, "size": 0 },
    { "path": "src/note.txt", "is_dir": false, "size": 12 }
  ],
  "truncated_items": 1
}
```

`tree_summary` is shortened by `--depth`/`--limit`; `truncated_items` counts what was withheld. Display paths are not member identifiers — feed `cat` the literal path you know, not a truncated display line.

`test` / `verify` (stdout, exit 0):

```json
{
  "archive_path": "bundle.zip",
  "total_entries": 5,
  "tested_entries": 3,
  "passed": true,
  "error_message": null
}
```

`unpack` / `smart-unpack` (stdout, exit 0):

```json
{
  "extracted_files": 3,
  "extracted_bytes": 35,
  "target_dir": "\\\\?\\G:\\work\\out",
  "smart_adjusted": false
}
```

`target_dir` is an extended-length `\\?\` path. Compare and reuse it as an opaque string; do not reassemble it from your own arguments.

Edit commands (`add`, `delete`, `rename`) report:

```json
{
  "archive_path": "bundle.zip",
  "total_files": 4,
  "added": ["newfile.txt"],
  "deleted": [],
  "renamed": [["newfile.txt", "moved.txt"]]
}
```

`cat` has no JSON. Its stdout is the member's bytes; everything else goes to stderr:

```powershell
cmd /c "mothzip.exe cat bundle.zip src/note.txt > note.out"
```

Failure is a single-line JSON object on **stderr**:

```json
{"error":"corrupt deflate stream"}
```

The message text is not a stable API — OS-derived parts are localized on non-English Windows. Branch on the exit code, not on string matching. Only `--json` produces the JSON error object; without it, stderr carries plain text (argument-parser failures start `error:`, runtime failures start `Error:`).

## 4. Detecting failure

1. **The exit code is the signal.** 0 means every requested operation completed; 1 means processing failed; 2 means the command line was wrong.
2. **stderr carries the reason** as `{"error": "…"}` when `--json` is set. Parse it for logs; do not parse it for control flow.
3. **stdout can be non-empty on failure.** `test` writes a `passed: false` result object to stdout *and* the error object to stderr, both with exit 1, when it cannot open the archive, when a 7z block fails its CRC, and when a TAR+codec trailer fails its checksum (measured for `TAR.GZ`/`TAR.ZST`/`TAR.XZ`/`TAR.LZ4`). A ZIP member checksum failure leaves stdout empty. So: never treat "JSON appeared on stdout" as success, and read `passed` only after exit 0.
4. **`cat` and `--to-stdout` can emit bytes before failing.** `--to-stdout` has no staging or commit step, so a failed run may already have written a partial archive. The documented rule is to discard the output whenever the exit code is nonzero; redirect to a temporary file and rename it into place only after exit 0. A downstream pipe that closes early ends the command with exit 0, not a failure.
5. **`--reproducible` fails fast on a bad epoch and cannot cover encrypted output.** An invalid `SOURCE_DATE_EPOCH` exits 1 before any output exists (`SOURCE_DATE_EPOCH must be whole seconds since 1970-01-01, got '…'`). AES-encrypted archives differ between runs because the IV is random, so do not use this flag as an equality check for them.
6. **Never combine stdout and stderr into one stream** (`2>&1`) in a wrapper: a valid JSON result and an error object then interleave and neither parses.

## 5. Budgets and large archives

`--max-bytes N` bounds decompressed bytes for every path that expands payload — `unpack`, `smart-unpack`, `cat`, and `test` — with a default of 10 GiB. Exceeding it fails the operation (exit 1) and leaves the destination untouched. Measured with a 5-byte budget: `cat` produced 0 bytes and exit 1, `unpack` created no files, and an existing file at the destination kept its original content.

Two deliberate exceptions, both measured: `peek` lists structure without decompressing and is not bounded by the budget, and `pack` compresses (it does not expand) so the budget does not apply. Raise the budget explicitly for large or highly compressible inputs; the byte count includes TAR headers and padding. That difference is measurable and deliberate: with a 1000-byte member, `pack k.zip`/`k.7z` read at `--max-bytes 1000`, while `k.tar` needs 2559 because it counts the container stream it actually reads (headers and padding included). Budget for the container, not only for the payload, when you read TAR or TAR+codec. This is not a CPU-time or memory sandbox.

## 6. Integrity guarantees

What actually rejects a corrupted payload depends on the container and on which command you run. Measured on archives Mothzip itself created, by flipping one byte inside the payload region at ~55% of the file:

| Container | Checksum in the format | `test` detected | `unpack` detected |
| --- | --- | --- | --- |
| ZIP | CRC32 per member | yes | yes |
| 7z | per-block CRC | yes | yes |
| TAR | none (header checksum only) | no | no — extraction succeeds |
| `.gz` / `TAR.GZ` | gzip CRC32 + ISIZE | yes | yes |
| `.xz` / `TAR.XZ` | CRC64 per block | yes | yes |
| `.zst` / `TAR.ZST` | content checksum written by Mothzip | yes | yes |
| `.lz4` / `TAR.LZ4` | content checksum written by Mothzip | yes | yes |
| `.br` / `TAR.BR` | **none** | no | no — extraction succeeds |

Two consequences worth designing around:

- **`test` now consumes the TAR stream through its trailer, so it enforces the codec checksum for TAR+codec archives.** A corrupted `TAR.GZ`/`TAR.ZST`/`TAR.XZ`/`TAR.LZ4` fails `test` with exit 1 (`passed: false` on stdout plus the stderr error), reporting `corrupt gzip stream does not have a matching checksum`, `Restored data doesn't match checksum`, `invalid block checksum`, or `ContentChecksumError`. Earlier builds reported `passed: true` and only `unpack` failed, so a wrapper that used both may now see the failure one step earlier.
- **Brotli provides no corruption detection.** A corrupted `.br` or `TAR.BR` payload passes both `test` and `unpack`. gzip and xz carry their own checksums; zstd and LZ4 frames written by Mothzip include a content checksum, so archives this tool produced are checked by both `test` and `unpack`. Streams written by other tools without that checksum have weaker detection.

`peek` is structure only. It succeeded on a password-protected ZIP with no password supplied, which is the clearest illustration that a successful `peek` says nothing about integrity. ZIP members compressed with Zstd (method 93) are read but never written; Mothzip writes deflate.

## 7. Supported and unsupported boundary

Reading, inspection, verification, streaming, extraction: ZIP (ZipCrypto, AES), 7z (encrypted, solid, split volumes), RAR 3/4/5 (encrypted, solid, multipart — create is not supported), TAR, `TAR.GZ`/`.tgz`/`TAR.ZST`/`TAR.XZ`/`TAR.LZ4`/`TAR.BR`, and the single-stream `.gz`/`.zst`/`.xz`/`.lz4`/`.br`.

Creating: ZIP, 7z, TAR, the five TAR+codec suffixes, and the five single-stream suffixes (exactly one file's bytes; a directory that holds more than one file, or a second input, is rejected: `GZIP format only supports compressing a single file (use .tar.gz for multiple files or directories)`).

Split output is 7z-only. `pack .\backup.7z … --volume-bytes 65536` writes `.\backup.7z.001`, `.\backup.7z.002`, … with every part at most `N` bytes except the last, and a non-7z output is refused with exit 1 (`Volume splitting is only supported for 7z output`). Any output path works: a bare relative name, a `.\`-prefixed name, an absolute path, and a subfolder path all succeed (measured in 1.2.0), and a repack removes stale higher-numbered parts left by a previous, larger set (measured: a 4-part set replaced by a 2-part set left only `.001` and `.002`).

Levels: gzip/xz 0–9, zstd 1–22, brotli 0–11, ZIP/7z 0–9, LZ4 none — passing `--level` to LZ4 is an explicit error. Out-of-codec values are refused before any output file exists (measured: `--level 12` for brotli, `--level 10` for gzip, `--level 3` for LZ4 each exited 1 and left no file; `--level 30` exited 2 at the parser). ZIP/7z level 0 stores data: a 200,035-byte input produced a 200,649-byte ZIP, so level 0 is a packaging choice, not a size reduction.

Editing: `add`, `delete`, and `rename` work on ZIP and 7z only. Rejected with exit 1, measured: `TAR and compressed stream archive editing is not supported` for TAR/TAR+codec/stream, `Editing a split 7z set is not supported; create a replacement volume set with pack` for a split set, and `Edit the ZIP input before converting it to a self-extractor` for an SFX executable. `--member PATH` (repeatable) plus `--flat` select members at extraction time and are supported on `unpack` and `smart-unpack`.

Format detection is by content, not by extension: a ZIP renamed to `.alz` or `.egg` was read as ZIP in 1.2.0. Unreadable content fails with exit 1 (`invalid Zip archive: Could not find EOCD`), so an unsupported family surfaces as an error rather than as silent misreading. Real ALZ/EGG archives are not supported, and damage repair or recovery is not implemented — it is not on the roadmap.

## 8. CI and agent integration

PowerShell (verified end to end):

```powershell
$ErrorActionPreference = 'Stop'
$exe = 'C:\tools\mothzip\mothzip.exe'
$archive = 'C:\work\bundle.zip'
$member = 'src/note.txt'
$errFile = Join-Path $env:TEMP 'mothzip.err.json'

$json = & $exe test $archive --json --non-interactive 2> $errFile
if ($LASTEXITCODE -ne 0) {
    $detail = (Get-Content $errFile -Raw) | ConvertFrom-Json
    throw "mothzip test failed: $($detail.error)"
}
$result = $json | ConvertFrom-Json
if (-not $result.passed) { throw "mothzip reported an integrity failure" }

# Raw bytes: redirect through cmd.exe so nothing re-encodes the stream.
$dest = Join-Path $env:TEMP 'member.bin'
cmd /c "`"$exe`" cat `"$archive`" `"$member`" > `"$dest`""
if ($LASTEXITCODE -ne 0) {
    Remove-Item $dest -ErrorAction SilentlyContinue
    throw "mothzip cat failed with exit $LASTEXITCODE; partial output discarded"
}
```

Python (`subprocess`, verified):

```python
import json
import subprocess

MOTHZIP = r"C:\tools\mothzip\mothzip.exe"

def run(args):
    proc = subprocess.run([MOTHZIP, *args], capture_output=True, check=False)
    return proc.returncode, proc.stdout, proc.stderr

def run_json(args):
    code, out, err = run([*args, "--json", "--non-interactive"])
    if code == 0:
        return json.loads(out)
    message = err.decode("utf-8", "replace").strip()
    try:
        message = json.loads(err.decode("utf-8"))["error"]
    except (ValueError, KeyError):
        pass
    raise RuntimeError(f"mothzip exit {code}: {message}")

run_json(["peek", "bundle.zip"])      # structure
run_json(["test", "bundle.zip"])      # integrity
run_json(["unpack", "bundle.zip", "-o", "out"])

code, out, err = run(["cat", "bundle.zip", "src/note.txt"])
if code != 0:
    raise RuntimeError(f"cat failed: {err.decode('utf-8', 'replace')}")
open("note.txt", "wb").write(out)     # bytes only after exit 0
```

Both snippets were executed: success paths returned the JSON above, a corrupted archive raised through the error object, and `cat` produced a byte-identical file (12 bytes, hash equal to the original). For an agent loop: pass `--json --non-interactive` on every call, key retries on exit 1 with a changed argument (budget, password, destination), and never retry exit 2.

## 9. Known limitations

- **7z creation memory is set by the LZMA2 dictionary, and `--7z-dict` controls it.** Measured on a 60 MB text input (peak RSS, output size relative to the default):

  It is refused (exit 1) for any destination that is not 7z, because a silently ignored setting would tell a caller it had been applied. The upper bound is the LZMA2 format limit (1536 MiB) and the memory it costs scales with it by the measured factor below, so pick the value from the memory you have rather than the maximum.

  | `--7z-dict` | peak RSS | output size |
  | --- | --- | --- |
  | default (level 5 → 8 MiB) | 85.1 MiB | baseline |
  | 4 | **47.1 MiB** | +0.02% |
  | 2 | **28.1 MiB** | +0.30% |
  | 1 | **18.5 MiB** | +1.2% |
  | 16 | 161.1 MiB | −0.001% |
  The dictionary is the match finder's state (about 10.5 MiB of peak RSS per MiB of dictionary), so this is the only lever that does not depend on the level. Anything above the input size buys nothing: 16 MiB was smaller than the payload and produced a marginally *smaller* file than 8 MiB. The 7z header itself is compressed by the crate with its own dictionary, so a commit still costs tens of MiB regardless of `--7z-dict`. The level does not change the factor; it only picks the default dictionary (level 0-1 = 256 KiB/1 MiB, 2-4 = 2/4/4 MiB, 5-6 = 8 MiB, 7-9 = 16/32/64 MiB), so multiply the dictionary you get by the same ~10.5 MiB per MiB. With no flag, level 5 means an 8 MiB dictionary and the 85.1 MiB peak above.
- **Raising the level raises memory sharply, not only for 7z.** On the 32 MiB mixed baseline used for the perf suite: `pack f.tar.zst --level 22` peaked at **744.6 MiB** against **10.8 MiB** at the default zstd level (3), `pack f.7z` at level 9 peaked at **360.2 MiB** against **100.4 MiB** at the default level (5), and solid `unpack` of a 7z archive peaked at **71.1 MiB** while every other unpack path stayed at **7.6–9.9 MiB**. These are environment-dependent measurements, not ceilings. Keep the default level for large inputs; for large 7z output use `--level 4` or lower.
- **Throughput is not a published guarantee.** Repeating the same script against the same binary moved CPU-bound cases (`7z`, maximum-level `zstd`) by up to **43%** under concurrent load — `pack f.7z` at the default level took 11.3 s in one run and 16.2 s in another. This page therefore quotes no MB/s figures; regression checks use a **±10%** band for ordinary cases and **±25%** for CPU-bound cases instead of an absolute target.
- **Brotli cannot detect corruption** (no payload checksum), and TAR carries no payload checksum at all. A corrupted Brotli stream therefore fails only when the bitstream no longer decodes; a flip that still decodes is returned as data. See the table in §6.
- **`--to-stdout` is not a transaction.** 7z output is refused (it needs a seekable stream), there is no staging or commit, and a failure can follow bytes already emitted. Treat stdout as untrusted until exit 0.
- **Encrypted archives are not reproducible.** `--reproducible` cannot make an AES-encrypted archive byte-identical: two runs of the same password-protected ZIP differed (measured), because AES uses a random IV.
- **Windows only.** Windows 10/11 x64; archive links and device objects are unsupported, common macOS/Explorer metadata is filtered, and release binaries are not code-signed.
- **No repair, no ALZ/EGG, no RAR creation.** Failed operations leave the destination unchanged (staging then commit), and an observed failure while replacing an existing file restores it from a backup; power loss, forced termination, and hostile concurrent filesystem changes are outside that guarantee.

## Verification evidence

The guarantees above were validated by the repository's audit runs, which an integrator can re-run against a local build:

| Script | Cases |
| --- | --- |
| [`audit/release_validation.py`](../audit/release_validation.py) | 99 cases / 158 invocations |
| [`audit/codec_cli_validation.py`](../audit/codec_cli_validation.py) | 83 |
| [`audit/edit_cli_validation.py`](../audit/edit_cli_validation.py) | 48 |
| [`audit/selection_cli_validation.py`](../audit/selection_cli_validation.py) | 13 |
| [`audit/sfx_validation.py`](../audit/sfx_validation.py) | 9 |
| [`audit/split_cli_validation.py`](../audit/split_cli_validation.py) | 6 |
| [`audit/zip_zstd_validation.py`](../audit/zip_zstd_validation.py) | 14 |
| [`audit/gui_window_validation.py`](../audit/gui_window_validation.py) | 6 cases / 126 checks |
| [`audit/delegation/check-compatibility.py`](../audit/delegation/check-compatibility.py) | 17/17 |
| [`audit/delegation/edit-validation.py`](../audit/delegation/edit-validation.py) | 21/21 |
| [`audit/delegation/split-validation.py`](../audit/delegation/split-validation.py) | 11/11 |

The same build passed `cargo test --locked` (88 unit tests), `cargo clippy --locked --all-targets -- -D warnings` (0 warnings), and `cargo fmt --check` (clean).

## 10. New in 1.2.x

These features are in the shipped 1.2.x builds; the CLI examples below were run against them. The contracts above already include them, so this section is the release-level summary.

**Explorer menus in exactly three groups.** The right-click menu on files and folders is now:

| Menu entry | Command |
| --- | --- |
| `ZIP으로 압축 (&Mothzip)` | `mothzip pack-auto --format zip "%1"` |
| `압축하기 (&Mothzip)` | a cascade of 13 formats: `ZIP (.zip)`, `7Z (.7z)`, `TAR (.tar)`, `TAR.GZ (.tar.gz)`, `TAR.ZST (.tar.zst)`, `TAR.XZ (.tar.xz)`, `TAR.LZ4 (.tar.lz4)`, `TAR.BR (.tar.br)`, `GZIP (.gz)`, `Zstandard (.zst)`, `XZ (.xz)`, `LZ4 (.lz4)`, `Brotli (.br)` |
| `알아서 풀기 (&Mothzip)` (archives only) | `mothzip smart-unpack "%1" --open` |

Archive extensions expose only the single extraction entry. The earlier `여기에 풀기` / `각각 폴더에 풀기` verbs are removed, and both install and uninstall delete the legacy `MothZip.UnpackHere` / `MothZip.UnpackFolder` keys, so an upgraded machine keeps only the current menu.

**Double-click opens a list window.** `MothZip.Archive\shell\open` runs `mothzip open "%1"`. The window reads the central directory and lists entries without extracting anything: five columns (name, size, compressed size, modified time, encryption), with `압축 풀기` (all), `선택 풀기`, `검사`, and `닫기`. Enter extracts, Esc closes, and cancelling the destination-folder dialog does nothing. A corrupt or password-protected archive keeps the window open, reports the error in the status line, and disables the actions. `open` is rejected in headless runs — measured: `mothzip open out.zip --non-interactive` exits 1 with `The browsing window is unavailable with --non-interactive; use peek, cat, or unpack`.

**`pack --format <fmt>` forces the container.** Accepted values are `zip`, `7z`, `tar`, `tar.gz`, `tar.zst`, `tar.xz`, `tar.lz4`, `tar.br`, `gz`, `zst`, `xz`, `lz4`, and `br`; without it the output suffix still decides. Measured: `mothzip pack forced.7z src --format zip` wrote a ZIP under the `.7z` name (`archive_path` reported `forced.7z`).

**`pack-auto SOURCE... [--format <fmt>]` takes several sources.** One source writes `<stem>.<ext>` beside it (`one\only.txt` → `one\only.zip`). Several write a single archive beside the selection, named after the containing folder of the first source (`sel\a.txt` + `sel\b.txt` → `sel\sel.zip`); bare names with no folder component fall back to `archive.zip`. `--format` defaults to `zip` and applies the same 13 values (`mothzip pack-auto one\only.txt --format 7z` → `one\only.7z`). Single-stream formats take one file's bytes; a selection that resolves to more than one file is refused with `GZIP format only supports compressing a single file (use .tar.gz for multiple files or directories)`.

**`pack --to-stdout` writes the archive to stdout.** The positional `OUTPUT` is still required and only selects the format:

```powershell
cmd /c "mothzip.exe pack out.zip src --to-stdout > out.zip"
```

`--to-stdout` conflicts with `--json` and `--volume-bytes` (exit 2, 0 bytes on stdout), and 7z is refused with exit 1 (`7Z rewrites its signature header when it finishes, so it needs a seekable output; write it to an archive path instead of stdout`). There is no staging or commit step, so a failure exits 1 after bytes may already have been emitted; a closed downstream pipe is a success (exit 0). Measured byte identity with the file-writing path: a 593-byte ZIP and a 209-byte `TAR.ZST` from the same input had identical SHA-256 either way.

**`pack --reproducible`.** Entries are sorted by member-name bytes and every timestamp is pinned to `SOURCE_DATE_EPOCH` (0 when unset; ZIP clamps to 1980-01-01). Measured: ZIP, 7z, TAR, TAR.GZ, and TAR.ZST were byte-identical across two runs, and the ZIP stayed byte-identical after a source mtime change; `SOURCE_DATE_EPOCH=1700000000` produced ZIP `date_time` `(2023, 11, 14, 22, 13, 20)`; an invalid value exits 1 with `SOURCE_DATE_EPOCH must be whole seconds since 1970-01-01, got 'not-a-number'`. AES-encrypted archives are excluded from the guarantee because the IV is random — two `--reproducible` runs of the same password-protected ZIP differed.

**TAR+codec `test` now detects corruption.** After one payload byte was flipped, `test` returned exit 1 with `passed: false` for `TAR.GZ`, `TAR.ZST`, `TAR.XZ`, and `TAR.LZ4`, matching `unpack`; see the table and the first bullet in §6.

**RAR `cat` streams with bounded memory.** On large members peak RSS fell from 545.7 MB to **8.9 MB** (−98.4%), and it stayed flat at 8.87–9.75 MB from 64 KiB to 512 MiB members.

**ZIP Zstandard (method 93) members are readable.** ZIPs written by other tools with Zstd members are read; Mothzip still writes deflate (and stored at level 0), and `add`/`edit` never emit method 93. See §6.

**`--volume-bytes` accepts any output path.** A bare relative name no longer fails with `os error 3`; bare, `.\`-prefixed, absolute, and subfolder paths all succeed, and stale higher-numbered parts from a previous set are removed. See §7.

**7z write memory follows the LZMA2 dictionary.** The match-finder state and the measured dictionary table are in §9; prefer `--level 4` (4 MiB dictionary, about 47 MiB peak) or `--7z-dict` for large 7z output.

See [`docs/RELEASE.md`](RELEASE.md) for packaging and the release checklist.
