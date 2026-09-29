# 화면·입력·에이전트 관찰

이 설계의 가장 큰 절약은 기존 terminal engine이 이미 한 일을 반복하지 않는 것이다. PTY byte, VT parser, cell grid, history, copy mode, redraw는 tmux가 계속 소유한다. agent 관찰은 그 결과의 필요한 일부만 읽는다.

## 1. Native terminal 경로

기준 tmux의 PTY read callback은 pipe/control 경로와 parser를 처리하고 read를 다시 열 시점을 기존 flow control에 맡긴다. masil observer는 `window_pane_get_new_data`의 소비자나 control-output offset 소유자로 등록하지 않는다. 느린 observer 때문에 원래 PTY read가 밀리는 구조를 피하기 위해서다.

`input_parse_buffer`가 cell을 바꾸고 `screen_write_stop`까지 끝낸 뒤 관찰용 dirty를 표시한다. parsing 중 OSC callback에서 즉시 snapshot을 만들지 않는다. 한 번의 escape sequence 처리 도중 상태를 완성된 화면처럼 읽지 않는다.

upstream `events_add_sink`는 lifecycle metadata를 얻는 시작점으로 활용한다. sink callback은 동기 호출이고 payload 수명이 짧으므로 필요한 scalar/ID만 복사한다. pane-created가 layout·active pane 갱신보다 먼저 발생하는 지점은 loop safe point에서 최종 상태를 다시 읽는다. pane가 사라질 때는 삭제 전에 identity를 보존한다.

copy mode나 다른 mode에서 `wp->screen`은 사용자가 보는 mode 화면일 수 있다. agent의 현재 terminal 관찰은 `wp->base`를 읽는다. 그렇지 않으면 사용자가 과거 출력으로 스크롤한 순간 과거 approval을 현재 요청으로 잘못 판단할 수 있다.

## 2. Generation과 dirty 이유

| 변화 | 갱신할 관찰 identity |
| --- | --- |
| PTY output parse batch 완료 | screen generation, dirty output |
| terminal resize / reflow | geometry + screen generation |
| reset / clear / alternate screen 교체 | screen generation, screen kind |
| pane respawn / PTY 교체 | PTY generation, 모든 이전 snapshot 무효화 |
| 같은 shell의 agent 재실행 | binding epoch, native run 연결 재검증 |
| title/path/OSC semantic 정보 변경 | metadata revision. 상태 판단에 쓰는 field면 observation revision도 갱신 |
| pane 종료/제거 | lifecycle tombstone, pending observation 취소 |

실행 ref, generation, cursor, dimensions, screen kind와 cell을 한 safe point에서 얻는다. copied cell 수, 원래 영역, clipped columns/rows/bytes, source revision을 함께 보낸다. 다른 frame에서 받은 title과 현재 cell을 같은 snapshot evidence로 섞지 않는다.

copy mode에서 스크롤한 위치나 선택 변경은 native UI 상태다. base 화면이 바뀌지 않았다면 agent detector를 다시 돌릴 이유가 없다. observer가 base를 읽기 위해 사용자의 copy mode를 종료하거나 selection을 수정하지 않는다.

binding epoch의 권위는 masil managed launch wrapper 또는 검증된 integration의 frontend 시작/종료 handshake다. 동일 shell의 PTY/PID만으로 A 실행, shell 복귀, B 실행의 전환을 증명할 수 없다. 검증된 전환에서 이전 run token을 폐기하고 새 frontend run과 native session을 연결한다. native engine이 계속 살아 있어도 종료된 TUI frontend의 binding은 유지하지 않는다.

수동 실행·foreground 전환을 해당 integration으로 확인할 수 없으면 `unbound/unknown`으로 표시하고 managed submit을 끈다. 늦게 온 이전 hook이나 title/cwd 유사성으로 새 binding을 확정하지 않는다. 일반 TUI 실행과 직접 입력은 그대로 사용할 수 있다.

## 3. Snapshot 최소화

snapshot은 전체 scrollback 대신 active grid 하단의 제한된 영역을 읽는다. `grid`의 history offset을 고려해 active rows를 계산하고, terminal width가 예산보다 크면 clipped임을 명시한다. extended cell·combining mark·wide char를 임의 byte 경계로 자르지 않는다.

```text
pane 변경
  → 관심 등록 여부 검사
  → dirty generation을 기존 entry에 합침
  → deadline/rate/byte budget이 있는지 확인
  → 필요한 pane만 bounded snapshot
  → 같은 revision이면 재검사 생략
  → detector fact와 근거 revision 생성
```

한 pane에 미처리 dirty entry는 하나다. 연속 출력에서 queue가 출력량만큼 자라지 않는다. 화면이 바뀌지 않은 idle pane은 주기적으로 capture하지 않는다. stale deadline·연결 확인은 공유 timer heap으로 관리한다.

