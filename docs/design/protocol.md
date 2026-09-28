# IPC와 작업 결과 계약

이 문서는 최종 wire protocol v1의 설계다. 현재 실행 가능한 부분은 [코어 관찰 IPC](../core-observation.md)의 hello/inventory/snapshot/stats다. watch·event·action·durable 관리 요청은 아직 제공하지 않는다. native tmux CLI·control mode·socket protocol은 이 계약으로 대체하지 않는다. 자원 상한은 [성능 예산](performance.md)의 이름을 참조한다.

## 1. 연결과 encoding

| 연결 | 역할 | 수명 |
| --- | --- | --- |
| core ↔ agentd | 선택한 object의 수명·snapshot·guarded action·요약 cache | core boot마다 인증된 coordinator 하나 |
| rmux-agent CLI ↔ agentd | 관리 요청, 결과 조회, wait, 사용자 확인 | 요청 또는 wait 동안 |
| provider integration ↔ agentd | 해당 run의 hook·native event | 가능한 경우 연결 재사용 |
| remote metadata bridge ↔ agentd | 원격 environment의 관리 정보 | 명시적으로 연결한 동안 |

frame은 `uint32 big-endian payload_length`와 UTF-8 JSON object다. 길이를 확인한 뒤 buffer를 할당한다. 압축, 임의 binary FD 전달, 임의 함수 호출, raw PTY stream은 v1에 없다. snapshot과 관리 prompt가 크면 별도 message 종류로 나누며 JSON object 중간을 의미 없이 쪼개지 않는다.

core 수신 frame은 `CORE_RX_FRAME`, core 송신과 agentd public frame은 `PUBLIC_FRAME`을 넘지 않는다. 길이 0, 초과 길이, 중간 종료, deadline 초과는 protocol 오류다. partial read/write는 정상 처리하고 `EAGAIN`이면 event loop로 돌아간다. socket가 쓸 수 있을 때까지 동기적으로 기다리지 않는다.

C codec은 yyjson 0.13.0을 고정 후보로 선택한다. strict UTF-8, non-standard JSON 비활성화, read/write depth 8, 고정 allocator pool을 사용한다. 모든 object의 중복 key를 거절한다. tmux의 현재 `json.c`는 일반 문자열 escaping·decoding과 자료형에 제약이 있어 이 wire codec으로 재사용하지 않는다. Rust는 serde/serde_json을 쓰되 동일 depth·key·schema 제한을 별도 검증한다. parser 라이브러리의 기본값만으로 계약이 충족된다고 가정하지 않는다.

v1 일반 object는 최대 32 key, array는 page당 최대 64 item이며 더 큰 목록은 pagination한다. ID는 고정 형식 또는 최대 128 bytes, 표시 label은 최대 256 UTF-8 bytes다. payload·경로처럼 다른 한도가 필요한 field는 schema에 별도 선언한다. C는 문자열 길이를 명시적으로 사용하고 OS path·argv에서 NUL을 거절한다. 깊이·item 수·문자열 한도를 통과해도 frame/pool 전체 한도를 넘어갈 수 없다.

64-bit sequence와 generation은 십진 문자열이다. enum은 문자열이고 알 수 없는 필수 enum은 거절한다. 숫자 deadline은 단위가 붙은 bounded integer다. schema가 허용하지 않은 field는 같은 major version에서 거절하며, 협상한 extension field만 예외로 한다. 경로는 UTF-8 표시 문자열과 원본 OS bytes를 구분한다. 비 UTF-8 경로는 base64 bytes field로 전달하고 shell string으로 재조합하지 않는다.

## 2. Handshake와 권한

연결 직후 `hello`는 protocol major/minor, role, 지원 capability, frame 한도, environment ID와 알고 있는 boot ID를 제시한다. 상대는 선택한 version·한도와 실제 boot ID를 반환한다. major 불일치는 종료하고 낮은 minor로 조용히 의미를 바꾸지 않는다.

socket은 server owner가 소유한 private directory에 둔다. directory 권한, socket 소유자, peer UID를 확인한다. stale socket 제거는 OS lock·owner·boot 확인 뒤 수행하며 이름이 같다는 이유만으로 다른 socket을 삭제하지 않는다. 같은 UID의 악의적 프로세스를 별도 보안 영역으로 간주하지 않는다.

native tmux의 multiuser/readonly 정책은 그대로 남는다. 첫 agent 확장은 server owner UID 범위에서 제공한다. 다른 UID의 client에 owner 권한을 넘겨주지 않는다. client를 거친 명령은 실제 client의 readonly 상태와 허용 범위를 core에서 다시 확인한다. owner UID의 독립 CLI는 별도 owner 요청으로 표시하며 client 요청인 것처럼 위장하지 않는다.

