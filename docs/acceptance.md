# 완료 판정과 검증

현재 문서는 최종 제품이 충족할 조건을 정한다. 터미널 코어와 관찰 IPC의 초기 구현·검증은 [실행 기록](validation/2026-09-28-core.md)에 있다. 아래 전체 플랫폼·provider 행렬의 통과를 뜻하지 않는다.

## 판정 방식

요구사항 상태는 `미구현`, `구현/미검증`, `검증 통과`, `차이 있음`, `환경에서 실행 불가`로 구분한다. 마지막 상태는 통과가 아니다. 모든 범위를 확인할 수 없을 때 `tmux 전체 호환`으로 표시하지 않는다.

각 결과에는 요구 ID, 기준 tmux SHA·build 설정, masil revision, OS·terminal·locale·환경, 재현 입력, 관찰 출력, 차이, 증거를 남긴다. 기준 목록의 개수는 기능 완료율이 아니다.

기준 tmux와 masil을 같은 조건에서 비교한다. 제품명·실행 파일명 및 실행마다 다른 PID·timestamp·socket 경로는 명시한 규칙으로 정규화할 수 있지만, 이를 핑계로 출력 field·ID 관계·순서·오류를 버리지 않는다.

[사용자 제공 Herdr 조사](research/herdr-feedback-2026-09-28.md)의 이슈 상태와 masil의 검증 결과는 별개다. 보고서에 있는 `closed`, 수정 배포 또는 `pending-release`는 통과 증거가 아니다. 모형 화면으로 재현한 조건과 실제 provider 통합에서 관찰한 조건도 따로 기록한다.

## 기준 환경

- 무설정 비교는 격리된 socket과 임시 HOME/config 디렉터리에서 시작한다. 사용 중인 server와 실제 agent 계정·대화를 테스트 대상으로 삼지 않는다.
- locale, TERM, terminal 크기, terminfo, build feature, shell, `VISUAL`/`EDITOR`, key encoding을 기록한다.
- 원래 TUI 시나리오는 가짜 로그만으로 대체하지 않는다. deterministic terminal fixture와 실제 CLI 통합 검증을 함께 사용한다.
- tmux upstream `regress/`를 테스트 설계의 근거로 삼는다. masil에 적용 가능한 경우 executable 진입점을 바꿔 실행하고, 원본 기대값을 masil 결과에 맞춰 바꾸지 않는다.
- byte stream, terminal cell/attribute·cursor·mode, 명령 결과, format, event, 실제 프로세스 상태를 요구별로 나누어 검사한다.

환경 기록에는 client/server/provider/integration 각각의 버전·release channel, terminal 버전과 key-reporting 설정, 입력 언어·IME·배열, 중첩 tmux/byobu, SSH·WSL, shell·direnv·Nix 초기화, native session ID의 대응을 포함한다. OS 이름만 같은 재현을 같은 조건으로 취급하지 않는다.

전체 조합을 무조건 전수 실행하는 대신 위험에 따른 조합과 지원 환경의 보고된 실패 경로를 명시한다. 일반 pane 입력과 popup/prompt 입력, 직접 입력과 paste, 원래 terminal과 중첩 terminal, 서로 다른 provider를 대조한다. 장비가 없어 실행하지 못한 조합은 이유를 기록하고 성공으로 처리하지 않는다.

## 키보드 검증

| ID | 시나리오 | 통과 조건 |
| --- | --- | --- |
| K-01 | 무설정 시작 후 key table과 주요 옵션 조회 | source snapshot 및 같은 build의 tmux 결과와 일치 |
| K-02 | `C-b C-b`, 미등록 키, prefix2, nested terminal | 소비와 전달, timeout, 대상이 기준과 같음 |
| K-03 | pane/window/session 생성·이동·split·swap·zoom | 기본 키 입력에 따른 선택과 geometry 일치 |
| K-04 | repeat와 `switch-client -T`, `bind/unbind/reset` | 최초/후속 반복과 table 수명·fallback 일치 |
| K-05 | emacs/vi copy mode와 command prompt | 환경별 초기화, 선택·검색·복사·취소·history·완성 일치 |
| K-06 | mouse off/on, app mouse 보고, drag·scrollbar·메뉴 | client 기능과 app 입력을 잘못 가로채지 않음 |
| K-07 | Escape 지연, Meta, modifier, paste, focus, extended keys | 같은 조건의 기준 tmux와 byte·논리 키 처리 일치 |
| K-08 | agent 실행 및 공통 overlay 전후 | 기본 binding 변화 없음. overlay 종료 후 원래 입력 대상 복원 |
| K-09 | floating pane의 `*`, `@`, `g`, Tab/BTab와 mode 입력 | 기준 개발판의 새 기능까지 일치 |
| K-10 | 한글 `한글 테스트`·중국어·일본어 입력 직후 Enter, UTF-8 chunk 분할, 직접 입력/paste, pane/prompt 대조 | host가 전달한 확정 텍스트와 agent/입력창 수신 내용 일치. 마지막 음절·byte 손실이나 추가 Enter 없음 |
| K-11 | AltGr·러시아어/영어 배열 전환·Ctrl 키와 terminal 보고 설정 on/off, 지원하는 SSH/WSL 경로 | 문자와 제어 키를 혼동하지 않고 tmux 기준 입력 의미 유지. 특정 환경의 우회 설정을 기본값으로 강제하지 않음 |

