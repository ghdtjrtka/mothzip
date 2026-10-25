# Mothzip

An ad-free archive CLI for Windows, with JSON output for agents and Explorer actions for people. MIT licensed. No account, background service, bundled ads, or external archiver executable.

**Windows 10/11 x64 only, single executable, no runtime.** The command line is the primary
interface: `--json` results, exit codes `0`/`1`/`2`, `--non-interactive`, and a `--max-bytes`
budget on every path that expands payload (`peek` lists structure without decompressing and is not bounded). Explorer adds a three-item menu and a small browsing window that lists,
extracts and checks an archive. Scope is deliberately narrow — no RAR creation, no repair, no
ALZ/EGG archives, no preview pane. Building from source and running the release gate:
[CONTRIBUTING.md](CONTRIBUTING.md).

[한국어 안내](README.ko.md)

## Formats

| Format | Extract / inspect / test / stream | Create |
| --- | --- | --- |
| ZIP | Yes; ZipCrypto and AES passwords | Yes; optional AES-256 |
| 7z | Yes; encrypted, solid, split inputs | Yes; optional AES-256 and split volumes |
| RAR 3/4/5 | Yes; encrypted, solid, multipart inputs | No |
| TAR | Yes | Yes |
| TAR.GZ / TGZ, TAR.ZST, TAR.XZ, TAR.LZ4, TAR.BR | Yes | Yes |
| GZIP (.gz) | Yes; concatenated members included | One input file |
| Zstandard (.zst), XZ (.xz), LZ4 (.lz4), Brotli (.br) streams | Yes | One input file |

Checksums differ by codec: gzip (CRC32/ISIZE) and xz (CRC64) verify inside the format, and zstd/LZ4 frames written by Mothzip carry a content checksum, so `test` and `unpack` reject corruption in archives this tool created. Brotli defines no payload checksum, so a corrupted Brotli stream is not detected. zstd/LZ4 streams from other tools that omit a checksum have limited detection.

A `.zst` stream is unrelated to ZIP's Zstd compression method. ZIP members that other tools wrote with Zstandard (method 93) are read; Mothzip itself always writes deflate, or stored at level 0.

Keep every volume together and pass the first part, such as `backup.7z.001` or `backup.part1.rar`. Support for a format family does not imply every codec or damaged archive can be recovered.

## Install

Windows 10/11 x64. Extract the release ZIP, then run **Install.cmd**, or:

```powershell
.\mothzip.exe install
mothzip status --json
```

Installation is per user at `%LOCALAPPDATA%\Programs\MothZip`. It adds Explorer actions, a Start menu shortcut, user PATH, and a Windows Installed apps entry. Open a new terminal after installing. A custom directory can be selected with `install --dir "C:\Your\MothZip"`.

Double-click an associated archive to open the list window: it reads the central directory without extracting anything and offers extract-all, extract-selected, test, and close (Enter extracts, Esc closes). Right-clicking files and folders gives three groups: `ZIP으로 압축` (one-click ZIP), `압축하기` (a cascade of 13 formats), and, for archives, `알아서 풀기` (smart extract); the older `여기에 풀기` / `각각 폴더에 풀기` entries are removed on upgrade. Windows 11 may place these actions under **Show more options**. Set default associations through Windows **Open with / Default apps**; installation does not overwrite protected UserChoice settings.

Remove through **Settings → Apps → Installed apps → Mothzip**, or `mothzip uninstall`. Only manifest-owned files are removed; unrelated files are retained. Self-removal returns `status: "scheduled"` and `cleanup_result_path`. Agents must read that result before treating removal as completed.

For portable use, run the executable directly. `register` / `unregister` change Explorer registration while leaving portable files intact. Release binaries are not code-signed.

## Commands

```powershell
mothzip pack backup.7z .\project --level 5 --json
mothzip pack backup.7z .\project --volume-bytes 104857600
mothzip smart-unpack backup.7z -o .\restored --json --non-interactive
mothzip unpack bundle.rar -o .\restored --json
mothzip unpack bundle.zip -o .\restored --member docs/readme.md --member src --flat
mothzip peek bundle.zip --depth 2 --limit 30 --json
mothzip test bundle.zip --json
mothzip cat bundle.zip project/Cargo.toml
mothzip pack logs.tar.gz .\logs
mothzip pack logs.tar.zst .\logs
mothzip pack logs.tar.xz .\logs
mothzip pack file.gz .\file.txt
mothzip pack app.js.br .\app.js
mothzip pack forced.7z .\project --format zip
mothzip pack out.zip .\project --reproducible
mothzip pack-auto .\report.txt --format 7z
mothzip pack-auto .\report.txt .\notes.txt
mothzip add bundle.zip .\new-file.txt
mothzip delete bundle.7z old/notes.txt
mothzip rename bundle.zip docs/old.md docs/new.md
mothzip sfx setup.exe bundle.zip
```

