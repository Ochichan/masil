# Native Agent 관리

`masil-agent agent`는 현재 masil 서버와 명시적으로 등록한 다른 서버의 코딩 Agent를 찾고 관리한다. OpenCode HTTP 관찰 daemon 설정 없이 사용할 수 있다. 원래 Agent의 TUI와 tmux 기본 키는 유지한다.

비교 기준은 로컬 Herdr commit `0d5d6f1f317e238c8297076bc6ab5c3a0cd56283`다. 이 문서의 지원 범위는 구현과 테스트를 기준으로 하며, 모든 Agent의 최신 설치본을 실행 검증했다는 뜻은 아니다.

## 시작

masil 안에서는 현재 서버 socket을 사용한다. 밖에서는 `--socket /path/to/masil.sock`을 추가한다. 이전 빌드로 실행 중인 서버는 새 기능에 필요한 identity format이 없으므로 새 빌드로 서버를 다시 시작해야 한다. 살아 있는 작업을 종료하지 말고 별도 socket의 새 서버에서 먼저 확인할 수 있다.

```sh
masil-agent agent providers
masil-agent agent list
masil-agent agent start backend codex --cwd /path/to/project
masil-agent agent start review claude --cwd /path/to/project --split %0
masil-agent agent start resumed codex --cwd /path/to/project --session SESSION_ID
masil-agent agent ui
masil-agent agent sidebar
```

`start`는 새 창 또는 명시한 pane 옆의 split을 만든다. shell에 실행 문자열을 타이핑하지 않고 인자 배열로 프로그램을 시작한다. 이름은 소문자로 시작하는 1–32자의 소문자·숫자·`-`·`_`다. 같은 이름으로 동시에 실행을 요청하면 하나만 등록할 수 있다. 경로·실행 파일을 확인할 수 없으면 실행을 시작하지 않는다.

원래 TUI를 직접 실행한 pane도 전경 프로세스로 식별되면 목록에 나온다. `attach %N NAME`으로 이름을 붙인다. 이름 등록은 native 대화 ID를 확인했다는 뜻이 아니다.

## 조회와 조작

```sh
masil-agent agent get backend
masil-agent agent explain backend
masil-agent agent read backend --history
masil-agent agent rename backend api
masil-agent agent focus api
masil-agent agent send-keys api C-c
masil-agent agent draft api '검토할 내용'
# get 결과의 run을 사용한다.
masil-agent agent prompt api '검토를 시작해 주세요' --run RUN --operation 1
masil-agent agent prompt-receipt api --run RUN --operation 1
masil-agent agent wait api --state blocked --timeout 30
masil-agent agent resume api --name continued
```

`read`는 현재 terminal 화면이며 `--history`는 최대 200행의 최근 scrollback을 포함한다. provider의 대화 transcript API는 아니다. 명령 출력은 JSON이다. 오류는 stderr와 exit 2, wait의 연결 상실·대상 소멸·실행 교체는 exit 3, timeout은 exit 4다. `wait` 성공은 요청한 상태를 관측했다는 뜻이며 task 성공을 뜻하지 않는다. `--after-change`는 현재 상태에서 한 번 변화한 뒤 조건을 만족할 때 반환한다.

`send-keys`와 `interrupt`는 명시적인 키 전달이다. 반환값은 `keys_delivered` 또는 `interrupt_key_delivered`이며 provider 수락이나 작업 취소 완료를 주장하지 않는다. `close`는 해당 pane을 종료한다. 화면에서는 interrupt·close를 확인한 뒤 실행한다.

`draft`는 텍스트를 `masil-agent-draft` tmux buffer에 준비한다. `prompt`와 화면의 Send prompt는 idle 상태와 전경 Agent를 확인한 뒤 paste와 Enter를 전달한다. 여러 줄이나 Tab이 있으면 native bracketed paste가 필요하다. blocked·working·unknown 또는 교체된 실행에는 보내지 않는다. copy mode나 pane 동기화가 켜져 있거나 준비 중 화면·제목·progress가 바뀌면 전송을 거절한다. paste가 실패하면 Enter를 보내거나 delivered로 기록하지 않는다. 결과의 `delivered`는 provider 수락이나 turn 시작을 뜻하지 않는다.

네트워크 재시도에는 최초 조회의 `--run RUN --operation N`을 고정한다. 같은 이름의 Agent가 다시 실행되면 이전 run의 재시도와 receipt 조회를 거절한다. 같은 번호와 내용은 기존 receipt를 돌려주고 다시 입력하지 않는다. 번호를 생략하면 새 요청이다. 최근 16개 receipt를 보관하며 만료된 번호나 pending 요청은 자동 재전송하지 않는다. receipt는 해당 pane/run 수명 안에서만 조회할 수 있다. [R-02 입력 계약](reliability.md)에 전달 보장의 범위를 기록했다.

`focus`에는 연결된 client가 필요하다. 여러 client가 있으면 `agent --client CLIENT focus TARGET`처럼 지정한다. 다른 client의 공유 선택을 바꿀 수 있는 충돌은 기존 native helper가 거절한다.

## 상태와 확인