snapshot text 전체를 provider event와 함께 무조건 저장하지 않는다. detector가 소비한 뒤 buffer를 pool에 반환한다. 장애 재현용 fixture를 남기는 동작은 명시적으로 선택한다. snapshot hash는 같은 내용을 다시 검사하지 않기 위한 최적화이며 identity·generation 검증을 대체하지 않는다.

관찰 중인 pane이 많으면 priority와 전체 byte budget으로 분배한다. 사용자가 보고 있는 목록의 pane, 최근 working/attention 변화, 나머지 background 순으로 선택하되 오래 밀린 대상에는 age를 반영한다. budget 초과로 늦어진 pane은 관찰 지연을 표시한다. 모든 pane을 목표 Hz로 갱신한다고 약속하지 않는다.

## 4. 상태 모델

프로세스 수명, turn 상태, 사용자 주의 요청은 서로 다른 field다. `working/blocked/done` 하나의 enum에 모든 사실을 넣으면 pane exit와 turn 완료, approval과 질문이 섞인다.

```text
RunProjection
  identity: environment / core boot / pane / PTY / binding / native session
  process: starting | live | exited | unknown
  activity: working | idle | unknown
  attention: approval | question | error | completion | none
  turn_ref: native turn/request ref 또는 unbound
  evidence: source, source cursor, revision, observed_at, authority
  freshness: fresh | stale | gap | unsupported
```

UI는 이 projection을 요약하되 원본 evidence를 조회할 수 있게 한다. `idle`과 `done`은 동의어가 아니다. completion attention은 특정 turn 종료 evidence에서 만들고, task 완료는 사용자의 별도 판정이다.

## 5. Evidence와 reducer

| source | 신뢰할 수 있는 범위 | 단독으로 해서는 안 되는 판단 |
| --- | --- | --- |
| 동일 native session의 structured event | event schema가 보장한 request/turn 상태 | 다른 background session·child event를 foreground 완료로 전파 |
| run-bound hook/plugin | token·run·sequence가 검증한 사실 | 지연·중복·누락을 무시하고 현재 상태 덮어쓰기 |
| process/PTY lifecycle | 실제 프로세스/terminal의 생존·종료 | process live를 working, exit 0을 사용자 task 완료로 해석 |
| screen snapshot | 특정 generation의 보이는 문구·cursor·영역 | 무출력이나 문구 부재를 확실한 완료·승인 가능 상태로 판정 |

source 우선순위만으로 최신 상태를 고르지 않는다. source authority, 연결 epoch, native request ID, 사건 순서, freshness를 함께 검증한다. 늦게 도착한 이전 turn의 `ended`가 새 turn의 working을 끝내지 않게 한다.

parent와 child run은 별개다. child 완료는 parent의 참고 사실이며 parent turn 종료 evidence가 아니다. provider 내부 background session은 native session ref로 분리한다. 매핑이 불분명하면 unknown으로 남기고 이름이나 같은 cwd로 합치지 않는다.

연결 상실은 activity를 idle로 바꾸지 않는다. 마지막 사실과 stale 시각을 유지한다. native source가 승인 대기를 보고하면 화면 detector의 working 추정보다 우선하지만, native source가 끊긴 뒤 오래된 approval에 자동 응답하지 않는다.

## 6. Provider Adapter 구현 전략

각 provider는 실행 identity 찾기, event 정규화, capability 판단, 안전한 action, resume 확인을 제공한다. provider 이름별 if문을 core renderer나 store schema에 흩어 놓지 않는다.

| 연결 방식 | 사용 조건 | 비용 제한 |
| --- | --- | --- |
| native hook/plugin | 같은 실행의 session/run/request ID를 전달할 수 있음 | socket 재사용 우선. hook마다 daemon discovery·전체 로그 scan 금지 |
| HTTP/SSE | 원래 TUI가 연결한 동일 engine/session임을 입증 | host별 연결 수·event 크기·전체 buffer 제한 |
| native session metadata | 공식 또는 소스에서 확인한 identity/resume 정보 | 변경 알림 또는 필요 시 bounded read. 전체 디렉터리 반복 scan 금지 |
| screen fallback | stable snapshot에서 제한된 휴리스틱이 유효 | rate·pattern·문자 수 제한, confidence 표시 |

OpenCode의 server/event 구조, T3의 provider Adapter 분리, Herdr의 lifecycle/report 접근을 참고하되 실제 CLI version별 capability fixture를 갖춘다. 특정 제공자가 현재 어떤 모든 hook을 보장한다고 이 설계만으로 선언하지 않는다. 지원표는 version과 시험 결과가 있어야 채운다.

HTTP client는 provider endpoint별 재사용한다. finite request에는 connect·total deadline과 decoded body byte limit를 둔다. SSE는 일반 total timeout을 걸어 주기적으로 끊지 않는다. connect deadline, event 크기, inactivity/liveness 정책, cursor와 reconnect budget을 별도 둔다. 긴 stream 하나 때문에 전체 body를 `bytes()`로 모으지 않는다.

