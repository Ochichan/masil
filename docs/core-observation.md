# 구현된 코어 관찰 IPC

2026-09-28 구현 계약. [최종 protocol 설계](design/protocol.md)의 읽기 전용 부분만 제공한다. 구현은 [masil-bridge.c](../core/masil-bridge.c), CLI는 [agent/src/main.rs](../agent/src/main.rs), 통합 검증은 [test_bridge.py](../tests/test_bridge.py)에 있다.

## 연결과 capability

서버 시작 시 `MASIL_BRIDGE_SOCKET`을 설정하면 별도 Unix stream socket을 만든다. 0700 등 소유자만 접근할 수 있는 부모 directory와 같은 UID peer가 필요하다. socket 권한은 0600이며 기존 경로를 덮어쓰지 않는다. listener와 accepted FD는 child에 상속하지 않는다. 종료할 때 생성 당시 device/inode와 같은 socket만 지운다.

frame은 4-byte big-endian payload 길이와 UTF-8 JSON object로 구성한다. 요청에는 `v: 1`, `kind`, 문자열 `request_id`가 있어야 한다. 첫 요청은 `hello`다. 중복 key, 허용하지 않은 최상위 field, 잘못된 UTF-8/JSON, 크기·깊이 초과를 거절한다. CLI는 응답 frame 크기, 중복 key, 깊이, protocol version, request ID를 확인한다.

```json
{"v":1,"kind":"hello","request_id":"1"}
```

응답은 `core_boot_id`, `negotiated_version`, `capabilities`, `limits`를 포함한다. 현재 version은 1.1이며 `inventory`, `snapshot`, `stats`, `watch`, `events`가 true이고 `actions`, `submit`은 false다. 소비자는 false인 기능을 추정해 호출하거나 native `send-keys`를 관리 요청의 수락 증거로 바꾸면 안 된다.

## 상한과 scheduling

| 자원 | 구현 상한 |
| --- | --- |
| 요청 frame | 8 KiB |
| 응답 frame | 64 KiB |
| JSON depth | 8 |
| JSON parser pool | 재사용하는 512 KiB 고정 pool |
| 연결 | 전체 32, hello 미완료 4 |
| 송신 대기 | 연결별 64 KiB, 전체 512 KiB |
| inventory page | 64 pane, 다음 page 확인을 포함해 최대 65개 방문 |
| watch scope | 연결당 1~64 pane, 전체 512개 서로 다른 pane |
| event journal | 4,096개 fixed scalar record, 1 MiB 미만 |
| event journal scan | callback당 최대 256 record 방문 |
| screen dirty 알림 | pane당 최대 4 Hz, idle 0 Hz |
| snapshot 영역 | pane base screen 아래쪽 32행 × 왼쪽 최대 240열 |
| snapshot decoded text | 16 KiB, cell의 UTF-8 중간에서 자르지 않음 |
| snapshot 빈도 | 전체 공유 64 requests/s |
| snapshot byte | 전체 공유 512 KiB/s, 128 KiB burst |
| stalled 연결 | handshake, 부분 frame, 막힌 송신에 5초 deadline |

callback의 읽기·쓰기·요청 수에 quota를 둔다. 느린 관찰 client의 무한 queue를 만들지 않는다. idle authenticated 연결에 주기 polling을 붙이지 않는다. snapshot token은 요청 시 monotonic time으로 보충한다. token 초과는 `rate_limited`로 반환하고 무기한 대기열에 넣지 않는다.

이 상한은 구현된 방어 장치다. 아직 모든 조합의 장시간 adversarial load를 실행한 것은 아니다. native terminal 성능과 동시에 검증할 부하 행렬은 남아 있다.

## identity와 inventory

core 시작마다 새로운 `core_boot_id`를 만든다. pane ID만 저장해 두고 재시작 후 같은 pane으로 취급하면 안 된다. `pty_generation`은 PTY의 실행 수명이 바뀔 때 증가하며 `screen_generation`은 화면 변경·resize·reset·alternate screen 전환 등에 증가한다. 응답에서는 정수 정밀도 손실을 피하려고 10진 문자열로 보낸다.

```json
{"v":1,"kind":"inventory","request_id":"2"}
```

응답은 `revision`, `panes`, `next_cursor`, `complete`를 포함한다. pane 항목에는 `pane_id`, 두 generation, width/height, dead가 있다. continuation에는 이전 응답의 `revision`과 `next_cursor`를 각각 `revision`, `cursor`로 넣는다. revision이 달라졌으면 `resync_required`이며 처음부터 다시 읽는다. red-black tree의 cursor 탐색과 최대 65개 방문으로 각 page의 처리량을 제한한다.

`dead`는 terminal process lifecycle 정보다. provider의 작업 완료나 입력 대기 상태를 의미하지 않는다. revision/generation이 정수 범위를 소진하면 기존 값을 재사용하지 않고 실패한다.

## snapshot과 불완전한 화면

```json
{"v":1,"kind":"snapshot","request_id":"3","pane_id":"%0"}
```

선택적으로 `expected_core_boot_id`, `expected_pty_generation`, `expected_screen_generation`을 보낼 수 있다. 불일치는 각각 `boot_mismatch`, `pty_generation_mismatch`, `screen_generation_mismatch`로 거절한다. 삭제된 대상은 `target_gone`이다.

