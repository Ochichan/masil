# tmux 호환 계약

## 범위와 기준

tmux에서 가능한 기능을 rmux에서도 모두 제공한다. 호환 대상은 익숙한 단축키 몇 개가 아니라 터미널 동작, 명령과 출력, 설정, 자동화, client 연결, control mode까지 포함하는 사용자 관찰 가능 동작이다.

초기 기준은 tmux `94796f6b1182507efac8a272fc309a79e22e58a5`, `configure.ac`의 `next-3.9`다. 이 개발판에 있는 floating pane과 새 layout 관련 기능도 범위에 포함한다. 기준 파일과 목록은 [참조 문서](reference/README.md)에 기록한다.

이 문서는 지원 목표다. 고정 tmux 소스 전체를 코어에 도입했으며 macOS의 기본 명령·키·옵션 및 회귀 검증을 수행했다. 항목별 전체 지원 판정과 구분하여 [검증 결과와 남은 제한](validation/2026-09-28-core.md)을 기록한다.

## 호환의 의미

1. 같은 초기 상태·설정·터미널 조건에서 같은 조작을 하면 같은 의미의 결과가 나와야 한다.
2. 명령 이름·alias·인자·flag·target·기본값·실패 조건·exit status를 유지한다.
3. 기계가 읽는 출력과 이벤트는 필드·escaping·순서·블록 경계를 유지한다. timestamp, PID, socket 경로 등 실행마다 다른 값은 관계를 보존해 비교한다.
4. 사람이 조작하는 화면은 키 안내, 선택, focus, mode, pane geometry, terminal cell 내용을 포함해 비교한다. rmux 고유의 탐색 UI는 허용하되, 같은 pane 영역에서의 터미널 동작과 사용자 지정 status/format의 의미를 유지한다. 기능 존재 여부만 확인하지 않는다.
5. 원래 tmux가 OS나 terminal capability에 따라 제한하는 기능은 같은 조건에서 비교한다. 환경 의존성은 기능 삭제의 근거가 아니다.
6. 더 안전하거나 편리해 보인다는 이유로 기본 의미를 조용히 바꾸지 않는다. rmux 확장은 명시한 추가 동작으로 제공한다.
7. `-f` 없이 시작하면 [rmux UI 레이어](mouse-ui.md)를 사용자 설정보다 먼저 적용한다. 레이어는 옵션 값과 기본 바인딩이 없는 키(`MouseDown1Control0`~`6`, pane 제목의 `MouseUp1Control3`·`MouseDrag1Control3`, prefix 뒤 `C-s`·`C-r`)만 바꾸며, 옵션 표·키 표의 기본값은 그대로다. 같은 조건의 stock tmux와 비교할 때는 `-f`로 시작한다. `status-position`의 `left`, `right`는 rmux 확장 값이다. `split-window -G`도 rmux 확장이다. floating pane을 floating 묶음 안에서 나누며, 묶음은 한 덩어리로 움직이고 크기가 바뀌고 앞으로 나온다. 묶음이 없는 창에서 floating·tiling 명령은 stock tmux와 같게 동작한다.

[신뢰성 계약](reliability.md)의 단계별 응답은 rmux 추가 기능에 적용한다. 예를 들어 호환 `send-keys`의 성공은 기존 tmux 의미를 유지하며, 이를 agent의 prompt 수락으로 확대하지 않는다. 추가 관찰 경로로 provider 수락을 확인할 수 있어도 기존 exit status나 control-mode 출력에 임의의 field를 삽입하지 않는다.

## 필수 기능군

