# Mothzip

사람은 탐색기 우클릭으로, 에이전트는 헤드리스 CLI와 JSON으로 사용하는 Windows 압축 프로그램입니다. 무광고·계정 불필요·백그라운드 서비스 없음. MIT 오픈소스이며 별도 압축 프로그램을 설치하지 않아도 됩니다.

**Windows 10/11 x64 전용, 단일 실행 파일, 런타임 불필요.** 주 인터페이스는 명령줄입니다: `--json`
결과, 종료 코드 `0`/`1`/`2`, `--non-interactive`, 페이로드를 푸는 모든 경로의 `--max-bytes` 예산(`peek`은 압축을 풀지 않고 구조만 읽으므로 예산 밖). 탐색기에는
3개짜리 메뉴와, 목록·풀기·검사를 하는 작은 창이 추가됩니다. 범위는 의도적으로 좁습니다 — RAR 생성,
손상 복구, ALZ/EGG, 미리보기 창은 없습니다. 소스 빌드와 릴리스 게이트 실행 방법은
[CONTRIBUTING.md](CONTRIBUTING.md)를 보세요.

## 지원 기능

| 형식 | 풀기·조회·무결성 검사·스트리밍 | 만들기 |
| --- | --- | --- |
| ZIP | ZipCrypto/AES 암호 포함 | AES-256 암호 선택 가능 |
| 7z | 암호·solid·분할 입력 포함 | AES-256 암호 선택 가능, 분할 볼륨 |
| RAR 3/4/5 | 암호·solid·분할 입력 포함 | 미지원 |
| TAR | 지원 | 지원 |
| TAR.GZ / TGZ, TAR.ZST, TAR.XZ, TAR.LZ4, TAR.BR | 지원 | 지원 |
| GZIP(.gz) | 연결된 멤버 포함 | 파일 한 개 |
| Zstandard(.zst)·XZ(.xz)·LZ4(.lz4)·Brotli(.br) 단일 스트림 | 지원 | 파일 한 개 |

체크섬은 코덱마다 다릅니다. gzip(CRC32/ISIZE)과 xz(CRC64)는 포맷 자체에 검사값이 있고, MothZip이 만든 zstd·LZ4 프레임에는 content checksum이 들어가므로 이 도구가 만든 아카이브의 손상은 `test`와 `unpack`이 거부합니다. Brotli 포맷에는 payload 체크섬이 없어 손상된 Brotli 스트림은 검출되지 않습니다. 다른 도구가 체크섬 없이 만든 zstd/LZ4 스트림은 손상 검출이 제한됩니다.

`.zst` 스트림은 ZIP 내부의 Zstd 압축 방식과 무관합니다. 다른 도구가 Zstd(방식 93)로 만든 ZIP 멤버는 읽지만, MothZip은 항상 deflate로 쓰며 레벨 0에서는 무압축으로 저장합니다.

분할 파일은 모두 같은 폴더에 두고 첫 파일(`.7z.001`, `.part1.rar` 등)을 지정하세요. 모든 코덱이나 손상 파일의 복구까지 보장하는 것은 아닙니다.

## 설치와 제거

Windows 10/11 x64용 ZIP을 풀고 **Install.cmd**를 실행하세요. 명령행에서는 다음과 같습니다.

```powershell
.\mothzip.exe install
mothzip status --json
```

설치 위치는 `%LOCALAPPDATA%\Programs\MothZip`입니다. 현재 사용자의 우클릭 메뉴, 시작 메뉴, PATH, Windows 설치된 앱 목록에 등록합니다. 설치 뒤 터미널을 새로 열면 `mothzip` 명령을 사용할 수 있습니다. 다른 폴더에는 `install --dir "C:\Your\MothZip"`으로 설치합니다.

압축 파일을 더블클릭하면 중앙 디렉터리만 읽어 목록 창을 엽니다(압축을 풀지 않음). 창에는 `압축 풀기`(전체)·`선택 풀기`·`검사`·`닫기`가 있고, Enter는 풀기, Esc는 닫기입니다. 파일·폴더 우클릭은 `ZIP으로 압축`, 13형식 캐스케이드인 `압축하기`, 압축 파일에는 `알아서 풀기`의 3계열을 제공하며, 이전 `여기에 풀기`·`각각 폴더에 풀기`는 업그레이드 시 제거됩니다. Windows 11에서는 **더 많은 옵션 표시**에 나올 수 있습니다. 기본 앱은 Windows **연결 프로그램/기본 앱**에서 선택합니다. 보호된 기본 앱 설정을 임의로 덮어쓰지 않습니다.

제거는 **설정 → 앱 → 설치된 앱 → Mothzip → 제거**, 또는 `mothzip uninstall`입니다. 설치 명세에 등록된 파일만 지우고 따로 넣은 파일은 보존합니다. 자기 제거의 JSON `scheduled`는 예약 상태입니다. 에이전트는 `cleanup_result_path`의 `completed`까지 확인해야 합니다.

포터블 실행도 가능합니다. `register`/`unregister`는 파일을 유지한 채 우클릭 등록만 변경합니다. 배포 실행 파일에는 코드 서명이 없습니다.

## 사용 예

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

