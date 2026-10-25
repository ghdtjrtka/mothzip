# 압축 파일 형식 아이콘

기존 나방집 마크 아래에 압축 형식 라벨을 붙인 파일 아이콘이다. 앱 자체 아이콘은 기존 마크를 유지한다.

![파일 아이콘 미리보기](../assets/file-icons/preview.png)

지원 형식 15개와 일반 아카이브 아이콘 1개를 실행 파일에 내장한다. 각 ICO는 16/20/24/32/40/48/64/96/128/256px 이미지를 포함한다. 작은 크기에서는 픽셀 문자로 라벨을 표시하고, 큰 복합 형식 아이콘은 TAR와 코덱을 두 줄로 표시한다.

## 탐색기에 표시하기

1. Windows **설정 → 앱 → 기본 앱**을 연다.
2. **Mothzip**을 찾는다.
3. 사용할 압축 확장자의 기본 앱을 Mothzip으로 선택한다.

파일 연결에 `Bandizip.zip` 같은 이름이 남아 있어도 프로그램이 설치돼 있다는 뜻은 아니다. 현재 PC에서는 반디집 실행 파일과 설치 등록 항목이 없음을 확인했다. 이 설정은 보호된 UserChoice를 직접 고치거나 다른 프로그램의 ProgID를 덮어쓰지 않고, 정상 Windows UI에서 바꾼다.

복합 파일 이름은 Windows가 마지막 확장자의 연결을 사용할 수 있다. 예를 들어 `.tar.zst`는 ZST 아이콘으로 보일 수 있다. 이를 파일마다 동적으로 구분하는 별도 셸 확장은 추가하지 않았다.

## 재생성 및 구현

```powershell
.\tools\generate-file-icons.ps1
```

- 형식·리소스 기준: `src/core/file_types.rs`.
- 원본 마크: `assets/moth_icon.png`.
- ICO·PNG·SVG 및 미리보기: `assets/file-icons/`.
- `build.rs`가 리소스를 내장한다. 파일 형식별 ProgID와 DefaultIcon은 공통 registry profile에서 만든다.
- 아이콘 리소스 번호를 음수로 지정해 그룹의 위치가 아니라 ID로 참조한다. 이전 공통 ProgID에는 일반 ARC 아이콘을 유지한다.
- OpenWithProgids는 다른 앱과 공유하므로 등록/제거 시 Mothzip 값만 관리한다. 등록이 끝나면 Explorer의 연결 캐시를 갱신한다.

Windows 파일 형식 연결은 [Microsoft의 ProgID 문서](https://learn.microsoft.com/en-us/windows/win32/shell/fa-progids)를 따른다.

## 이번 검증과 설치

- **[measured]** 단위 검사 125개, clippy 경고 0.
- **[measured]** 회귀 검사 103사례·161회 호출 PASS.
- **[measured]** 16개 아이콘 그룹·160개 이미지가 ICO 원본과 일치. Windows API로 16/32/256px 아이콘을 48회 추출해 확인.
- **[measured]** Mothzip 1.2.3 설치, 파일 해시와 등록 검증 완료.
- **[unverified]** 현재 세션에서 기본 앱 선택을 자동 완료하지 못했다. Windows 설정 창 접근 오류로 위 사용자 선택 단계가 남아 있다.

설치 및 패키지 바이너리 SHA-256:
`fe43616dd5bd2abb81b8fa93306b02799ce7a0e78c229c3cc822a31ab161ffb9`

증거:

- `audit/runs/file-icons-20261026-044331-64ebc8/results.json` — 실제 설치본의 리소스 검증.
- `audit/runs/20261026-043411-9ebc4d/release-validation.json` — 회귀 결과.
- `target/icon-tests-normal.log`, `target/icon-clippy.log`, `target/icon-install-verified.json`.
