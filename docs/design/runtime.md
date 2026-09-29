# 실행 구조와 Module Interface

이 문서는 [아키텍처 선택](../architecture.md)을 구현 가능한 책임과 실행 순서로 구체화한다. 제시한 이름과 수치는 설계 계약이며 구현되어 있는 symbol이 아니다. 자원 상한의 기준표는 [성능 예산](performance.md)이다.

## 1. 프로세스와 thread

| 구성 | 정상 수명 | 기본 실행 자원 |
| --- | --- | --- |
| masil native client/server | tmux와 동일 | 기존 C/libevent 실행 모델. terminal 기능 때문에 새 Rust runtime을 시작하지 않음 |
| agentd | agent 기능을 명시적으로 활성화한 environment에서만 | Tokio current-thread loop 1개, SQLite writer thread 1개 |
| blocking pool | DNS 등 불가피한 blocking 호출 시만 | 최대 2 thread. 호출 전 별도의 bounded admission 적용 |
| Git·삭제·전송 helper | 해당 job 수행 중만 | 기본 동시 2 process. OS CPU 수와 무관 |
| provider 연결 | 필요한 instance/run에만 | 제한된 async task와 재사용 socket. pane별 thread 없음 |

`new_current_thread().enable_all()` runtime을 독립 프로세스의 `Runtime::block_on`으로 구동한다. C event loop 안에서 Rust future를 돌리거나 기다리지 않는다. `spawn_blocking` 자체의 내부 queue는 상한 정책으로 사용하지 않으며, 시작한 blocking 작업의 abort가 보장된다고 가정하지 않는다.

긴 파일·Git 작업은 취소·진행 관찰이 가능한 child process로 격리한다. SQLite writer는 그 pool을 쓰지 않는다. CPU를 쓰는 큰 작업은 loop에서 실행하지 않고 크기를 제한해 분할하거나 별도 job으로 보낸다.

## 2. 기본 시작과 지연 활성화

1. `masil`은 기준 tmux의 config·socket·session 시작 규칙을 따른다. C 확장의 private bridge listener는 대기할 수 있지만 agentd·DB·provider probe를 시작하지 않는다.
2. `masil-agent status` 같은 조회는 agentd가 없으면 `not_started`를 반환한다. 조회가 daemon·agent·인증 browser를 시작하지 않는다.
3. 사용자의 `launch`, `observe enable`, 명시적 `serve` 또는 허용한 restore 실행이 agentd를 시작한다. 같은 core boot에 대해 한 agentd만 붙도록 OS lock과 handshake를 사용한다.
4. agentd는 DB 복구, core inventory 동기화, provider 연결 순서로 준비한다. native terminal은 이 과정을 기다리지 않는다.
5. agentd restart는 실제 core inventory와 기존 provider session을 재확인한다. 저장된 launch 요청을 자동으로 실행 목록에 올리지 않는다.

core boot 식별은 매 시작마다 달라진다. 지속적인 environment 식별은 agent 설정과 저장소가 관리하고, socket 경로가 같다는 이유만으로 이전 프로세스와 동일하다고 간주하지 않는다.

core는 bridge disconnect에서 추가 summary를 즉시 stale로 만든다. 연결만 살아 있고 agentd가 멈춘 경우를 위해 summary에 source freshness와 coordinator lease를 분리해 둔다. 활성 bridge당 heartbeat 하나만 사용하고, lease 만료 timer도 pane 수와 무관하게 하나다. heartbeat가 갱신돼도 provider evidence의 오래된 시각을 새로 쓰지 않는다. 추가 UI가 닫혀 있거나 status를 사용하지 않아도 managed action의 deadline은 적용한다.

## 3. C 코어의 변경 면적

tmux 소스를 독립 엔진처럼 재편하지 않는다. upstream 영역은 layout을 유지하고, 새 코드는 별도 `masil-*.c/h` 파일로 묶는다. 기존 파일에는 의미 있는 관찰·수명 지점에서 짧은 호출만 추가한다.

| 지점 | 추가할 일 | 넣지 않을 일 |
| --- | --- | --- |
| pane PTY read/parse 뒤 | 관심 pane의 generation/dirty bit 갱신 | bytes 복사·JSON·정규식·DB |
| resize/reflow/reset/active screen 변경 | geometry/screen generation 무효화 | 전체 agent 재검사 |
| pane/PTY 생성·respawn·실제 제거 | object/run 수명 사건 또는 resync 필요 표시 | provider 요청, 동기 파일 작업 |
| client attach/detach/선택 변화 | client reference와 논리 view 변화 보고 | 모든 client focus를 다시 계산하는 agent 정책 |
| event-loop extension callback | 제한된 frame 처리·snapshot·guarded 동작 | 긴 loop, socket flush 대기, 외부 응답 대기 |
| opt-in agent menu/status 조회 | 이미 받은 작은 cache 읽기 | provider/agentd 호출, process-tree 검사 |
| managed restore/launch의 child 경로 | strict cwd 처리와 결과 pipe | native tmux spawn의 fallback 변경 |