## tmux 전체 기능 검증

| ID | 대상 요구 | 대표 검증 |
| --- | --- | --- |
| C-01 | T-01, T-06 | 두 client attach, 하나 detach, client crash, 출력 지속, readonly, 크기 차이, socket 접근·server 종료 |
| C-02 | T-02, T-03 | session group, 한 window의 여러 session 연결, unlink와 kill의 차이, index·이름·respawn |
| C-03 | T-04, T-05 | split/join/break/resize/zoom, floating/tiled·stack·clip, 저장 layout, modal pane, 크기 한계 |
| C-04 | T-07, T-08 | K 시나리오 전체와 copy-mode command 목록 전체, view mode·history·검색 경계 |
| C-05 | T-09, T-17 | 모든 buffer 동작, capture flags, pipe 방향·종료, OSC 52, copy-pipe와 bracketed paste |
| C-06 | T-10, T-11, T-12 | 등록 명령/alias/flag/target 전체, 상대 ID와 모호한 target, parsing·queue·오류·exit code |
| C-07 | T-10 | 모든 옵션의 scope·상속·unset·type·범위·array·user option, config 포함·조건·실행 순서 |
| C-08 | T-13, T-14 | 정적/문맥별 format 변수, 조건·반복·문자열·시간·job, style·status·title·prompt |
| C-09 | T-15 | tree/client/buffer/customize/clock/switch mode, menu/popup의 표시·선택·취소·입력·resize |
| C-10 | T-16 | hook 목록 전체, after/error/재진입·scope·array, run-shell·if-shell·wait-for, 환경 전파 |
| C-11 | T-18 | control client의 명령·오류 블록, notification 전체, output escaping, flow control·subscription·layout 변화 |
| C-12 | T-19, T-20 | VT fixture, alt screen·Unicode·combining·문자 폭·style·hyperlink·focus·키·mouse·clipboard·passthrough |
| C-13 | T-21, T-22 | 도움말·진단·config 오류, 실제 설정/스크립트/plugin, SSH·중첩, platform/build/terminal matrix |

대표 시나리오는 최소 분류다. 92개 명령 중 하나가 빠져도 다른 명령의 성공으로 대체하지 않는다. 옵션·hook·copy command·format·notification도 각각 확인한다. 정적 목록에 잡히지 않는 동적 format과 mode 내장 동작도 검증한다.

## 에이전트 통합 검증

| ID | 대상 요구 | 통과 조건 |
| --- | --- | --- |
| G-01 | A-01 | 원래 TUI의 composer·도구 출력·승인·검색·모델/세션 선택을 직접 사용할 수 있음 |
| G-02 | A-02 | 두 pane·같은 backend·다른 session, session 전환·fork·child session·run 교체에 오연결 없음 |
| G-03 | A-03, A-04 | hook과 화면 충돌, stale 신호, unsupported 화면, scroll 중에도 근거와 상태가 일관됨 |
| G-04 | A-05, A-06 | native TUI와 공통 입력의 동시 조작, request ID, 입력 수락 단계, 취소·kill 구분과 중복 방지 |
| G-05 | A-07 | workspace/worktree/task 연결을 바꿔도 tmux ID와 기존 명령 의미 유지 |
| G-06 | A-08 | client detach, server 정상 종료·crash, reconnect/resume를 구분하고 확인 없는 prompt 재실행 없음 |
| G-07 | A-09 | 공유 checkout·진행 중인 작업·대화 rollback 불가 상황에서 불완전한 복원을 성공으로 표시하지 않음 |
| G-08 | A-10 | 관찰/대기 시작 경계 뒤의 해당 작업 변화만 처리하며, 과거 idle 이벤트로 완료 판정하지 않음 |
| G-09 | 전체 | 한 provider 오류·느린 hook·끊긴 API가 다른 pane의 입력과 출력을 막지 않음 |