| ID | 기능군 | 포함해야 하는 동작 |
| --- | --- | --- |
| T-01 | Server와 실행 진입 | server 시작·종료, socket 선택과 경로, 별도 server, 환경 변수, client 종료 코드, daemon 생명주기 |
| T-02 | Session | 생성·열거·이름·선택·attach/detach·kill, 기본 session 선택, session group, linked window |
| T-03 | Window | 생성·인덱스·이름·선택·이동·swap·link/unlink·respawn·kill, 여러 session이 같은 window를 참조하는 의미 |
| T-04 | Pane | 생성·split·join·break·이동·swap·resize·zoom·mark·respawn·kill, synchronize-panes, 실행 종료·remain-on-exit·exit 상태 |
| T-05 | Layout와 floating pane | 기본/사용자 layout, 저장 문자열과 새 layout 표현, tile/floating 전환, 위치·크기·stack·clip·빈 window, 선택·focus |
| T-06 | 여러 client | 같은 session 동시 연결, session 전환, client별 크기·viewport·focus, readonly, suspend/lock/detach, 접근 제어 |
| T-07 | 키 입력 | prefix/prefix2, root/prefix/custom table, mode별 처리, 반복·timeout·extended keys, send-keys/send-prefix, 리매핑·unbind·reset |
| T-08 | Copy/view mode | scrollback, cursor·word·paragraph·jump·검색·선택·rectangle·line·mark·copy-pipe, emacs/vi, history/alternate screen |
| T-09 | Buffer와 clipboard | named/자동 buffer, load/save/list/show/delete/choose/paste, bracketed paste, OSC 52·set-clipboard·copy-command |
| T-10 | 설정과 옵션 | 설정 파일 탐색·source-file·조건문·brace·quote·escape·변수, server/session/window/pane scope, 상속·unset·array·user option·alias |
| T-11 | 명령 실행 | 모든 등록 명령과 alias, prefix abbreviation의 모호성, 인자 파싱, command queue, 비동기 대기, 조건·confirm·error 처리 |
| T-12 | Target 해석 | session/window/pane ID와 이름·index, 상대 target·marked target·mouse target·정확한 이름·패턴, 잘못된 대상과 모호성 |
| T-13 | Format과 style | 모든 공개 format 변수·문맥별 변수, 연산자·조건·반복·검색·시간·문자열·shell job, style·색·정렬·폭·Unicode |
| T-14 | Status와 prompt | status line·window list·title·자동 이름·알림, command prompt·완성·history·emacs/vi 입력, 검색과 사용자 안내 |
| T-15 | 선택 UI와 overlay | choose-tree/client/buffer, customize·clock·switch mode, menu·popup, keyboard/mouse 선택·취소·tag·filter·preview |
| T-16 | Hook와 자동화 | 모든 내장 hook, after hook, 사용자 event·monitor·event wait, 배열·scope·재진입/순서·error 조건, run-shell/if-shell/wait-for, 환경 갱신 |
| T-17 | 입출력 도구 | capture-pane의 escape/history/line 옵션, pipe-pane의 방향·open/close·프로세스 수명, display-message, history 정리 |
| T-18 | Control mode | `-C`/`-CC`, 명령 응답의 `%begin`/`%end`/`%error`, 모든 notification, 출력 escaping, flow control, pause/continue, subscription |
| T-19 | 터미널 호환 | VT 입력 처리·화면·cursor·색·style·title·termios·resize, alternate screen·UTF-8·문자 폭·combining·hyperlink·focus·mouse 보고 |
| T-20 | Terminal capability | TERM/terminfo, terminal-features/overrides, 입력 협상·extended keys·clipboard·passthrough, build 조건부 기능 |
| T-21 | 진단과 도움말 | list-commands/list-keys/show-options/show-hooks/show-messages, 명령별 usage·오류, config 오류와 log |
| T-22 | 기존 사용 환경 | 기존 tmux 설정, shell script와 plugin 호출, SSH·중첩 multiplexing, 기준 tmux가 지원하는 OS와 terminal 조합 |

이 표는 분류표다. 표에 이름이 없더라도 기준 manpage·명령/옵션/키 목록·소스의 공개 동작·회귀 사례에 있는 기능은 범위에 포함한다. [기준 목록](reference/tmux-baseline.json)의 각 항목은 관련 기능군과 검증 결과에 연결해야 한다.

## 기본 키와 에이전트 확장

[기본 키보드 계약](default-keybindings.md)을 따른다. 기본 prefix는 `C-b`다. tmux가 갖고 있는 `root`, `prefix`, `move`, `copy-mode`, `copy-mode-vi` 바인딩을 유지한다. 다른 mode의 내장 키 처리도 포함한다.

