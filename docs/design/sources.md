# 기술 선택의 근거

조사 기준일은 2026-09-28이다. 로컬 source 사실, official library 문서, masil의 설계 판단을 구분한다. 아래 링크는 성능 목표를 달성했다는 증거가 아니다. library version은 구현 시 lockfile과 build 기록으로 고정한다.

## 1. tmux의 실제 실행 경로

모든 tmux source 링크는 로컬 조사 commit `94796f6b1182507efac8a272fc309a79e22e58a5`다. 숫자는 해당 source 위치이며 이후 upstream에서 달라질 수 있다.

| 확인한 사실 | primary source | 설계에 반영한 내용 |
| --- | --- | --- |
| PTY callback은 control/pipe 처리와 기존 parser를 사용 | [window.c:1585](https://github.com/tmux/tmux/blob/94796f6b1182507efac8a272fc309a79e22e58a5/window.c#L1585), [input.c:1026](https://github.com/tmux/tmux/blob/94796f6b1182507efac8a272fc309a79e22e58a5/input.c#L1026) | output 재파싱 없이 완료 뒤 dirty 표시 |
| PTY read 재개는 기존 소비 offset과 연관 | [server-client.c:1895](https://github.com/tmux/tmux/blob/94796f6b1182507efac8a272fc309a79e22e58a5/server-client.c#L1895) | observer를 raw byte 소비자로 등록하지 않음 |
| event sink는 동기 dispatch와 짧은 payload 수명 | [events.c:64](https://github.com/tmux/tmux/blob/94796f6b1182507efac8a272fc309a79e22e58a5/events.c#L64) | scalar copy, safe-point reconcile |
| mode가 pane의 표시 screen을 바꿀 수 있음 | [window.c:1722](https://github.com/tmux/tmux/blob/94796f6b1182507efac8a272fc309a79e22e58a5/window.c#L1722), [window-copy.c:392](https://github.com/tmux/tmux/blob/94796f6b1182507efac8a272fc309a79e22e58a5/window-copy.c#L392) | snapshot은 원래 base grid 사용 |
| active grid에는 history offset이 있고 alternate screen 전이가 있음 | [grid.c:31](https://github.com/tmux/tmux/blob/94796f6b1182507efac8a272fc309a79e22e58a5/grid.c#L31), [screen.c:692](https://github.com/tmux/tmux/blob/94796f6b1182507efac8a272fc309a79e22e58a5/screen.c#L692) | active rows와 screen generation 검증 |
| pane-created가 layout 최종 선택보다 앞서며, pane-resized는 resize 후 보고 | [spawn.c:590](https://github.com/tmux/tmux/blob/94796f6b1182507efac8a272fc309a79e22e58a5/spawn.c#L590), [window.c:1655](https://github.com/tmux/tmux/blob/94796f6b1182507efac8a272fc309a79e22e58a5/window.c#L1655) | lifecycle event와 완성된 snapshot을 구분 |
| foreground run-shell은 client queue를 기다리게 함 | [cmd-run-shell.c:125](https://github.com/tmux/tmux/blob/94796f6b1182507efac8a272fc309a79e22e58a5/cmd-run-shell.c#L125), [cmd-queue.c:719](https://github.com/tmux/tmux/blob/94796f6b1182507efac8a272fc309a79e22e58a5/cmd-queue.c#L719) | masil helper 예시는 run-shell -b |
| active pane/current window는 window/session 공유 상태 | [cmd-select-pane.c:64](https://github.com/tmux/tmux/blob/94796f6b1182507efac8a272fc309a79e22e58a5/cmd-select-pane.c#L64), [cmd-switch-client.c:149](https://github.com/tmux/tmux/blob/94796f6b1182507efac8a272fc309a79e22e58a5/cmd-switch-client.c#L149) | client-only focus의 공유 충돌 명시 |
| native spawn의 chdir 실패에는 대체 경로가 있음 | [spawn.c:469](https://github.com/tmux/tmux/blob/94796f6b1182507efac8a272fc309a79e22e58a5/spawn.c#L469) | managed child에서 별도 strict cwd |
| tty queue와 실제 write, slow client discard는 다른 단계 | [tty.c:221](https://github.com/tmux/tmux/blob/94796f6b1182507efac8a272fc309a79e22e58a5/tty.c#L221), [tty.c:639](https://github.com/tmux/tmux/blob/94796f6b1182507efac8a272fc309a79e22e58a5/tty.c#L639) | physical display 성공을 주장하지 않음 |
| option 변경이 전역 redraw 처리를 유발할 수 있음 | [options.c:1358](https://github.com/tmux/tmux/blob/94796f6b1182507efac8a272fc309a79e22e58a5/options.c#L1358), [format.c:4429](https://github.com/tmux/tmux/blob/94796f6b1182507efac8a272fc309a79e22e58a5/format.c#L4429) | namespace format callback과 작은 cache |
| json.c는 범용 JSON codec과 다른 제한을 가짐 | [json.c:29](https://github.com/tmux/tmux/blob/94796f6b1182507efac8a272fc309a79e22e58a5/json.c#L29), [json.c:939](https://github.com/tmux/tmux/blob/94796f6b1182507efac8a272fc309a79e22e58a5/json.c#L939) | 별도 bounded codec 선택 |
| public control mode에는 no-output과 자체 흐름 제어가 있음 | [control.c:323](https://github.com/tmux/tmux/blob/94796f6b1182507efac8a272fc309a79e22e58a5/control.c#L323), [control.c:582](https://github.com/tmux/tmux/blob/94796f6b1182507efac8a272fc309a79e22e58a5/control.c#L582) | stock wrapper 비교 유지, 전용 bridge와 public control 분리 |

전체 명령·키 기준, Herdr/OpenCode/T3의 고정 source는 [참조 목록](../reference/README.md)에 있다. Herdr 사용자 불만은 [사용자 제공 조사](../research/herdr-feedback-2026-09-28.md)이며 이 표의 source 검증과 증거 수준이 다르다.

## 2. Tokio와 platform

Tokio current-thread runtime은 자체 worker pool 없이 호출 thread에서 future를 구동한다. `spawn_blocking`은 별도 thread를 만들 수 있고, 그 queue는 application의 bounded admission을 대신하지 않는다. 시작한 blocking 작업의 abort도 보장되지 않는다. 따라서 persistent DB worker를 별도 thread로 두고 blocking pool 앞에서 제한한다. [Runtime 문서](https://docs.rs/tokio/latest/tokio/runtime/), [Builder](https://docs.rs/tokio/latest/tokio/runtime/struct.Builder.html), [spawn_blocking](https://docs.rs/tokio/latest/tokio/task/fn.spawn_blocking.html).

Rust target tier와 Tokio/mio runtime 지원은 서로 다르다. tmux가 지원하는 OS 모두에서 같은 dependency가 같은 수준으로 검증됐다고 추정하지 않는다. [Rust platform support](https://doc.rust-lang.org/rustc/platform-support.html), [Tokio supported platforms](https://docs.rs/tokio/latest/tokio/#supported-platforms).

libevent의 높은 priority event는 낮은 priority 처리를 지연시킬 수 있다. 같은 priority의 순서도 protocol ordering으로 의존하지 않는다. [libevent event priority](https://libevent.org/libevent-book/Ref4_event.html#_events_with_priorities).

## 3. SQLite

rusqlite의 bundled feature는 SQLite를 함께 build/link하는 경로다. 지원 환경의 system SQLite 버전 차이를 줄이기 위해 기본 배포에 선택했다. bundled라는 이름만으로 필요한 수정이 포함됐다고 판단하지 않고 실제 SQLite version을 확인한다. [rusqlite 공식 저장소](https://github.com/rusqlite/rusqlite).

WAL은 동시에 writer 하나를 허용하고, 긴 reader는 checkpoint 진행을 막아 WAL 증가를 일으킬 수 있다. checkpoint mode별 blocking/BUSY 동작이 다르므로 writer/maintenance와 짧은 조회를 함께 설계했다. [WAL](https://sqlite.org/wal.html), [wal_checkpoint](https://sqlite.org/pragma.html#pragma_wal_checkpoint).

WAL에서 FULL은 commit의 sync를 요청한다. NORMAL과 전원/OS 장애 durability가 같다고 취급하지 않는다. macOS의 fullfsync 계열 설정도 별도다. [synchronous](https://sqlite.org/pragma.html#pragma_synchronous), [fullfsync](https://sqlite.org/pragma.html#pragma_fullfsync), [checkpoint_fullfsync](https://sqlite.org/pragma.html#pragma_checkpoint_fullfsync).

공식 문서는 WAL reset bug가 3.7.0~3.51.2에 존재했을 가능성과 동시 write/checkpoint 조건을 설명하며 3.51.3 및 일부 backport 수정을 제시한다. 구현 dependency는 수정 포함 여부를 확인해 고정한다. [WAL reset bug](https://sqlite.org/wal.html#the_wal_reset_bug), [3.51.3 release](https://sqlite.org/releaselog/3_51_3.html). 이 기록은 현재 masil에 취약 dependency가 설치됐다는 뜻이 아니다.

## 4. C JSON codec

yyjson 0.13.0은 ANSI C89와 소수 source file로 통합하는 경로를 문서화한다. strict parsing을 유지하고 고정 pool allocator, input byte cap, reader/writer depth cap을 함께 사용한다. duplicate key는 자동 거절되지 않으므로 schema wrapper에서 거절한다. allocator가 있다는 사실만으로 모든 helper가 bounded인 것은 아니다. [고정 release](https://github.com/ibireme/yyjson/releases/tag/0.13.0), [0.13.0 header](https://github.com/ibireme/yyjson/blob/0.13.0/src/yyjson.h), [API 문서](https://github.com/ibireme/yyjson/blob/0.13.0/doc/API.md).

read-memory estimate는 overflow/오류 조건을 확인하고 hard pool과 함께 쓴다. in-situ read는 입력 buffer 수명·padding 의무를 추가하므로 측정 전 기본 최적화로 채택하지 않는다. 해당 library가 공식 외부 보안 감사를 받았다고 이 조사에서 확인한 것은 아니다.

## 5. HTTP와 detector

reqwest Client는 연결 pool을 재사용한다. finite body는 크기를 제한해 stream으로 읽고, SSE에는 finite request의 total timeout을 그대로 적용하지 않는다. read timeout과 전체 deadline의 의미도 구분한다. [Client](https://docs.rs/reqwest/latest/reqwest/struct.Client.html), [ClientBuilder](https://docs.rs/reqwest/latest/reqwest/struct.ClientBuilder.html), [Response bytes_stream](https://docs.rs/reqwest/latest/reqwest/struct.Response.html#method.bytes_stream).

regex의 단일 search와 match 반복 열거는 최악 비용이 다를 수 있다. pattern·compiled size·DFA cache·haystack을 각각 제한하고 boolean 판정에 불필요한 match 열거를 피한다. [Untrusted haystacks](https://docs.rs/regex/latest/regex/#untrusted-haystacks), [RegexBuilder](https://docs.rs/regex/latest/regex/struct.RegexBuilder.html), [Matches](https://docs.rs/regex/latest/regex/struct.Matches.html).

## 6. Source가 결정하지 않는 것

C core + Rust agentd 분리, FULL admission 순서, 관찰 area·Hz·pool 상한, RSS/latency 목표는 masil의 설계 선택이다. upstream의 benchmark나 제공된 Herdr 사례에서 이 숫자를 얻은 것이 아니다. [성능 matrix](performance.md)를 실행해 검증한다.

작은 native runtime이 JavaScript runtime보다 이 제품에서 얼마나 빠를지는 아직 측정하지 않았다. 현재 선택의 근거는 runtime 이름 자체가 아니라 raw output 중복 처리 제거, 기본 기능의 지연 활성화, shared-state 소유권, bounded 작업량이다.
