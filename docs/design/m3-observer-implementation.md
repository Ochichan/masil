# M3 첫 native session 관찰 묶음

기준 `ee913cc`. 이번에는 명시적으로 시작하는 경량 agentd와 OpenCode의 읽기 전용 native session 관찰을 구현한다. 원래 TUI의 PTY 경로와 tmux 기본 키는 유지한다.

## 제공 범위

- `masil-agent --socket MANAGER serve --core CORE --config FILE`: foreground agentd. 단일 Tokio current-thread runtime. 별도 DB·daemon autostart·provider process 시작 없음.
- 같은 manager socket의 `status`, `agents`, `inspect ID`, `stop`. status 조회만으로 process·인증·provider를 시작하지 않는다. stop은 agentd만 종료하고 core/provider는 그대로 둔다.
- config는 최대 8개 OpenCode endpoint/project source와 합계 64개 명시적 session 관찰 대상. 같은 source는 하나의 HTTP client/SSE 연결을 공유한다. 각 관찰 대상은 로컬 표시 ID, pane ID, native session ID를 가진다.
- HTTP는 사용자가 지정한 numeric loopback의 http endpoint만 지원한다. proxy·redirect·TLS·원격·자동 discovery는 이번 범위에 없다. source별 선택적 password_env와 username으로 기존 OpenCode Basic auth에 접속한다. 인증 비밀번호를 응답·로그·디스크에 남기지 않으며 transcript를 보관하지 않는다.
- 명시적 endpoint/session/pane 지정은 **사용자 association**이다. provider session 관찰과 현재 pane TUI의 identity 증명을 구분한다. TUI route handshake가 없는 초기 구현의 binding은 unverified이며 pane의 작업 상태·제어 권한으로 승격하지 않는다. PTY 교체·종료·삭제·core gap은 association을 invalidated로 바꾼다.

## 상태의 의미

provider는 native session 존재·parent ID, activity working/idle/retrying/unknown, attention approval/question/none/unknown, pending request ID와 count를 확인한다. 존재 여부는 미관측·stale에서 null이며, 검증된 응답에서만 true/false다. idle은 완료가 아니며 완료 알림·성공·prompt 전달·승인은 제공하지 않는다. transient error의 이력과 성공 판정은 이 묶음에서 제공하지 않는다. 다른 session 또는 child의 idle은 선택 session을 종료시키지 않는다. 자동 child attention 합산은 미지원으로 표시하고 명시적으로 등록한 child를 별도 행으로 관찰한다.

core의 생존과 native provider의 freshness를 따로 표시한다. SSE EOF·oversize·malformed·liveness 만료에서 기존 상태를 fresh로 남기지 않는다. 마지막 activity 근거와 unknown/stale을 구분한다. 재접속은 bounded backoff 뒤 현재 상태를 재조회하며 과거 event replay나 누락 구간 복구를 주장하지 않는다.

native event는 상태를 다시 확인할 신호로 사용한다. source별 변경을 합쳐 bounded HTTP snapshot으로 재조회한다. 변경 없는 session을 pane별 timer로 polling하지 않는다. 초기 동기화·재연결·snapshot 중 event는 별도 epoch/dirty fence로 처리하고 변경이 겹치면 재조회 또는 syncing을 유지한다. SSE heartbeat는 활동/완료 event가 아니다.

## 자원과 IPC

- management UDS는 private owner directory, socket 0600, same-UID peer, inode 확인 후 cleanup. 기존 경로를 덮어쓰지 않는다. core socket 옆의 OS lock으로 같은 core에 agentd 중복 시작을 거절한다.
- command frame 8 KiB, response 64 KiB, client 최대 32, handshake/request/write deadline 3초. control socket의 느린 client는 provider/core task를 막지 않는다.
- provider finite body 최대 256 KiB, SSE event 최대 64 KiB, decoded depth/collection 상한, source당 pending request 최대 256. HTTP finite 전체 5초, SSE inactivity 45초, 부분 event 완료 5초. oversized/unsupported는 관찰 실패로 표시한다.
- SSE batch는 256 KiB로 제한하고 32개 단위로 다른 task에 실행 기회를 준다. snapshot 완료 후 pending event를 모두 처리한 경계와 부분 frame 부재를 확인한 뒤 publish한다. 이 확인의 deadline도 5초다. HTTP 실패는 dirty revision이 달라도 stale/backoff로 처리한다.
- Tokio watch channel에 source별 최신 snapshot만 보관한다. 무한 event queue나 transcript cache 없음. metadata 상태의 중간 변화가 합쳐질 수 있으며 이 채널을 history/receipt로 쓰지 않는다.
- core watch는 전체 선택 scope 한 연결. snapshot text를 읽거나 출력 stream을 복제하지 않는다. event gap/EOF 뒤 자동 재binding하지 않는다.
- 모든 읽기 전용 query는 bounded RAM projection을 반환한다. status 미시작은 not_started. missing/invalid target 및 provider 상태는 구분한다. config/runtime source의 데이터는 code나 shell argv로 실행하지 않는다.

## 구현과 검증

- [x] shared config/state model, minimal dependency feature/lock 고정.
- [x] private agentd, core watch projection, query/stop CLI, 단일 runtime.
- [x] OpenCode GET/SSE adapter, 권한·질문 우선순위·session 격리·stale/reconcile.
- [x] 실제 private core + mock OpenCode에서 end-to-end; 전체 HTTP method가 GET인지 확인.
- [x] 느린 source와 client, auth/oversize/EOF, core respawn/remove, 중복 daemon lock, no-autostart 검증.
- [x] 기존 core/watch/CLI 회귀 및 새 Rust 단위 검증.
- [x] 가능하면 격리된 설치 OpenCode 1.18.32의 schema/GET smoke. 모델 요청 없음.
- [x] agentd idle RSS/CPU/thread 수와 50-session 공유 연결 측정. 짧은 probe는 budget 통과로 표시하지 않음.
- [x] 독립 검토, 구현 문서/근거/실행 기록 갱신, commit.

최종 M3 전체 완료와 구분한다. verified TUI binding plugin, screen fallback, attention menu, provider 확대와 M4 durable 제어는 이 묶음의 완료 조건이 아니다.
