# 신뢰성 계약

상태: 2026-09-28 제품 요구 초안. [사용자가 제공한 Herdr 조사](research/herdr-feedback-2026-09-28.md)를 rmux의 목표와 검증 조건으로 바꾼 문서다. Herdr의 현재 결함을 독립 검증한 결과나 rmux의 구현 완료 기록이 아니다.

사용자가 모든 pane을 다시 열어 확인하지 않아도 입력·상태·작업 복귀를 판단할 수 있어야 한다. 이를 위해 확인한 결과, 아직 모르는 결과, 실패한 결과를 UI와 자동화 응답에서 같은 의미로 사용한다.

## 적용 범위

[tmux 전체 기능과 기본 키 계약](tmux-compatibility.md)은 유지한다. 기존 tmux 명령의 exit status·출력·control mode를 이 문서에 맞춰 바꾸지 않는다. rmux의 추가 API·명령·알림에는 아래 결과 모델을 적용하고, tmux 조작의 추가 진단은 별도 경로로 제공한다.

사용자가 원래 TUI에 직접 보내는 키와 rmux가 합성하는 입력은 구분한다. 상태가 `unknown`이라는 이유로 사용자의 Enter나 Ctrl 입력을 가로채지 않는다. 자동화가 Enter·승인 응답을 합성할 때는 해당 동작의 전제와 대상을 검증한다.

## R-01. 성공의 단계를 구분한다

rmux의 관리 작업에는 operation ID, 대상 environment·pane·agent run·native session·turn 또는 request ID, 관찰 시점과 결과 근거를 연결한다. 모든 작업이 아래 단계를 전부 제공할 필요는 없다. 관찰할 수 없는 단계는 미확인으로 남긴다.

| 표시할 수 있는 단계 | 확인해야 할 사실 | 이 단계만으로 주장하지 않는 결과 |
| --- | --- | --- |
| 접수됨 | rmux가 요청을 기록했고 같은 ID로 결과를 조회할 수 있음 | 입력 전달, provider 수락 |
| 전달됨 | 해당 전송 경로가 대상에 입력 또는 요청을 전달했다고 확인 | provider가 prompt로 수락함, 승인 처리됨 |
| 수락 확인 | 같은 native session의 provider가 해당 요청을 수락했다고 확인 | turn 시작 또는 성공 |
| 실행 확인 | 해당 요청과 연결된 turn 또는 작업의 시작 근거 | 완료, 사용자 task 달성 |
| 종료 확인 | 그 작업의 종료와 결과를 확인 | 후속 checkpoint나 전체 task의 완료 |

PTY write는 전송 경로 수준의 확인이다. 화면에서 글자가 사라졌거나 처리 로그가 늦게 기록되는 것만으로 수락·유실을 확정하지 않는다. 종료 결과는 성공, 실패, 취소 등을 구분한다. 관찰 단절과 timeout은 외부 작업의 실패가 확정됐다는 뜻이 아니다.

UI, CLI, API는 동일한 단계와 근거를 사용한다. 성공 여부 하나만 반환할 수 있는 rmux 추가 명령은 어떤 단계까지 확인하는 명령인지 명시하고, 더 강한 완료 조건을 기다릴 수 있어야 한다. 중요한 관리 요청의 식별·전달 경계는 재시작 후에도 조회할 수 있게 보존하며, 보존하지 못한 상태를 복원했다고 표시하지 않는다.

## R-02. 모르는 결과 때문에 중복 입력하거나 승인하지 않는다

동일 operation key의 재조회·재요청은 기록된 대상과 요청 내용이 같을 때 같은 작업으로 다룬다. key는 environment·principal·namespace·operation ID를 함께 포함한다. 같은 key에 다른 대상이나 내용이 오면 거부한다. 새 요청으로 같은 문장을 다시 보내는 사용자의 명시적 행동과 네트워크 재시도는 구분한다.

응답 단절 이후에는 기존 결과와 현재 provider 상태를 먼저 확인한다. 전달 여부가 불명확하면 `unknown`으로 남기고 자동 재전송하지 않는다. provider의 중복 제거 보장이나 미전달 증거가 있는 재시도는 그 범위를 기록한다. 외부 CLI에 대한 exactly-once 실행을 약속하지 않는다.

