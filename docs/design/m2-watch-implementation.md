# M2 실시간 관찰 구현 묶음

2026-09-28. 기준 commit `4381ec2`. 이번 범위는 읽기 전용 코어 관찰을 push stream으로 연결하는 것이다. provider 상태 추정, prompt/approval, DB, daemon은 후속 단계다.

## 계약

- 기존 hello/inventory/snapshot/stats 요청을 유지한다. hello minor=1, watch/events capability=true. actions/submit=false.
- hello 뒤 `watch` 요청은 `v`, `kind`, `request_id`, `expected_core_boot_id`, `pane_ids`를 갖는다. pane ID는 중복 없는 `%N` 1~64개다. 전체 연결에서 서로 다른 관찰 pane 최대 512개. 잘못된 대상/세대/용량은 원자적으로 거절한다. native pane 수는 제한하지 않는다.
- watch 응답은 `kind=watch`, request ID, core boot, 새 `stream_epoch`, `fence_seq`, `scope_revision`, `complete=true`, 선택 pane의 현재 inventory metadata를 포함한다. 최대 64개를 한 event-loop safe point에서 복사하므로 scope baseline은 완전하다. 기존 paginated inventory와 혼동하지 않는다.
- watch 연결은 전용 stream이 된다. 별도 조회는 다른 연결을 쓴다. scope 변경과 재접속은 새로운 watch와 baseline으로 시작한다. 과거 event의 reconnect replay는 제공하지 않는다.
- event frame은 `kind=event`, core boot, stream epoch, 10진 문자열 `event_seq`, `pane_id`, PTY/screen generation, `reason`을 포함한다. reason은 `screen_dirty`, `resized`, `pty_changed`, `exited`, `removed`. event는 현재 정보를 다시 읽으라는 invalidation이다. provider started/done 또는 성공한 respawn을 의미하지 않는다. 처음 ACK의 fence 이하 event는 보내지 않는다. 다른 scope event를 걸러내므로 sequence는 연속일 필요 없이 엄격히 증가한다.
- boot+stream epoch가 다른 event를 기존 projection에 적용하지 않는다. 새 연결에서는 inventory를 새로 만든다. 과거 event를 다시 얻었다고 표시하지 않는다.

## 코어 비용과 누락

- 관찰 pane에만 slot index를 붙인다. PTY output hook은 scalar generation/dirty flag를 O(1) 갱신한다. hook에서 JSON, socket write, grid copy, heap allocation을 하지 않는다.
- 서로 다른 pane 최대 512개 slot과 전역 4,096개 이하의 고정 크기 event ring을 둔다. ring은 1 MiB 안에 들어간다. 사용한 slot만 순회하며 per-client 관심 목록은 최대 64개다.
- 연속 screen dirty는 pane당 최대 4 Hz로 합친다. pending dirty에만 공유 one-shot timer를 걸고 idle일 때 제거한다. lifecycle/resize/PTY invalidation은 순서를 보존하여 ring에 즉시 scalar copy하고, 송신은 다음 bounded callback에서 처리한다. 삭제 hook은 pointer가 아닌 tombstone 값을 보존한다.
- event 전달은 기존 callback frame/byte/soft-time quantum과 global TX cap을 지킨다. journal scan도 별도 visit cap을 둔다. 다른 scope의 event가 많아도 한 callback이 ring 전체를 훑지 않는다. native PTY 읽기를 observer 때문에 중지하지 않는다.
- observer cursor보다 journal이 앞으로 넘어가면 `kind=gap`, boot, stream epoch, `after_seq`, `first_available_seq`, `last_seq`, `code=resync_required`를 보낸 뒤 연결을 닫는다. frame reserve가 없거나 송신이 막히면 연결 종료 자체가 observation loss다. 조용히 최신값만 보내 fresh로 유지하지 않는다.
- generation/sequence overflow는 확장 관찰을 fail closed 한다. slot은 마지막 구독자 종료 때 회수하고 pending timer와 stream buffer도 정리한다.
- stats에 watched pane/subscription/journal/pending dirty 개수, flush timer 활성 여부, event/coalescing/gap counter를 추가한다. idle timer가 없고 자원 회수가 되는지 외부에서 확인한다.

## Rust CLI

`rmux-agent --socket PATH watch [--count N] %0 [%1 ...]`를 제공한다. ACK와 event를 NDJSON으로 즉시 flush한다. count는 event 개수이며 기본은 계속 관찰한다. 조용한 stream은 3초 timeout으로 종료하지 않는다. frame을 받기 시작하면 기존 bounded frame/parse 검증을 적용한다.

watch ACK와 event의 boot/epoch, 숫자 문자열, pane scope, monotonic sequence를 검증한다. gap 또는 예상치 않은 EOF는 관찰 상실을 표시하고 exit 3, malformed protocol은 exit 2다. 자동 재접속으로 gap을 숨기지 않는다. stdout pipe가 닫히면 정상 정리한다. 기존 명령의 동작과 exit 의미는 유지한다.

## 검증과 완료 기준

- [x] 현재 코어/문서/측정 결과를 기준 commit으로 보존.
- [x] C watch admission, baseline fence, event ring, dirty coalescing, bounded pump, gap/cleanup.
- [x] Rust streaming CLI와 잘못된 event를 거절하는 protocol 테스트.
- [x] 실제 PTY에서 watch 이후 출력/resize/respawn/exit/remove, 처음 fence 및 여러 구독자 확인.
- [x] duplicate/invalid/over-limit 등록의 원자성, idle 무송신·무timer, unwatch/EOF 자원 회수.
- [x] 느린 reader와 ring overflow에서 gap 또는 disconnect, 동시에 native 명령/입력 진행.
- [x] 기본 tmux 호환, 기존 bridge/forkpty failure, Rust 및 stream 통합 검증.
- [x] idle watch 1/50개와 출력 부하의 추가 CPU/RSS를 짧게 측정하고 full budget 판정과 구분.
- [x] 독립 검토의 material finding 해결, 구현 계약/사용법/검증 기록 갱신, 두 번째 commit.
