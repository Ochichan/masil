# M3 shared attention·live projection 검증

2026-09-28, macOS 26.6.2 arm64. 기준 commit `b3ecd5e` 이후 승인·질문 목록, 여러 클라이언트가 공유하는 메모리 확인, 전체 상태 snapshot stream을 구현했다. C core, tmux 기본 키, provider 제어 capability는 변경하지 않았다. [사용법](../attention.md), [구현 범위](../design/m3-attention-implementation.md).

## 결과

| 검사 | 최종 결과 |
| --- | --- |
| `make test` | 통과 |
| stock tmux 공개 동작 비교 | 10개 통과 |
| 기존 bridge·watch CLI·forkpty 실패 주입 | 통과 |
| core watch integration | 8개 통과 |
| 기존 agentd integration | 9개 통과 |
| 새 attention integration | 6개 통과 |
| 새 projection CLI adversarial 검사 | 3개 통과 |
| benchmark helper | 6개 통과 |
| Rust 단위 검사 | 28개 통과 |
| `cargo fmt -- --check` / `cargo clippy --all-targets -- -D warnings` | 통과 |
| 격리된 설치 OpenCode 1.18.32 GET smoke | 통과 |

[전체 검사 로그](data/2026-09-28-attention-tests.log), [clippy 로그](data/2026-09-28-attention-clippy.log), [설치본 결과](data/2026-09-28-attention-opencode.json)를 보존했다. 설치본 smoke는 별도 HOME/XDG/cwd/config/auth 환경의 빈 서버와 missing native session을 사용한다. 모델 작업, 실제 TUI route, provider 승인 응답을 실행하지 않았다.

## 확인한 경계

- 서로 다른 관리 socket 연결에서 같은 ack를 읽었다. 재확인은 같은 결과이며 provider pending 요청과 GET-only 동작은 유지됐다.
- 공개 목록의 16개 상한 밖 pending ID를 바꾸어도 새 revision을 부여하고 이전 ack를 거절했다. ID 순서만 바뀌면 기존 확인을 보존했다.
- producer가 성공한 A→B→A snapshot을 watch channel에 게시하고 소비자가 마지막 A만 읽는 경우에도 generation으로 예전 확인을 무효화했다. native session 부재·재등장과 source reconnect도 구분했다.
- ack 요청 전에 source channel만 갱신하고 projection 알림을 처리하지 않은 경우, ack 경로가 최신 source를 다시 읽어 오래된 revision을 거절했다.
- source freshness가 없으면 `unavailable`에 표시하고 ack를 거절했다. daemon 재시작 뒤 이전 epoch도 거절했다.
- live snapshot은 초기 상태·확인 변경을 전송했다. 화면 출력만 변했을 때는 동일한 목록을 재전송하지 않았다.
- 최대 길이 ID를 가진 64개 항목이 64 KiB frame 안에 들어갔다. streaming 16개를 채워도 query/stop은 응답했고, 닫힌 idle 연결은 자리를 반환했다.
- 64개 항목과 읽지 않는 구독자들에 확인 변경을 반복했다. 원래 socket을 닫지 않은 상태에서 전송 deadline 후 새 구독자가 들어오는 것으로 정리를 확인했다.
- CLI는 revision 건너뛰기를 허용하고 중복·역행, epoch/scope 변경, malformed metadata, 초과 frame을 거절했다. EOF와 잘린 frame을 정상적인 count 완료로 표시하지 않았다.

독립 검토에서 초기 subscriber snapshot 복사본을 연결 수명 동안 보관하는 경로를 찾았다. 초기 전송 후 해당 복사본을 해제하고 전체 검사와 자원 측정을 다시 실행했다. 최종 코드 검토에 남은 차단 사항은 없다.

## 짧은 자원 측정

동일 release binary로 source 하나에 mock native session 1개 또는 50개를 등록했다. 초기 동기화·구독 이후 100ms를 기다리고 약 5초 동안 idle RSS·누적 CPU·thread 수를 읽었다. core는 cat pane 하나와 필요한 빈 pane으로 구성했다. 아래 RSS는 daemon만 포함하며 provider fixture와 core/terminal child는 제외한다.

| 관찰 대상 | live 구독자 | agentd RSS | thread | native SSE |
| --- | ---: | ---: | ---: | ---: |
| 1 | 0 | 3.23 MiB | 1 | 1 |
| 50 | 0 | 4.80 MiB | 1 | 1 |
| 1 | 16 | 3.44 MiB | 1 | 1 |
| 50 | 16 | 4.91 MiB | 1 | 1 |

네 구간 모두 idle HTTP 재조회는 0회였다. 측정 종료 시 새 데이터 또는 EOF가 읽기 대기 중인 구독자 수도 0이었다. 이 metric은 frame 수가 아니라 socket readiness다. `ps` 누적 CPU 차이는 표시 해상도에서 0.00초였으며 실제 CPU가 절대 0임을 뜻하지 않는다.

release 파일 크기는 1,918,880 bytes다. [구독자 0개 원시 결과](../benchmarks/data/2026-09-28-attention-idle-0.json)와 [구독자 16개 원시 결과](../benchmarks/data/2026-09-28-attention-idle-16.json)에 binary hash·시간·byte 수를 남겼다. 측정값 변경 없이 readiness metric 이름을 명확히 정리했다.

```sh
python3 tests/performance/agentd_probe.py --seconds 5 --subscribers 0 \
  --output .build/attention-idle-0.json
python3 tests/performance/agentd_probe.py --seconds 5 --subscribers 16 \
  --output .build/attention-idle-16.json
```

반복 표본이나 장시간 hot source 성능 gate는 아니다. 최대 크기 목록과 느린 구독자는 기능 검사로 확인했지만, 해당 부하의 지속 CPU·RSS budget을 측정하지는 않았다. 실제 50개 AI 프로세스·endpoint 비용, Linux/BSD 동작, durable 읽음 상태도 이번 결과로 보장하지 않는다.

## 다음 범위

원래 TUI frontend의 pane/PTY 소유권과 session 전환을 증명하는 binding handshake, native menu/focus, durable 확인·완료 이력과 prompt/승인 제어가 남아 있다. [OpenCode TUI source 조사](../research/opencode-tui-binding-2026-09-28.md)는 후속 구현의 근거이며 현재 `frontend_verified=false`는 유지한다.
