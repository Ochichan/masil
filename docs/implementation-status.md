# 현재 코어 구현

2026-09-28. tmux의 고정 소스 전체를 실행 가능한 masil 코어로 도입했다. PTY, VT parser, grid/history, renderer, 키 테이블, command queue, control mode를 유지하고 제품 socket 격리와 읽기 전용 관찰 IPC를 추가했다. 원본 C 소스와 고지는 [core](../core/README), 가져온 파일별 해시는 [UPSTREAM.json](../core/UPSTREAM.json)에 있다.

전체 masil 제품의 완료 상태는 아니다. OpenCode native session의 읽기 전용 관찰과 agentd에 더해, 2026-09-29에 native Agent 관리 CLI·사이드패널을 추가했다. 24종 provider 식별, 원래 TUI 실행, 상태·확인·키 전달·draft 준비·검사 후 prompt 제출, native session 재개와 항목별 snapshot 복구를 제공한다. [Agent 관리의 구현 범위와 남은 차이](managed-agents.md)를 참고한다. verified native TUI binding, durable prompt 수락, provider 승인 응답, SQLite, worktree job은 미구현이다. 명시적으로 등록한 원격/로컬 endpoint의 Agent 통합 목록·조작·native 연결은 제공한다.

## 빌드와 실행

현재 검증 환경은 macOS arm64와 Debian 13.7 x86_64다. C compiler, make, Python 3.12 이상, pkg-config, libevent, ncurses/terminfo, utf8proc, jemalloc이 필요하다. Rust CLI를 빌드할 때만 Cargo가 필요하다. macOS에서는 기존 Homebrew library를 사용했고, Debian에서는 개발 패키지를 private sysroot에 풀어 빌드했다. [Debian 빌드·실제 SSH 검증](validation/2026-09-28-debian-ssh.md)에 환경과 재현 명령을 기록했다.

```sh
make core
./bin/masil -L main new-session -s work
```

기본 키는 고정 tmux 그대로다. `C-b %`로 좌우 분할, `C-b "`로 상하 분할, `C-b c`로 window 생성, `C-b d`로 detach한다. 다시 연결할 때는 `./bin/masil -L main attach-session -t work`를 사용한다. 각 pane에서 `claude`, `codex`, `opencode` 등 설치된 TUI를 직접 실행할 수 있다. 이 CLI들이 준비됐는지, 작업을 완료했는지는 현재 masil이 판정하지 않는다.

기본 config 탐색은 upstream의 `.tmux.conf` 경로를 유지한다. 사용자 설정 없이 확인하려면 새 서버를 시작할 때 `-f /dev/null`을 쓴다. 이미 시작한 서버의 config를 바꾸는 옵션은 아니다.

```sh
make                 # C 코어와 Rust 관찰 CLI
make baseline        # 같은 source와 compiler 옵션의 stock 비교용 실행 파일
make test            # 호환·IPC·실패 주입·측정 도구·Rust 검사
```

`make baseline`은 `~/Documents/git_clones/tmux`의 고정 commit을 읽는다. 다른 clone은 `MASIL_TMUX_SOURCE`로 지정한다. clone은 수정하지 않는다. m4/autoconf/automake가 없으면 SHA-256을 확인한 공식 archive를 `.build` 아래에서 빌드한다. 시스템 패키지나 실행 파일은 설치·교체하지 않는다. 빌드 로그와 compiler/flag/binary 해시는 `.build/logs/`, `.build/core/build.json`, `.build/baseline/build.json`에 남는다.

## 호환 진입점과 격리

| 항목 | 실제 구현 |
| --- | --- |
| 전체 terminal source | tmux `94796f6b1182507efac8a272fc309a79e22e58a5`, `next-3.9` |
| 기본 command/key/option | 같은 build의 stock dump와 비교 통과. 92개 등록 명령 유지 |
| prefix | `C-b`, 추가 기본 binding 없음 |
| mouse | 이 commit의 빌드 기본값인 `on` 유지 |
| masil UI 레이어 | `-f` 없이 시작하면 사용자 설정보다 먼저 적용. `+`·설정·masil 메뉴 버튼, pane 제목줄, 메뉴 유지, 테마. floating 묶음, 제목줄 드래그, 세션 저장·불러오기(prefix `C-s`·`C-r`, 15분 자동 저장). 기본 binding이 없는 키만 사용. [마우스 UI](mouse-ui.md) |
| `status-position` | `top`, `bottom`에 masil 확장 `left`, `right`(세로 상태줄) 추가. 폭은 `@masil-status-width` |
| 설정 화면 | `masil-agent settings`. 선택은 `~/.config/masil/settings.conf`의 `@masil-*` 옵션 |
| default / `-L` socket | `masil-UID` directory. stock의 `tmux-UID`와 분리 |
| `-S` | 사용자가 지정한 경로를 사용 |
| native protocol | 8-bit version 136. stock과 양방향 접속 거절 |
| `TMUX` / `TMUX_PANE` | upstream 의미 유지. 같은 masil 서버 상속은 동작하고 stock 서버 상속은 거절 |
| `-V` | 기존 script의 version 파싱을 위해 `tmux next-3.9` 유지. masil provenance는 build manifest에서 확인 |
| `tmux` 하드코딩 script | 선택적 [compat/tmux](../compat/tmux) shim 제공. 시스템 tmux 교체 없음 |