현재 소스의 `window_pane_read_callback`은 control client fanout 뒤 parser를 호출한다. observer 때문에 새 full-output control client를 붙이면 이 fanout과 출력 보관 비용이 늘 수 있다. 공개 control mode는 호환 기능으로 그대로 두고 내부 관찰은 전용 bridge를 사용한다.

## 4. Event loop와 처리량 제어

C core는 기존 순서를 유지한다. extension은 별도 bounded queue와 callback을 가진다. high-priority 무한 callback으로 입력이나 upstream 작업을 굶기지 않는다. libevent의 같은 priority callback 순서도 의미 이벤트의 ordering 보장으로 사용하지 않는다.

extension 한 번의 처리에는 frame 수·byte 수·wall-time quantum을 함께 둔다. 나머지는 다음 기회로 넘긴다. snapshot을 만드는 도중 기한에 도달하면 불완전한 결과 또는 deferred 결과를 내고 과도한 copy를 계속하지 않는다. 이것은 soft cooperative budget이며 real-time scheduler 보장이 아니다.

Rust loop는 ready queue를 한 번에 끝까지 비우지 않는다. control, native event, snapshot detection, projection, UI fanout에 각각 batch budget을 두고 순환한다. 재시도 timer는 deadline heap으로 모으고, per-pane ticker를 만들지 않는다.

## 5. 명령 대기와 terminal 입력의 분리

masil이 제공하는 binding·메뉴·command prompt 예시는 반드시 **`run-shell -b`**로 helper를 시작한다. foreground `run-shell`은 tmux의 invoking client command queue를 기다리게 하므로 긴 `masil-agent wait`나 fsync가 그 client의 키 처리 앞에 놓일 수 있다.

core bridge의 guarded action도 그 client의 일반 command queue 끝에 대기 작업으로 넣지 않는다. extension 전용 queue에서 순수 검증과 짧은 core 상태 변경만 수행한다. 필요한 upstream primitive를 재사용하되 `CMD_RETURN_WAIT`를 invoking client에 전파하지 않는다. 기존 명령 실행이 비동기인 경우 extension operation의 continuation으로 보관한다.

사용자가 직접 foreground `run-shell`, `wait-for` 등을 실행했을 때의 원래 tmux 대기는 유지한다. `terminal hot path를 막지 않는다`는 원칙은 masil이 추가로 만드는 대기를 금지하는 조건이다.

## 6. Interface 정의

### Terminal core → Core bridge

```text
note_lifecycle(object_ref, kind, generation)
mark_screen_dirty(pane_ref, reasons)
note_client_view(client_ref, session_ref, window_ref, pane_ref)
```

호출자는 bridge를 기다리지 않는다. 관심 없는 pane에서는 빠른 조건 검사로 끝난다. wire serialization은 후속 callback에서 한다. object 제거 전에 필요한 식별자만 값으로 보존하고 raw pointer를 넘기지 않는다.

### Coordinator → Core bridge

```text
inventory(cursor, limit)
watch_objects(scope, event_cursor)
observe_bottom(pane_ref, expected_generation, budget)
guarded_action(operation_key, dispatch_ticket, target_ref, preconditions, action)
publish_summary(pane_ref, summary_revision, bounded_summary)
```

`guarded_action`은 원래 92개 명령의 대체 실행기가 아니다. managed launch, 정확한 target에 대한 제한된 input action, client menu·focus 등 에이전트 기능에 필요한 동작만 허용한다. 일반 tmux 기능은 기존 명령 경로를 사용한다.

### Coordinator → Provider Adapter

```text
bind(run_ref, native_ref, connection_config) -> capabilities
observe(binding) -> normalized facts + source cursor
perform(binding, operation, expected_native_request) -> evidence
reconcile(binding) -> current facts | unsupported | unknown
close(binding, reason)
```

원래 TUI를 관찰하는 Adapter와 별도 headless agent 실행기는 같은 것으로 취급하지 않는다. 관찰 stream이 끝났다고 native process를 자동 종료하지 않는다. state reducer는 provider 이름 분기 대신 사실·capability를 입력받는다.

### Coordinator → Durable store / Work executor

```text
store.commit(batch, durability_class) -> committed sequence | failure
store.read_page(query, bounded_limit) -> rows
work.start(job_ref, argv, cwd_policy, resource_key) -> accepted
work.cancel(job_ref) -> cancellation_requested
```

writer와 worker의 결과는 bounded completion channel로 돌아온다. 작업 예약의 성공을 외부 효과의 완료로 바꾸지 않는다. Interface 수준의 fault injection으로 DB stall, process exit, duplicate request, 연결 상실을 검증한다.

