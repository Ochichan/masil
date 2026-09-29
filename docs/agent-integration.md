# 에이전트 통합의 최종 목표

공통 결과·실패 처리의 정의는 [신뢰성 계약](reliability.md)을 따른다. 제공된 Herdr 조사 사례는 요구의 근거이며, 해당 문제가 masil에서 재현됐다는 뜻이 아니다.

## 원래 TUI를 유지하는 계약

masil은 pane 안에서 에이전트의 원래 TUI를 실행한다. 원래 composer, transcript, 도구 출력, 승인·질문, slash command, 모델 선택, native session 전환을 사용할 수 있어야 한다.

PTY는 화면·입력 경로이고, provider adapter는 같은 실행의 식별·관찰·제어 경로다. 상태 정보를 얻기 위해 두 번째 headless agent를 실행하고 이를 원래 TUI의 상태로 표시하지 않는다.

T3 Code의 Codex app-server, Claude SDK, ACP 연동은 원래 TUI를 보여 주는 방식과 다르다. masil은 그 공통 계약을 참고하되, 시작·연결·프로세스 소유권은 원래 TUI 요구에 맞게 검증한다. OpenCode처럼 같은 backend session을 여러 client가 볼 수 있어도 같은 terminal 화면을 공유하는 것은 아니다.

## 최종 기능

| ID | 사용자에게 제공할 기능 | 필요한 조건 |
| --- | --- | --- |
| A-01 | 등록한 provider의 원래 TUI 실행, 이름·작업·디렉터리 지정 | 일반 tmux pane 기능을 유지하며 launch 정보를 관리 |
| A-02 | 현재 pane과 native agent session의 연결 확인 | 실제 native ID 또는 검증 가능한 integration 보고. 알 수 없으면 미확인 표시 |
| A-03 | 실행·입력 대기·승인 대기·실패·turn 종료 관찰 | 근거·발생 시점·현재 유효성·소유 실행을 포함 |
| A-04 | attention 목록, 필터, 해당 pane으로 이동, 읽음 처리 | 동일 사용자 범위의 확인을 client 간 공유하고 badge·목록·이동을 일치시킴. 기존 tmux 알림과 충돌하지 않음 |
| A-05 | 실행·취소·질문 답변·승인 응답·prompt 제출의 공통 접근 | 기능별 capability와 native 대상/request ID 확인 |
| A-06 | 입력 예약과 작업 간 전달 | provider 수락·실행 경계·완료를 확인할 수 있는 연동에서 제공 |
| A-07 | 작업·workspace·worktree·대화 이력 연결 | tmux의 session/window/pane 구조와 독립적으로 조회 |
| A-08 | detach·재접속·restore·native resume | 대상별 배치·cwd·실행·대화 연결 결과와 선택적 재시도를 제공. 불확실한 실행을 자동 재시도하지 않음 |
| A-09 | diff·checkpoint·지원되는 되돌리기 | 공유 checkout 보호, 파일 상태와 대화 상태의 복원을 별도로 검증 |
| A-10 | CLI/API로 관찰·대기·제어하고 원인 설명 조회 | 정확한 대상·대기 기준, 대상 소멸/연결 상실 종료 이유, 단계별 결과와 기계 판독 가능한 근거 |

기능이 없는 provider도 일반 TUI 실행은 가능해야 한다. 공통 제어의 지원 수준을 전부 아니면 전무로 만들지 않는다.

## 식별과 상태

tmux pane ID, agent run, provider 종류·instance, native agent session ID를 구분한다. session 선택·fork·resume·프로세스 교체·하위 작업에 따라 연결이 바뀔 수 있다. 오래된 run의 hook이나 다른 pane의 이벤트가 현재 상태를 덮지 않도록 한다.

상태 모델은 다음 정보를 구분한다. [관찰 설계](design/terminal-and-observation.md)의 projection과 [IPC 설계](design/protocol.md)의 identity·receipt를 구현 기준으로 사용한다. 최종 schema fixture는 구현 단계에서 고정한다.