`pack` chooses the format from the output extension, and `--format` forces one of `zip`, `7z`, `tar`, `tar.gz`, `tar.zst`, `tar.xz`, `tar.lz4`, `tar.br`, `gz`, `zst`, `xz`, `lz4`, or `br`. `--to-stdout` sends the archive to stdout instead of writing the output path; the path is still required and only selects the format (`cmd /c "mothzip.exe pack out.zip .\project --to-stdout > out.zip"`), it conflicts with `--json` and `--volume-bytes`, 7z is refused, and a closed pipe counts as success. `--reproducible` sorts members and pins every timestamp to `SOURCE_DATE_EPOCH` so repeated runs are byte-identical; encrypted archives are excluded. `--level` accepts the range of the target codec: gzip 0–9, xz 0–9, zstd 1–22, brotli 0–11, ZIP and 7z 0–9; LZ4 has no level and rejects one explicitly. An out-of-range level is refused before any output file is created, and level 0 stores ZIP/7z data. `--volume-bytes N` splits 7z output into numbered `OUTPUT.7z.001` volumes. `pack-auto SOURCE...` writes `<stem>.<ext>` beside a single source, or one archive beside the selection for several, named after the first source's folder (`archive.zip` when the names carry no folder). `smart-unpack` preserves a single top-level folder and wraps loose files in an archive-named folder. A bare supported archive path also selects smart extraction.

`--member PATH` (repeatable) extracts only the named members or directory subtrees, and `--flat` writes files directly into the output directory without the archive's folders. `add`, `delete`, and `rename` edit ZIP and 7z archives; RAR, TAR, stream, split 7z, and SFX editing are rejected. `sfx OUTPUT.exe INPUT.zip` wraps a ZIP as a self-extracting executable and preserves its password encryption.

Use `--password-stdin` to read one UTF-8 password line without a prompt. `-p` / `--password` expose their value in the process command line. TAR and GZIP reject password encryption. Interactive extraction can prompt with console echo disabled.

## For developers and agents

[The CLI contract for scripts, CI, and agents](docs/AGENT-CLI.md) lists every flag that exists, the JSON written to each stream, and the integrity guarantees that do not exist — read it before wiring Mothzip into a pipeline.

- `--json` prints results on stdout and error objects on stderr; `--non-interactive` removes prompts and desktop windows.
- Exit codes: **0** success, **1** processing failure, **2** invalid command line.
- `cat` writes raw bytes to stdout even with `--json`; discard its output whenever the exit code is nonzero.

## Agent contract

- `--json`: results on stdout, error objects on stderr. `--non-interactive`: no prompts or desktop windows. `--open` conflicts with both.
- Exit codes: **0** success, **1** processing failure, **2** invalid command line.
- `cat` always emits raw bytes, including with `--json`. Discard output on a nonzero exit: corruption can be discovered after partial output. A closed downstream pipe is treated as successful cancellation.
- `peek` is structural inspection; `test` / `verify` reads every payload for integrity. A successful peek is not an integrity check.
- `--limit` and `--depth` bound the displayed tree. Truncated display names are not member identifiers for `cat`.

## File handling and scope

Extraction stages and verifies content before replacing files. Existing files are backed up during replacement and restored after an observed commit failure. Destination escapes, ADS, reserved Windows names, links, device objects, and file/directory conflicts are rejected. Normalization collisions are disambiguated to retain both files. CP949 ZIP names and Korean decomposed names are supported.

**Existing regular files with matching output names are replaced.** Use a new directory when you need both versions. Power loss, forced termination, and hostile concurrent filesystem changes are outside the rollback guarantee. Failed recovery reports the retained recovery path.

`--max-bytes` bounds decompressed bytes for every read, extraction, `cat`, and `test` path; the default is 10 GiB. TAR headers, padding, and trailing data count toward it. This is not a CPU-time or memory sandbox. Archive links and device objects are unsupported; common macOS/Explorer metadata is filtered.

Creation, extraction, inspection, verification, and streaming are implemented, with selected-member (`--member`) and flat (`--flat`) extraction, split 7z creation, `add` / `delete` / `rename` for ZIP and 7z, and ZIP SFX creation. Editing RAR, TAR, and stream archives, editing split 7z or SFX files, repair/recovery, RAR creation, and ALZ/EGG archives are not implemented. The browsing window lists, extracts and checks; it has no preview pane, password prompt, drag and drop, or background loading.

## Build and verify

Rust **1.93+**, Windows MSVC toolchain and Windows SDK:

```powershell
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
cargo build --release --locked
python audit/release_validation.py --binary target/release/mothzip.exe
```

The build embeds the moth/zipper icon and package version and links the CRT statically; Windows system DLLs remain required. Python 3 and symlink creation permission are needed for adversarial fixtures. Archive fixtures stay under `audit/runs`; live installation checks are separate.

Use `cargo audit` with a current RustSec database. `audit/package_release.py` requires passing validation and dependency audit evidence, checks source/binary hashes, includes dependency licenses, and round-trips the distributable. See [release information](docs/RELEASE.md).