provider hook에는 run별 token과 허용 event 범위를 부여한다. token은 command line·진단 bundle에 노출하지 않고 private FD 또는 권한을 제한한 파일로 공급한다. hook이 `principal`, client 대상, 다른 run ID를 바꾸어 권한을 넓힐 수 없다. 원격 연결은 인증된 SSH 환경 ID에 묶고 로컬 UID 숫자와 원격 UID 숫자를 동일 사용자 증명으로 쓰지 않는다.

## 3. 식별자와 generation

| 식별자 | 의미와 검증 |
| --- | --- |
| environment_id | 설정·저장소가 소유하는 지속적 실행 환경 ID |
| core_boot_id | C server 시작마다 새로 생성. 이전 socket 경로와 별개 |
| object_ref | boot ID + 종류 + native ID + object generation. 제거 후 재사용 혼동 금지 |
| pty_generation | 같은 pane의 respawn/PTY 교체 때 증가 |
| binding_epoch | 같은 PTY shell에서 agent가 끝나고 새 agent가 실행되는 경우도 구분 |
| run_id / native_session_ref | rmux 실행 수명과 provider 대화 ID. 서로 대체 불가 |
| client_ref | boot ID + 연결 ID + attach generation |
| screen_generation | 출력·resize·reflow·reset·alternate screen 전환을 모두 반영 |
| stream_epoch / event_seq | 관찰 stream 재설정과 그 안의 순서 |
| operation_key | environment + principal + 발급 namespace epoch/nonce + operation ID의 복합 key. 모든 저장·전송·중복 판단에서 동일하게 사용 |
| dispatch_ticket | core connection epoch + 증가하는 dispatch sequence. 한 번 저장한 effect attempt에는 같은 ticket만 사용 |

pointer, PID 단독, pane 번호 단독, cwd, pane title은 실행 identity가 아니다. counter overflow 시 재사용하지 않고 해당 확장 stream을 새 epoch로 재동기화한다.

```json
{
  "v": 1,
  "kind": "guarded_action",
  "request_id": "req-42",
  "operation_key": {
    "environment_id": "env-uuid",
    "principal": "owner-ref",
    "namespace_epoch": "12",
    "namespace_nonce": "namespace-uuid",
    "operation_id": "op-uuid"
  },
  "dispatch_ticket": {"connection_epoch": "connection-uuid", "seq": "42"},
  "target": {
    "core_boot_id": "boot-uuid",
    "pane_id": "%7",
    "pty_generation": "3",
    "binding_epoch": "9"
  },
  "action": "show_agent_menu",
  "client_ref": "client-uuid",
  "expected_view_revision": "18"
}
```

예시는 필드 관계를 설명한다. 최종 schema와 generated C/Rust 타입은 구현 단계에서 versioned fixture와 함께 고정한다. prompt·approval·focus는 서로 다른 action schema를 가진다.

## 4. Inventory와 event의 순서

core의 live object가 권위 있는 상태다. agentd는 그 상태의 projection을 유지한다. 각 의미 사건에 sequence를 부여하지만 parser byte마다 event를 만들지 않는다.

1. 명시적으로 선택한 watch scope의 inventory 요청에서 `stream_epoch`, `fence_seq`, `scope_revision`을 잡는다.
2. bounded page마다 ID·generation·수명 metadata를 값으로 복사한다. 모든 page 전후에 scope revision이 같아야 한다. core loop를 정지하고 전체 server를 한 번에 복사하지 않는다.
3. 그동안의 event는 정해진 journal에 보관한다. scope의 topology/identity가 바뀌거나 journal이 넘치면 snapshot을 폐기하고 `resync_required`를 보낸다.
4. 완전한 `inventory_end`를 받은 뒤에만 새 inventory를 적용한다. 그 뒤 fence보다 큰 event를 순서대로 적용하고 중복 seq를 무시한다.
5. 반복적인 변화로 inventory를 완성하지 못하면 `busy_resyncing`과 오래된 정도를 표시한다. 부분 inventory를 완전한 목록처럼 사용하지 않는다.

watch 등록 한도는 추가 관찰 대상에만 적용한다. native tmux의 pane/session/client 개수를 제한하지 않는다. 전체 탐색은 page를 읽어 선택하는 방식이며, 관찰하지 않는 pane은 terminal 기능을 그대로 제공한다. 한도 초과 대상은 `observation_capacity_exceeded`다.

화면 snapshot은 inventory와 별도다. 한 pane의 metadata와 cell을 같은 safe point에서 복사하고 시작·종료 generation을 비교한다. 다른 pane까지 같은 시각이었다고 주장하지 않는다. 불완전한 snapshot은 `complete=false`와 누락 이유를 가진다.

