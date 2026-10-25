# Mothzip release information

## Public repository description

Ad-free Windows archive CLI with JSON output: ZIP, 7z, TAR and gzip/zstd/xz/lz4/brotli creation, RAR extraction, ZIP/7z editing, reproducible output, a double-click archive browser, SFX, and Explorer integration.

Topics are in `.github/repository-metadata.json`. Repository creation and publication are separate from preparing and testing these local artifacts.

## Release notes

### 1.2.3

- **One name everywhere: Mothzip.** The Korean name is gone from the installer entry, the Start
  Menu shortcut, the window titles, the file-type description and the menu label. The registry
  ProgIds and command names are unchanged (`MothZip.Pack`, `MothZip.SmartUnpack`,
  `mothzip.exe`), because changing them would break the associations of every existing install.
  A Start Menu shortcut published under the retired name is removed on install, so the menu does
  not keep two entries.

### 1.2.2

Review pass before publication. Everything below was reproduced first and re-measured after.

- **Release profile aborts instead of unwinding.** `panic = "abort"` removes the unwinding tables and
  takes the executable from 8,104,448 to 6,347,776 bytes (−21.7%); `lto = "thin"` and `strip = true`
  were measured and rejected because they made the binary larger. Nothing in the tree catches a
  panic, and `cargo test` keeps unwinding in the default profile.