`pack`은 출력 확장자로 형식을 고르고, `--format`으로 `zip`, `7z`, `tar`, `tar.gz`, `tar.zst`, `tar.xz`, `tar.lz4`, `tar.br`, `gz`, `zst`, `xz`, `lz4`, `br` 중 하나를 강제할 수 있습니다. `--to-stdout`은 출력 경로 대신 stdout으로 아카이브를 내보냅니다. 경로는 형식 선택용으로 여전히 필요하고(`cmd /c "mothzip.exe pack out.zip .\project --to-stdout > out.zip"`), `--json`·`--volume-bytes`와 함께 쓸 수 없으며 7z는 거부됩니다. 닫힌 파이프는 성공으로 끝납니다. `--reproducible`은 멤버를 이름 바이트순으로 정렬하고 타임스탬프를 `SOURCE_DATE_EPOCH`로 고정해 같은 입력을 바이트 단위로 같게 만들며, 암호 아카이브는 제외됩니다. `--level`은 대상 코덱의 범위를 따릅니다(gzip 0–9, xz 0–9, zstd 1–22, brotli 0–11, ZIP/7z 0–9). LZ4에는 레벨이 없어 레벨을 주면 명시적 오류가 납니다. 범위 밖 값은 출력 파일을 만들기 전에 거부되며, ZIP/7z의 레벨 0은 무압축 저장입니다. `--volume-bytes N`은 7z 출력을 `OUTPUT.7z.001` 형식의 번호 볼륨으로 나눕니다. `pack-auto SOURCE...`는 소스가 하나면 옆에 `<stem>.<ext>`를, 여러 개면 선택 항목 옆에 아카이브 하나를 만들며 이름은 첫 소스의 폴더 이름을 따릅니다(폴더 성분이 없는 이름이면 `archive.zip`). `smart-unpack`은 단일 폴더 입력은 그대로 풀고, 흩어진 파일은 압축 파일 이름의 폴더에 묶습니다.

`--member PATH`(반복 가능)는 지정한 멤버나 하위 폴더만 풀고, `--flat`은 압축 안의 폴더 구조 없이 출력 폴더에 바로 씁니다. `add`·`delete`·`rename`은 ZIP과 7z 아카이브만 편집합니다. RAR·TAR·스트림 편집, 분할 7z 편집, SFX 자체 편집은 거부됩니다. `sfx OUTPUT.exe INPUT.zip`은 ZIP을 자체 해제 실행 파일로 감싸고 암호 설정을 보존합니다.

`--password-stdin`으로 UTF-8 암호 한 줄을 전달할 수 있습니다. `-p`는 프로세스 명령행에 노출되므로 자동화에는 stdin을 권장합니다. TAR과 GZIP은 암호 암호화를 거부합니다.

`--json`의 결과는 stdout, 오류 객체는 stderr입니다. 종료 코드는 성공 0, 처리 실패 1, 사용법 오류 2입니다. `cat`은 항상 원문 바이트를 출력하므로 종료 코드가 0일 때만 전체 내용을 신뢰하세요. `peek`은 구조 조회이며 전체 무결성 확인에는 `test`를 사용합니다. 축약된 표시 이름을 `cat`의 원본 경로로 사용하지 마세요.

## 개발자와 에이전트

스크립트·CI·에이전트에서 호출할 때 필요한 플래그, 스트림별 JSON, 그리고 보장되지 않는 무결성 범위는 [CLI 계약 문서](docs/AGENT-CLI.md)에 정리했습니다. 파이프라인에 붙이기 전에 먼저 확인하세요.

- `--json`은 결과를 stdout, 오류를 stderr로 보냅니다. `--non-interactive`는 프롬프트와 창을 없앱니다.
- 종료 코드는 성공 0, 처리 실패 1, 잘못된 명령줄 2입니다.
- `cat`은 `--json`을 함께 줘도 원시 바이트를 stdout에 씁니다. 종료 코드가 0이 아니면 그 출력을 버리세요.

## 파일 보존과 현재 범위

추출 데이터를 작업 위치에서 검사한 뒤 기존 파일을 교체합니다. 반영 중 오류가 나면 기존 파일을 복구하고, 복구가 막히면 보존 위치를 알려줍니다. 경로 탈출·ADS·예약 이름·링크·장치 파일·파일/폴더 충돌을 검사합니다. 정규화 후 이름 충돌은 번호로 구분합니다.

**출력 이름이 같은 기존 일반 파일은 교체됩니다.** 둘 다 보관하려면 새 출력 폴더를 지정하세요. 전원 차단·강제 종료·다른 프로세스의 악의적 동시 변경까지 보장하는 파일시스템 트랜잭션은 아닙니다.

`--max-bytes`가 풀기·조회·`cat`·`test`의 비압축 데이터 한도를 정하며 기본값은 10 GiB입니다. TAR의 헤더·패딩·후행 데이터도 포함됩니다. CPU/메모리 샌드박스는 아닙니다. 아카이브 안의 링크·장치 객체는 거절하며 일부 macOS/탐색기 메타데이터는 제외합니다.

압축 생성·풀기·조회·검사·스트리밍에 더해 선택 멤버 추출(`--member`)과 `--flat`, 7z 분할 볼륨 생성, ZIP/7z `add`·`delete`·`rename`, ZIP SFX 생성을 지원합니다. RAR·TAR·스트림 편집, 분할 7z와 SFX 자체 편집, 손상 복구, RAR 생성, ALZ/EGG 형식은 지원하지 않습니다. 목록 창은 보기·풀기·검사만 하며 미리보기 창, 암호 입력창, 끌어다 놓기, 백그라운드 로딩은 없습니다.

빌드에는 Rust 1.93 이상과 Windows SDK가 필요합니다. 검증·패키징 명령과 자세한 계약은 [README](README.md)를 확인하세요.