- 실행 연결: 연결됨, 끊김, 종료됨, 아직 식별하지 못함.
- 관찰한 작업 상태: working, 입력 대기, 승인 대기, idle, 실패, unknown.
- 종료 결과: 특정 turn 또는 프로세스가 어떤 이유로 끝났는지. task 성공과는 별도다.
- 관찰 근거: provider event, 공식 integration, 프로세스 정보, terminal 화면, 관찰 시점.
- 조작 가능성: 현재 연결 방식에서 실제 사용할 수 있는 capability와 이유.

관찰 우선순위는 provider별로 검증한다. 신뢰할 lifecycle 이벤트와 단순 화면 추정이 경쟁해 상태를 번갈아 덮지 않는다. 화면 감지는 사용자가 scroll한 viewport와 분리한다. 새 UI 형태를 인식하지 못한 경우 `unknown`을 허용한다.

`running process`는 `working agent`가 아니고, `idle`은 `task complete`가 아니다. 출력이 없다는 사실만으로 완료나 승인 대기를 확정하지 않는다.

부모와 자식의 관련 범위를 명시하고, 다른 background session의 종료가 전경 작업의 완료를 만들지 않게 한다. 기다려야 하는 자식의 질문·승인은 부모 attention에 나타난다. 같은 사건에 대한 client별 확인 기록은 실행 상태를 변경하지 않는다. 상세 규칙은 R-03과 R-05를 따른다.

## Capability 계약

각 adapter는 native TUI 실행, 세션 식별, lifecycle 관찰, prompt 수락 확인, interrupt, approval, user input, resume, history, conversation rollback 등의 지원 여부를 현재 연결에 대해 보고한다.

같은 provider라도 버전·실행 방식·설치된 hook/plugin·연결 상태에 따라 capability가 달라질 수 있다. 상태 관찰이 가능하다고 승인 응답도 가능하다고 추정하지 않는다.

native TUI의 승인은 항상 남는다. 공통 승인 UI는 유효한 request ID·정확한 session·native 선택지 ID를 보존할 수 있을 때 제공한다. 화면의 버튼 문구나 좌표를 일반적인 승인 API처럼 사용하지 않는다. 기존 provider의 승인·sandbox 정책을 느슨하게 만들지 않는다.

## 입력과 취소의 의미

입력 요청 기록, PTY 전달, provider 수락, turn 시작, turn 종료를 구분한다. PTY write 성공을 prompt 수락으로 표시하지 않는다. 원래 TUI와 공통 입력 기능이 동시에 입력할 때 충돌·순서·대상을 처리한다.

`steer`, `queue`, 새 turn 시작은 provider가 그 의미를 제공할 때 구분해 노출한다. legacy OpenCode와 V2 preview의 입력 계약을 혼동하지 않는다. 확실한 수락 정보를 얻을 수 없는 경우 전달 단계까지만 보고한다.

interrupt 요청, `C-c` key 전달, 프로세스 terminate/kill은 서로 다른 조작이다. 조작 후 확인된 상태를 보고하고, 결과가 불명확하면 pending/unknown으로 남긴다. 재접속만으로 prompt·승인·위험한 조작을 자동 반복하지 않는다.