공통 제어를 지원하지 않는 provider는 capability를 낮춰 표시하는 것이 올바른 결과일 수 있다. 이는 tmux 기능을 생략해도 된다는 뜻이 아니다.

## 신뢰성 회귀 시나리오

[R-01~R-12](reliability.md)의 구체적 통과 조건이다. 아래 표는 테스트 계획이며 현재 결과는 전부 미구현/미검증이다. fault injection이나 fixture 검증만으로 실제 host/provider 조합의 검증을 대신하지 않는다.

| ID | 대상 요구 | 시나리오와 통과 조건 |
| --- | --- | --- |
| V-01 | R-01, R-02 | provider 처리 로그를 늦추고 prompt 결과를 조회한다. 접수·전달·수락·실행의 확인 단계가 구분되고 로그 지연을 유실로 단정하지 않으며, 동일 ID 재조회/재요청으로 입력이 늘지 않는다. |
| V-02 | R-01, R-02 | PTY 전달 전후에 응답·연결을 끊거나 server를 재시작한다. 확인된 단계를 보존하고 불명확한 외부 효과는 unknown으로 남긴다. 같은 ID에 다른 내용·대상을 붙이면 거부하며 자동 재전송하지 않는다. |
| V-03 | R-02 | composer, trust 선택, permission, 일반 질문, unknown 화면을 교체한다. 검증되지 않은 자동 Enter/승인 입력은 전송하지 않고, 사용자가 직접 보내는 Enter는 원래 tmux 규칙대로 전달한다. 모형 검증과 실제 agent 검증을 구분한다. |
| V-04 | R-03 | 전경·독립 background·부모/자식·이전 run의 이벤트를 섞는다. 관련 자식의 승인 대기는 attention에 나타나고 unrelated idle/종료가 부모 완료를 만들지 않는다. stale/충돌 근거를 설명할 수 있다. |
| V-05 | R-04 | pane 직접 종료와 window/session 삭제, 완료/삭제 경합을 비교한다. 실제 target 제거에는 종료 이유와 waiter 정리가 감지 기한 내 발생한다. detach/link 제거 후 target이 살아 있으면 대기를 잘못 끝내지 않는다. |
| V-06 | R-05 | client A가 사건 N을 확인한 뒤 B가 재접속하고 N+1이 발생한다. badge·미확인 목록·이동 명령이 일치하며 B의 늦은 N 확인은 N+1을 지우지 않는다. 다른 사용자 범위와 server 재시작도 검사한다. |
| V-07 | R-01, R-07 | tmux buffer 성공·OSC 52 전달·OS clipboard 성공/실패/확인 불가를 구분한다. 중첩 경로와 비중첩 경로에서 알려진 테스트 문자열로 결과를 확인한다. 결과 확인 불가를 복사 완료로 표시하지 않는다. |
| V-08 | R-06 | animation·동시 출력·resize·reflow 도중 단어/드래그 선택 후 복사한다. 원래 선택 내용이 유지되거나 명시적으로 무효화되며, 바뀐 다른 내용을 성공으로 복사하지 않는다. |
| V-09 | R-07 | PNG bytes, file URI, drop으로 들어온 경로, 공백·한글 파일명, 로컬/원격 경로를 대조한다. 첨부 방식과 대상 접근 여부를 구분하고 임의 업로드나 읽을 수 없는 경로의 첨부 성공 표시가 없다. |
| V-10 | R-07 | 로컬/원격 링크에 한 번의 열기 동작을 보낸다. 지정한 client에서 중복 browser 실행이 없고, 지원하지 않는 클릭·확인 불가·실패가 결과에 나타난다. |
| V-11 | R-08 | 12개 복구 대상 중 3개의 native session만 연결되게 만든다. 배치 결과와 대화 연결 결과를 따로 표시하고 실패 항목의 ref를 보존한다. 선택적 재시도로 성공 항목을 중복 실행하지 않는다. |
| V-12 | R-08 | 느린 shell·direnv·Nix 준비, 없는 cwd, native session ID 불일치를 만든다. 준비 단계·기한·실패를 보여 주고 임의 cwd나 빈 새 대화로 바꿔 복구 성공이라고 표시하지 않는다. |
| V-13 | R-09 | 일시 단절과 사용자 명시 중지 후 reconnect를 비교한다. 중지 상태는 재접속·목록 조회·건강 확인으로 다시 시작되지 않고 사용자 재개 후에만 바뀐다. |
| V-14 | R-09, R-12 | 인증 만료, tcsh 등 지원 shell, 원격 command 실패와 다국어 stderr를 검증한다. 진행 가능한 인증/오류 안내가 있고 무한 reconnect나 원시 출력만의 성공 판정이 없다. 인증 정보는 진단 공유에서 제거된다. |
| V-15 | R-10 | 큰 worktree 준비/삭제·파일 전송 중 timeout과 취소를 요청한다. 실제 진행·취소 가능 구간·잔여 결과를 조회할 수 있고, 다른 pane의 입력·focus·API는 정해진 지연 예산 안에 유지된다. |
| V-16 | R-10 | 특정 client의 연결·tty 출력 경로를 정지/재시작하고 server와 agent를 유지한다. 재접속이 같은 작업으로 돌아오며 UI 장애 복구 때문에 agent를 죽이거나 중복 시작하지 않는다. |
| V-17 | R-10 | idle과 동시 출력, 활성/숨은 pane, 느린 여러 client, 이용 가능한 CPU topology를 나눠 측정한다. 처리량·API 지연·CPU·queue가 기록되고 공유된 성능 예산을 통과한다. 신고된 256 CPU 수치를 다른 기계의 합격 기준으로 사용하지 않는다. |
| V-18 | R-01, R-11 | 여러 client, 공유 session·linked window에서 client-only focus를 요청한다. 다른 client에 영향이 생기면 shared_focus_conflict로 거절한다. 명시적 shared 요청은 영향 범위를 반환한다. 접수·논리 선택·redraw·tty write를 구분하고, 출력 discard·resize·단절 뒤 물리 표시 성공을 주장하지 않는다. |
| V-19 | R-11 | environment 왕복, terminal 축소, scrollbar가 있는 긴 목록, 원격 마지막 줄을 검사한다. 정한 범위의 선택·레이아웃이 복원되고, 접기·닫기·키보드 이동에 접근할 수 있다. |
| V-20 | R-12 | 버전·관찰 근거·operation 단계를 포함한 진단 묶음을 만든다. 알려진/미확인 정보를 구분하고 synthetic prompt·token·clipboard 내용이 기본 공유 자료에 섞이지 않는지 검사한다. |
| V-21 | R-10, R-11 | 1~3개 agent와 기존 editor/terminal 배치를 함께 쓰고 사용자 지정 worktree 준비 절차를 실행한다. 불필요한 창 이동 없이 원래 작업으로 돌아가고 준비 실패·취소·재시도를 구분한다. |
| V-22 | R-01, R-02 | admission·dispatch intent·외부 효과·receipt commit 사이마다 종료한다. 전체 operation_key와 ticket으로 조정하며 불명확한 effect를 새 ticket으로 재실행하지 않는다. namespace가 다른 같은 UUID와 만료 key도 구분한다. |
| V-23 | R-02, R-10 | core ledger 상한의 여러 배에 해당하는 완료 작업을 순차 실행한다. durable retire 이후 공간을 회수하고, 회수한 ticket·이전 연결 epoch의 replay는 거절한다. ledger 포화는 effect 전에 거절하고 native 입력은 유지한다. |
| V-24 | R-02, R-03 | composer readiness 직후 사용자 입력·trust/approval 전환·같은 shell의 agent 교체를 경합시킨다. 원자적 native 검증/reservation이 없거나 binding이 불명확하면 관리 submit을 끈다. |
| V-25 | R-03, R-10 | inventory page 사이 수명 변경·journal overflow·invalid frame·slow subscriber를 주입한다. partial snapshot을 완전 상태로 적용하지 않고 queue/pool 상한과 gap·stale 처리가 유지된다. |
| V-26 | R-08 | cwd preflight 뒤 path 교체·권한 변경·shell의 cwd 변경·exec 실패를 만든다. 관리 launch가 대체 디렉터리에서 실행되지 않고 native tmux fallback은 기준대로 유지된다. |

