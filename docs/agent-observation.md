# OpenCode session 관찰

M3의 첫 구현 범위다. `masil-agent serve`는 OpenCode의 native HTTP/SSE 상태와 masil pane의 생존 정보를 메모리에 모은다. 기본 터미널 실행에는 필요하지 않으며 조회 명령으로 자동 시작되지 않는다. tmux 기본 키는 그대로다.

## 시작과 조회

core 관찰 socket을 켠 서버가 먼저 필요하다. [코어 관찰 실행 예제](implementation-status.md#선택적-관찰-기능)를 참고한다. OpenCode TUI가 사용하는 **같은 서버**의 numeric loopback HTTP 주소와 session ID를 명시한다. 실행 중인 TUI와 별도로 `opencode serve`를 시작하면 별개 서버이므로 기존 TUI 상태를 볼 수 없다. OpenCode TUI의 `--hostname`, `--port`로 해당 서버 주소를 고정할 수 있다. [공식 서버 문서](https://opencode.ai/docs/server/)

예시 `sources.json`:

```json
{
  "sources": [{
    "id": "local-opencode",
    "endpoint": "http://127.0.0.1:4096",
    "directory": "/absolute/path/to/project",
    "sessions": [{
      "id": "backend",
      "pane_id": "%0",
      "session_id": "ses_REPLACE_WITH_ACTUAL_ID"
    }]
  }]
}
```

인증을 사용하는 서버에는 source의 `username`과 `password_env`를 추가한다. username 기본값은 `opencode`다. `password_env`는 비밀번호가 들어 있는 환경변수의 **이름**이다. endpoint에 비밀번호를 넣지 않는다. 요청 인증에만 쓰며 응답·로그·config 파일에 비밀번호를 복사하지 않는다.

```sh
./bin/masil-agent --socket "$masil_runtime/manager.sock" serve \
  --core "$masil_runtime/observe.sock" --config sources.json
# 다른 terminal에서:
./bin/masil-agent --socket "$masil_runtime/manager.sock" status
./bin/masil-agent --socket "$masil_runtime/manager.sock" agents
./bin/masil-agent --socket "$masil_runtime/manager.sock" inspect backend
./bin/masil-agent --socket "$masil_runtime/manager.sock" stop
```

`serve`는 foreground 실행이다. `stop`·SIGINT·SIGTERM은 관찰 daemon만 종료한다. core, pane의 TUI, OpenCode 서버는 계속 실행된다. config는 시작할 때 읽고 변경되지 않는다. 대상 변경은 daemon을 종료하고 새 config로 다시 시작한다.

`attention`으로 미확인 승인·질문 목록을 보고 `ack`로 특정 요청 묶음을 확인 처리할 수 있다. `watch-agents`는 전체 현재 목록을 변경 때만 전송한다. 여러 클라이언트가 공유하는 메모리 확인 상태와 정확한 revision 사용법은 [대기 요청 확인](attention.md)에 있다. ack는 provider 승인 응답이 아니다.

## 결과의 의미

| 필드 | 의미 |
| --- | --- |
| `native.exists` | fresh에서 검증된 존재/부재 true/false. 미관측·syncing·stale은 null |
| `native.activity` | working / idle / retrying / unknown. idle은 완료·성공이 아니다 |
| `native.attention` | approval / question / none / unknown. 승인 대기가 질문 대기보다 우선하며 양쪽 count는 남는다 |
| `native.freshness` | syncing / fresh / stale. source 단위로 현재 GET 결과와 SSE 연결의 유효성을 표시 |
| `native.last_activity` | 연결 손실·재동기화 전 마지막으로 관측한 activity. 현재 상태로 쓰지 않는다 |
| `native.parent_session_id` | native session의 parent. 자동 child attention 합산은 제공하지 않는다 |
| `native.provider_version` | `/global/health`가 보고한 실행 서버 버전. session 생성 버전과 구분 |
| `core.process` | running / exited / removed / unknown. provider의 작업 상태와 별도 |
| `core.freshness` | connecting / fresh / stale / gap. core 연결 손실 뒤 자동 재연결하지 않는다 |
| `binding` | explicit_unverified 또는 invalidated. 현재 pane의 TUI가 해당 session을 표시한다는 증명이 아니다 |
| `frontend_verified` | 이번 구현에서는 항상 false |

`inspect`는 source epoch·오류 reason·관측 시각·재조회 횟수·pending ID도 제공한다. `agents`는 짧은 행 목록이다. 관찰 시각·parent·last_activity 등 마지막 근거를 읽을 때는 freshness를 함께 확인한다. pending count가 0이어도 freshness가 없으면 “대기 없음”으로 판단하면 안 된다.

pane PTY 교체·종료·삭제, core stream gap 또는 동기화 후 연결 손실은 association을 `invalidated`로 바꾼다. 이후 출력이 와도 자동으로 유효하게 되지 않는다. provider 관찰은 독립적으로 계속될 수 있다. ID를 잘못 지정하거나 OpenCode TUI에서 다른 session으로 이동해도 이를 현재 UI identity로 확인하는 plugin은 아직 없다.

승인 응답, prompt 전송, 완료 판정, transient error 이력, transcript, durable receipt, 대화 복구는 제공하지 않는다. 모든 capability에서 input / approval / completion / child_aggregation은 false다.

## 연결과 상한

- config 최대 64 KiB, source 1–8개, 총 관찰 1–64개. 같은 endpoint/directory는 하나의 source에 모은다. native session ID는 source 안에서 중복할 수 없다. pane ID는 `%0`처럼 정규 표기만 허용한다.
- endpoint는 numeric loopback plain HTTP만 허용한다. DNS hostname, TLS, proxy, redirect, 원격 접속은 지원하지 않는다. project directory는 시작 전에 canonical path로 고정한다.
- source당 HTTP client와 SSE 하나를 공유한다. 초기 동기화와 관련 native event 이후에만 GET snapshot을 읽는다. idle pane별 HTTP polling은 없다. HTTP 결과와 동시에 도착한 변경 신호가 있으면 재조회하며, 그 사이 상태는 syncing이다.
- finite HTTP 전체 snapshot 5초, body당 256 KiB, SSE inactivity 45초, partial event 5초, event 64 KiB·batch 256 KiB. snapshot 완료 후에는 대기 중인 SSE와 부분 frame을 처리한 경계를 확인해야 fresh가 된다. 이 확인도 5초 안에 끝나지 않으면 stale로 바꾸고 재연결한다. 실패는 0.5–30초 backoff를 적용하며 event replay·유실 구간 복구는 주장하지 않는다.
- source당 pending permission+question 최대 256개. inspect에 남기는 ID는 관찰당 합계 16개이며 총 count·truncation을 함께 제공한다.
- 관리 socket은 owner private directory, 0600, same UID, 최대 32개 동시 client. 요청 8 KiB·응답 64 KiB·읽기/쓰기 각각 3초. 기존 socket 경로는 덮어쓰지 않는다.
- canonical core socket 옆의 `.agentd.lock`에 OS lock을 잡아 중복 관찰 daemon을 거절한다. lock 파일은 종료 후에도 남으며 내용이나 존재가 daemon 생존 여부를 뜻하지 않는다. crash 후 남은 manager socket은 소유자가 process 종료를 확인하고 처리해야 한다.

manager wire framing은 4-byte big-endian 길이 + UTF-8 JSON이다. 기본 요청은 `{v:1,kind,request_id}`이며 inspect는 `id`를 추가한다. attention·ack·watch-agents의 확장 schema는 [목록 계약](attention.md)에 있다. 일반 query는 한 연결에 요청/응답 하나이며 watch-agents만 계속 전송한다. status의 sources/observations는 개수이며, agents의 observations는 배열이다. CLI exit 0은 조회 성공, 2는 요청/연결/프로토콜 오류, 3은 존재하는 socket에 연결할 수 없는 status 또는 stream 손실, 5는 manager error 응답이다. 미시작 status는 `not_started`와 exit 0을 반환한다.

## API 기준과 검증

OpenCode 고정 source `b471c2b4495747353af768fbf2e0790c9d820ce2` 및 설치 버전 1.18.32를 기준으로 했다. read path는 `/global/health`, `/event`, `/session/status`, `/session/:id`, `/permission`, `/question`이다. health를 제외한 요청에는 directory query를 붙인다. 사용한 session API는 해당 버전의 legacy compatibility route이며 `/api/session`의 다른 schema와 혼용하지 않는다. 버전 변경 후에는 fixture와 실제 설치본 smoke를 다시 실행해야 한다.

```sh
make test
cargo fmt --manifest-path agent/Cargo.toml -- --check
cargo clippy --locked --manifest-path agent/Cargo.toml --all-targets -- -D warnings
# 선택적 설치본 확인: 사용자 HOME/config/auth/plugin과 분리하며 모델 요청 없음
python3 tests/smoke_opencode.py --output .build/opencode-smoke.json
.build/bench-venv/bin/python tests/performance/agentd_probe.py \
  --seconds 5 --output .build/agentd-probe.json
```

Tokio current-thread runtime과 `reqwest`의 최소 feature를 사용한다. TLS·압축·HTTP/2·system proxy를 활성화하지 않는다. terminal 입출력과 화면은 daemon을 거치지 않는다. [Tokio 실행 모델](https://tokio.rs/tokio/topics/bridging), [reqwest feature 설명](https://docs.rs/reqwest/0.13.5/reqwest/)

실행 결과와 남은 검증은 [M3 관찰 검증 기록](validation/2026-09-28-agentd.md)에 정리한다.
