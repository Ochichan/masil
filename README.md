# rmux

rmux는 tmux의 기능과 기본 키 조작을 갖추고, 각 코딩 에이전트의 원래 TUI를 pane 안에서 실행·관리하는 터미널 멀티플렉서다.

현재는 tmux 전체 소스를 기반으로 터미널 코어, 읽기 전용 관찰 IPC, 선택적 경량 agentd를 구현했다. `bin/rmux`로 session·window·pane·attach/detach·copy mode·control mode를 사용할 수 있다. `rmux-agent watch`는 pane 변경·종료·누락을, `serve`는 OpenCode native session의 작업·승인·질문 대기를 관찰한다. prompt 전송, 승인 처리, 대화 복구는 다음 단계다.

`attention`은 승인·질문 대기 목록을, `ack`는 daemon 수명 동안 클라이언트들이 공유하는 확인 표시를 제공한다. `watch-agents`로 전체 목록의 최신 상태를 계속 받을 수 있다. 확인 명령은 provider 요청을 승인하지 않는다.

`rmux-agent ui`는 검색·필터·상세 보기·확인 표시·pane 이동을 제공하는 터미널 관리 화면이다. `sidebar`는 34열 사이드패널을 열고 tmux 줌으로 확대한다. 마우스 선택·메뉴·휠·스크롤바·구분선 드래그, 영어·한국어, 어두운·밝은·터미널 테마를 지원한다. [관리 화면 사용법](docs/agent-desk.md)을 참고한다.

```sh
make                 # 코어와 선택적 Rust 관찰 CLI
./bin/rmux -L main new-session -s work
```

기본 prefix는 `C-b`이며 고정한 tmux의 기본 키와 옵션을 유지한다. 기존 `.tmux.conf` 탐색도 유지한다. 시스템 tmux와 Herdr 설치는 변경하지 않는다. 빌드 전제, 관찰 기능 사용법, 검증 범위는 [현재 구현](docs/implementation-status.md)에 있다.

macOS 초기 비교에서 50-pane idle 서버 RSS는 rmux 4.44 MiB, stock tmux 4.39 MiB, 설치된 Herdr 0.8.2는 48.61 MiB였다. 입력 왕복 중앙값은 각각 2.67 / 2.65 / 16.68 ms였다. 단일 pane 출력 CPU와 짧은 burst에서는 Herdr가 앞섰다. [측정 조건과 전체 결과](docs/benchmarks/2026-09-28.md)는 agent 관리 계층이 없는 현재 코어의 비교이며 최종 성능 gate 통과를 뜻하지 않는다.

## 확정된 방향

- tmux에서 가능한 기능은 rmux에서도 모두 가능해야 한다.
- 기본 키보드 설정은 tmux의 기본 설정이어야 한다. 에이전트 기능이 기존 키를 차지하지 않는다.
- Claude Code, Codex, OpenCode 등은 원래 TUI를 유지한다.
- Herdr의 터미널 실행 관리와 T3 Code의 제공자별 공통 제어 방식을 참고한다.
- 빠르고 가벼운 기본 경로를 위해 tmux C/libevent 코어와 지연 시작하는 Rust 에이전트 관리 프로세스를 분리한다.

최종 범위와 단계별 구현 순서는 구분한다. 먼저 구현하지 않은 tmux 기능도 최종 지원 의무에 남는다.

전달·상태·복구 결과는 확인한 단계까지만 표시한다. 사용자가 제공한 Herdr 불편 조사를 바탕으로, 부분 성공과 미확인 결과를 드러내고 중복 실행을 피하는 신뢰성 요구를 추가했다.

## 선택한 설계

`rmux`는 tmux의 PTY·VT parser·grid·history·키 처리·renderer를 그대로 소유한다. 장기 설계의 `rmux-agent`는 관찰·관리 명령·SQLite 저장을 담당한다. 현재 Rust CLI는 core 관찰과 명시적으로 시작하는 OpenCode 관찰 daemon을 제공한다. native session과 pane의 연결은 아직 사용자 지정 association이며 현재 TUI identity를 증명하지 않는다. 입력과 출력 byte가 Rust나 DB를 거치지 않으며 제품 안에 두 번째 terminal parser를 두지 않는다.

에이전트 기능을 끄면 추가 Rust process와 정기 polling이 없어야 한다. 켰을 때는 변경된 pane만 bounded snapshot으로 읽고, 느린 관찰·DB·파일 작업이 terminal을 기다리게 하지 않는다. 초기 목표는 agentd RSS 1 agent에서 24 MiB 이하, 50 agents에서 48 MiB 이하이며 core와 provider의 비용은 별도 계측한다. [첫 관찰 daemon 측정](docs/validation/2026-09-28-agentd.md)은 관리 기능 전체의 성능 gate와 구분한다.