재연결 snapshot은 현재 상태를 복구한다. 놓친 과거 `started`/`ended` 사건을 복구해 주지는 않는다. native provider history에서 해당 turn을 확인할 수 없으면 그 구간은 unknown이다.

## 5. Queue, backpressure, 손실

| 종류 | 포화 시 정책 | 재연결 시 |
| --- | --- | --- |
| 수명·identity event | 우선 reserve에 기록. 보존 불가 시 stream에 gap 표시 후 연결 재동기화 | authoritative inventory 재확인 |
| dirty/snapshot 요청 | pane별 최신 generation으로 합치기, 낮은 관심 작업 defer | 최신 snapshot만 요청 |
| summary/UI projection | 대상별 최신 revision만 유지 | 현재 summary 재전송 |
| 관리 action | 효과 전에 admission 거절. 이미 효과 가능 구간이면 receipt 조회 요구 | durable operation과 core dedup ledger 조정 |
| wait 응답 | 완료 사유 보존. 느린 subscriber 분리 | operation/target 결과 조회 |
| provider event | 가능하면 cursor replay. 불가능하면 source gap·unknown | provider reconcile |

높은 우선순위도 frame/byte quantum을 소비한다. lifecycle 폭주가 terminal event loop를 독점하지 않는다. 가득 찬 TCP/UDS를 기다리거나 observer 때문에 PTY `EV_READ`를 끄지 않는다.

priority reserve까지 없으면 추가 stream을 끊는다. agentd는 disconnect 즉시 그 stream의 상태를 stale로 만든다. 개별 event를 버리고 완료 상태를 유지하지 않는다. slow UI subscriber는 자신의 연결만 잃으며 coordinator의 global queue를 붙잡지 않는다.

## 6. 관리 요청과 receipt

```text
received → accepted_durable → dispatch_intent_durable
                               ↓
                    delivered → native_accepted → started → ended
```

이 화살표는 모든 provider에서 모두 관찰된다는 뜻이 아니다. 단계별 evidence를 별도로 저장하며, 늦게 도착한 관찰을 앞 단계의 시각으로 꾸미지 않는다. `ended`에는 completed/failed/cancelled 등 native 결과가 있고 사용자의 task 전체 완료와는 별개다.

| 단계 | 반환할 수 있는 증거 |
| --- | --- |
| received | 요청을 parsing했고 admission 검사 중. durable 성공 아님 |
| accepted_durable | operation ID·target·fingerprint·필요 payload가 저장 commit 완료 |
| dispatch_intent_durable | 외부 효과를 시도할 의도가 commit됨. 아직 전달 완료 아님 |
| delivered | 해당 전달 방식이 확인한 범위. core buffer 등록과 PTY FD write도 세부 단계로 구분 |
| native_accepted | 동일 native request/turn에 연결된 provider acknowledgement |
| started | 해당 요청이 시작됐다는 authoritative evidence |
| ended | 해당 turn 또는 요청의 terminal evidence |

`rejected`, `target_gone`, `unsupported`, `expired`, `failed`, `outcome_unknown`은 단순 성공/실패 bool로 합치지 않는다. caller deadline이 끝나도 effect가 진행 중일 수 있다. 응답은 operation ID와 조회 가능한 마지막 확인 단계를 포함한다.

같은 `operation_key`의 target·payload digest·action이 같으면 기존 receipt를 반환한다. 다르면 `idempotency_conflict`다. namespace만 같거나 UUID만 같다는 이유로 같은 operation으로 판단하지 않는다. DB, payload staging, core ledger, provider correlation, receipt에서 전체 key를 사용한다. ledger를 잃었거나 기록이 만료된 요청은 새 요청처럼 재전송하지 않는다. 만료 key는 `receipt_expired`로 거절하고 사용자가 새 intent를 만들도록 한다.

core dispatch는 연결 epoch마다 순서가 정해진 ticket을 사용한다. core는 소비한 sequence watermark를 유지하고, 이미 소비한 ticket은 ledger에서 결과를 조회하거나 `receipt_not_retained`로 거절한다. 다시 효과를 실행하지 않는다. receipt commit 확인 뒤 ledger entry를 회수하는 절차와 재연결 fencing은 [저장 설계](state-and-recovery.md)에 정의한다.

## 7. Prompt와 approval 전달

일반 키보드는 이 protocol을 통과하지 않는다. 관리 prompt만 bounded payload로 접수한다. 큰 prompt는 `payload_begin/chunk/end`로 agentd에서 조립하고 전체 길이·digest를 확인한 뒤 durable admission한다. 중간 upload는 accepted가 아니며 TTL 뒤 폐기한다.

