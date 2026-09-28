# 성능 예산과 측정

이 문서의 수치는 **구현 전 후보 목표와 설계 상한**이다. 측정한 결과가 아니며, 이 문서 작성으로 어떤 gate도 통과하지 않았다. 값 변경은 측정 근거와 영향받는 계약을 함께 기록한다.

## 1. 비용을 나누는 기준

비교 대상은 같은 pinned tmux, compiler, build feature, OS, terminal, history limit, pane geometry와 출력 workload다. rmux의 비용은 다음과 같이 보고한다.

```text
사용자 전체 footprint
  = native core/client + rmux 추가 관리
  + agent/provider runtime + SSH/terminal + 해당 작업의 child process

rmux 추가 관리
  = core 확장 증분 + agentd + rmux helper + SQLite/IPC buffer
```

provider 메모리를 제외한 수치를 전체 메모리처럼 발표하지 않는다. shared page가 중복되는 RSS 합산의 한계도 적고, Linux에서는 PSS, macOS에서는 physical footprint를 함께 기록한다. 파일 cache, mmap, child process, stack reservation과 resident 사용량을 구분한다.

기본 terminal 경로와 agent 기능을 켠 경로를 별도로 측정한다. agent를 쓰지 않을 때 Rust process·DB·provider probe가 실행되지 않아야 한다.

## 2. Extension hard limit 초안

이 표는 rmux가 추가로 할당하는 자원의 초기 기본 상한이다. 기존 tmux의 pane/client/history/clipboard 크기를 제한하는 표가 아니다. 설정으로 높이는 경우 영향받는 합계와 admission 정책도 다시 계산한다.

| 이름 | 기본값 | 포화 시 |
| --- | --- | --- |
| WATCHED_PANES | environment당 512 | 추가 observer 등록 거절. terminal은 정상 사용 |
| CORE_RX_FRAME | 8 KiB | 길이 검사 단계에서 거절 |
| PUBLIC_FRAME | 64 KiB | 큰 메시지를 typed chunk로 나누거나 거절 |
| JSON_DEPTH | 8 | parsing/schema 오류 |
| CORE_CODEC_POOL | 512 KiB | 해당 frame 거절. heap fallback 금지 |
| CORE_GLOBAL_POOLS | 8 MiB 합계 | 낮은 우선 작업 defer, 효과 전 admission 거절 |
| CORE_PER_WATCH | 4 KiB | fixed metadata만 유지. snapshot/body inline 저장 금지 |
| CORE_EVENT_JOURNAL | 1 MiB 또는 4,096 event 중 먼저 | gap·inventory 재동기화 |
| CORE_TX_QUEUE | 512 KiB 합계 | summary 합치기, 우선 reserve 소진 시 disconnect |
| CORE_ACTION_LEDGER | 2,048 entry, 512 KiB | 신규 effect 전에 capacity 거절 |
| CORE_INPUT_STAGING | 1 MiB, 동시 4 payload | 해당 managed input admission 거절 |
| SNAPSHOT_AREA | 하단 최대 32 rows × 240 columns | clipped 영역과 이유 표시 |
| SNAPSHOT_TEXT | decoded UTF-8 최대 16 KiB | 완전한 cell 경계에서 중단, complete=false |
| SNAPSHOT_ENCODED | 최대 64 KiB | escape 후 초과하면 complete=false로 축소 |
| SNAPSHOT_POOL | core/agentd 각각 256 KiB | 기존 snapshot 소비까지 defer |
| MANAGED_PAYLOAD | operation당 256 KiB | 접수 전 거절. native typing/paste 한도와 무관 |
| UPLOAD_STAGING | agentd 합계 2 MiB, 동시 8, idle TTL 30초 | 접수 전 거절/만료 |
| CLI_CONNECTIONS | 32, 미인증 handshake는 별도 최대 4 | 신규 연결 거절 |
| WAITER_COUNT | 1,024 | wait만 거절. 기존 operation 유지 |
| DURABLE_QUEUE | 합계 2 MiB 또는 256 request | 신규 durable action 거절 |
| PROVIDER_BODY | finite 응답 decoded 최대 1 MiB | body 중단, 관찰/요청 오류 |
| PROVIDER_EVENT | event당 64 KiB | source gap 표시, reconnect/reconcile |
| PROVIDER_BUFFER_POOL | 모든 endpoint 합계 4 MiB | 읽기/관찰량 제한. core PTY와 무관 |
| SUBSCRIBER_QUEUES | 전체 1 MiB, 연결별 최대 64 KiB | 느린 subscriber 분리 |
| SQLITE_CACHE | 목표 최대 4 MiB, mmap 비활성 | 실제 SQLite heap 증분 별도 계측 |
| LIVE_CACHE_BUDGET | 8 MiB | 오래된 비필수 projection 제거, 신규 등록 거절 |
| BLOCKING_WORKERS | lazy 최대 2 thread, 입장 대기 8 | semaphore 앞에서 거절/defer |
| EXTERNAL_JOBS | 동시에 최대 2 helper process | resource별 bounded job 대기 |