[아키텍처부터 읽기](docs/architecture.md) → [상세 실행 설계](docs/design/runtime.md) → [성능 예산](docs/design/performance.md) → [구현 순서](docs/design/implementation-plan.md).

## 문서

| 문서 | 다루는 내용 |
| --- | --- |
| [현재 구현과 실행](docs/implementation-status.md) | 빌드·사용법, 실제 기능과 남은 작업 |
| [에이전트 관리 화면](docs/agent-desk.md) | 사이드패널·확대 화면, 마우스·키보드, 영어·한국어 |
| [UI 상호작용 설계](docs/ui/agent-desk.md) | 화면 구성, 입력 소유권, 상태·복구·반응형 계약 |
| [Debian·SSH 검증](docs/validation/2026-09-28-debian-ssh.md) | 원격 Linux 빌드, 실제 SSH 마우스·한국어·사이드패널, 수정과 재현 방법 |
| [코어 관찰 IPC](docs/core-observation.md) | 구현된 wire protocol·상한·generation·실패 의미 |
| [OpenCode 관찰 daemon](docs/agent-observation.md) | 시작·조회·중지, 상태와 연결의 한계, 자원 상한 |
| [대기 요청 확인과 live 목록](docs/attention.md) | 미확인 목록, revision별 shared ack, snapshot stream |
| [확인·스트림 검증](docs/validation/2026-09-28-attention.md) | 동시 클라이언트·재접속·최대 범위·느린 구독자 |
| [관찰 daemon 검증](docs/validation/2026-09-28-agentd.md) | 실제 core 통합·OpenCode smoke·idle 비용 |
| [실시간 관찰 검증](docs/validation/2026-09-28-watch.md) | watch·용량·누락·backpressure·추가 비용 |
| [코어 검증 기록](docs/validation/2026-09-28-core.md) | 실제 테스트 결과, upstream 공통 실패와 재검증 |
| [Herdr 성능 비교](docs/benchmarks/2026-09-28.md) | 조건별 실측과 원시 결과, 해석의 한계 |
| [성능 중심 아키텍처](docs/architecture.md) | 선택한 언어·스택, 대안 비교, 데이터 흐름과 불변식 |
| [상세 실행 설계](docs/design/runtime.md) | 프로세스·thread·Module Interface·C 변경 지점 |
| [IPC와 결과 계약](docs/design/protocol.md) | framing·identity·generation·backpressure·receipt·권한 |
| [상태와 복구](docs/design/state-and-recovery.md) | SQLite schema·crash 구간·중복 방지·restore·원격 의도 |
| [화면과 관찰](docs/design/terminal-and-observation.md) | 기존 grid 재사용·dirty scheduling·provider Adapter·입력/UI |
| [성능 예산](docs/design/performance.md) | 메모리·큐·CPU·지연·저장 상한, workload와 측정법 |
| [구현 계획](docs/design/implementation-plan.md) | source tree 제안·milestone·fault fixture·upstream 유지 |
| [기술 근거](docs/design/sources.md) | 고정 tmux source와 official library 문서 |
| [제품의 최종 목표](docs/product-goals.md) | 완성된 rmux의 사용 경험, 필수 요구, 단계별 도달점 |
| [tmux 호환 계약](docs/tmux-compatibility.md) | 전체 기능 범위, CLI·설정·스크립트·control mode 호환 |
| [기본 키보드 계약](docs/default-keybindings.md) | prefix, 키 테이블, copy mode, 환경별 기본값, 충돌 금지 |
| [에이전트 통합 목표](docs/agent-integration.md) | 원래 TUI 보존, 세션 식별, 상태 관찰, 승인·입력·복구 |
| [신뢰성 계약](docs/reliability.md) | 결과 단계, 중복 방지, 다기기 확인, 입력·clipboard·복구·원격 실패 처리 |
| [완료 판정과 검증](docs/acceptance.md) | 요구사항별 비교 검증과 최종 출시 기준 |
| [용어](CONTEXT.md) | tmux session과 agent session 등 혼동하기 쉬운 개념 |
| [참조 기준](docs/reference/README.md) | 고정한 소스 버전과 추출 목록의 범위 |
| [Herdr 조사 반영 기록](docs/research/herdr-feedback-2026-09-28.md) | 제공된 사례·조건·후속 정정과 rmux 요구의 연결, 미확인 근거의 구분 |

핵심 선택은 [코어/agentd 분리 ADR](docs/adr/0001-tmux-core-rust-agentd.md)와 [durable 관리 요청 ADR](docs/adr/0002-durable-management-intent.md)에 기록했다.

tmux의 초기 비교 기준은 로컬 클론의 `94796f6b1182507efac8a272fc309a79e22e58a5`, `next-3.9` 개발판이다. 기준을 고정한 것은 검증을 재현하기 위해서다. 이후 tmux 기능과 기본값 변화도 추적한다.
