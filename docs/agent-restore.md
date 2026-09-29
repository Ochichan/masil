# 에이전트 저장과 복원

`masil-agent agent save PATH`는 현재 masil 서버에서 감지하거나 관리하는 에이전트를 JSON 스냅샷으로 저장한다. 스냅샷에는 형식 버전, 에이전트 이름, provider, 작업 디렉터리, 저장 당시 pane의 boot ID·PTY generation·pane ID·run ID, provider가 보고한 native session reference가 들어간다. masil이 시작할 때 기록한 원래 추가 인자는 정확히 구분할 수 있을 때만 저장한다.

터미널 화면, 관찰 결과, process 환경 변수, credential 저장소는 스냅샷에 넣지 않는다. 명령줄 인자는 스냅샷에 남을 수 있으므로 비밀값을 명령줄 인자로 전달하지 않아야 한다.

스냅샷 디렉터리는 현재 사용자가 소유하고 권한이 `0700`처럼 다른 사용자에게 닫혀 있어야 한다. 파일은 `0600`으로 같은 디렉터리의 임시 파일에 쓴 뒤 원자적으로 바꾼다. 심볼릭 링크, 다른 사용자의 파일, group 또는 world 권한이 있는 파일은 읽거나 덮어쓰지 않는다. 복원 기록이 이미 있는 경로에는 새 스냅샷을 덮어쓰지 않는다.

`masil-agent agent restore PATH`는 native session reference가 있는 항목만 새 window에서 시작한다. session reference가 없는 항목을 새 세션으로 시작하려면 `--allow-fresh`를 명시해야 한다. 복원은 저장된 pane을 respawn하거나 제거하지 않는다. 같은 저장 boot ID, PTY generation, pane ID가 아직 실행 중이거나 같은 이름 또는 native session이 이미 관리 중이면 새 process를 만들지 않는다.

복원 결과의 `native_session_requested`는 요청 사실만 뜻한다. `native_session_verified`는 `false`이며 provider가 실제로 기존 대화를 열었거나 작업을 성공시켰다는 뜻이 아니다.

## 복원 receipt

복원은 스냅샷 옆의 `.<스냅샷 파일명>.restore.json`에 별도 receipt를 쓴다. 입력 스냅샷은 바꾸지 않는다. 각 process를 시작하기 전에 `pending`을 fsync하고, 시작 결과를 받은 뒤 pane ID와 run ID를 포함한 `started`를 fsync한다.

- `started` 항목은 같은 스냅샷을 다시 복원해도 건너뛴다.
- 남아 있는 `pending` 항목은 결과가 불명확한 것으로 보고한다. 자동으로 다시 제출하지 않는다.
- 작업 디렉터리나 executable 확인처럼 process 시작 전 실패가 증명된 항목은 `failed_not_started`로 기록한다. 사용자가 문제를 고친 뒤 restore를 다시 실행하면 그 항목만 재시도한다.
- 시작 호출 뒤 오류가 난 항목은 `unknown`으로 보고하고 pending 상태를 유지한다.

save와 restore는 정규화한 스냅샷 경로마다 같은 잠금을 사용한다. 따라서 `..`가 포함된 별칭 경로도 같은 파일로 처리되며, 다른 masil 서버에서 같은 스냅샷을 복원하거나 save와 restore를 동시에 실행할 수 없다. restore는 서버별 잠금도 함께 사용하므로 같은 masil 서버에서는 서로 다른 스냅샷의 restore도 하나씩 실행된다. 충돌한 요청은 새 process를 만들거나 스냅샷과 receipt를 바꾸기 전에 실패한다. 결과는 항목마다 `stage`, `action`, `error`를 제공하므로 일부 항목 실패가 다른 안전한 항목의 처리를 숨기지 않는다.

전체 호출 결과의 stage는 `restore_finished`다. `all_started`, `partial`, `counts`가 모든 항목이 새로 시작했거나 이미 실행 중인지, 실패·불명 상태·fresh 거부가 남았는지를 따로 표시한다.
