# masil 용어

masil은 tmux 방식의 터미널 작업 공간에 에이전트의 작업 상태와 조작을 추가한다. 터미널 구성과 에이전트 대화를 구분해 부른다.

## 터미널 구성

**Environment**:
연결 대상 masil server와 그 server가 접근하는 실행·파일 작업 환경이다. client가 있는 머신과 다를 수 있다.
_Avoid_: client의 로컬 환경과의 혼용

**Server**:
터미널 세션과 실행 중인 작업을 관리하는 masil 인스턴스다.
_Avoid_: 모델 서버, provider

**Client**:
server에 접속해 화면을 보거나 명령을 전달하는 연결 주체다.
_Avoid_: 에이전트

**Session**:
tmux 의미의 session이며, window들의 연결과 현재 선택을 가진 작업 단위다.
_Avoid_: 대화, agent session

**Window**:
하나 이상의 pane과 그 배치를 가진 단위다. 같은 window가 여러 session에 연결될 수 있다.
_Avoid_: pane, 운영체제 창

**Pane**:
터미널 작업을 표시하고 입력을 전달하는 단위다. 타일 배치 또는 floating 배치에 속할 수 있다.
_Avoid_: agent session, task

**Session group**:
window 연결 목록을 함께 관리하는 tmux session들의 묶음이다.
_Avoid_: 에이전트 팀

**Workspace**:
에이전트 작업이 참조하는 프로젝트 디렉터리나 checkout을 나타내는 masil의 추가 개념이다.
_Avoid_: tmux session의 새 이름

**Worktree**:
같은 Git 저장소의 별도 checkout으로, 작업 디렉터리를 분리하는 단위다.
_Avoid_: branch 자체, pane

## 에이전트 작업

**Provider**:
masil이 실행하거나 연동하는 코딩 에이전트 제품이다. 여기서 Codex·Claude Code·OpenCode는 provider이고, 그 안에서 선택하는 모델은 별개다.
_Avoid_: 모델 공급 API와의 혼용

**Agent session**:
provider가 식별하는 대화와 작업 이력의 연속이다. pane을 다시 만들거나 다른 client에서 보더라도 같은 agent session일 수 있다.
_Avoid_: 수식어 없는 session

**Agent run**:
특정 agent session을 원래 TUI로 실행하는 한 번의 수명이다. 같은 대화를 resume해도 새 agent run이다. 별도 native engine이 살아 있더라도 종료된 TUI의 실행 수명이 이어지는 것은 아니다.
_Avoid_: agent session, turn

**Agent binding**:
특정 pane의 현재 실행과 native agent session을 근거에 따라 연결한 관계다. 같은 pane에서 다른 에이전트를 실행하면 이전 연결과 구분한다.
_Avoid_: cwd나 pane 이름이 같다는 이유만으로 확정한 연결

**Turn**:
사용자 입력에 따라 에이전트가 수행하는 한 차례의 응답·도구 작업 단위다.
_Avoid_: 화면 출력 한 번

**Task**:
사용자가 완료하려는 작업 목표다. 하나 이상의 turn이나 agent session에 걸칠 수 있다.
_Avoid_: pane의 동의어

**Observation**:
에이전트 상태를 판단하는 근거다. provider 이벤트, integration 보고, 프로세스 상태, 화면 증거를 포함한다.
_Avoid_: 근거 없이 확정한 상태

**Observation freshness**:
관찰 근거가 현재 실행을 설명하는지, 오래됐거나 중간 사건이 누락됐는지 나타내는 정보다.
_Avoid_: 에이전트가 일하고 있는지 여부

**Capability**:
현재 연결된 provider와 연동 방식이 실제로 제공하는 관찰 또는 조작 기능이다.
_Avoid_: 제품 이름만으로 추정한 지원

**Attention**:
승인, 질문, 오류, 완료 확인 등 사용자가 살펴볼 이유가 생긴 상태다.
_Avoid_: 모든 idle 상태

**Attention acknowledgement**:
특정 사용자 범위에서 특정 attention 사건을 확인했다는 기록이다. 작업의 실행 상태나 명령 접수 확인과는 다르다.
_Avoid_: approval, 모든 client의 읽음 상태

**Operation receipt**:
masil의 특정 관리 요청이 어느 단계까지 확인됐는지 나타내는 기록이다. 요청 접수, 전달, provider 수락, 실행, 종료는 서로 다른 사실이다.
_Avoid_: 외부 작업 완료의 포괄적인 success

**Operation key**:
사용자 범위와 실행 환경 안에서 같은 관리 요청의 재시도를 식별하는 값이다. 같은 문장을 의도적으로 새로 보내는 요청과 구분한다.
_Avoid_: 입력 본문의 동일성, provider turn ID

**Outcome unknown**:
외부 효과가 발생했을 수 있지만 현재 증거로 성공이나 실패를 확정할 수 없는 결과다.
_Avoid_: 다시 실행해도 된다는 뜻의 실패

**Approval request**:
provider가 특정 작업의 허용 여부를 묻는 요청이다. 일반 질문이나 입력 가능한 composer와 다르다.
_Avoid_: 모든 blocked 상태

## 연결과 복원

**Detach**:
client가 session에서 접속을 해제하는 동작이다. server와 작업의 종료를 뜻하지 않는다.
_Avoid_: stop, kill

**Resume**:
provider가 기존 agent session의 이력을 사용해 작업을 이어가는 동작이다.
_Avoid_: 기존 프로세스가 살아남았다는 의미의 복원

**Restore**:
저장한 배치·디렉터리·식별 정보 등을 바탕으로 작업 환경을 다시 구성하는 동작이다.
_Avoid_: 무조건적인 작업 재실행

**Restore result**:
복구 대상별 배치·작업 디렉터리·실행·native 대화 연결의 결과다. 일부 대상이나 일부 단계만 성공한 경우도 구분한다.
_Avoid_: 배치만 복원한 뒤의 전체 복구 성공

**Checkpoint**:
특정 시점의 작업 파일 상태를 다시 확인하거나 복원하기 위한 기록이다. agent session의 대화 이력과는 별개다.
_Avoid_: 대화 rollback, 일반 Git branch commit