24종 provider 명령·별칭과 22개 bundled 감지 manifest를 제공한다. manifest는 Herdr의 고정한 source에서 가져왔으며 [라이선스](reference/herdr-agent-LICENSE.txt)를 포함한다. 전경 프로세스, 실제 live pane 화면, OSC 제목, 선택적 run 보고를 근거로 `idle`, `working`, `blocked`, `unknown`을 표시한다. 감지 규칙이 맞지 않으면 `unknown`이다. 단순히 출력이 멎었다는 이유로 완료를 표시하지 않는다.

`blocked`는 일반적인 입력 필요 상태다. 승인 또는 질문으로 단정하지 않는다. 기존 OpenCode 관찰 daemon의 구조화된 permission/question 관찰은 별도 경로로 계속 제공한다.

같은 실행에서 working 뒤 idle을 관측하면 `Returned idle`로 확인 목록에 남긴다. 이는 관측한 상태 변화이며 task 성공이나 provider의 완료 receipt가 아니다. 처음 관측한 idle은 새 완료로 표시하지 않는다.

관찰한 상태 변화마다 pane의 revision을 갱신한다. 읽음 표시는 서버의 pane option으로 공유하며 해당 revision에만 적용한다.

```sh
# get 결과에서 run과 revision을 읽은 다음 사용한다.
masil-agent agent ack api --run RUN --revision REVISION
```

서버 boot ID, pane PTY generation, 전경 프로세스 그룹, 등록한 run을 구분한다. respawn·서버 재시작·같은 shell 안의 Agent 재실행 뒤에는 이전 상태를 재사용하지 않는다. 키 전달 직전에도 전경 명령과 실행 identity를 native 명령 큐에서 확인한다.

## 감지 규칙과 integration

`$XDG_CONFIG_HOME/masil/agent-detection/PROVIDER.toml` 또는 `~/.config/masil/agent-detection/PROVIDER.toml`로 감지 규칙을 교체할 수 있다. 잘못된 override는 오류로 표시하며 조용히 무시하지 않는다. 새 CLI 명령과 새 화면은 현재 파일을 읽는다. `agent reload`는 override의 유효성을 검사한다.

선택적 native session/lifecycle 보고와 export는 [integration 문서](agent-integrations.md)를 참고한다. provider 설정은 사용자가 명시적으로 적용해야 하며 masil은 조회나 실행만으로 기존 설정을 덮어쓰지 않는다. 복구 파일과 중복 실행 방지는 [재개 문서](agent-restore.md)를 참고한다.

목록의 provider·상태·workspace 필터와 정렬을 서버에 저장할 수 있다. [저장한 목록 보기](agent-views.md)를 참고한다. 목록 필터는 개별 target 조회나 실행 이름 중복 검사에 영향을 주지 않는다.

## 자원과 검증 범위

Agent 화면이 열려 있는 동안만 서버별로 1초 간격의 native inventory 조회를 예약한다. 로컬·원격 조회는 독립적으로 진행되어 느린 서버가 다른 목록을 막지 않는다. Agent의 화면 generation·제목·실행 identity가 같으면 화면 capture와 규칙 평가를 재사용한다. 닫힌 화면을 위한 polling이나 자동 시작 daemon은 없다. 관리는 서버당 최대 64 pane, native 응답당 최대 64 KiB, 명령당 3초로 제한한다. terminal 입출력 byte는 Rust 관리 코드를 통과하지 않는다.

자동 검증은 private masil 서버, 합성 manifest와 모델을 호출하지 않는 provider fixture를 사용한다. 실제 Claude/Codex 등 설치본의 최신 TUI·hook 호환성, SSH와 다른 OS에서의 새 관리 기능은 별도 live 검증이 필요하다.

## Herdr와 남아 있는 차이

| 항목 | 현재 masil |
| --- | --- |
| provider catalog·manifest 감지 | 24종 catalog, 22종 manifest, local override와 explain |
| 실행·목록·조회·이름·이동·키·대기 | native CLI와 관리 화면. 시작은 새 창/split |
| native resume | 지원 provider의 session ref를 사용한 새 실행. 요청한 ref와 실제 접속 확인은 구분 |
| snapshot/restore | 명시적 파일 저장·복구, 항목별 결과와 durable pending receipt |
| prompt 자동 제출 | 사용자 결정에 따라 idle·전경 실행 확인 뒤 paste+Enter, operation receipt와 중복 방지 |
| provider integration | callback bridge와 export. 설치본별 live 호환 검증은 미완료 |
| completion/done | working→idle 확인 알림. 작업 성공으로 승격하지 않음 |
| custom agent view | provider·상태·workspace 필터와 정렬 저장·초기화 |
| 다중 원격 서버 Agent 통합 | 로컬 + 최대 8개 endpoint, 통합 목록·상태·조작, stale 표시, 원래 native TUI 연결 |

원격 endpoint 등록·통합 목록·SSH 연결 방법은 [원격 Agent 서버](agent-endpoints.md)에 있다. 실제 provider 설치본과 실제 SSH 호스트에서의 인증·버전 조합은 별도 live 검증 대상이다.