R-06의 다국어·키보드 입력 검증은 K-10, K-11도 필수다. 기존 G 시나리오는 유지하며, V 시나리오가 대신 통과한 것으로 계산하지 않는다.

해당 기능을 제공하는 release에서는 거짓 성공, 확인되지 않은 자동 승인/중복 실행, 잘못된 session이나 cwd로의 복구, 다른 작업을 막는 전역 정지를 차단 조건으로 취급한다. 연결 감지·target 제거 알림·취소·화면 적용에는 시험 전에 기한과 측정 방법을 정한다. 시간 제한 전체를 기다린 결과만 보고 정상 처리로 판정하지 않는다.

## 성능과 자원

동일 geometry·출력 부하에서 pane 1개, 15개, 50개를 기준 tmux와 비교한다. 활성 pane·숨은 pane, 여러 window/session, client 1개/여러 개, 느린 client, agent 관찰 off/on을 분리한다. idle, 동시 출력, worktree 같은 장시간 파일 작업을 서로 다른 부하로 기록한다.

입력부터 표시까지의 지연, CPU, 메모리, 출력 처리량, resize, history 검색, 알림 지연을 기록한다. unbounded queue, client 때문에 멈추는 PTY, 숨은 pane의 불필요한 전체 redraw를 검사한다.

