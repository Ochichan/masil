# 마우스 UI 레이어, 설정 화면, 세로 상태줄

상태: 구현됨. 2026-09-28 사용자 결정과 계획 검토를 반영했다. 사용법은 [마우스 UI와 설정 화면](../mouse-ui.md)에 있다.

## 사용자 결정

- Herdr처럼 처음 실행부터 마우스로 pane·창·세션을 만들고 조작한다. rmux UI 레이어는 기본으로 켠다.
- tmux 옵션·키 테이블의 기본값 자체는 바꾸지 않는다. 레이어는 설정 계층이며 `rmux -f FILE`, `~/.tmux.conf`, 설정 화면이 이를 끄거나 덮어쓴다.
- 상태줄 위치는 위·아래에 더해 왼쪽·오른쪽 사이드바를 지원한다. 사이드바는 C 코어의 세로 상태줄이다.
- 각종 설정은 TUI 설정 화면에서 바꾼다.

## Herdr 0.8.2 대조

로컬 `herdr 0.8.2`를 격리한 HOME의 PTY에서 실행해 확인했다.

| Herdr 마우스 동작 | rmux |
| --- | --- |
| 탭 클릭, 휠로 탭 전환 | tmux 기본 상태줄 클릭·휠 |
| 탭 바 `+` 새 탭 | 레이어의 `+` 버튼 |
| 탭 오른쪽 클릭: New tab, Rename, Close | tmux 창 메뉴. 레이어가 클릭 후에도 열려 있게 한다 |
| pane 오른쪽 클릭: Rename, Split, Zoom, Close | tmux pane 메뉴, pane 제목 클릭으로 이름 변경 |
| pane 경계 드래그, 스크롤바 | tmux 기본 드래그, 레이어가 스크롤바를 켠다 |
| pane 제목줄 | 레이어가 `pane-border-status`와 버튼이 있는 형식을 켠다 |
| 사이드바: space 목록, new, menu, 접기 | 세로 상태줄, rmux 메뉴(새 세션, 세션과 창 목록), 접기 버튼 |
| 설정 화면 | `rmux-agent settings` |

## 레이어 적재

`-f`가 없을 때만 코어가 `settings.conf`와 내장 레이어를 사용자 설정 파일보다 먼저 대기열에 넣는다(`core/rmux-ui.c`, `cfg.c`). `settings.conf`는 `@rmux-*` 선택만 담는다. 레이어는 `set -gF opt "#{?#{@rmux-X},#{@rmux-X},default}"`와 실행 시점의 `if -F`로 그 선택을 읽는다. `%if`는 파일을 읽을 때 평가되어 앞선 선택이 아직 적용되지 않으므로 쓰지 않는다.

레이어 원본은 `core/rmux-ui-layer.conf` 하나다. 빌드가 C 문자열 헤더를 만들고, Rust는 `include_str!`로 같은 파일을 쓴다. Rust 테스트가 레이어의 기본값과 설정 목록의 기본값을 비교한다. 레이어는 `base`, `panes`, `colors`, `styles`, `bar`, `keys` 구획으로 나뉜다. 상태줄 문구의 시계·언어는 그릴 때 `@rmux-*`를 읽으므로 다시 적용할 필요가 없다.

설정 화면은 선택을 바꾸기 전에 그 선택에 딸린 `set -g[F]` 줄마다 현재 값이 레이어 값과 같은지 확인한다. 고른 옵션 자체와, 아직 레이어 값인 옵션만 다시 적용한다. `colors`와 `keys`는 rmux 자신의 `@` 옵션과 바인딩만 바꾸므로 통째로 다시 실행한다. 레이어 켜기와 초기화는 전체 레이어 뒤에 사용자 tmux 설정 파일을 다시 읽어 시작할 때의 우선순위를 되살린다.

코어는 자기 실행 파일 옆의 `rmux-agent`를 찾아 `@rmux-agent` 옵션에 넣는다. 환경 변수는 쓰지 않으며 `-f`로 시작하면 설정하지 않는다.

