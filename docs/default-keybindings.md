# 기본 키보드 계약

## 필수 원칙

rmux의 기본 키 설정은 기준 tmux의 기본 키 설정과 같아야 한다. `C-b` prefix만 같고 나머지를 바꾼 구성을 호환 기본값이라고 부르지 않는다.

기준은 [참조 SHA](reference/README.md)의 `key-bindings.c`, `options-table.c`, `tmux.c`, mode별 입력 처리다. [전체 기본 바인딩](reference/tmux-default-bindings.conf)은 C 문자열과 메뉴 매크로를 풀어 소스에서 추출한 것이다. 이 파일은 비교 자료이며 현재 실행 가능한 rmux 설정이라는 뜻은 아니다.

## 기본값과 초기화

| 항목 | 기준 값 또는 규칙 |
| --- | --- |
| `prefix` | `C-b` |
| `prefix2` | `None` |
| `key-table` | `root` |
| `repeat-time` | 500ms |
| `initial-repeat-time` | 0ms. 이 경우 최초 반복도 `repeat-time`을 사용한다. |
| `escape-time` | 10ms |
| `prefix-timeout` | 0ms, 비활성 |
| `assume-paste-time` | 1ms |
| `mode-keys` | 옵션 테이블 기본값 emacs. 시작 환경에 따라 vi로 바뀔 수 있다. |
| `status-keys` | 옵션 테이블 기본값 emacs. 시작 환경에 따라 vi로 바뀔 수 있다. |
| `mouse` | 고정한 `next-3.9`의 `Makefile.am`은 `TMUX_MOUSE=1`을 지정하므로 기본 빌드는 on이다. macro가 없는 source fallback은 0이다. 실제 비교 build의 정의를 함께 기록한다. |
| `extended-keys` | 옵션 테이블 기본값 off. terminal 협상과 개별 app 요청 처리는 별도 검증한다. |
| `extended-keys-format` | xterm |
| `focus-events` | off |

`tmux.c`는 `VISUAL`이 설정되어 있으면 이를 선택하고, 아니면 `EDITOR`를 확인한다. 선택한 값의 basename에 `vi`가 들어 있으면 `mode-keys`와 `status-keys`를 vi로 초기화하고, 아니면 emacs로 초기화한다. 명시적인 사용자 설정은 tmux와 같은 순서로 적용한다. 따라서 기본 copy mode를 항상 vi 또는 항상 emacs로 고정하지 않는다.

tmux의 mouse 옵션과 pane 안 프로그램 자체의 mouse 입력은 구분한다. 에이전트 통합을 켰다는 이유로 `mouse`, `focus-events`, `escape-time`의 기준 기본값을 바꾸지 않는다. 2026-09-28 실제 build 검증에서 기존 문서가 macro 없는 fallback을 기본 build로 잘못 설명한 점을 정정했다. rmux와 같은 source의 stock build 모두 mouse on이다.

## 주요 prefix 키

아래는 읽기 쉬운 요약이다. 전체 바인딩과 정확한 명령 문자열은 추출 파일을 기준으로 한다. `Prefix`는 기본 `C-b`, `C-`는 Ctrl, `M-`는 Meta/Alt 표현이다. terminal이 보내는 실제 byte와 extended key 처리까지 검증한다.

| 키 | 동작 |
| --- | --- |
| `Prefix C-b` | prefix를 pane에 전달 |
| `Prefix c` | 새 window |
| `Prefix n` / `p` / `l` | 다음 / 이전 / 직전에 선택했던 window |
| `Prefix 0` … `9` | 해당 index의 window 선택 |
| `Prefix w` / `s` | window / session 선택 트리 |
| `Prefix ,` / `$` | window / session 이름 변경 |
| `Prefix d` | 현재 client detach |
| `Prefix D` | client를 골라 detach |
| `Prefix (` / `)` / `L` | 이전 / 다음 / 마지막 session으로 현재 client 전환 |
| `Prefix "` | 위·아래 pane으로 분할, `split-window` |
| `Prefix %` | 왼쪽·오른쪽 pane으로 분할, `split-window -h` |
| `Prefix o` | 다음 pane 선택 |
| `Prefix ;` | 직전에 활성화된 pane 선택 |
| `Prefix 방향키` | 해당 방향의 pane 선택, repeat 지원 |
| `Prefix C-o` / `M-o` | pane 회전 / 반대 방향 회전 |
| `Prefix {` / `}` | 위 / 아래 pane과 swap |
| `Prefix z` | pane zoom 전환 |
| `Prefix !` | pane을 별도 window로 분리 |
| `Prefix x` / `&` | 확인 후 pane / window 종료 |
| `Prefix q` | pane 번호 표시 |
| `Prefix Space` | 다음 layout |
| `Prefix M-1` … `M-7` | 기준 소스의 일곱 layout 선택 |
| `Prefix E` | pane 크기를 균등하게 재배치 |
| `Prefix M-방향키` / `C-방향키` | pane 크기를 5 / 1 단위로 조정. floating pane의 방향별 의미도 유지 |
| `Prefix [` / `PPage` | copy mode / 위로 스크롤하며 copy mode |
| `Prefix ]` | 가장 최근 buffer 붙여넣기 |
| `Prefix #` / `=` / `-` | buffer 목록 / 선택 / 가장 최근 buffer 삭제 |
| `Prefix :` / `?` / `/` | command prompt / 키 목록 / 지정 키 설명 |
| `Prefix m` / `M` | pane mark 전환 / mark 해제 |
| `Prefix f` | pane 검색 |
| `Prefix C` | 옵션 customize mode |
| `Prefix t` | clock mode |
| `Prefix r` | client redraw |
| `Prefix C-z` | client suspend |
| `Prefix <` / `>` | window / pane 메뉴 |
| `Prefix *` | floating pane 생성 |
| `Prefix @` | tile/floating 전환 |
| `Prefix g` | `move` 키 테이블 진입 |
| `Prefix Tab` / `BTab` | floating pane을 사용한 window / session switch mode |

