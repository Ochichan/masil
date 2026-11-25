# masil

[English](README.md) | **한국어**

여러 AI 코딩 에이전트를 한 터미널에서 나란히 실행하고 관리하는 tmux 호환 워크스페이스입니다. 원할 때만 켤 수 있는 모바일 앱을 통해 스마트폰에서도 진행 상황을 보고, 프롬프트를 보내고, 새 에이전트를 띄울 수 있습니다.

Claude Code, Codex, OpenCode, Antigravity CLI, Gemini CLI, Cursor 같은 터미널 AI 코딩 도구를 실제 터미널 패널에서 그대로 실행하면서 기존 tmux 단축키(`C-b`), `~/.tmux.conf`, 네이티브 속도를 유지합니다. 에이전트를 여러 개 동시에 돌려도 어느 세션이 작업 중이고, 입력을 기다리고, 끝났는지 한눈에 파악해 바로 이동할 수 있습니다.

## 왜 masil인가요?

터미널 AI 코딩 도구를 매일 쓰다 보면 금방 같은 불편을 겪게 됩니다.

- 저장소 여러 곳에 에이전트를 서너 개 띄워 두면 어느 탭이 작업 중인지, 승인 프롬프트에서 멈춰 있는지 놓치기 쉽습니다.
- 파일 변경 내역을 확인하거나, 잘못된 수정을 되돌리거나, 새 Git 워크트리에 에이전트를 추가할 때마다 흐름이 끊깁니다.
- 자리를 잠시 비우면 휴대폰에서 진행 상황을 보거나 다음 지시를 이어서 보내기가 번거롭습니다.

masil은 무거운 일렉트론 앱으로 터미널을 감싸거나 셸 앞단에 프록시를 두지 않고 이 문제를 해결합니다.

- **네이티브 tmux 코어 유지**: 터미널 멀티플렉서 코어(`core/`)가 C 기반 tmux에서 파생되어 PTY, 스크롤백, 렌더링, `~/.tmux.conf`가 원래 속도 그대로 동작합니다.
- **원본 CLI 그대로 실행**: `claude`, `codex`, `opencode`, `agy` 등 각 도구의 원본 TUI를 수정 없이 패널에서 실행하고 관찰합니다.
- **터미널 TUI + 선택형 모바일 앱**: 평소에는 키보드·마우스 TUI로 터미널 안에서 완결하고, 원격 접속이 필요할 때만 모바일 게이트웨이를 켤 수 있습니다.
- **근거 기반 상태 표시**: 프로세스 상태, 화면 시그널, 훅 이벤트 등 실제로 확인된 근거만 표시하며 승인이나 작업 완료를 임의로 추정하지 않습니다.
- **AI로 즉석 제작하는 로컬 플러그인**: Claude Code, Codex, Antigravity에 공식 `masil-plugin` 스킬을 설치해 테마, 패널, `/slash` 커맨드, 타임라인 카드, 훅, 프리셋을 대화로 만들고 TUI와 모바일에 실시간 반영합니다.

## 빠른 시작

### 1. 설치 (macOS / Linux)

**Homebrew (macOS / Linux):**

```sh
brew tap Ochichan/masil https://github.com/Ochichan/masil
brew install --HEAD Ochichan/masil/masil
```

업데이트는 `brew upgrade --fetch-HEAD Ochichan/masil/masil`로 합니다. 설치해도 기존 시스템 `tmux` 바이너리와 소켓에는 전혀 영향을 주지 않습니다.

**사전 빌드 아카이브 (macOS 15+ Apple Silicon, Ubuntu 24.04 x86_64):**

