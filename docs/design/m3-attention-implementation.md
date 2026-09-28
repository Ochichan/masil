# M3 shared attention과 실시간 projection

기준 commit `b3ecd5e`. 이번 묶음은 이미 관찰하는 OpenCode session 중 승인을 기다리거나 질문을 보낸 대상을 찾고, 같은 daemon을 사용하는 클라이언트들이 확인 상태를 공유하도록 한다. 기본 key·terminal core·provider 제어는 바꾸지 않는다.

## 제공하는 명령

- `attention [--all]`: 기본은 아직 확인하지 않은 approval/question 목록. `--all`은 확인한 pending 항목도 포함. 응답은 현재 상태의 projection이며 완료 이력이 아니다.
- `ack ID --epoch E --revision R`: attention 응답의 daemon epoch와 해당 항목 revision이 현재와 같고 source가 fresh일 때만 확인 처리. 반복 확인은 같은 결과. 새 pending 요청·source 재접속·요청 종류 변경에는 새 revision을 부여한다. 다른 클라이언트에서도 동일한 확인 결과를 읽는다.
- `watch-agents [--count N]`: 전체 관찰 목록을 NDJSON으로 즉시 한 번 내보내고 변경 때 최신 snapshot을 보낸다. 각 frame에 epoch/revision/complete=true를 포함한다. 중간 snapshot은 합쳐질 수 있으며 history나 delivery receipt가 아니다. 출력 없는 pane별 polling은 없다.
- 기존 `agents`/`inspect` 행에 attention 확인 metadata를 추가한다. provider 작업 상태와 사용자의 확인 여부는 별개다. 조회·ack는 provider 입력이나 승인을 실행하지 않는다.

## 상태와 수명

pending fingerprint는 source epoch와 **전체** permission/question ID 집합으로 만든다. 공개 inspect의 16개 ID 상한과 확인 대상을 혼동하지 않는다. 순서만 달라진 집합은 같은 사건이며, 17번째 이후 ID 교체도 새 사건이다. source당 pending 합계 256개 상한을 유지한다.

성공한 provider snapshot을 source watch channel에 게시하기 **전** 세션별 pending generation을 기록한다. A→B→A가 소비자에게 마지막 A로만 전달돼도 generation이 달라 이전 확인을 적용하지 않는다. ack를 처리할 때는 대기 중인 변경 알림과 무관하게 source의 최신 값을 다시 읽고, 같은 동기 실행 구간에서 freshness·identity 검증과 확인 처리를 끝낸다.

fresh가 아닌 source는 현재 pending/ack를 조작할 수 없다. attention 응답은 stale/unknown 관찰 대상도 별도로 표시해 빈 목록을 "아무도 기다리지 않음"으로 오해하지 않게 한다. core의 stale/invalidated association은 provider pending 정보와 별도로 남긴다. `frontend_verified=false`와 `explicit_unverified`의 기존 의미를 유지한다.

확인은 단일 daemon 수명 동안만 RAM에 보관한다. daemon epoch는 시작마다 난수로 새로 만들며 오래된 daemon의 ack는 거절한다. source reconnect 뒤 같은 ID가 보여도 새 revision으로 미확인 처리한다. 재시작을 넘어 유지되는 읽음·완료 알림은 M4의 durable store 범위다.

## 자원과 프로토콜

source/core의 watch channel 변화를 하나의 최신 projection channel로 모은다. queue는 최신 상태 하나이며 읽기 전용 subscriber가 느려도 source/core를 기다리게 하지 않는다. 조회는 RAM만 읽는다. 의미 없는 core 화면 출력 변화는 projection을 재전송하지 않는다.

관리 client 전체 32개 중 streaming watch 최대 16개로 제한해 query/stop을 위한 자리를 남긴다. frame 64 KiB, 요청 8 KiB, 송신 deadline 3초를 유지한다. subscriber가 출력하지 못하면 해당 연결만 종료한다. watch는 전체 snapshot이므로 sequence를 건너뛰는 것이 허용되지만 epoch 변경·역행·malformed·EOF는 성공 종료가 아니다. `--count`에 도달했을 때만 정상 종료한다. stdout BrokenPipe는 일반 CLI pipeline 종료로 처리한다.

## 검증과 완료 조건

- [x] 순서 변경/공개 ID 상한 밖 교체/재접속/오래된 ack의 reducer 테스트.
- [x] daemon 전체 공유 확인, unknown 명시, live snapshot, slow subscriber 분리.
- [x] CLI argument/frame validation과 end-to-end 실제 private core + mock provider.
- [x] 기존 전체 검사, fmt, clippy와 설치 OpenCode smoke.
- [x] idle 비용과 source/SSE 공유 유지 확인. 짧은 probe는 최종 성능 gate가 아니다.
- [x] 독립 검토, 사용법·구현 상태·검증 기록 갱신, local commit.

TUI route binding, native attention menu·focus, 완료 알림, durable ack, prompt·승인 제어는 이 묶음과 구분한다. binding 조사 결과는 다음 구현의 근거로 남기며 source/core identity를 확인하지 않은 frontend를 verified로 바꾸지 않는다.
