# 에이전트 관리 화면

기존 tmux 키는 그대로 사용한다. 에이전트 관리 화면은 별도 Rust 프로세스로 실행하며 기본 prefix나 키 테이블을 추가하지 않는다. 사이드패널은 직접 열었을 때만 생긴다.

먼저 [관찰 daemon 실행 안내](agent-observation.md)에 따라 core bridge와 agentd를 시작한다. 관리 화면은 daemon이나 provider를 자동으로 시작하지 않는다. 현재 관찰 대상은 config에 지정한 OpenCode session이다.

## 열기

```sh
# 별도 터미널 전체를 사용하는 관리 화면
./bin/masil-agent --socket "$masil_runtime/manager.sock" ui

# 기존 masil에 연결된 클라이언트가 하나일 때 사이드패널 열기
./bin/masil-agent --socket "$masil_runtime/manager.sock" sidebar \
  --core-native "$masil_runtime/core.sock"

# 한국어, 밝은 테마로 시작
./bin/masil-agent --socket "$masil_runtime/manager.sock" ui \
  --core-native "$masil_runtime/core.sock" --lang ko --theme light
```

`--socket`은 agentd 관리 socket, `--core-native`는 masil의 기본 tmux protocol socket이다. 관찰용 `observe.sock`을 `--core-native`에 넣지 않는다. `masil-agent`와 `masil` 실행 파일은 같은 디렉터리에 둔다.

사이드패널은 34열이며 창 너비가 100열 이상이어야 한다. 기존 활성 pane을 유지하므로 `C-b`와 방향키 또는 마우스로 패널에 들어간다. 같은 창에서 다시 열면 기존 패널을 재사용한다. `z` 또는 Expand를 누르면 tmux 기본 pane zoom으로 전체 관리 화면이 된다. 다시 누르면 분할을 복원한다. 확대 상태에서 Go to pane을 누르면 분할을 복원한 뒤 선택한 pane으로 이동한다.

사이드패널은 tmux 창의 일부이므로 공유 레이아웃에 들어간다. 다른 클라이언트가 함께 보는 창·세션을 변경하는 동작, 읽기 전용 클라이언트, 모달이 열린 창은 거절한다. 연결된 클라이언트가 여러 개면 `--client /dev/ttysNNN`으로 지정할 수 있지만 공유 화면 보호는 유지한다. `sidebar --target %N`으로 생성할 창의 pane을 지정할 수 있다. 기존 줌은 먼저 `C-b z`로 복원한다.

`q`로 관리 화면을 종료한다. 사이드패널에서는 UI 프로세스와 그 pane이 끝나고 나머지 pane은 유지된다. 사용자의 remain-on-exit 설정은 새 사이드패널에만 off로 덮어쓴다. 다른 프로그램으로 respawn한 과거 사이드패널은 재사용하거나 닫지 않는다.

## 조작

| 목적 | 키보드 | 마우스 |
| --- | --- | --- |
| 에이전트 선택 | 방향키, j/k, Home/End, PageUp/PageDown | 행 클릭 |
| 상세 보기 | Enter | Details |
| 설정된 pane으로 이동 | g | Go to pane, 행 더블클릭 |
| 요청 확인 표시 | a | Mark seen |
| 검색 | /, 편집 중 Ctrl-U로 지우기 | 검색창, Clear |
| 필터 | Tab으로 필터 이동, 좌우·Enter | 필터 클릭 |
| 작업 메뉴 | Shift-F10 | 행 오른쪽 클릭 |
| 스크롤 | 해당 영역 포커스에서 방향키·페이지 키 | 해당 영역 위 휠, 스크롤바 드래그 |
| 목록 너비 | 기본 비율 사용 | 넓은 화면의 가운데 구분선 드래그 |
| 사이드패널 확대·복원 | z | Expand / Restore layout |
| ID를 tmux 버퍼에 복사 | c | 작업 메뉴 Copy ID |
| 영어·한국어 | l | 상단 언어 버튼 |
| 테마 | t | 상단 테마 버튼 |
| 도움말 | ? | Help |
| 돌아가기·닫기 | Esc, q | 메뉴 밖 클릭, Close |

Tab과 Shift-Tab으로 조작 영역을 이동한다. 메뉴·도움말을 닫는 클릭은 뒤쪽 화면으로 전달하지 않는다. 버튼은 누른 대상과 놓은 대상이 같을 때만 실행한다. 선택된 요청이 바뀌면 이전 클릭은 취소한다. 검색은 Unicode 글자 단위로 편집하며 bracketed paste는 문자열로만 처리한다.

마우스 캡처 중 호스트 터미널에서 텍스트를 선택하려면 해당 터미널의 선택용 보조키가 필요할 수 있다. 특정 보조키가 모든 터미널에서 같다고 가정하지 않는다. Copy ID는 `masil-agent-id` tmux 버퍼에 기록한다. 시스템 클립보드 복사 성공을 주장하지 않는다.

## 상태 읽기

Mark seen은 현재 요청 묶음을 확인했다는 표시다. provider 권한을 승인하지 않으며 새 요청까지 함께 확인하지 않는다. 확인 상태는 같은 agentd를 보는 클라이언트들이 공유하고 daemon 재시작 때 초기화된다.