API 지연과 long-running operation의 진행 조회·취소 지연도 측정한다. CPU topology와 worker/thread 수를 기록하고 논리 CPU 수만으로 작업량을 불필요하게 늘리지 않는지 확인한다. 특정 Linux 서버에서 보고된 사용량을 일반 환경의 발생률이나 예상 수치로 사용하지 않는다.

초기 수치와 hard limit 초안은 [성능 예산 B-01~B-16](design/performance.md)에 있다. S0~S8 workload와 marker 정의로 비교 측정하고, 결과에 따라 변경할 값은 근거를 남긴다. 아직 어떤 수치도 달성했다고 주장하지 않는다. 측정이 없는 상태는 성능 완료가 아니다.

## 아키텍처 gate

[실행 계획 M0~M7](design/implementation-plan.md)에 따라 아래 조건을 누적 검증한다. 구현 전 문서 검사는 이 gate의 runtime 통과를 대신하지 않는다.

- agent off에서 추가 process·정기 timer가 없고, agentd crash/DB stall 중 native 입력·출력이 유지된다.
- default binding·command/option 축약·control mode가 agent on/off에서 동일하다. 제공 binding은 `run-shell -b`를 사용한다.
- snapshot은 base grid와 모든 화면 generation 전이를 따르고, 관찰자가 raw PTY flow control에 참여하지 않는다.
- thread 수·queue·payload·watch 상한이 CPU 수나 무제한 pane 수를 따라 증가하지 않는다. native tmux 객체 수는 추가 관찰 quota와 별개다.
- C/Rust codec은 같은 invalid UTF-8·중복 key·깊이·길이 fixture를 거절하고 실제 heap/RSS 고수위를 기록한다.
- store receipt의 durability, core ledger 회수, namespace/ticket fencing은 V-22/23과 실제 crash 검증을 통과한다.

## 최종 완료 조건

1. P-01부터 P-05까지 충족한다.
2. T-01부터 T-22까지의 필수 동작과 기준 목록을 모두 검증한다. 알려진 tmux 비호환이 남아 있으면 전체 호환 완료가 아니다.
3. 기본 키는 K 시나리오와 전체 바인딩/모드 비교를 통과한다. 에이전트 기능 on/off 모두 포함한다.
4. 지원한다고 선언한 provider/version/연동 방식은 G 시나리오를 통과한다.
5. 지원 OS·terminal·build matrix, 성능 예산, 복구 보장, 미지원 provider capability가 문서화되고 결과와 일치한다.
6. 사람과 스크립트가 쓰는 기본 tmux 경로가 agent 기능 때문에 바뀌지 않는다.
7. 제공하는 기능에 해당하는 R-01~R-12와 K-10/K-11·V 시나리오를 통과한다. 다기기 확인 상태, 부분 복구, 미확인 결과를 포함하고 아직 관찰할 수 없는 provider 기능은 그 한계를 표시한다.

외부 조사 사례를 문서에 연결한 상태와 masil 동작을 검증한 상태를 별도로 유지한다. 원문 확인·fixture 재현·실제 환경 검증·수정 배포·재검증 기록을 서로 대신하지 않는다.

부분 구현의 release는 가능하지만 단계와 검증 범위를 명확히 표시한다. 부분 release를 최종 목표 달성으로 바꾸어 적지 않는다.
