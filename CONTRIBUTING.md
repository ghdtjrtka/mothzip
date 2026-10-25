# Contributing to Mothzip

Mothzip is a Windows-only archive CLI built for scripts and agents: one executable, no runtime, no
background service, no ads, no telemetry. Its scope is deliberately narrow — archives it can create,
read and verify, plus a small browsing window. It has no RAR creation, no repair, and no ALZ/EGG
support. Read [README.md](README.md) for the feature scope and
[docs/AGENT-CLI.md](docs/AGENT-CLI.md) for the command contract.

## The one rule: measurements, not claims

Every behaviour change in this repository has to come with a case that fails before it and passes
after it. Reviewers here check the *evidence*, not the summary:

- A test or audit case must assert what the user observes (bytes on disk, exit code, stderr JSON).
- "It works now" is not a result. Name the command, the case and the number.
- If you cannot measure something, say so in the PR under `Unverified`. That is respected; claiming
  a measurement you did not take is not.
- Do not weaken a check to make a build pass (no deleted assertions, no `#[allow]` to silence a
  warning, no skipped case counted as passing).

## Build

Requirements: Windows 10/11 x64, Rust **1.93+** with the MSVC toolchain, and the Windows SDK.

```powershell
cargo build --release
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

The release profile sets `panic = "abort"`: a panic here is a bug, not a recoverable state, and the
unwinding tables cost about 1.7 MB of the download. Nothing uses `catch_unwind`, and `cargo test`
runs in the default profile, so tests still unwind. If you add code that relies on catching a
panic, move it out of the release path or raise the profile change in the pull request.

The release binary is a single file with a statically linked CRT
(`.cargo/config.toml` sets `+crt-static`), so it needs no Visual C++ Redistributable.

### Troubleshooting: `D8050` / "cannot execute c1.dll"

If your checkout lives in a directory that carries a **Low mandatory integrity label** (for example
a workspace created by a sandboxing tool), executables built inside it inherit that label. Cargo's
build scripts then run at low integrity and MSVC's `cl.exe` fails to spawn `c1.dll`, which surfaces
as `zstd-sys` build failure `D8050`. Nothing in the source is wrong. Build outside that tree:

```powershell
$env:CARGO_TARGET_DIR="$env:TEMP\mothzip-build"
cargo build --release
```

Do not add a temporary-directory fallback to the product to work around it.

## Verify like a release, not like a smoke test

The scripts under `audit/` are the release gate. They create their own fixtures, then check bytes,
exit codes and preserved state. Run at least these before opening a PR that touches `src/`:

```powershell
python audit/release_validation.py --binary target\release\mothzip.exe
python audit/codec_cli_validation.py --binary target\release\mothzip.exe
python audit/edit_cli_validation.py --binary target\release\mothzip.exe
python audit/zip_zstd_validation.py --binary target\release\mothzip.exe
python audit/selection_cli_validation.py --binary target\release\mothzip.exe
python audit/sfx_validation.py --binary target\release\mothzip.exe
python audit/split_cli_validation.py --binary target\release\mothzip.exe
```

The compatibility check additionally needs the independent reference fixtures (RAR and archive sets
produced by other tools); those are not in the repository, so it is optional locally and skipped in
CI.

Two audit scripts need notes:

- `audit/perf_bench.py` measures throughput and peak RSS. Its absolute numbers vary with machine
  load; compare against your own previous run, not against a number in a document.
- `audit/menu_validation.py` and `audit/gui_window_validation.py` exercise the installed Explorer
  menu and the browsing window. They write to `HKCU` and open a real window, so run them only on a
  machine where Mothzip is installed (`mothzip install`).

## Layout

```
src/core/      archive engine — no CLI, no UI
  format.rs      name table (one source of truth for suffixes), detection, container readers
  codec.rs       stream codec contract + GZIP/Brotli; codec/{zstd,xz,lz4}.rs are the backends
  pack.rs        archive creation: plan, sinks (file/stdout/stdin), 7z dictionary, reproducible
  extract.rs     staging + guards + commit; selection and flat extraction
  inspect.rs     peek / cat / test
  catalog.rs     one member list for every reader
  edit.rs        add / delete / rename on ZIP and 7z
  guard.rs       budget readers/writers, path sanitising, zip-bomb checks
  commit.rs      the one transactional commit used by every writer
src/cli/       argument parsing and JSON/text output
src/shell/     Windows integration: registry profile, installer, shortcut, PATH, status
src/ui/        the browsing window (Win32 ListView); no archive logic of its own
```

Two invariants worth knowing before you edit:

1. **Containers and codecs are separate.** A new stream codec implements `decoder`/`encoder` in
   `src/core/codec/<name>.rs` and adds one row to `StreamCodec`; TAR and single-stream paths pick it
   up with no per-format code.
2. **One producer per decision.** Extension classification, the archive plan, the source inventory
   and the commit are single functions. If you need a second copy of one of them, the change is
   probably in the wrong place.

## Adding a stream codec

1. Implement `pub fn decoder(file: File) -> io::Result<Box<dyn Read>>` and
   `pub fn encoder<W: Write + 'static>(sink: W, level: Option<u32>) -> io::Result<Box<dyn FinishEncoder>>`
   in `src/core/codec/<name>.rs`.
2. Write exactly one stream/frame per archive, and return every concatenated member when reading.
3. Truncated, corrupt or trailing data must be `io::ErrorKind::InvalidData` — never a short read that
   looks like success. Write that case as a test; a decoder that quietly returns fewer bytes is a
   bug, not a limitation.
4. Declare the level range and default in `StreamCodec`; an out-of-range level must be rejected
   before any output file exists.
5. Compare against an independent implementation (Python `zstandard`, `lzma`, `lz4.frame`,
   `brotli`, or the reference tool) in both directions by SHA-256. `audit/codec_cli_validation.py`
   is the pattern.
6. Keep memory bounded: a decoder must not buffer the whole stream to answer `read`.

## Failure contract (do not break these)

- Exit codes: `0` success, `1` processing failure, `2` invalid command line.
- `--json` writes the result to stdout and errors as a JSON object on stderr.
- A failed extraction or edit leaves existing files unchanged; staging directories are removed.
- `cat` and `--to-stdout` may emit bytes before failing. Consumers must discard output on a nonzero
  exit; the documentation states this rather than pretending otherwise.
- Path escapes, ADS, reserved device names and links are refused, not sanitised silently. A
  duplicate member name inside an archive is not an error on extraction (the last entry wins,
  as other tools do); an edit that would create a second member with an existing name is
  refused.

## Commits and pull requests

- Conventional commits (`feat:`, `fix:`, `docs:`, `test:`, `refactor:`, `chore:`), scoped by area
  when useful (`fix(pack): …`).
- One purpose body per commit: what changed for whom, and the measurement that shows it.
- Do not mix a refactor with a behaviour change in one commit; reviewers cannot check both at once.
- Never commit `audit/runs/`, `audit/tools/`, captured logs or local handoff notes; they are ignored
  on purpose.
- No generated binaries in the tree. Releases ship from CI output.

## Security

Report vulnerabilities through GitHub's private security advisory on this repository rather than a
public issue. Known, documented limitations (not vulnerabilities to report again): RAR5 dictionary
size is not capped, so a hostile archive can request a large allocation; Brotli carries no payload
checksum, so silent corruption cannot be detected; releases are not code-signed.