CORE_GLOBAL_POOLS에는 journal, TX, codec, staging, action ledger, snapshot, inventory buffer를 모두 포함한다. 독립 limit를 단순히 전부 더해 다시 추가 할당하지 않는다. 합계 quota reservation이 먼저다. `CORE_PER_WATCH × WATCHED_PANES`는 별도 최대 2 MiB다. allocator overhead와 executable/stack은 이 pool 숫자 밖이므로 RSS gate에서 추가로 잡는다.

agentd도 buffer ownership을 옮겨 동일 payload의 queue별 복제를 피한다. store transaction·JSON parse·TLS 등의 내부 할당은 quota 밖으로 숨기지 않고 계측한다. library가 hard cap을 직접 제공하지 않는 부분은 입력·동시성·cache 제한으로 묶고 RSS/heap 고수위 검증을 통과해야 한다. 표의 application pool 합계만으로 process RSS hard bound를 주장하지 않는다.

## 3. CPU·timer·batch 기본값

| 항목 | 초기 기본값 | 해석 |
| --- | --- | --- |
| active dirty snapshot | pane당 최대 4 Hz | 변화 없으면 0회. native event가 충분하면 생략 |
| background working snapshot | pane당 최대 1 Hz | 전체 budget이 우선 |
| idle unchanged snapshot | 0 Hz | per-pane poll timer 없음 |
| 전체 snapshot budget | 최대 64회/초, decoded 512 KiB/초 | event-driven token bucket. burst는 최대 128 KiB |
| core extension callback | 4 incoming frame, 32 KiB 또는 200 µs soft quantum | 먼저 도달한 조건에서 yield. 한 bounded parse가 deadline을 넘는 경우 기록 |
| Rust control/reducer batch | 64 event 또는 500 µs soft quantum | 다른 class에 실행 기회 제공 |
| UI summary fanout | 대상별 최대 10 Hz | 바뀌지 않은 값 전송 없음. critical attention도 다음 작은 batch에서 반영 |
| durable group commit | 최대 2 ms 또는 32 request, 256 KiB | 먼저 도달한 조건에서 transaction. effect는 commit 완료 후 |
| inactive source check | shared deadline heap | 각 pane ticker와 CPU 수 비례 worker 금지 |
| coordinator lease | 활성 bridge당 2초 heartbeat, 6초 미수신 시 stale | agent off에서는 timer 없음. native source freshness와 별개 |
| HTTP finite request | connect 2초, total 10초 기본 | provider별 조정 가능. SSE에는 동일 total deadline 적용 금지 |
| reconnect | 0.5초부터 exponential backoff, 최대 30초 + jitter | user-stop이면 예약 취소. offline 구간 API latency와 혼합 금지 |

JSON escaping으로 16 KiB text가 64 KiB를 넘을 수 있다. raw text 한도와 encoded frame 한도를 모두 검사하고, 필요하면 cell 단위로 더 줄인다. frame 크기 제한은 200 µs 실행 보장이 아니다. 최악 입력 parse와 cell copy가 budget을 넘으면 더 작은 frame/영역 또는 incremental 작업으로 조정한다.

CPU 100%는 **logical core 하나를 계속 사용하는 것**으로 표기한다. 256 logical CPU 머신에서 OS 전체 대비 퍼센트만 보고 낮은 사용량이라고 판단하지 않는다. wall time, process CPU seconds, context switch와 wakeup을 같이 보고한다.

## 4. 목표 지표

수치의 적용 fixture는 다음 절의 S0~S8이다. reference hardware의 CPU·RAM·storage·OS·전원 모드를 결과에 고정한다. network/provider 응답 시간은 rmux 내부 처리 지연과 분리한다.