마지막 다섯 항목을 포함한 정확한 동작은 현재 `next-3.9` 기준이다. 예전 stable 버전의 치트시트로 현재 기준을 덮어쓰지 않는다.

## 키 테이블 전체

| 테이블 | 소스 초기화 항목 수 | 보존 대상 |
| --- | ---: | --- |
| `prefix` | 93 | 일반 키, repeat flag, command prompt, 메뉴, floating 진입 |
| `move` | 19 | floating 위치·크기·이동 메뉴 |
| `root` | 30 | prefix 없는 mouse·scrollbar·drag 등 |
| `copy-mode` | 78 | emacs식 copy 조작과 mouse |
| `copy-mode-vi` | 88 | vi식 copy 조작과 mouse |
| 합계 | 308 | `key_bindings_init`의 문자열 초기화 항목 수 |

이 수는 런타임에서 canonical key alias를 처리한 후의 고유 바인딩 수나 전체 입력 기능 수가 아니다. `list-keys`의 실제 출력은 향후 같은 기준 빌드로 확인한다.

`window-tree`, `window-buffer`, `window-client`, `window-customize`, `window-clock`, `window-switch`, `menu`, `popup`, command prompt에는 별도의 입력 처리도 있다. 308개 바인딩만 구현하고 키보드 호환 완료로 판정하지 않는다.

## Copy mode에서 혼동하지 않을 동작

| 동작 | emacs `copy-mode` | vi `copy-mode-vi` |
| --- | --- | --- |
| 선택 시작 | `C-Space` | `Space` |
| 복사 후 종료 | `C-w` 또는 `M-w` | `Enter` 또는 `C-j` |
| 선택 해제 | `C-g` | `Escape` 또는 `C-[` |
| mode 종료 | `Escape`, `q`, `C-c` 등 | `q` |
| 직사각형 선택 전환 | `R` | `v` 또는 `C-v` |
| 아래 / 위 검색 | `C-s` / `C-r`, incremental | `/` / `?` |
| `Space` | page down | 선택 시작 |

`copy-pipe-and-cancel`과 `copy-command`의 연동도 보존한다. `Enter`나 `Escape`가 모든 mode에서 같은 의미라고 가정하지 않는다. prefix가 없는 pane 입력의 `C-c`, `C-d`, `C-z`, `Escape`, `Tab`, `Enter`는 원래 terminal과 mode 규칙에 따라 전달한다.

## 에이전트 기능과 충돌 방지

- 기본 tmux 바인딩을 대체하거나 추가 해석하지 않는다.
- 에이전트 전용 global hotkey는 초기 기본값에 넣지 않는다. 공통 기능은 CLI와 `Prefix :`의 명령으로 접근할 수 있게 한다.
- 사용자가 명시적으로 bind한 전용 key table과 키는 지원한다. 추천 설정은 기본 설정과 구분한다.
- 원래 agent composer, 승인 화면, 검색, 계획 모드, 모델 선택 등에 필요한 입력을 보존한다.
- 공통 agent overlay를 열었을 때 입력 소유자와 닫는 동작을 분명히 한다. overlay를 닫으면 기존 pane과 mode로 돌아간다.
- tmux 업그레이드 때 새 기본 키와 rmux 추천 키의 충돌을 검사한다. rmux의 편의 키를 유지하기 위해 tmux 기본값을 변경하지 않는다.

## 검증 계약

한중일 IME와 비영어 배열을 기본 입력 검증에 포함한다. 원래 pane, rmux의 popup/command prompt, 검색·이름 편집에서 확정 텍스트 직후 Enter, 긴 paste, UTF-8 chunk 경계, AltGr와 Ctrl 조합을 따로 확인한다. Windows host terminal→SSH→Unix server와 중첩 tmux 경로도 해당 지원 환경에서 검사한다.

rmux는 terminal이 전달하는 확정 텍스트와 key protocol을 보존한다. 관찰할 수 없는 IME preedit 상태를 추정해 Enter를 앞당기거나 마지막 글자를 제거하지 않는다. provider별·입력창별 대조를 남겨 host 입력 문제와 rmux 처리 문제를 구분한다.

출력·animation·reflow 중 selection 유지와 실제 복사 결과는 [신뢰성 계약 R-06, R-07](reliability.md)을 따른다. agent 상태의 `unknown`은 직접 사용자 키를 가로채는 근거가 아니다. 자동화의 합성 입력 제한과 수동 키 전달을 구분한다.

무설정·고정 terminal 조건에서 기준 `list-keys`, `show-options`와 비교한다. `VISUAL`/`EDITOR`가 없는 경우, vi 값인 경우, 다른 editor 값인 경우를 분리한다. source 설정, bind/unbind/reset, `-r` 반복, table 전환, copy/prompt 모드, mouse on/off, 느린 Escape 입력, extended key 조건을 포함한다.

정확한 키 의미는 [완료 판정](acceptance.md)의 K 시나리오로 검증한다. 현재 기록은 소스 기반이며 실행 검증 결과가 아니다.
