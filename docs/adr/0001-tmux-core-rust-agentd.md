# tmux 코어와 Rust 에이전트 관리를 별도 프로세스로 둔다

2026-09-28 설계 선택. tmux 전체 기능·기본 키와 원래 agent TUI를 보존하기 위해 pinned tmux C/libevent 코어를 작은 patch로 확장하고, 추가 제어·관찰은 지연 시작하는 Rust agentd에 둔다. 독립 mux 재작성과 단일 C 프로세스에 provider/DB를 넣는 안을 검토했으며, terminal bytes의 중복 처리와 에이전트 장애가 기본 입력에 미치는 영향을 줄이는 쪽을 선택했다.

fork 유지와 두 실행 파일의 비용을 부담한다. stock control-mode wrapper 비교가 같은 계약을 더 작은 변경으로 충족하면 core patch를 줄인다. 상세 판단과 검증 조건은 [아키텍처](../architecture.md), [구현 계획](../design/implementation-plan.md)에 있다.