Working은 native provider가 보고한 활동 상태다. Idle은 완료가 아니다. 설정된 pane과 native session의 연결은 아직 포그라운드 TUI identity를 검증하지 못한다. UI는 그 한계를 표시하고 코어 boot와 PTY generation을 비교한 후에만 pane 이동을 실행한다.

관찰 연결이 끊기면 마지막 행을 남기고 오래된 근거로 표시한다. 확인·이동은 비활성화하고 읽기 전용 재연결을 시도한다. Retry로 즉시 재시도할 수 있다. 실행 중인 에이전트 프로세스에는 입력하지 않는다.

100열·24행 이상이면 목록과 상세가 함께 보인다. 80열에서는 Enter로 상세를 연다. 28열·9행 미만이면 최소 크기 안내와 닫기만 표시한다. 언어·테마는 `$XDG_CONFIG_HOME/masil/ui.json` 또는 `~/.config/masil/ui.json`에 저장한다. CLI 옵션은 시작 화면에만 적용한다. 읽을 수 없거나 지원하지 않는 설정 파일은 덮어쓰지 않고 현재 세션에서만 변경한다.

관찰 화면은 에이전트 프로세스에 입력하지 않는다. 실제 TUI 실행·분할·복사 모드는 기존 tmux 경로를 사용한다. 설계와 테스트 범위는 [상호작용 계약](ui/agent-desk.md)과 [검증 기록](validation/2026-09-28-agent-desk.md)에 정리한다.

## 네이티브 에이전트 관리

daemon 설정 없이 현재 masil 서버의 에이전트를 관리하려면 다음 명령을 사용한다. `--socket`을 생략하면 현재 `TMUX` 환경에서 서버 socket을 찾는다.

```sh
masil-agent agent ui
masil-agent agent sidebar
masil-agent agent --socket /path/to/masil.sock ui --lang ko --theme dark
```

`ui`는 현재 터미널에서 전체 관리 화면을 열고, masil 메뉴의 **Agents**는 일반 새 창에서 같은 화면을 연다. `sidebar`는 현재 창에 34열짜리 명시적 네이티브 분할을 만든다. 같은 서버와 창에 이미 열린 관리 사이드패널이 있으면 선택해 재사용하며 중복으로 만들지 않는다. 기존 창이 확대된 상태에서는 레이아웃을 먼저 복원하도록 요청한다. 사이드패널의 Expand/Restore는 소유권 표식을 다시 확인한 뒤 네이티브 pane zoom을 전환한다.

관리 화면은 열려 있는 동안에만 1초 간격으로 네이티브 pane 목록을 읽는다. 조회와 작업은 입력 처리와 별도 task에서 실행되며 동일한 결과가 반복될 때 화면을 다시 그리지 않는다. 연결 실패, 빈 목록, 검색 결과 없음과 좁은 사이드패널 상태를 각각 표시한다.

아래 작업은 하단 작업줄, `Shift-F10` 또는 행 오른쪽 클릭의 **Agent actions** 메뉴에서 찾을 수 있다. 메뉴 항목과 동일한 키가 동작하며 버튼과 확인 대화상자는 마우스로도 사용할 수 있다.

| 작업 | 키 | 의미 |
| --- | --- | --- |
| 새 에이전트 | `n` | 이름, provider, 작업 디렉터리를 받아 새 창에서 프로세스를 시작한다. |
| pane으로 이동 | `g` | 표시된 core boot와 PTY 세대가 일치할 때 선택한다. |
| 이름 변경 | `r` | 표시된 실행 세대에 묶인 관리 이름을 바꾼다. |
| 네이티브 세션 재개 | `s` | 보고된 session ref가 있을 때 새 이름으로 새 pane을 시작한다. 세션 수락 여부는 확인되지 않은 상태로 표시한다. |
| 초안 준비 | `d` | 텍스트를 `masil-agent-draft` tmux buffer에만 준비한다. 붙여넣거나 전송하지 않는다. |
| 인터럽트 | `x` 또는 `Ctrl-C` | 확인 후 검증된 포그라운드 프로세스에 `C-c`를 전달한다. provider의 처리 여부는 주장하지 않는다. |
| 화면 읽기 | `v` | 최근 화면 내용을 상세 inspector에 추가한다. |
| pane 닫기 | `X` | 확인 후 선택한 pane과 프로세스를 닫는다. |

`blocked`는 **Needs input**으로 표시한다. 화면 근거가 구체적인 질문이나 승인 요청 종류를 증명하지 않으므로 Question 또는 Approval로 바꾸지 않는다. 실행 중이던 같은 run이 idle로 바뀌면 **Returned idle**로 표시하고 공유된 revision을 확인할 수 있지만, 이것도 작업 성공을 뜻하지 않는다. Mark seen은 공유된 현재 요청 revision만 확인한 것으로 기록한다. 새 프로세스 시작 또는 세션 재개 결과도 provider 수락이나 네이티브 세션 검증을 주장하지 않는다.