## 7. Strict cwd launch

기준 tmux의 native spawn은 `chdir` 실패 시 home 또는 `/`로 fallback한다. 이 동작은 native tmux 명령에서 유지한다. masil의 관리 launch/restore에는 별도의 내부 `strict_cwd` 실행 문맥을 둔다.

관리 child는 실행 직전에 요청 디렉터리를 열고 그 directory FD를 기준으로 `fchdir`·identity 확인을 한다. 실패하면 대체 디렉터리에서 agent를 실행하지 않고 오류를 전용 pipe로 보고한다. preflight 성공만으로 부모가 launch 성공을 반환하지 않는다. 검사와 실행 사이의 삭제·permission 변경·symlink 교체를 검사한다.

가능한 파일 접근과 shell 초기화는 child에서 수행하고 core event loop를 기다리게 하지 않는다. 부모는 nonblocking pipe·SIGCHLD로 진행을 관찰한다. child-side cwd 준비, exec 시도, native agent 준비 확인은 서로 다른 receipt다. pipe EOF만으로 provider readiness를 확정하지 않는다.

관리 launch 기본은 argv 기반 direct exec다. 사용자가 shell 초기화 경로를 선택하면 shell이 cwd를 다시 바꿀 수 있으므로 초기 directory 검사와 agent readiness 시점의 cwd 확인을 분리한다. 후자를 확인할 수 없으면 cwd 복구 전체를 성공으로 표시하지 않는다. 준비 script와 agent 인자는 shell command 문자열로 섞지 않는다.

실패한 restore target은 기록을 남기고 재시도할 수 있다. 빈 pane/placeholder를 사용한다면 원래 shell이나 새 대화가 실행 중인 것으로 표시하지 않는다. 사용자의 native tmux `remain-on-exit` 동작을 전역 변경하지 않는다.

## 8. Focus와 공통 UI

tmux의 active pane은 window에, current window는 session에 속한다. 같은 session 또는 linked window를 보는 여러 client에서는 `select-pane`가 모두의 화면에 영향을 줄 수 있다.

masil의 client-target focus는 action 직전에 영향받을 client를 같은 core loop에서 계산한다. 다른 client의 선택까지 변하면 `shared_focus_conflict`를 반환한다. 사용자가 공유 범위 변경을 명시한 경우에만 그 범위로 실행한다. 새 독립 per-client pane 선택 모델을 몰래 추가하지 않는다.

기본 agent 목록은 client-owned menu로 제공한다. pane-owned mode를 특정 client만 보는 UI처럼 사용하지 않는다. 긴 목록은 page/filter와 cached summary로 처리한다. 지속적인 overview가 필요하면 사용자가 만든 일반 pane을 사용하고 tmux의 공유 pane 의미를 알린다.

focus 결과에는 `logical_selection_applied`, `redraw_queued`, `tty_output_drained`처럼 관찰 가능한 단계를 사용한다. 마지막 단계도 host terminal의 실제 화면 표시를 확인했다는 뜻은 아니다. core가 보장할 수 없는 `pixels_visible` 성공을 만들지 않는다.

## 9. 메모리와 수명

core의 확장 상태는 등록된 pane별 작은 summary, generation, dirty queue entry와 소수의 전역 bounded buffer다. history나 transcript를 두 벌 보관하지 않는다. snapshot buffer는 pool에서 빌리고 처리 후 돌려준다.

agentd는 actor-owned map에 live binding·최근 observation·waiter·operation 요약만 둔다. 대화 본문과 오래된 event 전체를 RAM에 재구축하지 않는다. DB query는 pagination하고 최근 상태만 cache한다.

DB thread와 helper는 종료 시 각자의 resource를 정리한다. agentd 종료가 core로 process kill을 전파하지 않는다. 의도적인 `stop agent` 요청과 observer shutdown은 별도 action이다.

## 10. 플랫폼과 빌드

C core의 기존 BSD/Linux/macOS/Solaris 동작과 build feature를 유지한다. Rust target tier, Tokio/mio, SQLite binding의 보장은 그와 같지 않다. agentd를 core 실행의 필수 dependency로 만들지 않고, 지원 platform별 compile·runtime gate를 둔다.

기본 개발/성능 검증은 macOS arm64와 Linux x86_64/aarch64부터 시작한다. 다른 tmux platform의 core 호환은 범위에 남고, agentd의 미검증 지원은 명시한다. Windows terminal에서 SSH로 이 core에 접속하는 경로는 별도로 검사한다.

provider 추가 때문에 terminal core build에 Node·Python·JS runtime을 끌어오지 않는다. 기존 agent가 필요로 하는 runtime은 provider의 비용으로 별도 기록한다. 배포 size와 RSS에서는 포함/제외 항목을 함께 보여 준다.
