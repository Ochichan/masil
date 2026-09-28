# 구현 순서와 검증 계획

이 문서는 최종 구현을 위한 작업 분해다. 현재 `core/`, `agent/`, `scripts/`, `tests/`를 만들었고 M0/M1의 초기 실행과 M2의 읽기 전용 통신을 구현했다. 실제 결과는 [현재 구현](../implementation-status.md)에 있다. 아래 source tree는 최종 배치 제안이며 생성된 파일 목록은 아니다.

## 1. 제안 source tree

```text
core/                         pinned tmux source, 기존 파일 구조 유지
  rmux-bridge.c/h              socket, framing, handshake, quota, event 순서
  rmux-observe.c/h             watch registry, generation, bounded grid snapshot
  rmux-actions.c/h             guarded action, dedup, strict managed spawn 문맥
  rmux-ui.c/h                  cached format, client menu, redraw evidence
  compat/yyjson/               고정 C codec과 원본 고지
agent/                        Cargo workspace
  crates/agent/                CLI + serve 진입점, coordinator 구성
  crates/protocol/             wire types, schema 검증, fixture
  crates/runtime/              run/operation/attention reducer, scheduler
  crates/store/                SQLite schema·transaction·retention
  crates/providers/            실제 provider Adapter와 fixture
  crates/work/                 bounded Git·파일·SSH job executor
tests/
  compatibility/              pinned stock tmux와 공개 동작 비교
  protocol/                   C/Rust cross-implementation·fault fixture
  reliability/                crash·duplicate·restore·client 경합
  performance/                workload 생성·marker·결과 집계
docs/                         현재 계약과 설계, ADR
```

crate 수는 build 시간과 독립 검증 이득을 보고 합칠 수 있다. provider 하나마다 crate/process를 만들지 않는다. 공통 protocol과 store처럼 별도 검증할 이유가 있는 Module만 분리한다. C/Rust의 in-process FFI workspace는 만들지 않는다.

초기 Rust dependency는 Tokio의 필요한 feature, serde/serde_json, rusqlite, HTTP Adapter의 reqwest와 필요한 TLS backend, bounded detector의 regex로 제한한다. HTTPS가 필요한 build에서는 rustls 경로를 우선 선택하고 실제 platform·binary size로 검증한다. 불필요한 compression·cookie·HTTP/3 기능은 기본 포함하지 않는다. CLI·error·tracing 보조 dependency도 feature와 배포 size를 측정한 뒤 고정한다. provider SDK 전체가 필요한지 먼저 확인한다. 공식 Interface가 작으면 HTTP/event 계약만 구현한다.

release build의 panic/abort·LTO·strip·allocator 설정은 C와 Rust 각각 측정해 선택한다. agentd panic이 core에 전파되지는 않지만 unresolved operation의 reconcile은 여전히 필요하다. 특정 compiler flag만으로 빠르다고 판단하지 않는다.

## 2. 첫 비교 실험

최종 선택은 thin fork지만, 구현 시작 시 작고 독립적인 두 실험으로 비용을 확인한다.

| 실험 | 만드는 최소 동작 | 선택을 바꿀 근거 |
| --- | --- | --- |
| stock tmux wrapper | no-output control mode의 metadata, 명시적 capture, 원래 TUI 1개 | identity·lifecycle·snapshot 계약을 같은 비용으로 충족하면 core patch 축소 |
| thin core bridge | agent off 경로, pane generation, 작은 bottom snapshot | 최소 patch로도 native 입력·control flow가 변하면 추가 전 설계 수정 |

원래 TUI를 두 번째 headless backend로 바꿔 빠른 결과를 얻는 실험은 비교 대상이 아니다. 필요한 계약을 빠뜨린 prototype의 작은 RSS를 완제품 우위로 발표하지 않는다.

## 3. Milestone과 작업 단위