2026-09-29 사용자의 명시적 결정으로 `agent prompt`와 화면의 Send prompt에는 Herdr 방식의 제출을 허용한다. 현재 idle 관찰, 전경 provider, server boot·PTY·전경 process group·run·native session metadata·관찰 revision·화면 output generation·제목·progress를 확인하고 native 명령 큐에서 다시 검사한 뒤 paste와 Enter를 전달한다. blocked·working·unknown 상태에는 제출하지 않는다. copy mode·pane 동기화 중에는 거절하며, paste 실패 시 후속 Enter와 delivered 기록을 중단한다. 여러 줄이나 Tab이 있으면 bracketed paste가 켜져 있어야 한다. 이는 provider 내부 입력 소비의 원자성이나 prompt 수락을 보장하지 않으므로 전달 단계까지만 표시한다.

재시도와 특정 receipt 조회에는 원래 run을 명시한다. prompt에는 run별 단조 증가 operation 번호와 요청 내용의 SHA-256, pending/delivered receipt를 남긴다. 최근 16개 receipt를 조회할 수 있고 만료된 번호는 다시 실행하지 않는다. 같은 번호의 다른 내용은 거절하며 pending이 남으면 새로운 자동 제출도 막는다. provider 승인 응답은 여전히 유효한 native request ID와 선택지를 소비 시 검증해야 한다. 상태 재감지나 연결 복구가 자동 승인으로 이어져서는 안 된다.

## R-03. 상태는 해당 실행의 근거로 판단한다

각 관찰은 source, 유효성, native 식별자, agent run을 가진다. 이전 run·다른 pane·관련 없는 background session의 이벤트가 현재 작업을 덮지 못한다. 관찰이 끊기면 마지막 확인 상태와 현재 미확인을 구분하고, 이 상태를 새 작업 완료 알림으로 만들지 않는다.

부모·자식 작업은 실제 소유 관계와 turn에 따라 묶는다. 기다려야 하는 자식의 승인 요청은 부모의 attention에 나타나고, 자식 하나의 종료가 부모 전체 완료를 뜻하지 않는다. 같은 디렉터리나 provider를 쓴다는 이유만으로 다른 background 작업을 합산하지 않는다. 표시에는 어떤 작업 때문에 working/blocked로 판단했는지 설명할 수 있어야 한다.

권한 요청, 사용자 질문, prompt 입력 가능 상태는 다르다. 모든 `blocked`를 승인 요청으로 해석하지 않는다. provider별로 권위 있는 lifecycle 신호와 화면 추정의 우선순위를 정하고 충돌 근거를 남긴다.

## R-04. 대기는 대상의 수명과 함께 끝난다

wait는 target과 run/turn, 시작 기준을 고정한다. pane 직접 종료, window나 session 삭제를 통한 실제 대상 제거, 프로세스 종료, 서버 연결 상실을 별도 종료 이유로 반환한다. 이미 없어진 대상을 전체 작업 timeout까지 계속 기다리지 않는다.

client detach, 다른 pane 선택, window link 하나의 제거처럼 대상이 살아 있는 변화는 대상 소멸로 처리하지 않는다. 같은 ID의 표시 대상이 다른 run으로 바뀌어도 이전 wait를 새 작업에 자동 연결하지 않는다.

완료와 삭제가 경합하면 하나의 최종 결과를 정하고 대기 등록을 해제한다. 이벤트 유실 시 현재 대상 존재 여부를 다시 확인한다. 서버 단절은 정해진 연결 감지 기한에 따라 알리며, 대상이 여전히 실행 중인지 모르면 성공·종료로 추정하지 않는다.

## R-05. 여러 기기의 확인 상태는 일관된다

attention 발생 사건과 사용자의 확인을 분리한다. 동일 사용자 범위·environment의 확인 기록은 server 측에서 공유·보존하고, view 선호와 별도로 관리한다. 초기 agent 확장의 사용자 범위는 해당 server owner의 OS principal이며, 같은 UID를 공유하는 사람들을 서로 격리됐다고 간주하지 않는다. native tmux의 공유 window/session 선택 의미도 유지한다. 상세 권한은 [protocol](design/protocol.md)을 따른다.

badge, 미확인 목록, 다음 미확인 작업으로 이동하는 기능은 같은 사건과 확인 기준을 사용한다. 배경 구독이나 재접속만으로 확인 처리하지 않는다. 정해진 사용자 확인 동작과 대상 사건을 기록하고, 다른 기기가 접속해도 이미 확인한 사건을 새 완료로 다시 표시하지 않는다.

오프라인 client의 늦은 확인은 자신이 본 사건까지만 적용한다. 더 최근에 발생한 완료·질문·오류를 지우지 않는다. UI가 바뀌었다는 것과 사용자가 attention을 확인했다는 것도 구분한다.

## R-06. 입력과 선택을 보존한다