응답에는 pane identity/generation, 전체 width/height, cursor, `screen_kind`, 읽은 source 영역, `clipped`, `complete`, `text`가 있다. copy mode overlay를 보고 있어도 application의 `wp->base`를 읽는다. history 전체나 raw PTY stream은 전달하지 않는다. 한 행이라도 범위 밖이면 `complete=false`다. 정상적인 39행 pane에서도 32행 cap 때문에 false일 수 있다. 이를 작업 완료나 관찰 실패로 해석하지 않는다.

출력 없이 child가 종료되어 remain-on-exit banner가 추가될 때도 screen generation을 바꾼다. 강제 respawn은 기존 PTY를 파괴하기 전에 generation을 바꾸므로 새 fork가 실패해도 이전 실행과 혼동하지 않는다. 이 두 경로는 별도 회귀 테스트로 확인했다.

## 실시간 watch

```json
{"v":1,"kind":"watch","request_id":"watch","expected_core_boot_id":"hello의 boot ID","pane_ids":["%0","%2"]}
```

같은 연결에서 hello 뒤 요청한다. scope에 중복·없는 pane·용량 초과가 있으면 전체 등록을 거절한다. 선택한 최대 64개 pane의 metadata를 한 safe point에서 읽어 `complete=true`인 watch ACK로 반환한다. ACK에는 `stream_epoch`, `fence_seq`, `scope_revision`, `panes`가 있다. 세 숫자는 10진 문자열이며 epoch는 core boot 안에서 증가한다. 기존 전체 inventory pagination과 별도의 원자적인 선택 scope baseline이다.

이후 연결은 전용 push stream이다. 다른 조회는 별도 연결을 사용한다. scope 변경은 연결을 닫고 새 watch로 시작한다. reconnect replay나 모든 pane의 자동 등록은 지원하지 않는다. 새 watch마다 새 baseline과 epoch를 받고, 놓친 과거 이력을 복원했다고 표시하지 않는다.

`event` frame은 `v`, `kind`, `core_boot_id`, `stream_epoch`, `event_seq`, `pane_id`, `pty_generation`, `screen_generation`, `reason`을 갖는다. unsolicited frame에는 `request_id`가 없다. event sequence는 ACK의 fence보다 크고 stream에서 엄격히 증가한다. 다른 scope의 event를 걸러내므로 숫자가 연속일 필요는 없다.

| reason | 의미 |
| --- | --- |
| `screen_dirty` | base screen이 바뀌었으므로 필요한 snapshot을 다시 읽음 |
| `resized` | geometry/screen generation 변경 |
| `pty_changed` | 이전 PTY identity 무효화. 새 process 실행 성공을 의미하지 않음 |
| `exited` | native pane process 종료 관찰. provider task 완료 판정과 별개 |
| `removed` | pane 제거. pointer 대신 scalar tombstone 전송 |

event는 재조회가 필요한 이유를 알린다. screen text나 provider 상태 projection은 보내지 않는다. 연속 출력은 slot의 generation/dirty bit로 합치고 공유 one-shot timer에서 발행한다. lifecycle은 순서를 보존한다. respawn·exit·remove 앞의 pending dirty는 취소하여 과거 실행의 알림이 뒤늦게 따라오지 않게 한다. 마지막 구독자가 닫히면 slot을 반환하고, pending dirty가 없으면 timer를 제거한다. 제거된 pane의 slot도 마지막 구독자가 종료할 때까지 scope에 남는다.

느린 관찰자의 cursor가 journal 밖으로 밀리면 `gap`과 `resync_required`를 전송하고 연결을 닫는다. gap에는 `after_seq`, `first_available_seq`, `last_seq`가 있다. `after_seq`는 다른 scope까지 스캔한 서버 cursor여서 마지막 수신 event보다 클 수 있다. 보낼 공간이 없거나 TX가 5초간 막히면 연결 종료로 loss를 알린다. gap/EOF 이후 기존 projection을 fresh로 유지하면 안 된다.

```sh
./bin/masil-agent --socket /private/path/observe.sock watch %0 %2
./bin/masil-agent --socket /private/path/observe.sock watch --count 5 %0
```

CLI는 ACK와 event를 NDJSON으로 즉시 flush한다. `--count`는 event 수이며 0은 baseline만 출력한다. 바뀌지 않는 pane은 계속 기다린다. 첫 frame byte를 받은 뒤에는 3초 완료 deadline을 적용한다. boot/epoch/scope/sequence/schema를 검증하고 gap 또는 예상치 않은 EOF는 exit 3, 잘못된 protocol은 exit 2, 서버의 등록 거절은 exit 5다. stdout pipe가 닫히면 정상 종료한다. 자동 재접속으로 loss를 숨기지 않는다.

실행 결과는 [M2 검증 기록](validation/2026-09-28-watch.md)에 있다.

## 진단과 아직 없는 기능

`stats`는 연결·frame·byte·요청·거절·inventory·snapshot·rate limit counter와 현재 TX queue 크기를 반환한다. 사용자 prompt나 terminal text를 counter log에 넣지 않는다.

watch 관련 top-level 값은 `watched_panes`, `watch_subscriptions`, `journal_events`, `pending_dirty`, `flush_timer_active`다. watch_subscriptions는 관찰 중인 연결 수다. `counters.events`, `counters.coalesced`, `counters.gaps`는 10진 문자열이다.

다음 구현은 이 관찰 stream에 provider identity/event를 연결해야 한다. durable intent, receipt, 승인 fencing, SQLite 및 대화 resume는 이 읽기 전용 socket의 성공 응답만으로 대체할 수 없다.