| 단계 | 구현할 결과 | 필수 확인 | 다음 단계 조건 |
| --- | --- | --- | --- |
| M0 기준 실행 | pinned tmux 재현 build, 기준 목록 확인, workload runner | build feature/OS 기록, 기본 key/command dump, stock baseline | 실행 결과를 재현할 수 있음 |
| M1 최소 fork | executable/socket 격리, bridge off, 별도 rmux-agent skeleton | K-01~09, C 기본 smoke, B-01~03 | 기본 경로 회귀 없음 |
| M2 관찰 통신 | UDS/framing, generation, watch/inventory/snapshot/gap | malformed frame, memory quota, lifecycle/resize/copy-mode, I-01~04 | slow agentd가 PTY를 막지 않음 |
| M3 Agent 관찰 | provider 하나의 native identity/event, fallback, projection/menu | G-01~03, V-04/06/18, B-04~09/12~13 | unknown·parent/child·shared view 계약 성립 |
| M4 Durable 제어 | store, operation receipts, submit capability, wait/ack | V-01~05, commit 전후 kill, namespace 만료, approval 경합 | 중복 입력·거짓 성공·잘못된 승인 없음 |
| M5 복구와 job | strict managed launch, restore, worktree 준비/취소 | V-11/12/15/16/21, cwd 교체·부분 복구·DB 장애 | 대상별 재시도와 다른 pane 응답 유지 |
| M6 원격·추가 provider | SSH metadata bridge, stop intent, provider 확대 | V-09/10/13/14/19/20, 실제 version별 capability | 원격 결과·auth·지원표가 실제와 일치 |
| M7 전체 판정 | 모든 T/K/C/G/R/V 및 성능 matrix | platform별 전체 결과, upstream drift, 미지원 사유 | [최종 완료 조건](../acceptance.md)을 전부 충족 |

M1이 모든 tmux 기능을 새로 구현하는 단계는 아니다. upstream 기능을 가져온 뒤 이름·socket·확장 patch가 공개 동작을 바꾸지 않았는지 검증하는 단계다. M2 이후에도 전체 호환 검증은 누적한다. tmux fork라는 사실 자체가 호환성 증명은 아니다.

## 4. C 변경 지점과 검증 책임

| 기존 지점 | 짧게 추가할 호출 | 차이를 확인할 시나리오 |
| --- | --- | --- |
| server 초기화·종료 | bridge 초기화/close, boot ID | Rust 미설치/미실행에서도 native 정상 |
| `window.c` PTY read + `input.c` batch 완료 | 관심 pane dirty 표시 | raw output capture·pipe·control flow 동일 |
| `events.c` sink·pane/window/session lifecycle | scalar event, 수명 generation | create→layout→event 순서, unlink/kill, respawn |
| `screen.c`·resize/reset 전이 | snapshot generation 무효화 | resize/reflow/alt screen, stale snapshot 거절 |
| `spawn.c` 관리 child 경로 | strict cwd·exec 보고 | native fallback 동일, managed fallback 없음 |
| `server-client.c`·window/session 선택 | client view revision·공유 영향 검사 | session group·linked window·readonly·detach 경합 |
| `format.c`·status/menu | opt-in cached callback·대상 redraw | 기본 format/default key 불변, update fanout |
| `tty.c` output write/discard | 제한된 화면 전달 evidence | slow tty·discard·reset·resize에서 거짓 표시 성공 없음 |

모든 upstream 수정에는 어떤 rmux 불변식 때문에 필요한지와 대응 검증을 남긴다. command registry, default bindings, native option lookup에 새 agent 명령/옵션을 끼워 넣지 않는다. bridge 내부 동작은 agentd 전용 Interface로 한정한다.

## 5. 중요한 fault fixture

| 경계 | 주입할 실패 | 확인할 계약 |
| --- | --- | --- |
| frame parser | 초과 길이, depth, 중복 key, invalid UTF-8, 부분 frame | C/Rust 동일 거절, allocation 상한, core 생존 |
| inventory | page 사이 객체 생성/제거, journal overflow | 완전한 snapshot만 commit, gap 후 stale |
| screen | parse 중 event, resize/alt/reset, copy mode viewport | safe point와 base grid, identity 일치 |
| input | upload 중 단절, commit 전후 kill, partial PTY write | 일부 body 성공 금지, outcome_unknown, 자동 재전송 없음 |
| store | fsync 지연/오류, WAL checkpoint BUSY, disk full | durable success 진실성, native 입력 비차단 |
| operation | 중복·다른 payload·만료 key·새 boot | 재실행 방지와 receipt_expired |
| focus | linked window, 공유 session, readonly, client detach | 영향 scope 검증, logical/tty-write 결과 구분 |
| cwd | preflight 뒤 rename·symlink·permission 변경, shell 초기화 | managed strict cwd, readiness 미확인 표시 |
| provider | 이전 run event, child 종료, SSE gap/초과 body | unrelated 완료 차단, capability 강등 |
| job | 큰 directory, 취소 경합, helper 응답 정지 | resource별 격리, 잔여 변경 설명 |

