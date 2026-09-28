# 관리 요청은 외부 효과 전에 의도를 저장한다

2026-09-28 설계 선택. 관리 prompt·approval·restore의 접수와 dispatch 의도를 SQLite WAL/FULL에 commit한 뒤 외부 효과를 시도하며, 직접 terminal 키 입력은 이 경로에서 제외한다. 메모리 queue만 사용하는 안보다 관리 요청의 시작 비용은 늘지만, 응답 유실·재시작 때 같은 요청을 다시 보내 중복 실행하는 문제를 추적할 수 있다.

외부 효과와 DB commit을 하나의 transaction으로 묶을 수 없으므로 exactly-once를 주장하지 않는다. 불확실한 dispatch는 reconcile하거나 outcome_unknown으로 남긴다. [저장·복구 설계](../design/state-and-recovery.md)에 crash 구간과 retention을 정의한다.