원래 pane과 rmux 자체 prompt·검색·이름 편집 모두에서 호스트가 전달한 확정 텍스트를 손실 없이 처리한다. UTF-8 byte 경계, 한중일 IME의 확정 입력 직후 Enter, 비영어 배열·AltGr·modifier·extended key, 긴 paste를 검증한다. terminal이 제공하지 않는 IME 조합 상태를 rmux가 관찰한다고 가정하지 않는다.

문제의 회피책으로 사용자가 항상 끝에 공백을 넣거나 다른 editor에서 작성하도록 요구하지 않는다. terminal 설정 변경이 필요한 경우 영향을 받는 조합·근거·되돌리는 방법을 안내하고 사용자 설정을 조용히 바꾸지 않는다.

출력·spinner·resize·reflow 중 선택한 텍스트의 정체성을 유지한다. 유지할 수 없는 경우 선택 무효화를 알리고, 다른 텍스트를 복사한 뒤 성공으로 표시하지 않는다. 더블클릭과 드래그, copy mode와 host terminal selection을 각각 확인한다.

## R-07. 복사·첨부·링크의 목적지를 확인한다

tmux buffer 저장, 외부 terminal로의 OSC 52 전송, 로컬 OS clipboard 쓰기를 구분한다. clipboard 쓰기 확인 없이 `클립보드에 복사됨`이라고 표시하지 않는다. 확인 응답을 받을 수 없는 경로는 `복사 요청 전송, 결과 확인 불가`처럼 표시하고 buffer 보관·수동 복사 등 가능한 대안을 제공한다. 자동 확인을 위해 사용자의 기존 clipboard 내용을 몰래 읽지 않는다.

이미지 bytes, 파일 경로, file URI, host terminal이 drag-and-drop을 텍스트로 바꾼 입력은 서로 다르다. 로컬·원격 경로를 구분하고, 원격 agent가 읽을 수 없는 로컬 경로를 rmux의 첨부 기능이 성공한 것처럼 전달하지 않는다. 파일 전송 기능을 제공하면 목적지 접근 가능성·완료·취소·잔여 파일 처리를 확인한다. native TUI에 사용자가 직접 붙여넣은 일반 경로를 임의로 업로드하지 않는다.

링크 동작은 어느 client의 browser에서 열 것인지 정하고 한 사용자 동작에 중복 실행하지 않는다. 링크 검출·강조와 실제 열기 가능 여부를 구분한다. 열기 요청을 전달한 것과 browser 실행이 확인된 것도 구분한다.

## R-08. 복구는 작업별 결과를 보여 준다

복구 목록에는 요청한 pane별로 배치, cwd, 실행 시작, native session 연결, 준비 상태와 오류·재시도 조건을 남긴다. 예를 들어 12개 배치와 3개 대화만 확인했다면 이 부분 성공을 그대로 표시한다. 하나의 `복구 완료`로 숨기지 않는다.

shell이 시작됐다는 사실과 agent 입력 준비를 구분한다. 느린 zsh·direnv·Nix 초기화는 해당 작업의 단계·기한·진행 정보로 다루며, 고정 sleep 뒤 성공으로 간주하지 않는다. 존재하지 않거나 접근 불가능한 cwd를 임의의 home·시스템 디렉터리로 바꿔 작업을 실행하지 않는다.

agent resume는 저장한 native session ID와 실제 연결된 ID를 대조한다. 실패했다고 원래 session ref·대화 파일을 지우거나 빈 대화를 기존 대화로 대체하지 않는다. 실패 항목의 수동 재개와 선택적 재시도를 제공하고, 성공한 항목을 중복 실행하지 않는다. 프로세스가 살아남은 경우와 새 프로세스로 resume한 경우도 구분한다.

## R-09. 원격 연결은 사용자 의도를 유지한다

일시적 연결 끊김, 인증 필요, 사용자 중지, 원격 server 없음, 버전·기능 불일치를 구분한다. 사용자가 명시적으로 중지한 대상은 저장된 endpoint의 reconnect loop가 다시 시작하지 않는다. 단순 목록 조회나 상태 확인이 원격 agent·server 시작으로 이어지지 않는다.

재인증이 필요하면 진행 가능한 로그인 경로와 어느 연결의 인증인지 안내한다. 재시도만 반복하지 않는다. 인증 URL·token은 진단 로그에서 제거하고, 해당 사용자에게 필요한 상호작용으로만 노출한다.

원격 shell 종류와 인자 quoting, platform 차이, stderr와 오류 출처를 다룬다. 원시 shell 출력을 성공 여부 대신 던져 주지 않는다. 지원하는 Windows terminal에서 SSH로 Unix rmux에 접속하는 경로는 native Windows server 지원 여부와 별도로 검증한다.