| ID | 후보 통과 목표 | 측정 범위 |
| --- | --- | --- |
| B-01 | agent off에서 추가 Rust process/thread/정기 timer 0 | S0 process tree·timer 계측 |
| B-02 | agent off core RSS 증분 ≤1 MiB, startup p95 증분 ≤max(5%, 2 ms) | stock 대비 같은 build/warm 상태 |
| B-03 | core 입력 처리 p99 증분 ≤max(5%, 0.25 ms), frame/redraw p99 증분 ≤max(5%, 0.5 ms) | S0/S1, 같은 marker·표본 정의 |
| B-04 | agentd RSS 1 agent ≤24 MiB, 50 agents ≤48 MiB, 512 관찰 stress ≤96 MiB | provider·core·helper 제외 및 별도 총합 보고. steady + burst peak 모두 기록 |
| B-05 | core 확장 RSS 증분 50 관찰 ≤3 MiB, 512 stress ≤12 MiB | native history/grid 동일 조건 |
| B-06 | idle 50 agents에서 core+agentd 추가 CPU ≤0.5% of one core, 추가 wakeup 평균 ≤2/초 | native event·출력 없는 controlled fixture, 10분. 실제 provider heartbeat는 별도 표기 |
| B-07 | 50 active fixture의 observer 추가 CPU 평균 ≤15% of one core | aggregate output 1 MiB/초, 120×40, 한 client. baseline CPU도 함께 |
| B-08 | 동시 출력에서 terminal 내부 key→PTY write p99 ≤4 ms이고 stock 대비 증분 ≤1 ms | S3 deterministic fixture. host/SSH RTT 제외 |
| B-09 | hot metadata query p95 ≤10 ms, p99 ≤25 ms | local agentd, 50 agents, DB maintenance 동시. durable admission과 분리 |
| B-10 | durable admission p95 ≤20 ms, p99 ≤75 ms | local SSD/FULL. fsync 지연과 queue 대기 별도 측정 |
| B-11 | 관찰한 실제 target 제거→waiter 결과 enqueue p99 ≤50 ms | healthy local stream. 실제 CLI 출력까지 별도 측정 |
| B-12 | native evidence 수신→projection/attention fanout p99 ≤50 ms | provider 자체 전달 지연 제외 |
| B-13 | screen fallback 감지 p95 ≤500 ms active, ≤2초 background | 완전한 snapshot, 해당 rate 범위의 workload. 포화 시 stale 지표 필수 |
| B-14 | job cancel 요청 접수 p99 ≤25 ms, cooperative helper 종료 p95 ≤2초 | 불가역 단계는 too_late/unsupported로 분류 |
| B-15 | agentd warm start 후 50개 live identity 동기화 p95 ≤250 ms | provider native reconcile 대기는 별도. stale→ready 범위 명시 |
| B-16 | release 바이너리 크기: rmux ≤stock+1 MiB, rmux-agent ≤15 MiB | 같은 strip/link 조건, 지원 feature 목록 포함 |

`max` 형태의 회귀 한도는 측정 noise보다 작은 수치를 과장하지 않기 위한 절대 허용치를 포함한다. 표본 분산이 한도보다 크면 통과가 아니라 측정 불충분이다. B-04/B-16이 실패하면 기능 제거에 앞서 dependency feature·allocation·cache와 build 설정을 점검한다.

## 5. Workload matrix

| ID | workload | 잡아낼 문제 |
| --- | --- | --- |
| S0 | shell pane 1개, agent 확장 off, 시작/attach/키/idle | 기본 기능에 상시 비용 유입 |
| S1 | agent 1개와 일반 editor/build pane, 1~3개 작업 전환 | 작은 사용 규모의 latency·메모리 |
| S2 | 15/50 agent idle, native-event형과 fallback형 구분 | timer·thread·polling의 pane 수 비례 증가 |
| S3 | 50 pane 동시 출력, aggregate 1 MiB/초와 짧은 10 MiB/초 burst | parser 중복·observer backlog·입력 지연 |
| S4 | 50 pane, 1/4/8 clients, 다른 크기와 shared session/window | render fanout·shared focus·slow tty |
| S5 | 200 pane 보통 stress, 512 watch 한도, 한도 밖 native pane | quota·graceful refusal·history 보존 |
| S6 | 대형 worktree 삭제 + 다른 pane 입력 + metadata query | global lock·blocking loop·취소 |
| S7 | DB fsync stall/full, agentd kill, stalled subscriber/provider | 격리·bounded queue·거짓 성공 |
| S8 | SSH RTT 20/100/300 ms, packet loss/reconnect/auth stop | 원격 지연 구분·stale 응답·사용자 의도 |

