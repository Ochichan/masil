# 구현된 코어 관찰 IPC

2026-09-28 구현 계약. [최종 protocol 설계](design/protocol.md)의 읽기 전용 부분만 제공한다. 구현은 [rmux-bridge.c](../core/rmux-bridge.c), CLI는 [agent/src/main.rs](../agent/src/main.rs), 통합 검증은 [test_bridge.py](../tests/test_bridge.py)에 있다.

## 연결과 capability

서버 시작 시 `RMUX_BRIDGE_SOCKET`을 설정하면 별도 Unix stream socket을 만든다. 0700 등 소유자만 접근할 수 있는 부모 directory와 같은 UID peer가 필요하다. socket 권한은 0600이며 기존 경로를 덮어쓰지 않는다. listener와 accepted FD는 child에 상속하지 않는다. 종료할 때 생성 당시 device/inode와 같은 socket만 지운다.

frame은 4-byte big-endian payload 길이와 UTF-8 JSON object로 구성한다. 요청에는 `v: 1`, `kind`, 문자열 `request_id`가 있어야 한다. 첫 요청은 `hello`다. 중복 key, 허용하지 않은 최상위 field, 잘못된 UTF-8/JSON, 크기·깊이 초과를 거절한다. CLI는 응답 frame 크기, 중복 key, 깊이, protocol version, request ID를 확인한다.

```json
{"v":1,"kind":"hello","request_id":"1"}
```

응답은 `core_boot_id`, `negotiated_version`, `capabilities`, `limits`를 포함한다. 현재 capability는 `inventory`, `snapshot`, `stats`가 true이고 `watch`, `events`, `actions`, `submit`은 false다. 소비자는 false인 기능을 추정해 호출하거나 native `send-keys`를 관리 요청의 수락 증거로 바꾸면 안 된다.

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

## 진단과 아직 없는 기능

`stats`는 연결·frame·byte·요청·거절·inventory·snapshot·rate limit counter와 현재 TX queue 크기를 반환한다. 사용자 prompt나 terminal text를 counter log에 넣지 않는다.

지금은 event subscription이나 dirty-pane scheduler가 없다. 소비자가 모든 pane을 무한 polling하는 구조를 제품 기본값으로 도입하지 않는다. 다음 구현은 bounded watch/event/gap과 provider identity/event를 연결해야 한다. durable intent, receipt, 승인 fencing, SQLite 및 대화 resume는 이 읽기 전용 socket의 성공 응답만으로 대체할 수 없다.