압축을 켜는 경우 decoded byte 기준으로도 제한한다. redirect는 인증/환경 범위를 넘어가지 않게 검사한다. 같은 session에 연결할 수 없으면 별도 headless engine을 띄워 관찰 중인 원래 TUI처럼 보여주지 않는다.

## 7. Detector 비용

화면 detector는 필요한 literal·bounded pattern만 사용한다. 정규식은 Adapter 초기화 때 compile하고 pattern bytes, compiled size, DFA cache, haystack bytes를 제한한다. boolean 탐지에는 제한된 단일 search를 사용한다. 모든 match를 열거하는 반복 API의 비용을 단일 search와 같다고 가정하지 않는다.

화면 전체를 plain string으로 만든 뒤 provider별로 여러 번 복사하지 않는다. snapshot 하나와 제한된 cell metadata를 공유하여 detector가 필요한 부분을 읽게 한다. 장문의 transcript 요약이나 모델 호출을 detector hot path에 넣지 않는다.

truncated snapshot에서 보이지 않는 marker는 `없음`이 아니라 `확인 불가`다. completion/approval의 근거가 영역 밖에 있을 수 있다. detector가 의미를 해석할 수 없는 version의 TUI면 해당 capability를 낮추고 원래 TUI 조작을 유지한다.

## 8. 입력, IME, clipboard

tmux의 key decoder·extended key·escape-time·prefix·paste·copy mode 기본값은 기준 그대로다. core 앞에 Rust key parser나 전역 key hook을 두지 않는다. 원래 TUI와 masil 자체 menu 입력을 각각 한글/중국어/일본어·AltGr·modifier 조합으로 검증한다.

관리 prompt는 binary-safe payload와 explicit encoding을 사용한다. Enter를 문자열 끝에 덧붙이면 제출이 된다는 가정은 금지한다. bracketed paste 지원과 native submit capability는 별개다. 입력 크기를 넘으면 접수 전에 거절하며 일부만 보내고 성공을 반환하지 않는다.

copy mode와 selection은 upstream 동작을 우선 보존한다. 추가 UI redraw나 animation 때문에 선택을 새로 만들지 않는다. 기준 tmux에서도 재현되는 결함이면 compatibility 결과와 masil reliability gap을 둘 다 기록하고 명시적인 수정으로 다룬다. baseline이 같다는 사실만으로 R-06을 통과시키지 않는다.

clipboard 결과는 buffer 저장, OSC 52/helper 요청, 확인 가능한 OS clipboard 결과로 나눈다. nested tmux/SSH에서 응답 수단이 없으면 전송 요청까지만 표시한다. 원격 파일 attachment는 로컬 경로를 그대로 넣지 않고 업로드/원격 접근 확인 뒤 native attachment ref를 만든다. 실패하면 placeholder 입력을 agent에 보내지 않는다.

link 클릭은 client별 한 action으로 중복 제거하고 URL scheme과 원격/로컬 처리 정책을 명시한다. core에 browser runtime을 포함하지 않는다.

## 9. 추가 UI 비용

기본 화면과 status는 바꾸지 않는다. agent 목록은 요청 시 cached projection에서 page/filter로 만든다. opt-in status format은 `masil_` namespace의 cache callback으로 값만 읽는다. format expansion에서 DB·provider·shell을 호출하지 않는다.

상태 변경은 영향받는 client의 status redraw만 요청하고 짧은 coalescing window를 둔다. tmux user option `@masil_*`를 매 token마다 갱신하지 않는다. 현재 upstream option 변경 경로는 전역 style/redraw 처리를 유발할 수 있다.

menu는 client-owned다. 지속 목록 pane은 native shared pane이므로 같은 window를 보는 사용자에게도 보인다. 기본 키를 추가하거나 이 목록 때문에 다른 pane을 자동 축소하지 않는다. narrow terminal에서는 page/filter가 계속 가능해야 한다.

화면 적용 receipt는 core가 관찰한 logical selection·redraw queue·tty FD write까지다. 느린 terminal의 output discard/reset가 있으면 해당 출력 revision을 미확인으로 바꾼다. 물리적인 화면 표시 성공을 protocol의 보장으로 넣지 않는다.

## 10. 성능 저하 시 축소 순서

1. 동일 dirty와 UI summary를 합친다.
2. background screen fallback의 빈도와 관찰 영역을 줄이고 stale 정도를 표시한다.
3. 새 관찰 등록·관리 요청을 capacity 오류로 거절한다.
4. 회복되지 않는 observer/subscriber 연결을 끊고 재동기화한다.

이 과정에서 native PTY output, 직접 입력, tmux history limit, copy buffer, control client 계약을 임의로 축소하지 않는다. agent 상태를 정확하게 판단할 수 없으면 더 낮은 CPU로 거짓 idle을 만드는 대신 unknown을 반환한다.
