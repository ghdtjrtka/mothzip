# 🦋 mothzip (나방집)

> **Ultra-lightweight, ad-free archiver optimized for Windows Explorer context menus and AI Coding Agents.**  
> *100% Rust • Single Static Binary (~1.1 MB) • Zero Bloat • Zero Ads*

[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)
[![Built with Rust](https://img.shields.io/badge/Rust-1.80%2B-orange.svg)](https://www.rust-lang.org)
[![Platform: Windows](https://img.shields.io/badge/Platform-Windows%2010%20%7C%2011-0078D6.svg)](https://microsoft.com/windows)
[![Binary Size: 1.1MB](https://img.shields.io/badge/Binary%20Size-1.17%20MB-brightgreen.svg)]()

---

## 💡 Why mothzip?

Most modern archive managers are bloated with advertisements, purchase prompts, and heavy GUI file managers that nobody uses. In reality, **99% of user interactions happen via the Windows Explorer right-click context menu**.

At the same time, **AI Coding Agents (Cursor, Claude Code, Cline, etc.)** face severe pain points when dealing with archives:
1. **Context Window Flooding**: Running `unzip -l` or `tar -tf` on large archives dumps thousands of lines of raw text, blowing up LLM token budgets and API costs.
2. **Wasteful Temp Folders**: Extracting an entire multi-gigabyte archive just to read a single `package.json` wastes disk I/O and time.
3. **Security Risks**: AI agents executing autonomous scripts are vulnerable to Zip-Slip path traversals (`../../`) and Zip-Bombs.

**mothzip** bridges this gap:
- **For Humans**: Double-click registration adds **"알아서 풀기 (Smart Unpack)"** directly to Windows Explorer right-click. No ads, no memory-resident background services.
- **For AI Agents**: Token-capped structured JSON inspection (`peek --json`), zero-extraction streaming (`cat`), and built-in safety boundaries.

---

## ⚡ Key Features

### 1. 스마트 알아서 풀기 (Smart Unpack)
Eliminates desktop and folder clutter automatically:
* **Single Root Archive**: If all files are inside a root folder (e.g., `myapp/...`), extracts directly to current folder without redundant nesting (`dest/myapp/...`).
* **Flat / Loose Files Archive**: If multiple files or directories are loose at the root, automatically creates a dedicated subfolder (`dest/archive_name/...`) to keep your directory pristine.

### 2. 에이전틱 모드 (Agentic CLI)
* **`mothzip peek <archive.zip> --json`**: Reads the central directory in < 1ms to return file counts, compression ratio, extension distribution, and a depth-limited tree summary in structured JSON.
* **`mothzip cat <archive.zip> <internal/path>`**: Streams a specific file directly to `stdout` from memory without extracting the archive to disk.
* **Built-in Security Guards**:
  * **Zip-Slip Prevention**: Rejects any relative (`..`) or absolute paths escaping the extraction root.
  * **Zip-Bomb Protection**: Pre-checks uncompressed size before extraction to protect disk space.

### 3. 초경량 & 무설치 (Ultra-lightweight)
* Single `.exe` binary under **1.2 MB**.
* Cold startup under **5ms**.
* Zero external runtimes (no Python, no Java, no Electron, no C-compiler runtime dependencies).

---

## 🚀 Quick Start

### Installation & Context Menu Registration

1. Download `mothzip.exe` from [Releases](../../releases).
2. Open PowerShell or Command Prompt in the folder containing `mothzip.exe`:
   ```bash
   mothzip register
   ```
   *That's it!* Windows Explorer will immediately show:
   * Right-click on `.zip` / `.7z`: **`알아서 풀기 (&MothZip)`** and **`여기에 풀기`**
   * Right-click on any folder: **`ZIP으로 압축 (&MothZip)`**
   *(No administrator privileges required; writes cleanly to per-user `HKCU`).*

To remove context menu integration:
```bash
mothzip unregister
```

---

## 💻 CLI Usage

```text
mothzip [COMMAND] [OPTIONS]

COMMANDS:
  smart-unpack <ARCHIVE> [-o <DEST>]     Smart unpack (Explorer default)
  unpack       <ARCHIVE> [-o <DEST>]     Direct unpack into target destination
  pack         <OUTPUT.zip> <SOURCES...> Compress files or folders into a .zip
  peek         <ARCHIVE>                 Token-safe structure overview (Agentic)
  cat          <ARCHIVE> <PATH>          Stream file contents to stdout (Agentic)
  register                               Register Windows Explorer context menu
  unregister                             Remove Windows Explorer context menu

GLOBAL OPTIONS:
  --json       Output results in structured JSON format
  --quiet      Suppress non-essential output
```

### Examples

#### 1. Smart Unpack (우클릭 기본 동작)
```bash
mothzip smart-unpack release.zip
```

#### 2. Pack a Folder or Files
```bash
mothzip pack my_project.zip src/ Cargo.toml README.md
```

#### 3. Agentic Peek (`--json`)
```bash
mothzip peek large_repo.zip --depth 2 --limit 30 --json
```
Output:
```json
{
  "archive_name": "large_repo.zip",
  "total_entries": 3420,
  "total_compressed_bytes": 10485760,
  "total_uncompressed_bytes": 45120800,
  "compression_ratio": "4.3x",
  "has_single_root": true,
  "root_name": "repo-main",
  "extension_distribution": {
    ".rs": 140,
    ".json": 12,
    ".md": 4
  },
  "tree_summary": [
    { "path": "repo-main/", "is_dir": true, "size": 0 },
    { "path": "repo-main/Cargo.toml", "is_dir": false, "size": 1240 },
    { "path": "repo-main/src/", "is_dir": true, "size": 0 }
  ],
  "truncated_items": 3417
}
```

#### 4. Stream a Single File (`cat`)
```bash
mothzip cat large_repo.zip repo-main/Cargo.toml
```

---

## 🛠️ Building from Source

```bash
git clone https://github.com/your-username/mothzip.git
cd mothzip
cargo build --release
```
The optimized executable will be located at `target/release/mothzip.exe`.

---

## 📜 License

MIT License. Free for personal, open-source, and commercial use. Zero ads, forever.