같은 operation의 core 전송도 chunk를 제한된 staging buffer에 조립한다. `commit_input` 전에 boot·PTY·binding·예상 composer readiness를 다시 검증한다. 일부 chunk만 PTY에 쓰지 않는다. commit 뒤 PTY write가 부분 실패하면 입력 전체가 미전달이었다고 가정하지 않는다.

화면이 composer처럼 보였다는 사실이나 readiness event만으로 자동 submit capability를 제공하지 않는다. provider가 input 소비 시 expected request/revision을 원자적으로 검증하는 native submit을 제공하거나, integration이 native 사용자 입력과의 경합을 조정하는 reservation을 제공할 때만 관리 submit을 켠다. 그 보장이 없는 provider는 draft 준비와 사용자의 직접 제출만 지원한다. unknown/trust/approval 화면에 Enter를 합성하지 않는다. 화면 generation 검사는 잘못된 실행을 막는 조건이지 TUI 내부 상태를 잠그는 장치가 아니다.

approval은 native approval ID·run ID·expected revision·허용한 선택을 명시한다. 중간에 요청이 바뀌거나 끝났으면 거절한다. 일반 prompt와 approval 응답을 같은 `send text + Enter` 구현으로 합치지 않는다.

## 8. Client action과 wait

focus/menu 요청은 client_ref·view revision을 포함한다. core에서 readonly·연결 수명·대상 존재·공유 영향 범위를 한 번에 검사한다. 다른 client의 session/window 선택에 영향이 가는데 client-only 요청이면 `shared_focus_conflict`다. 공유 변경을 명시한 요청은 영향을 받는 client 목록을 receipt에 남긴다.

결과는 논리 선택 적용, redraw 예약, 관련 출력의 tty FD write 완료를 구분한다. tty buffer discard/reset, resize, detach가 발생하면 이전 frame의 drain evidence를 무효화한다. host terminal의 실제 표시 여부는 알 수 없다고 표시한다.

wait는 operation, run, pane, window 또는 session 중 대상 종류와 종료 조건을 고정한다. detach·window unlink를 실제 object 제거와 혼동하지 않는다. window/tab의 실제 제거로 하위 대상이 사라졌다면 waiter를 함께 종료한다. target ID가 재사용돼도 새 대상에 wait를 붙이지 않는다.

event gap/연결 상실 후 검증할 수 없으면 `observation_lost`로 wait를 끝내거나 caller가 선택한 bounded reconcile 단계로 간다. 무조건 원래 timeout까지 대기하지 않는다. 기다리는 client가 없어져도 operation을 자동 취소하지 않는다.

## 9. CLI와 종료 코드 초안

`rmux-agent launch`, `observe`, `status`, `prompt`, `approval`, `wait`, `focus`, `ack`, `restore`, `job`, `doctor`를 별도 executable에 둔다. 이것은 subcommand 설계 목록이며 이미 지원되는 CLI 안내가 아니다. tmux의 명령 축약 namespace에는 추가하지 않는다.

machine output은 schema version·operation ID·receipt·error code가 있는 JSON이다. CLI exit 0은 호출자가 지정한 기다림 단계가 확인됐다는 뜻이다. 기본 단계는 `accepted_durable`이며 `--wait-for started/ended` 같은 명시적 조건과 함께 출력한다. 일반 조회는 조회 성공을 뜻한다. 사람용 출력도 `접수됨`과 `작업 시작됨`을 구분한다.

제안 exit class는 2 잘못된 요청, 3 capability/권한 거절, 4 일시적 capacity·연결 문제, 5 확인된 실패, 6 timeout/결과 미확인이다. 세부 원인은 JSON error code로 구분한다. 사용자 script에 공개하기 전에 conformance fixture로 고정한다.

## 10. 원격 전송

터미널 attach는 native SSH PTY와 native rmux client 경로다. metadata bridge는 별도 비대화형 SSH 연결에서 framed stream을 전달한다. 서로의 backpressure가 같은 application queue를 공유하지 않는다.

SSH 명령에는 고정된 helper 진입점만 두고 cwd·prompt·파일 경로는 stdin frame으로 전달한다. 원격 shell quoting으로 payload를 구성하지 않는다. 인증 재요청은 해당 environment의 상태와 인증 안내로 반환한다. status 조회나 reconnect가 사용자가 중지한 원격 server/agentd를 다시 시작하지 않는다.

재접속마다 원격 boot·connection generation을 확인한다. 이전 연결의 응답으로 새 연결의 operation을 완료하지 않는다. 연결 지속 의도와 backoff는 [저장·복구 설계](state-and-recovery.md)를 따른다.