에이전트 기능 때문에 `C-b a`, `C-b g` 같은 키를 임의로 새 기본값으로 잡지 않는다. 특히 현재 기준에서 `g`는 floating pane 이동 테이블 진입이다. 현재 비어 보이는 키도 무단으로 가로채지 않는다.

추가 명령 이름은 tmux 명령/alias와 충돌하지 않게 정한다. 사용자 정의 table과 bind를 통해 단축키를 만들 수 있어야 한다. 추가 UI는 원래 pane의 키나 mouse 입력을 불필요하게 소비하지 않는다.

## CLI, 설정, plugin의 호환

- `rmux` 진입점에서 tmux 명령 문법과 조작 의미를 제공한다.
- `.tmux.conf`, 추가 source 파일, `@user-option`, format/hook, shell command 조합을 해석할 수 있어야 한다. 실제 탐색 경로와 우선순위는 기준 동작 및 명시적인 rmux 확장 규칙으로 문서화한다.
- `tmux`를 하드코딩한 기존 스크립트도 rmux 환경을 조작할 수 있는 호환 진입점이 필요하다. 선택적 전용 경로의 shim을 제공하는 [배포 설계](design/implementation-plan.md)를 따르며, 설치된 tmux 실행 파일을 조용히 덮어쓰지 않는다. 절대 경로로 stock tmux를 호출하는 script는 명시적 설정이 필요하다.
- `TMUX`, `TMUX_PANE` 등 기존 도구가 읽는 식별 정보를 검증한다. 값만 흉내 내고 다른 server를 조작하게 해서는 안 된다.
- `plugin 지원`이라는 표시는 설치 도구가 켜진다는 뜻이 아니라 plugin이 쓰는 공개 명령·옵션·format·hook·키의 효과가 맞는다는 뜻이다. 구체적인 plugin/version별 결과를 별도로 남긴다.

## OS와 터미널 범위

기준 README는 OpenBSD, FreeBSD, NetBSD, Linux, macOS, Solaris를 열거한다. 최종 tmux 호환 범위에서도 이 플랫폼들의 해당 기능을 추적한다. 초기 개발과 실행 검증은 사용 가능한 macOS 환경에서 시작할 수 있지만, 다른 플랫폼은 미검증으로 남기며 지원 완료로 표시하지 않는다.

조건부 빌드 기능은 빌드 flag·의존성·terminal capability와 함께 기록한다. UTF-8, true color, mouse, clipboard, extended key 등의 조건을 한 terminal emulator에서의 성공으로 일반화하지 않는다.

Windows native 지원은 tmux 호환 요구와 별도의 제품 결정이다. 추가하더라도 기존 Unix 계약을 축소하지 않는다.

Windows terminal·IME·WSL에서 SSH로 Unix rmux를 사용하는 client 경로는 native Windows server 지원과 별개로 추적한다. terminal의 키 보고 설정, 중첩 tmux/byobu, 입력 언어, client와 server의 버전 차이를 분리해 기록한다. 같은 증상의 모든 환경을 하나의 결함으로 일반화하지 않는다.

## 사용자 동작과 내부 구현의 경계

tmux와 같은 코드·언어·자료구조를 사용할 의무는 없다. 다만 내부 선택으로 공개 동작의 누락이나 차이를 정당화할 수 없다.

기존 tmux 바이너리가 rmux 내부 socket에 직접 접속하는 binary protocol 혼용은 초기 지원으로 선언하지 않는다. 공개 CLI/control mode와 기존 자동화는 rmux 진입점으로 작동해야 한다. 원래 tmux client와 직접 연결된다고 주장하려면 별도 상호운용 검증이 필요하다.

## 기준 변경과 호환성 주장

tmux 기준을 올릴 때 명령·옵션·hook·format·mode command·키·기본값·control notification·회귀 사례의 차이를 검토하고 기준 SHA를 기록한다. 새로운 기능은 backlog에 추가하며, 구현이 어렵다는 이유로 기준에서 삭제하지 않는다.

버전별 동작 차이는 호환 profile이나 명시적인 지원 범위로 표현한다. `tmux 완전 호환` 표시는 고정된 기준 버전과 [완료 판정](acceptance.md)을 함께 만족할 때만 사용한다.