shim을 쓰려면 필요한 shell에만 `PATH="$PWD/compat:$PATH"`를 설정한다. 절대 경로로 stock tmux를 호출하는 plugin은 자체 설정 변경이 필요하다. native protocol 호환과 CLI/control-mode 의미 호환은 별도다.

## 선택적 관찰 기능

기본 실행에서는 Rust process, 관찰 socket, 관찰용 주기 timer를 만들지 않는다. core의 native tmux 기능은 독립적으로 동작한다. 관찰 기능은 **서버를 시작할 때** `MASIL_BRIDGE_SOCKET`을 지정해야 켜진다. 기존 서버에 붙는 client의 환경을 바꾸는 것으로 켜지지 않는다.

```sh
make agent
masil_runtime=$(mktemp -d /tmp/masil-observe.XXXXXX)
MASIL_BRIDGE_SOCKET="$masil_runtime/observe.sock" \
  ./bin/masil -S "$masil_runtime/core.sock" -f /dev/null \
  new-session -d -s demo
./bin/masil-agent --socket "$masil_runtime/observe.sock" hello
./bin/masil-agent --socket "$masil_runtime/observe.sock" inventory
./bin/masil-agent --socket "$masil_runtime/observe.sock" snapshot %0
./bin/masil-agent --socket "$masil_runtime/observe.sock" stats
./bin/masil -S "$masil_runtime/core.sock" kill-server
```

parent directory는 현재 UID 소유이고 group/other 접근이 없어야 한다. socket은 0600, 연결자는 같은 UID로 제한한다. 관찰 protocol은 [구현 계약](core-observation.md)에 정리했다. 읽기 전용으로 제공하므로 prompt 전송이나 승인 성공을 반환하는 경로는 없다.

서버가 실행 중일 때 별도 terminal에서 `./bin/masil-agent --socket PATH watch %0`으로 실시간 변경 알림을 받을 수 있다. 연결당 최대 64개 pane, 전체 512개 관찰 대상을 허용한다. 변경 없는 pane은 polling하지 않는다. 느린 구독자가 이력을 놓치면 gap/EOF로 알리고 새 baseline이 필요하다. 사용법·상한은 [관찰 계약](core-observation.md), 실행 결과는 [M2 검증](validation/2026-09-28-watch.md)에 있다.

## 검증 상태와 다음 작업

`masil-agent ui`와 `sidebar`는 선택적인 터미널 관리 화면이다. 기존 tmux 기본 키를 바꾸지 않고 에이전트 목록·필터·검색·상세·확인 표시·guarded pane 이동을 제공한다. [실행과 조작 안내](agent-desk.md), [상호작용 설계](ui/agent-desk.md), [UI 검증 기록](validation/2026-09-28-agent-desk.md)을 참고한다.

OpenCode의 native 상태를 관찰하려면 [agentd 실행 안내](agent-observation.md)를 따른다. `serve`는 별도 foreground 프로세스이며 `status`, `agents`, `inspect`, `stop`으로 조회·종료한다. 상태와 pane의 연결은 `explicit_unverified`로 표시하고 TUI의 현재 session이라고 단정하지 않는다. [검증 기록](validation/2026-09-28-agentd.md)에 통합 결과와 idle 비용을 기록한다.

- [x] 전체 고정 terminal source, provenance, local build.
- [x] socket/protocol 격리와 tmux 기본 공개 동작 비교.
- [x] bounded read-only inventory/snapshot/stats와 Rust 관찰 CLI.
- [x] boot/PTY/screen generation, 출력 없는 종료와 실패한 respawn 검증.
- [x] macOS의 terminal 호환·IPC 통합 테스트와 upstream suite 실행.
- [x] 설치된 Herdr 0.8.2와 stock baseline의 1·15·50-pane 실제 비교 및 원시 결과 보존.
- [ ] upstream 메뉴 redraw golden fixture 공통 실패의 원인 확정.
- [x] Debian x86_64 빌드·프로젝트 검사와 실제 SSH PTY의 관리 화면·마우스·한국어 붙여넣기·native sidebar 검증.
- [ ] BSD, 실제 OS IME·clipboard·중첩 terminal, SSH 끊김·재접속을 포함한 환경 행렬 검증.
- [x] 선택 scope의 watch/event/gap, dirty 병합, 느린 구독자 정리, Rust streaming CLI.
- [x] foreground agentd, OpenCode GET/SSE 상태·승인·질문 대기 관찰, source/core freshness 분리.
- [x] shared attention 목록과 revision별 메모리 확인, bounded live snapshot CLI. [사용법](attention.md).
- [ ] verified TUI binding, provider 확대, native menu/focus와 durable 관리 계층.

upstream suite는 최초 전체 실행과 실패 항목 재검증을 합쳐 두 제품 각각 163/164 통과했다. `screen-redraw-menus.sh`는 stock에서도 같은 fixture 차이가 남는다. 정규화·재검증 조건과 원시 결과는 [검증 기록](validation/2026-09-28-core.md)에 있다. 전체 플랫폼의 완전 호환이나 출시 gate 통과를 주장하지 않는다.

설치된 Herdr 0.8.2 및 같은 소스의 stock tmux와 비교하는 [성능 보고서](benchmarks/2026-09-28.md)는 별도로 관리한다. agentd가 없는 terminal core 비용이며, Herdr의 완성된 관리 기능과 동일한 제품 범위를 비교한 것은 아니다.