R-01의 operation receipt로 결과를 재조회하고, 같은 key의 재요청과 의도적인 새 입력을 구분한다. 처리 로그가 늦다는 이유만으로 유실로 판정하지 않는다. 일반적인 자동 submit은 native 요청 검증 또는 동시 입력을 조정하는 reservation을 요구한다. 2026-09-29 사용자가 선택한 예외로 `agent prompt`는 [R-02](reliability.md#r-02-모르는-결과-때문에-중복-입력하거나-승인하지-않는다)에 정의한 idle·전경·실행 identity·화면 근거 검사와 run별 receipt를 거쳐 paste와 Enter를 전달한다. provider 수락이나 자동 승인은 제공하지 않는다. 사용자가 직접 원래 TUI에 보내는 Enter는 tmux 입력 계약대로 처리한다.

wait는 특정 target·run/turn의 수명을 따른다. pane 직접 종료뿐 아니라 window/session 삭제로 실제 대상이 제거되는 경로에서도 종료 이유를 반환한다. link나 client만 제거되고 대상이 살아 있는 경우는 구분한다. R-04는 timeout·삭제·완료 경합과 관찰 단절의 처리를 정의한다.

## 저장과 복구

- native 화면 history, 배치, cwd, 실행 정보, native session ref, 관찰 근거의 소유권과 보존 정책을 구분한다. agentd는 화면 history나 전체 transcript를 기본 복제·영속 저장하지 않는다.
- 살아 있는 server로의 reconnect는 현재 runtime에 다시 붙는 동작이다.
- server 재시작 뒤 restore는 새 runtime과 선택적 native resume를 만들 수 있다. 이전 프로세스가 계속 살아 있었다고 표시하지 않는다.
- 과거 승인 callback이나 native request가 여전히 유효한지 확인한다. 저장된 UI만으로 승인 기능을 활성화하지 않는다.
- 이벤트 재접속에서는 snapshot과 새 이벤트 사이의 누락·중복을 처리한다. 제공자가 replay를 지원하지 않으면 현재 상태를 재조회한다.
- checkpoint 복원은 진행 중인 작업, 다른 agent의 공유 파일, provider 대화와 조정한다. 불가능한 대화 rollback을 파일 복원 성공으로 숨기지 않는다.

R-08에 따라 복구는 대상별 결과를 제공한다. 느린 shell 준비, 잘못된 cwd, resume ID 불일치와 부분 성공을 나누고 기존 session ref를 보존한다. 원격의 사용자 중지·인증 필요·일시적 단절은 R-09에 따라 별도로 처리한다.

## Provider별 첫 탐색 결과

| 대상 | 확인한 근거 | 아직 실행으로 검증할 내용 |
| --- | --- | --- |
| OpenCode | Herdr TUI plugin은 선택 route와 native session을 연결한다. CLI는 외부 HTTP endpoint를 열면 같은 backend session의 API/SSE 접근이 가능하다. | 둘 이상의 pane, session 전환·child session·재접속, 원래 TUI와 외부 제어의 입력 충돌 |
| Codex | 이 Herdr 체크아웃의 integration은 주로 세션 식별을 제공하고 lifecycle은 화면에서 감지한다. T3는 별도 app-server 방식을 사용한다. | 원래 TUI를 유지한 상태의 공식 구조화 신호와 제어 범위 |
| Claude Code | 이 Herdr 체크아웃의 integration은 세션 식별을 보고한다. T3의 Agent SDK 경로는 원래 TUI와 다르다. | native TUI·hook 조합의 실제 lifecycle/approval/resume 범위 |
| 기타 provider | 일반 PTY 실행과 공통 adapter 계약을 적용할 수 있다. | provider마다 버전·연동 방식·capability를 별도 확인 |

이는 masil의 지원 완료표가 아니다. 원래 TUI 실행은 넓게 허용하고, 구조화된 통합 지원은 증거와 함께 추가한다. 조사한 소스 위치는 [참조 문서](reference/README.md)에 있다.

## tmux와 함께 성립해야 하는 조건

에이전트 목록이나 알림을 위해 기본 키를 재배정하지 않는다. 별도 `masil-agent` CLI와 `Prefix :`의 `run-shell -b` 경로로 접근하고, 사용자가 전용 binding을 추가할 수 있다. agent mode를 꺼도 [tmux 호환 계약](tmux-compatibility.md)의 모든 기능이 남는다.

에이전트가 없는 shell pane, 같은 pane에서 agent 종료 후 shell 복귀, 중첩 SSH, 여러 client의 서로 다른 focus를 포함해 검증한다. 한 agent의 감지 오류나 느린 integration이 terminal 입출력과 다른 pane을 멈추게 하지 않는다.

공통 focus 요청은 논리 선택·redraw·tty 출력 단계를 구분하고 실제 물리 표시 성공을 주장하지 않는다. client-only 요청의 공유 선택 충돌은 `shared_focus_conflict`로 반환한다. 원격 첨부의 경로·이미지 bytes, clipboard 목적지, worktree 작업의 진행·취소는 각각 R-07, R-10, R-11을 따른다.