[GitHub Releases](https://github.com/Ochichan/masil/releases)에서 아카이브와 `SHA256SUMS`를 내려받습니다. 필요한 런타임 라이브러리:

- macOS: `brew install libevent ncurses utf8proc jemalloc`
- Ubuntu 24.04: `sudo apt-get install libevent-core-2.1-7t64 libncursesw6 libtinfo6 libutf8proc3 libjemalloc2`

**소스 빌드:**

```sh
make            # bin/masil, bin/masil-agent 빌드
make install    # ~/.local/bin 설치 (다른 경로는 PREFIX=/path 지정)
```

자세한 설치 및 업그레이드 안내는 [docs/public/install.md](docs/public/install.md)를 참고하세요. Windows는 호스트로 지원하지 않습니다.

### 2. AI 코딩 에이전트와 함께 쓰기

tmux처럼 바로 터미널 세션을 시작합니다.

```sh
masil
```

masil 안에서 에이전트를 띄우거나 평소처럼 직접 CLI를 실행하면 자동으로 감지됩니다.

```sh
masil-agent agent start backend claude --cwd ~/Projects/api
masil-agent agent start tests codex --cwd ~/Projects/api --split %0
masil-agent agent list
```

터미널에서 **에이전트 데스크(Agent Desk)** 또는 **사이드 패널**을 열어 전체 상태를 관리합니다.

```sh
masil-agent agent ui        # 전체 화면 에이전트 데스크 (마우스·키보드, 한국어/영어, 다크/라이트/터미널 테마)
masil-agent agent sidebar   # 34컬럼 상시 사이드 패널 (tmux 줌으로 확장)
```

에이전트 데스크에서 자주 쓰는 단축키:
- `j`/`k` 또는 방향키: 에이전트 이동
- `/`: 실시간 검색 (한국어 초성 검색 지원: `ㄱㅂ` 입력 시 `개발서버` 검색)
- `:`: 에이전트·패널·작업을 한 번에 찾는 커맨드 팔레트
- `g`: 선택한 에이전트의 실제 터미널 패널로 즉시 이동
- `l` / `t`: 한국어·영어 전환 / 다크·라이트·터미널 테마 전환

### 3. 선택 사항: 스마트폰 앱 연결

원격 기능은 사용자가 직접 켜기 전까지 백그라운드에서 실행되지 않습니다. 폰과 연결하려면 호스트에서 한 줄만 실행합니다.

```sh
masil-agent remote setup
```

터미널 설정 마법사가 4개 탭으로 연결을 안내합니다.
1. **프로바이더 훅**: Claude Code, Codex, OpenCode, Antigravity CLI용 마커 훅을 설치합니다(기존 사용자 훅은 그대로 보존).
2. **릴레이·데몬**: 직접 LAN/Tailscale 접속(기본 포트 `7767`), 선택형 릴레이 주소, 백그라운드 데몬·자동 시작 서비스를 설정합니다.
3. **QR 페어링**: 5분간 유효한 1회용 페어링 QR 코드를 터미널에 띄웁니다(`o` 키를 누르면 브라우저에서 선명한 SVG QR 코드를 열 수 있습니다).
4. **연결 기기**: 페어링된 기기 목록을 확인하고 언제든 접근 권한을 즉시 철회할 수 있습니다.

페어링 후 모바일 앱과 호스트 간 모든 통신은 Noise NK 종단간 암호화(E2EE)로 보호됩니다.

## 지원하는 AI 코딩 도구

masil은 Claude Code, Codex, OpenCode, Antigravity CLI(`agy`), Gemini CLI, Cursor, GitHub Copilot CLI, Amp, Kilo, OMP 등 **24개 코딩 에이전트 CLI**를 인식합니다.

- **데스크톱 + 모바일 완전 지원 (훅 연동, 실시간 대화 동기화, 모델·추론 강도·실행 모드 제어)**: Claude Code, Codex, OpenCode, Antigravity CLI(`agy`)
- **데스크톱 패널 감지, 상태 추적, 안전한 프롬프트 전송, 세션 재개**: 네이티브 세션 재개를 지원하는 18개 프로바이더 포함

내 머신에 설치된 프로바이더는 `masil-agent agent providers`로 확인할 수 있습니다. 자세한 지원 현황은 [docs/public/providers.md](docs/public/providers.md)를 참고하세요.

## 주요 기능

### 터미널 워크스페이스 & 에이전트 데스크
- **tmux 그대로 호환**: 기본 `C-b` 프리픽스와 `~/.tmux.conf`를 그대로 사용합니다. 소켓은 `masil-UID` 디렉터리에 분리되어 기존 `tmux`와 충돌하지 않으며, `tmux` 이름을 호출하는 스크립트용 `compat/tmux` 심도 제공합니다.
- **내장 마우스 UI 레이어**: 상단/하단 상태바 메뉴, `+` 새 창·패널 버튼, 플로팅 패널 드래그, 스크롤바, 세션 스냅샷(`prefix C-s` 저장, `prefix C-r` 복원, 15분 자동 저장)을 지원합니다. 순정 tmux 동작만 원하면 `masil -f /dev/null`로 시작할 수 있습니다.
- **중복 방지 프롬프트 전송과 대기**: `masil-agent agent prompt NAME '테스트 실행'`은 에이전트가 대기 중이고 같은 프로세스인지 확인한 뒤 SQLite에 기록하고 전송해 중복 재전송을 막습니다. 스크립트에서는 `masil-agent agent wait NAME --state idle --timeout 600`으로 대기할 수 있습니다.
- **인박스 & 알림**: `masil-agent agent inbox enable`과 `masil-agent agent notify enable`을 켜면 승인 요청, 질문, 입력 대기, 턴 완료 이벤트를 모아 보고 OS 알림으로 받을 수 있습니다.
- **Git 워크트리 & 파일 체크포인트**:
  - `masil-agent worktree create REPO BRANCH`로 작업별 격리된 Git 워크트리를 생성합니다.
  - `masil-agent agent changes NAME`으로 에이전트가 수정한 파일 목록을 확인합니다.
  - `masil-agent agent checkpoint NAME make`와 `checkpoint NAME restore`로 미리보기 후 개별 파일을 안전하게 복구할 수 있습니다.
- **워크스페이스 템플릿 & 컬럼 레이아웃**:
  - `~/.config/masil/layouts/NAME.toml` 템플릿으로 여러 패널과 에이전트를 한 번에 띄웁니다(`masil-agent layout apply NAME --yes`).
  - `masil-agent layout columns on`으로 고정 너비 가로 스크롤 컬럼 레이아웃을 사용할 수 있습니다.
- **멀티 머신 제어**: `masil-agent agent endpoints add`로 SSH 너머 다른 머신의 masil 서버까지 한 화면에서 조회·제어합니다.

### 모바일 동반 앱 (`mobile/`)
Expo / React Native 기반 Android·iOS 앱입니다.

- **대화 중심 화면**: 어시스턴트 답변과 인라인 마크다운 코드를 깔끔하게 보여 주며, 연속된 도구 호출은 가벼운 1줄 요약으로 접어 대화 흐름에 집중할 수 있습니다(탭하면 전체 인자·출력 확인 가능).
- **실시간 터미널 전환**: 직접 키 입력이 필요할 때는 상단 **터미널** 탭으로 바로 전환해 라이브 패널을 조작할 수 있습니다.
- **GUI 에이전트 실행기**: 호스트의 프로젝트 폴더를 GUI 탐색기와 최근 저장소 칩으로 고르고, CLI·검색형 모델 드롭다운·추론 강도·실행 모드·새 Git 워크트리 브랜치 옵션을 지정해 바로 에이전트를 띄웁니다.
- **실행 중 세션 옵션 변경**: 이미 돌아가는 에이전트의 모델, 추론 강도, 실행 모드를 옵션 시트에서 바로 전환합니다.
- **워크스페이스 도구**: 실시간 Git Diff, 프로젝트 파일 탐색, 체크포인트·예약 확인, 음성 입력, 여러 호스트 전환을 지원합니다.
- **실시간 플러그인 & Creator Mode**: 입력창 `/slash` 커맨드 자동완성, 워크스페이스 패널 탭, 대화 타임라인 카드, TUI와 동기화되는 플러그인 테마, 실행 프리셋을 지원하며 **설정 -> 플러그인 -> AI로 플러그인 만들기**로 즉석 제작 세션을 바로 실행할 수 있습니다.

#### 모바일 앱 로컬 빌드

```sh
cd mobile
npm ci
npm run build:apk:arm64   # Android arm64 APK -> mobile/artifacts/masil-production-arm64.apk
npm run build:ios:local   # 로컬 iOS 빌드 (Xcode 필요)
```

### 셀프 호스팅 E2EE 릴레이 (`relay/`)

같은 Wi-Fi(LAN)나 Tailscale 환경에서는 별도 서버 없이 폰과 호스트가 직접 연결됩니다. 외부 네트워크에서 Tailscale 없이 접속하려면 작은 VPS에 `masil-relay`를 띄워 사용할 수 있습니다.

```sh
make relay                # bin/masil-relay 빌드
# 또는 Docker 사용:
cd relay && docker compose -f docker-compose.example.yml up -d
```

이후 `masil-agent remote pair --relay wss://your-relay.example.com`으로 페어링하면 됩니다. 릴레이 서버는 Noise NK로 암호화된 패킷만 중계하므로 프롬프트, 코드, 터미널 내용을 볼 수 없습니다.

## 로컬 플러그인 (Claude Code · Codex · Antigravity 즉석 제작)

바이너리나 모바일 APK를 다시 빌드하지 않고 Rust TUI와 모바일 앱 양쪽을 확장할 수 있습니다. 모든 플러그인은 `~/.config/masil/plugins/<id>/`(또는 경로로 등록한 로컬 폴더)에 위치하며 변경 사항이 연결된 클라이언트에 실시간 반영됩니다.

플러그인 하나(`masil-plugin.toml`)에 아래 6가지 기능을 자유롭게 조합할 수 있습니다.
- **테마 (`theme.toml`)**: Omarchy 스타일의 색상 파일만으로 코드 없이 TUI와 모바일 테마를 동시에 전환합니다.
- **워크스페이스 패널**: Masil UI Spec v1 선언형 블록(`metric_row`, `table`, `list`, `markdown`, `code`, `diff`, `progress`, `sparkline`, `callout`, `button`, `toggle`, `select`, `form`)을 반환하면 모바일 워크스페이스 도구 탭과 TUI 명령 팔레트(`:`)에서 네이티브로 렌더링됩니다.
- **슬래시 커맨드**: 모바일 입력창 `/` 자동완성과 TUI 명령 팔레트에 등록되며 토스트, UI 시트, 입력창 초안, 타임라인 카드를 반환할 수 있습니다.
- **타임라인 카드**: 구조화된 UI 카드를 에이전트 대화 타임라인에 영속적으로 표시합니다.
- **라이프사이클 훅**: `agent.state_changed`, `agent.started`, `agent.closed`, `inbox.event`, `theme.changed`, `prompt.before` 이벤트에 반응합니다.
- **에이전트 실행 프리셋**: 자주 쓰는 프로바이더, 모델, 추론 강도, 모드, 워크트리, 초기 프롬프트 조합을 모바일 실행기 상단 원탭 칩으로 추가합니다.

### 공식 `masil-plugin` AI 스킬 설치

내장 스킬을 한 번 설치해 두면 Claude Code, Codex, Antigravity가 최신 스키마, UI 명세, 검증 루프를 정확히 알고 플러그인을 제작합니다.

```sh
masil-agent plugin skill install --target all
masil-agent plugin skill status
```

설치 경로:
- Claude Code: `~/.claude/skills/masil-plugin/`
- Codex: `~/.codex/skills/masil-plugin/`, `~/.agents/skills/masil-plugin/`
- Antigravity (`agy`): `~/.gemini/config/skills/masil-plugin/`

이후 Claude Code, Codex, Antigravity 아무 세션에서나 바로 요청하면 됩니다.
- *"masil용 Tokyo Night 다크 테마 플러그인 만들어서 바로 적용해줘."*
- *"현재 저장소 TODO 체크리스트를 관리하는 masil 패널과 `/todo` 슬래시 커맨드 만들어줘."*
- *"에이전트가 blocked 상태가 되면 대화 타임라인에 알림 카드를 남기는 masil 훅 플러그인 만들어줘."*

에이전트가 `masil-agent plugin schema` 확인 -> `init` 생성 -> `check`·`test` 검증 -> `reload` 반영까지 자동으로 처리해 터미널과 휴대폰에 즉시 나타납니다.

### 기본 제공 예제 플러그인 체험

```sh
masil-agent plugin install examples/plugins/catppuccin-mocha
masil-agent plugin install examples/plugins/repo-pulse
masil-agent plugin theme set catppuccin-mocha
masil-agent plugin ls
```

모바일 앱(**설정 -> 플러그인 -> AI로 플러그인 만들기** 또는 워크스페이스 도구 상단 버튼)이나 TUI 명령 팔레트(`:` -> **AI로 Masil 플러그인 만들기**)에서도 **Creator Mode**를 바로 시작할 수 있습니다. 플러그인의 FD 3 Masil API 호출은 선언된 `scope = read|act|admin` 범위로 제한되며 승인 프롬프트는 항상 사람만 응답할 수 있습니다. 전체 명세는 [docs/public/plugins.md](docs/public/plugins.md)를 참고하세요.

## 아키텍처와 프라이버시

- **터미널 입출력 경로에 프록시 없음**: C 기반 코어(`core/`, tmux `next-3.9` 포크)가 PTY, VT 파싱, 스크롤백, 렌더링을 전담합니다. Rust 바이너리(`masil-agent`)는 에이전트 감지, TUI, 작업 기록, 선택형 원격 게이트웨이를 담당합니다.
- **기본 백그라운드 데몬 없음**: 일반 터미널 사용 시 원격 데몬이 뜨지 않으며 Node.js 런타임도 필요하지 않습니다.
- **로컬 진단**: `masil-agent doctor` (`--versions`, `--bundle DIR`)로 네트워크 연결 없이 로컬 설치·서버·프로바이더 상태를 JSON으로 점검할 수 있습니다.

더 자세한 문서는 아래를 참고하세요.
- [설치 및 업데이트 가이드](docs/public/install.md)
- [로컬 플러그인 및 AI 스킬 가이드](docs/public/plugins.md)
- [프로바이더 지원 및 설정](docs/public/providers.md)
- [아키텍처 및 호환성 경계](docs/public/architecture.md)
- [검증 및 벤치마크](docs/public/validation.md) / [성능 측정 기록](docs/public/performance-2026-11-18.md)

## 개발 및 검증 명령어

```sh
make all                                                # bin/masil, bin/masil-agent 빌드
cargo fmt --manifest-path agent/Cargo.toml --check
cargo clippy --locked --manifest-path agent/Cargo.toml --all-targets -- -D warnings
cargo test --locked --manifest-path agent/Cargo.toml
make test-ci                                            # 코어 회귀 테스트
make test-relay                                         # wire·relay Rust 테스트
PYTHONPATH=tests python3 -m unittest -v tests/test_plugins_e2e.py  # 로컬 플러그인 CLI·스킬 E2E
cd mobile && npm run typecheck && npm test              # 모바일 앱 타입체크·테스트
```

## 라이선스

masil은 MIT 라이선스를 따릅니다. `core/`는 tmux에서 파생되어 ISC 라이선스를 유지하며, 모바일 앱에 포함된 폰트(`Pretendard`, `Cascadia Mono`)는 SIL Open Font License 1.1을 따릅니다. 서드파티 고지는 [NOTICE](NOTICE)를 참고하세요.