모든 pane이 같은 TUI 화면을 무한 출력하는 fixture만 쓰지 않는다. 일반 line output, full-screen diff, alternate screen, wide/combining text, color·hyperlink·progress animation을 섞고 seed와 byte 수를 남긴다. 256 CPU 서버 같은 큰 머신에서도 thread 수가 CPU 수를 따라 늘지 않는지 별도 확인한다.

## 6. 측정 도구와 marker

microbenchmark는 bounded JSON parse/write, grid snapshot, detector, reducer, SQLite batch에만 쓴다. 제품 gate는 실제 PTY·tmux server/client·agentd를 연결한 end-to-end fixture로 판단한다.

| marker | 지연 정의 |
| --- | --- |
| input decoded / PTY bytes written | core가 해석한 입력이 실제 PTY FD까지 간 내부 시간 |
| PTY readable / parser complete / tty queue / tty write | output 처리와 client 전달의 구간별 시간 |
| observation source receive / reduce / publish | provider·network 지연을 제외한 관리 비용 |
| admission receive / queue / commit done / receipt emitted | durable 비용과 fsync 병목 |
| target remove / waiter resolved / subscriber write | 종료 신호의 실제 전파 |

monotonic clock을 사용하고 다른 머신 clock을 빼서 latency를 만들지 않는다. terminal emulator의 실제 표시 지연은 별도 관찰 장치가 있어야 측정하며 `tty write`와 동일시하지 않는다.

Linux perf와 OS process accounting, macOS Instruments/동등한 native profiler를 상황에 맞게 사용한다. 제품의 기본 실행에 profiler를 상주시키지 않는다. tracing은 off 또는 저비용 counter가 기본이고 고해상도 span은 bounded ring buffer를 사용한다. prompt·화면은 trace label에 넣지 않는다.

시작 benchmark는 cold/warm을 분리하고 최소 100회 반복한다. idle은 warmup 후 10분, throughput은 workload별 최소 60초씩 여러 반복으로 측정한다. percentile에는 표본 수·분포·outlier를 함께 남긴다. p99를 지지할 표본이 부족하면 반복 수를 늘린다. release build·고정 seed·전원 상태를 유지한다.

## 7. DB·로그·disk 한도

| 항목 | 초기 정책 |
| --- | --- |
| DB main+WAL 합계 | soft 128 MiB, admission high-water 224 MiB, hard 목표 256 MiB |
| WAL | 8 MiB부터 PASSIVE checkpoint 예약, 32 MiB에서 admission 압력, 64 MiB 넘김 방지 |
| managed body | 최대 24시간. reconcile 필요가 끝나면 조기 삭제 대상으로 전환 |
| operation/receipt metadata | 기본 30일. active/unknown 보존은 사용자에게 capacity 영향 표시 |
| diagnostic log | 4 MiB × 4 파일, 민감 body 기본 제외 |
| helper spool | global 16 MiB, job별 quota. native agent 파일은 포함하지 않음 |

DB 크기는 SQLite page/WAL/transaction overhead 때문에 byte 단위 사전 예측이 완전하지 않다. high-water에서 신규 쓰기를 제어하고 가장 큰 허용 transaction·checkpoint·maintenance의 여유 공간을 확보한다. 측정 없이 파일 크기가 hard 값을 절대 넘지 않는다고 주장하지 않는다. disk full은 항상 가능한 외부 실패다.

retention 만료를 실행 성공으로 해석하지 않는다. unresolved operation body가 만료되면 metadata와 outcome_unknown을 남기며 재전송을 막는다. active namespace의 dedup 기록 삭제는 금지한다.

## 8. Gate 실패 시 조정 순서

먼저 같은 일을 두 번 하는 경로, per-byte/per-pane allocation, 무제한 queue, polling, 잦은 process spawn을 제거한다. 다음으로 copied bytes·batch·cache·dependency feature를 줄인다. 그 뒤 실제 profile에서 이득이 확인되는 자료구조·allocator·LTO를 선택한다.

mutex를 lock-free 구조로 바꾸거나 JSON을 임의 binary format으로 바꾸는 최적화는 profile과 failure 검증이 있어야 한다. 빠른 정상 경로만 얻고 generation·durability·오류 의미가 사라지는 변경은 통과하지 않는다.

숫자를 조정해야 한다면 실패 fixture, 원인, 사용자 영향, 대안 비교를 남긴다. tmux 기능 삭제·기본 키 변경·native history 축소·unknown의 idle 치환으로 목표를 맞추지 않는다.