## R-10. 오래 걸리는 작업을 격리한다

worktree 준비·의존성 설치·삭제, 파일 전송, remote 연결 같은 작업은 진행 단계·대상·기한·취소 가능성을 조회할 수 있어야 한다. timeout 이후에도 실제 작업이 계속되는지 알린다. 취소 요청과 취소 완료를 구분하고, 취소가 불가능한 구간이면 이유를 표시한다. 이미 수행한 파일 삭제를 취소만으로 되돌렸다고 주장하지 않는다.

대상에 필요한 직렬화만 적용하고, 다른 pane의 입력·출력·전환·관찰 API까지 전체 잠금으로 막지 않는다. 특정 client의 연결·tty 출력이 멈춰도 살아 있는 server·agent를 재시작해야만 복구되게 하지 않는다. client 재접속 경로에서 작업 중복 실행을 피한다.

idle과 동시 출력 부하를 나눠 측정하고, pane·client·CPU 개수에 따라 polling·thread·queue 비용이 불필요하게 늘지 않도록 검증한다. 초기 latency·감지·취소 목표와 자원 상한은 [성능 설계](design/performance.md)에 두고 출시 전에 측정값으로 검증한다. 기한이 없는 `즉시`, `빠름`은 완료 판정이 아니다.

## R-11. 화면 제어의 대상과 적용 결과를 구분한다

pane으로 이동시키는 요청은 대상 client·environment와 선택할 pane을 명시한다. 접수, 논리 선택 적용, redraw 예약, 해당 tty 출력 완료를 구분한다. host terminal의 실제 물리 표시를 확인했다고 주장하지 않는다. headless 요청에 client 대상이 없거나 해당 client가 끊겼으면 적용 완료로 표시하지 않는다.

tmux는 active pane과 current window를 window/session에서 공유한다. client-only 요청이 다른 client의 선택까지 바꾸면 `shared_focus_conflict`로 거절한다. 사용자가 공유 범위 변경을 명시한 경우에만 영향 범위를 알려 실행한다. 별도의 per-client active pane 모델로 tmux 의미를 바꾸지 않는다.

client의 최근 선택·스크롤·목록 view 선호는 native 공유 범위와 구분해 보존한다. 다른 기기의 좁은 화면에 부적합한 추가 UI geometry를 강제하지 않는다. 대상이 없어져 fallback을 선택하면 이유를 표시한다. 작은 터미널·스크롤바·긴 목록·원격 하단 줄에서도 선택·닫기·접기 기능에 접근할 수 있어야 한다.

## R-12. 실패를 재현할 수 있는 진단을 제공한다

진단은 client/server/provider/integration 버전과 release channel, OS·terminal·키 보고 설정·locale·IME, SSH·WSL·중첩 tmux/byobu, cwd, 관찰 source, operation 단계, 연결 generation을 필요한 범위에서 기록한다. 알려진 값과 사용자가 입력한 값을 구분한다.

정상 입력 경로와 실패한 경로를 나눠 볼 수 있어야 한다. 민감한 prompt·terminal 전체 화면·파일 내용·인증 정보는 기본 수집에서 제외하고, 재현 fixture 또는 사용자 선택으로 최소 증거만 첨부한다. 공유 전 가림 처리를 확인할 수 있게 한다.

upstream 이슈의 열린/닫힌 상태, 수정 commit 존재, release 포함, 같은 환경에서의 재검증은 다른 사실이다. [조사 기록](research/herdr-feedback-2026-09-28.md)의 정정과 해결 미확인을 유지하고, 단순 close나 `pending-release`를 rmux의 검증 통과로 사용하지 않는다.

## 구현·출시 우선순위

| 우선순위 | 요구 | 적용 시점 |
| --- | --- | --- |
| 최우선 | R-01, R-02, R-04, R-08 | 해당 입력·자동화·복구 기능을 제공하는 첫 단계부터. 거짓 성공, 중복 실행, 의도하지 않은 승인, 잘못된 대화 재개를 차단 |
| 높음 | R-03, R-05, R-06, R-07 | 상태·알림·기본 입력·복사 기능의 완료 조건 |
| 함께 검증 | R-09, R-10, R-11, R-12 | 원격·장시간 작업·client 제어를 제공할 때 해당 신뢰성 조건도 충족 |

이 우선순위는 사용자 영향에 따른 rmux 설계 판단이다. Herdr 신고 빈도나 전체 사용자 비율을 측정한 순위가 아니다. tmux 전체 기능 지원 범위를 줄이지 않는다. 구체적인 통과 시나리오는 [완료 판정](acceptance.md)에 둔다.
