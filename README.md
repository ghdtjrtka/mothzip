# 🦋 mothzip (나방집)

> **Ultra-lightweight, ad-free archiver optimized for Windows Explorer context menus and AI Coding Agents.**  
> *100% Pure Rust • Single Static Binary (~1.3 MB) • Zero Bloat • Zero Ads • AES-256 Support*

[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)
[![Built with Rust](https://img.shields.io/badge/Rust-1.80%2B-orange.svg)](https://www.rust-lang.org)
[![Platform: Windows](https://img.shields.io/badge/Platform-Windows%2010%20%7C%2011-0078D6.svg)](https://microsoft.com/windows)
[![Binary Size: 1.3MB](https://img.shields.io/badge/Binary%20Size-1.37%20MB-brightgreen.svg)]()

---

## 💡 Why mothzip?

Most modern archive managers are bloated with advertisements, purchase popups, and heavy file managers that nobody uses. In reality, **99% of daily interactions happen via the Windows Explorer right-click context menu**.

At the same time, **AI Coding Agents (Cursor, Claude Code, Cline, AutoGen, etc.)** face severe pain points when dealing with archives:
1. **Context Window Flooding**: Running `unzip -l` or `tar -tf` on large archives dumps thousands of lines of raw text, blowing up LLM token budgets and API costs.
2. **Wasteful Temp Folders**: Extracting an entire archive just to read a single configuration file wastes disk I/O and time.
3. **Security Vulnerabilities**: Autonomous agents are vulnerable to Zip-Slip path traversals (`../../`), Windows DOS device bugs (`CON`, `PRN`), and deceptive Zip-Bombs.

**mothzip** solves both worlds out of the box:
- **For Humans**: One-command registration adds **"알아서 풀기 (Smart Unpack)"**, **"여기에 풀기"**, and **"각각 폴더에 풀기"** directly to Windows Explorer right-click. Also supports right-clicking **any individual file or folder** to compress it into a `.zip`. Zero ads, zero background services.
- **For AI Agents**: Token-capped structured JSON inspection (`peek --json`), zero-extraction streaming (`cat`), AES-256 decryption, and multi-layered security guards.

---

## ⚡ Key Features & Production Hardening

### 1. 스마트 알아서 풀기 (Smart Unpack)
Eliminates desktop and folder clutter automatically:
* **Single Root Archive**: If all files are already wrapped inside a root directory (e.g., `myapp/...`), extracts directly to target without redundant nesting.
* **Flat / Loose Files Archive**: If multiple files or directories are loose at the root, automatically creates a dedicated subfolder (`dest/archive_name/...`) to keep your directory clean.

### 2. 한국어 CP949 / EUC-KR 및 macOS NFD 한글 자모 분리 완벽 해결
* **CP949/EUC-KR 자동 감지**: 구버전 알집, 윈도우 탐색기, 공공기관 등에서 UTF-8 플래그 없이 압축된 한글 파일명을 자동 감지하여 깨짐 없이 원본 그대로 복원합니다.
* **macOS NFD 한글 자모 조합 (NFC)**: Mac에서 생성된 아카이브(`ㅎㅏㄴㄱㅡㄹ.txt`)를 유니코드 표준 부록 #15(UAX #15) 수학적 공식 기반으로 순수 Rust에서 자체 조합하여 탐색기에서 온전한 `한글.txt`로 복원합니다.

### 3. Windows 앱 및 탐색기 우클릭 등록 (Start Menu & Settings)
* `mothzip register` 한 번으로:
  * **탐색기 우클릭 메뉴**: .zip에 `알아서 풀기 (&MothZip)`, `여기에 풀기`, `각각 폴더에 풀기`, 일반 파일/폴더에 `ZIP으로 압축 (&MothZip)` 등록
  * **시작 메뉴 바로가기**: 나방 아이콘이 적용된 `MothZip (나방집)` 바로가기 자동 생성
  * **설정 > 앱 > 설치된 앱**: 나방 아이콘과 함께 제어판/설정에 정식 앱으로 등록 (`mothzip unregister` 간편 삭제 지원)
  * **실행 창 (`Win + R`)**: `mothzip` 명령어로 즉시 호출 가능

### 4. 암호화 아카이브 (AES-256 / ZipCrypto) 및 대화형 프롬프트
* `-p, --password <PASS>` 옵션을 통해 WinZip AES-128, AES-256 및 전통적인 ZipCrypto 암호화 아카이브를 완벽하게 해제하고 검사할 수 있습니다.
* 터미널에서 비밀번호를 생략하고 실행한 경우, 즉석에서 안전하게 비밀번호를 입력할 수 있는 대화형 프롬프트를 지원합니다 (AI 에이전트 모드에서는 데드락 없이 즉시 JSON 에러 반환).

### 5. 무결점 트랜잭션 안전성 및 파일 수정 시각 (Timestamp) 보존
* **트랜잭션 롤백**: 디스크 풀이나 CRC 오류 등 추출/압축 도중 장애 발생 시 미완성 쓰레기 파일을 즉시 자동 삭제하여 디스크 오염을 방지합니다.
* **수정 시각 완벽 복원**: 압축 파일 내부에 기록된 원본 수정 시간(mtime)을 Howard Hinnant 알고리즘으로 2초 정밀도로 완벽 복원합니다.
* **읽기 전용 속성 자동 조정**: 대상 파일이 Read-Only 속성을 가지고 있더라도 권한 충돌 없이 안전하게 교체합니다.

### 6. 에이전틱 모드 (Agentic CLI)
* **`mothzip peek <archive.zip> --json`**: 아카이브를 풀지 않고 중앙 디렉터리만 읽어 파일 수, 압축률, 확장자 통계, 깊이 제한 트리를 JSON으로 1ms 내 반환.
* **`mothzip cat <archive.zip> <internal/path>`**: 원하는 파일 1개만 메모리에서 즉시 `stdout`으로 스트리밍.
* **다층 방탄 보안 가드 (Adversarial Security Guards)**:
  * **Zip-Slip 차단**: 상대경로(`..`) 및 절대경로 탈출 완전 방어.
  * **Windows DOS 예약 장치명 차단**: `CON`, `PRN`, `AUX`, `NUL`, `COM1..9`, `LPT1..9`, `CLOCK$` 등 윈도우 디바이스 드라이버 파일 생성 차단.
  * **NTFS 대체 데이터 스트림 (ADS) 차단**: 콜론(`:`) 스트림 백도어 방어.
  * **크로스플랫폼 특수문자 정제**: Linux 등에서 생성된 `< > " | ? *` 파일명을 안전한 `_`로 치환하여 OS Error 123 방지.
  * **기만형 스트리밍 압축폭탄 차단**: 헤더 크기를 거짓 기재하여 실제로는 거대한 데이터를 쏟아내는 압축폭탄을 스트림 레벨에서 강제 차단.
  * **미존재 원본 및 자기 압축 방어**: 압축 전 원본 파일 실존 여부 및 자기 자신을 압축 파일로 덮어쓰는 오류 사전 차단.

---

## 🚀 Quick Start

### Windows Explorer Context Menu Registration

1. Download `mothzip.exe` from [Releases](../../releases).
2. Open PowerShell or Command Prompt in the folder containing `mothzip.exe`:
   ```bash
   mothzip register
   ```
   *That's it!* Windows Explorer will immediately show:
   * Right-click on `.zip`: **`알아서 풀기 (&MothZip)`**, **`여기에 풀기`**, **`각각 폴더에 풀기`**
   * Right-click on any single file or folder: **`ZIP으로 압축 (&MothZip)`**
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
  smart-unpack <ARCHIVE> [-o <DEST>] [-p <PASS>] [--to-folder]  Smart unpack (Explorer default)
  unpack       <ARCHIVE> [-o <DEST>] [-p <PASS>] [--to-folder]  Direct unpack into target destination
  pack         <OUTPUT.zip> <SOURCES...>                        Compress files or folders into a .zip
  pack-auto    <SOURCE>                                         Auto-compress file/dir to <name>.zip in place
  peek         <ARCHIVE> [--depth N] [--limit N]                Token-safe structure overview (Agentic)
  cat          <ARCHIVE> <PATH> [-p <PASS>]                     Stream file contents to stdout (Agentic)
  register                                                      Register Windows Explorer context menu
  unregister                                                    Remove Windows Explorer context menu

GLOBAL OPTIONS:
  --json       Output results in structured JSON format
  --quiet      Suppress non-essential output
```

### Examples

#### 1. Smart Unpack (우클릭 기본 동작)
```bash
mothzip smart-unpack release.zip
```

#### 2. Encrypted Archive Unpack
```bash
mothzip unpack confidential.zip -p "MySecret123!"
```

#### 3. Pack Multiple Files and Directories
```bash
mothzip pack my_backup.zip src/ Cargo.toml README.md
```

#### 4. Agentic Peek (`--json`)
```bash
mothzip peek large_repo.zip --depth 2 --limit 30 --json
```

#### 5. Stream a Single File (`cat`)
```bash
mothzip cat large_repo.zip repo-main/Cargo.toml
```

---

## 🛠️ Building from Source

```bash
git clone https://github.com/your-username/mothzip.git
cd mothzip
cargo test
cargo build --release
```
The optimized executable will be located at `target/release/mothzip.exe`.

---

## 📜 License

MIT License. Free for personal, open-source, and commercial use. Zero ads, forever.