- **`pack <out> C:\` no longer archives the current directory.** The source path was trimmed of its
  trailing separator, which turned a drive root into the drive-relative `C:`; it now resolves to the
  drive root and packs what it names.
- **`pack --from-stdin` is atomic and refuses options it cannot honour.** A failed run used to
  truncate an existing output and silently ignore `--password`, `--reproducible`, `--7z-dict`,
  `--volume-bytes` and positional sources. It now validates first, writes through a staging sibling,
  and replaces the destination only on success; those options are argument errors (exit 2).
- **Archive compression levels are range-checked before staging.** `--level 12` on 7z was accepted and clamped
  silently (measured on 1.2.1) and `--level 15` on ZIP failed after the staging file existed; both now exit 1 with
  `… compression level must be 0..=9` and no output file.
- **`--7z-dict` is refused for non-7z destinations** instead of being ignored.
- **One civil-date implementation and one 7z encoder chain** replace the duplicates in `pack.rs`
  and `edit.rs`.
- **Tests for the gaps the review named:** concatenated LZ4 frames, a truncated final frame, whole
  transaction rollback, staging-path uniqueness, plus release-gate cases for the stdin contract.

### 1.2.1

- Fix legacy Korean (CP949) member names decoding through the host ANSI code page first. On a
  Windows installation whose ANSI code page is not 949, a Korean ZIP or 7z name written before
  UTF-8 names were common decoded as mojibake; CP949 is now tried before the host code page. Latin
  text still decodes correctly because CP949 rejects it.
- Add the continuous integration workflow that runs formatting, clippy, unit tests and the release
  audit gate on every push.

### 1.2.0

- `pack --7z-dict <MiB>` sets the LZMA2 dictionary, which is what 7z creation memory is proportional to: on a 60 MB input the peak fell from 85.1 MiB to 47.1 MiB at 4 MiB (+0.02% size) and 28.1 MiB at 2 MiB (+0.30%).
- `pack --from-stdin` compresses stdin into a single-stream archive (`gz`/`zst`/`xz`/`lz4`/`br`), and composes with `--to-stdout` for pipe-to-pipe conversion. Container formats are refused because a pipe carries no entry names.

- Open a browsing window on double-click: it reads the central directory without extracting and shows name, size, compressed size, modified time, and encryption, with extract-all, extract-selected, test, and close (Enter extracts, Esc closes).
- Rebuild the Explorer menus in three groups: one-click `ZIP으로 압축`, a 13-format `압축하기` cascade, and a single `알아서 풀기` on archives. The earlier `여기에 풀기` / `각각 폴더에 풀기` verbs are deleted on install and uninstall.
- Force the container with `pack --format` (13 values) and stream output with `pack --to-stdout`; there is no staging, a closed pipe is success, and 7z is refused because it needs a seekable output.
- Let `pack-auto` take several sources and write one archive beside the selection, named after the first source's folder (`archive.zip` when the names carry no folder); one source still writes `<stem>.<ext>`.
- Reproduce output byte for byte with `pack --reproducible`: members are sorted and timestamps pinned to `SOURCE_DATE_EPOCH` (0 when unset). Encrypted archives are excluded because AES uses a random IV.
- Fix `test` for `TAR.GZ`/`TAR.ZST`/`TAR.XZ`/`TAR.LZ4`: a corrupt stream now fails with exit 1 instead of reporting `passed: true`.
- Stream RAR members with bounded memory: peak RSS on large members fell from 545.7 MB to 8.9 MB (−98.4%), flat from 64 KiB to 512 MiB.
- Read ZIP members that other tools compressed with Zstandard (method 93); Mothzip still writes deflate, or stored at level 0.
- Fix `--volume-bytes` with a bare relative output name, which failed with `os error 3`, and remove stale higher-numbered parts from an earlier volume set.
- Document how 7z write memory follows the LZMA2 dictionary: on a 60 MB input the default 8 MiB dictionary (level 5) peaked at 85.1 MiB, and `--7z-dict 4` peaked at 47.1 MiB for 0.02% more output, `--7z-dict 2` at 28.1 MiB for 0.30% more, `--7z-dict 1` at 18.5 MiB for 1.2% more. The level only picks that dictionary (level 0-1 = 256 KiB/1 MiB, 2-4 = 2/4/4 MiB, 5-6 = 8 MiB, 7-9 = 16/32/64 MiB), so the same ~10.5 MiB of peak RSS per MiB applies. Prefer `--level 4` or `--7z-dict` for large 7z output.
- Publish memory as the usable limit and no throughput target: on the 32 MiB mixed baseline the maximum zstd level peaked at 744.6 MiB against 10.8 MiB at the default, 7z level 9 at 360.2 MiB against 100.4 MiB, and solid 7z unpack at 71.1 MiB. Repeated runs moved CPU-bound cases by up to 43%, so regression bands are ±10% for ordinary cases and ±25% for CPU-bound cases.
- Keep the documented boundary unchanged: no repair/recovery, no ALZ, no EGG, and Windows 10/11 x64 only.

### 1.1.0

- Create TAR.GZ/TGZ, TAR.ZST, TAR.XZ, TAR.LZ4, and TAR.BR archives, and single-stream `.gz`, `.zst`, `.xz`, `.lz4`, and `.br` files from one input file.
- Edit ZIP and 7z archives with `add`, `delete`, and `rename`; RAR, TAR, stream, split 7z, and SFX editing are rejected.
- Extract only the members named by repeatable `--member PATH`, and place leaf names directly in the output directory with `--flat`.
- Split 7z output into numbered `OUTPUT.7z.001` volumes with `--volume-bytes`.
- Wrap a ZIP into a self-extracting executable with `sfx OUTPUT.exe INPUT.zip`, preserving password encryption.
- Select compression levels per codec: gzip 0–9, xz 0–9, zstd 1–22, brotli 0–11, ZIP and 7z 0–9; LZ4 has no level. Out-of-range levels are rejected before any output file is created.
- Validate format checksums where they exist and write a content checksum into zstd and LZ4 frames; Brotli defines no payload checksum, so corruption in a Brotli stream is not detected.
- Bound decompressed bytes on every read, extraction, `cat`, and `test` path with `--max-bytes` (default 10 GiB).

See the README for overwrite behavior, limits, unsupported features, and unsigned-binary status. This release does not claim feature parity with other archivers.

### 1.0.0

- Create ZIP, 7z, TAR, TAR.GZ/TGZ, and single-file GZIP archives.
- Extract, inspect, test, and stream ZIP, 7z, RAR 3/4/5, TAR, and GZIP; supported encrypted and multipart inputs included.
- Use JSON, explicit exit codes, password stdin, and non-interactive mode in automation.
- Install per user with Explorer actions, a native archive picker, Start menu shortcut, PATH, and Windows uninstall support.
- Preserve unrelated installation files on uninstall; report the asynchronous cleanup result.
- Embed a moth-and-zipper icon and version metadata; bundle dependency licenses.

## Release files

Packaging produces `mothzip-<version>-windows-x64.zip`, `mothzip-<version>-source.zip`, and `SHA256SUMS.txt`, named from `Cargo.toml`. Keep source and binary from the same run. The binary ZIP includes `Install.cmd`, English/Korean documentation, MIT license, third-party notices, and a manifest with the validated binary hash.

After selecting an authorized repository, GitHub CLI can apply the prepared information:

```powershell
$repository = 'OWNER/mothzip'
$metadata = Get-Content .github/repository-metadata.json -Raw | ConvertFrom-Json
gh repo edit $repository --description $metadata.description
foreach ($topic in $metadata.topics) { gh repo edit $repository --add-topic $topic }
```

Check uploaded bytes against `SHA256SUMS.txt` after publishing. Local packaging or an upload request alone is not proof that the public download matches.