test는 public Module Interface를 통해 실제 결과를 확인한다. 내부 함수 호출 횟수만으로 성공을 판정하지 않는다. crash-window fixture는 각 durable/외부 효과 지점에서 강제 종료하고 재시작 후 실제 입력 횟수와 receipt를 비교한다.

실제 TUI/terminal/IME 통합 검증은 deterministic fixture와 별도다. fixture가 모두 녹색이어도 provider version 지원표를 자동으로 채우지 않는다.

## 6. 실행 이름·config·배포

`rmux`와 `rmux-agent`를 별도 실행 파일로 제공한다. 선택적 `tmux` 호환 진입점은 rmux가 관리하는 전용 directory에서 opt-in으로 활성화한다. system binary를 대체하지 않는다. 절대 경로로 stock tmux를 실행하는 script는 명시적인 설정 변경이 필요함을 표시한다.

rmux 환경의 호환 진입점은 rmux server namespace로 연결한다. native `TMUX`/`TMUX_PANE` 관계, config 검색 경로, plugin이 다시 호출하는 executable, server version mismatch를 실제로 검사한다. 기본 `.tmux.conf` 의미를 바꾸지 않고 agent 설정은 별도로 둔다.

stock tmux client와 rmux server의 내부 imsg binary 혼용은 초기 지원 대상으로 선언하지 않는다. 공개 CLI/control mode 호환과 별개다. 우연히 연결된 한 version의 결과를 장기 binary 호환으로 홍보하지 않는다.

core-only build도 유지한다. Rust runtime 지원이 검증되지 않은 tmux 플랫폼에서는 native 기능을 계속 build할 수 있어야 한다. packaging이 optional agentd를 native core의 필수 시작 dependency로 만들지 않는다.

## 7. Upstream 유지 전략

초기 core 기준은 `94796f6b1182507efac8a272fc309a79e22e58a5`다. upstream source는 원래 파일 구조를 유지하고 rmux patch를 목적별로 분리한다. code formatting이나 이름 변경으로 upstream diff를 불필요하게 키우지 않는다.

upstream 갱신은 command/options/key/mode/format/control 목록 차이, terminal/PTY lifecycle 지점, 지원 platform 차이를 먼저 확인한다. [기준 추출 자료](../reference/README.md)를 재생성하고 의미 변경을 검토한다. baseline 숫자를 맞추려고 새 upstream 기능을 제외하지 않는다.

upstream bug fix와 rmux 정책 변경을 구분한다. 기본 공개 동작을 바꾸는 fix는 회귀 fixture와 문서화된 판단이 필요하다. 현재 개발판 baseline의 새 기능을 stable tmux에 없다는 이유로 빼지 않는다.

agentd upgrade와 core upgrade는 protocol capability로 조정한다. 불일치 시 agent 추가 기능을 거절해도 native core는 작동한다. DB schema 변경은 version·backup·failure recovery를 갖추며 downgrade가 안전하지 않으면 분명히 거절한다.

## 8. 요구사항 추적

| 계약 | 설계·검증 위치 |
| --- | --- |
| P-01/02, T-01~22, K/C | thin fork·registry/default 보존, baseline differential |
| P-03/04, A-01~10, G | 원래 TUI + run-bound Adapter + capability |
| P-05 | agent-off baseline과 B-01~16/S0~8 측정, 자원 상한·격리 |
| R-01/02/04 | protocol receipt·durable dispatch·wait·crash fixture |
| R-03/05 | reducer identity·attention event/ack·source gap |
| R-06/07 | native input·copy/attachment 결과 단계·K-10/11 |
| R-08/09 | target별 restore·strict cwd·원격 stop intent |
| R-10 | bounded queue/thread/job·성능 B/S matrix |
| R-11 | client menu·shared_focus_conflict·tty write evidence |
| R-12 | version/환경/근거 metadata, redacted diagnostic export |
| I-01~10 | architecture 불변식별 fault/differential/performance gate |

## 9. 이번 설계 이후의 첫 작업

구현을 시작하면 M0의 재현 가능한 stock baseline과 M1의 agent-off 최소 fork부터 만든다. 그 결과가 있어야 memory·latency 예산을 보정할 수 있다. provider 수 확대나 UI 꾸미기는 기본 terminal 경로의 회귀 여부를 확인한 뒤 진행한다.
