# 에이전트 관리 화면

기존 tmux 키는 그대로 사용한다. 에이전트 관리 화면은 별도 Rust 프로세스로 실행하며 기본 prefix나 키 테이블을 추가하지 않는다. 사이드패널은 직접 열었을 때만 생긴다.

먼저 [관찰 daemon 실행 안내](agent-observation.md)에 따라 core bridge와 agentd를 시작한다. 관리 화면은 daemon이나 provider를 자동으로 시작하지 않는다. 현재 관찰 대상은 config에 지정한 OpenCode session이다.

## 열기

```sh
# 별도 터미널 전체를 사용하는 관리 화면
./bin/rmux-agent --socket "$rmux_runtime/manager.sock" ui

# 기존 rmux에 연결된 클라이언트가 하나일 때 사이드패널 열기
./bin/rmux-agent --socket "$rmux_runtime/manager.sock" sidebar \
  --core-native "$rmux_runtime/core.sock"

# 한국어, 밝은 테마로 시작
./bin/rmux-agent --socket "$rmux_runtime/manager.sock" ui \
  --core-native "$rmux_runtime/core.sock" --lang ko --theme light
```

`--socket`은 agentd 관리 socket, `--core-native`는 rmux의 기본 tmux protocol socket이다. 관찰용 `observe.sock`을 `--core-native`에 넣지 않는다. `rmux-agent`와 `rmux` 실행 파일은 같은 디렉터리에 둔다.

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

마우스 캡처 중 호스트 터미널에서 텍스트를 선택하려면 해당 터미널의 선택용 보조키가 필요할 수 있다. 특정 보조키가 모든 터미널에서 같다고 가정하지 않는다. Copy ID는 `rmux-agent-id` tmux 버퍼에 기록한다. 시스템 클립보드 복사 성공을 주장하지 않는다.

## 상태 읽기

Mark seen은 현재 요청 묶음을 확인했다는 표시다. provider 권한을 승인하지 않으며 새 요청까지 함께 확인하지 않는다. 확인 상태는 같은 agentd를 보는 클라이언트들이 공유하고 daemon 재시작 때 초기화된다.

Working은 native provider가 보고한 활동 상태다. Idle은 완료가 아니다. 설정된 pane과 native session의 연결은 아직 포그라운드 TUI identity를 검증하지 못한다. UI는 그 한계를 표시하고 코어 boot와 PTY generation을 비교한 후에만 pane 이동을 실행한다.

관찰 연결이 끊기면 마지막 행을 남기고 오래된 근거로 표시한다. 확인·이동은 비활성화하고 읽기 전용 재연결을 시도한다. Retry로 즉시 재시도할 수 있다. 실행 중인 에이전트 프로세스에는 입력하지 않는다.

100열·24행 이상이면 목록과 상세가 함께 보인다. 80열에서는 Enter로 상세를 연다. 28열·9행 미만이면 최소 크기 안내와 닫기만 표시한다. 언어·테마는 `$XDG_CONFIG_HOME/rmux/ui.json` 또는 `~/.config/rmux/ui.json`에 저장한다. CLI 옵션은 시작 화면에만 적용한다. 읽을 수 없거나 지원하지 않는 설정 파일은 덮어쓰지 않고 현재 세션에서만 변경한다.

현재 UI에는 에이전트 생성, 프롬프트 전송, 권한 응답, 대화 복구가 없다. 실제 TUI 실행·분할·복사 모드는 기존 tmux 경로를 사용한다. 설계와 테스트 범위는 [상호작용 계약](ui/agent-desk.md)과 [검증 기록](validation/2026-09-28-agent-desk.md)에 정리한다.
