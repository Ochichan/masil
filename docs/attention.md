# 대기 요청 확인과 실시간 목록

OpenCode 관찰 daemon에 `attention`, `ack`, `watch-agents`를 추가했다. 같은 daemon을 사용하는 클라이언트는 확인 상태를 공유한다. 확인은 사용자가 목록을 읽었다는 표시이며 provider의 승인·질문에 응답하지 않는다. tmux 기본 키와 terminal 화면은 변경하지 않는다.

## 사용법

먼저 [OpenCode 관찰 daemon](agent-observation.md)을 시작한다.

```sh
./bin/rmux-agent --socket "$rmux_runtime/manager.sock" attention
./bin/rmux-agent --socket "$rmux_runtime/manager.sock" attention --all
./bin/rmux-agent --socket "$rmux_runtime/manager.sock" watch-agents
./bin/rmux-agent --socket "$rmux_runtime/manager.sock" watch-agents --count 3
```

`attention`의 `items`는 fresh인 native session 중 아직 확인하지 않은 approval/question 항목이다. `--all`은 확인한 pending 항목도 포함한다. `unavailable`에는 연결 확인 중·연결 손실·native session 부재 때문에 현재 pending 여부를 판단할 수 없는 관찰 대상이 들어간다. `items`가 비어 있어도 `unavailable`이 있으면 전체가 대기 중이 아니라고 결론 내릴 수 없다.

확인할 때는 응답의 최상위 `epoch`와 **해당 행의 `attention.revision`**을 전달한다. 최상위 `revision`은 전체 목록의 변경 번호이므로 확인 명령에 쓰지 않는다.

```sh
# E와 R을 attention 응답의 epoch 및 items[i].attention.revision으로 바꾼다.
./bin/rmux-agent --socket "$rmux_runtime/manager.sock" ack backend --epoch E --revision R
```

같은 revision에 대한 재확인은 같은 결과다. provider 요청은 계속 pending이며, `agents`, `inspect`, `attention --all`에서 확인 표시와 원래 승인/질문 상태를 함께 볼 수 있다. 새 pending 요청을 관찰하면 확인 표시를 해제한다. 부모·자식 session은 별도 항목이며 child 대기를 parent에 합산하지 않는다.

`agents`와 `inspect`의 각 행에 다음 metadata가 추가됐다.

```json
"attention": {
  "revision": "4",
  "acknowledged": false,
  "pending": true,
  "available": true
}
```

available=false일 때 acknowledged는 마지막 확인 기록일 뿐이며 현재 요청이 확인됐다는 의미가 아니다. provider freshness와 core/binding 유효성은 기존처럼 따로 읽는다. binding이 unverified/invalidated인 행을 현재 pane TUI의 검증된 상태라고 표시하지 않는다.

## 오래된 확인을 거절하는 기준

- daemon 시작마다 새 난수 epoch를 만든다. 이전 daemon의 확인은 `wrong_epoch`다.
- 같은 관찰 대상의 전체 permission/question ID 집합, native session의 존재, source 연결 epoch가 바뀌면 새 attention revision을 사용한다. 공개 inspect에서 생략한 17번째 이후 ID도 비교한다.
- 성공한 provider snapshot을 합치기 전에 세션별 generation을 기록한다. A→B→A가 클라이언트에 마지막 A로만 보이더라도 이전 A의 확인을 재사용하지 않는다.
- 단순 재조회에서 잠깐 syncing인 것은 요청 해제의 증거가 아니다. 마지막 확인은 보관하되 그동안 새로운 ack는 거절한다.
- ack 처리 시 최신 source 상태를 먼저 읽는다. 이미 바뀐 요청에는 `stale_revision`, 확인할 근거가 없으면 `observation_unavailable`, pending이 없으면 `no_attention`을 반환한다.

이 기록은 **daemon 메모리에만** 있다. daemon 재시작 후에는 미확인으로 시작한다. durable 읽음 이력, 완료 알림과 ack 저장은 아직 구현하지 않았다. native event 사이에서 한 번도 snapshot으로 관측하지 못한 일시적 요청을 이력처럼 재구성하지 않는다.

## 실시간 스트림 계약

`watch-agents`는 등록된 모든 관찰 대상의 현재 snapshot을 즉시 전송하고, 이후 변경 때 최신 snapshot을 NDJSON으로 출력한다. `--count N`은 초기 snapshot을 포함한 양수 개수다. 지정하지 않으면 연결 종료까지 기다린다.

각 wire frame은 기존 4-byte big-endian 길이 + JSON이다. 요청은 `{v:1,kind:"watch-agents",request_id}`이고 응답은 `{v:1,kind:"agents_snapshot",request_id,epoch,revision,complete:true,observations:[...]}`다. `complete`는 등록 범위가 모두 실렸다는 의미이며, 각 source의 freshness를 보장하지 않는다.

전체 revision은 엄격히 증가한다. 중간 snapshot을 합칠 수 있어 번호를 건너뛰는 것은 정상이다. history·요청 전달 receipt·모든 변화의 전송 보장은 없다. CLI는 epoch·scope 변경, 역행/중복 revision, malformed frame을 오류로 종료한다. EOF는 관찰 손실 exit 3, 형식 오류는 exit 2, 구독 용량 거절은 exit 5다. 지정한 count에 도달하거나 stdout pipeline이 닫혔을 때는 exit 0이다.

전체 32개 관리 연결 중 streaming은 최대 16개다. 각 전송은 3초 제한이며 느린 구독자만 정리한다. idle 구독자가 연결을 닫아도 즉시 자리를 반환한다. source/core 변경 알림은 변화가 있을 때만 25ms 동안 합치며 query/ack는 이 지연을 기다리지 않고 최신 상태를 읽는다. 목록 내용이 같으면 화면 출력만 바뀐 것으로 snapshot을 재전송하지 않는다. provider HTTP를 추가 polling하지 않는다.

현재는 CLI 목록과 JSON 스트림이다. native menu·pane 이동·기본 키 추가는 포함하지 않는다. [검증과 자원 측정](validation/2026-09-28-attention.md).