| control | 위치 | 동작 |
| --- | --- | --- |
| 0 | 창 목록 뒤 `+`, 사이드바 `+ New` | `new-window` |
| 1 | `Settings` | `new-pane -O -K`로 설정 화면 |
| 2 | 메뉴의 새 세션 | `new-session` |
| 3 | pane 제목 | pane 이름 변경 prompt |
| 4 | 사이드바 `<`·`>` | `@rmux-status-width` 접기·펴기 |
| 5 | 상태줄 `rmux` | rmux 메뉴 |
| 6 | 예약 | |

설정 화면은 `display-popup` 대신 대기하지 않는 modal floating pane으로 연다. next-3.9의 `display-popup`은 popup이 닫힐 때까지 호출한 클라이언트의 명령 대기열을 붙잡는다. 키보드는 popup으로 바로 가지만 마우스는 대기열을 거치므로 클릭이 막힌다.

## 세로 상태줄

`status-position`에 `left`, `right`를 추가했다. 옵션 목록 출력은 바뀌지 않으며 새 코어 옵션도 없다. 폭은 사용자 옵션 `@rmux-status-width`로 정하고 세션 캐시에 둔다.

- left/right일 때 `status_line_size()`는 0, `status_at_line()`은 -1이다. 가로 상태줄 경로는 상태줄이 없는 것처럼 동작한다.
- `status_column_size()`, `status_column_at()`, `status_column_left()`가 폭과 위치를 준다. 창 크기(`resize.c`, `new-session`), 장면 크기와 그리기 x 오프셋(`screen-redraw.c`), pane 출력 오프셋과 좌우 여백·클리핑(`screen-write.c`, `tty.c`), 커서, 메뉴 폭에 반영한다.
- 행은 `status-left`, 빈 줄, 창마다 한 줄, 빈 줄, `status-right`다. 행마다 `format_draw`와 range 목록을 가진다. 창 줄 전체가 그 창의 range다.
- 메시지와 prompt는 맨 아래 줄 전체에 겹쳐 그린다. 열 화면을 메시지 배경으로 복사하지 않는다.
- 마우스: 열 안의 이벤트는 행 range로 상태줄 위치를 정한다. 나머지는 입력 단계에서 창 영역 좌표로 옮긴다. 원래 좌표를 보관해 더블클릭 재생, 메뉴, pane prompt에서 두 번 옮기지 않는다. 드래그는 시작한 곳(열 또는 창)의 분류를 끝까지 유지한다. `mouse_x`, `mouse_status_range` 등 형식은 열을 안다.
- 행 range 목록은 TAILQ라서 행 수가 바뀌면 배열을 다시 만든다. 메시지·prompt가 떠 있는 동안에는 열 화면이 갱신되지 않으므로 그리는 행과 폭을 열 화면 크기로 제한한다.
- `display-menu -xW -yW`는 열의 창 행 옆, 열 가장자리에 메뉴를 둔다.

## 검증

- `tests/test_mouse_ui.py`: 레이어 적재와 `-f` 우회, 사용자 설정 우선, `+` 버튼, 메뉴 유지, 저장한 선택 적재, 좌우 열의 크기·행 클릭·창 메뉴·좁은 터미널, 열이 있을 때의 경계 드래그와 pane 선택, 메시지 겹침, 설정 CLI 적용·되읽기·레이어 끄기·초기화, 설정 화면 클릭.
- Rust 단위 테스트: `settings.conf` 갱신과 보존, 구획 균형, 레이어·목록 기본값 일치, 심볼릭 링크 거부.
- 검토에서 찾은 결함의 회귀: 높이 변경과 메시지·prompt 중 리사이즈, 열 안 드래그, 창 스타일 변경, 사용자 tmux.conf 보존, 레이어가 꺼진 동안의 저장 실패, `%1` pane 이름 변경, 공백이 있는 경로의 메뉴 설정 항목.
- ASAN/UBSan 빌드로 1x1~200열 리사이즈, 좌우 전환, 메시지·prompt 중 리사이즈, 드래그, 메뉴, 폭 8·80을 실행해 보고가 없었다.
- 기존 `make test` 전체와 `scripts/test_upstream.py`. upstream 비교에서 rmux만 실패하는 스크립트는 없었다. `screen-redraw-menus.sh`는 단독 실행에서 stock과 rmux가 함께 실패하는 기존 환경 의존 실패다.
