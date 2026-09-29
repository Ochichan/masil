# 에이전트 관리 UI 검증

기준 환경은 macOS 26.6.2 arm64다. 고정 tmux 기반 코어와 실제 agentd를 private socket으로 실행하고, 모델 호출이 없는 HTTP/SSE fixture로 관찰 데이터를 만들었다. yututui는 소스와 화면만 읽었고 실행·설정·음악 재생은 하지 않았다.

## 구현 범위

`ui`는 Ratatui/Crossterm 관리 화면, `sidebar`는 기존 masil 창에 직접 여는 34열 pane이다. 확대는 owned pane의 tmux zoom을 사용한다. 이 버전의 tmux 팝업은 창 전체가 공유하는 modal이므로 사용하지 않는다. 메뉴와 도움말은 UI 안에서 처리한다.

에이전트 목록·검색·필터·상세·확인 표시·pane 이동, 클릭·더블클릭·우클릭·휠·스크롤바·구분선 드래그, 영어·한국어, 세 가지 테마를 구현했다. 기본 tmux 명령 92개와 키·옵션을 유지했다. 코어에는 boot/PTY identity를 읽는 format 두 개만 추가했다.

## 실행한 검사

| 검사 | 결과 |
| --- | --- |
| `make test` | 전체 통과. 호환 10, core watch 8, agentd 9, attention 6, stream CLI 3, benchmark 도구 6, UI PTY 8 및 bridge/watch CLI/forkpty 실패 주입 포함 |
| 최종 Rust 테스트 | 62개 통과 |
| 최종 UI PTY 테스트 | 8개 통과 |
| `cargo clippy --locked --all-targets -- -D warnings` | 통과 |
| `cargo fmt --check`, `git diff --check` | 통과 |
| 최소·경계 크기 렌더링 | 너비 1–105, 높이 1–25 조합을 TestBackend로 검사 |

전체 suite 이후 추가한 경계 수정은 Rust와 UI PTY 전체를 다시 실행했다. 마지막 문구 수정 후에도 두 suite와 Clippy를 다시 통과했다.

실제 PTY에서는 다음 결과를 확인했다.

- 마우스 버튼을 누른 뒤 밖에서 놓으면 확인하지 않는다. 정상 클릭은 backend의 공유 확인 상태를 바꾼다. provider POST는 발생하지 않는다.
- UI 메뉴를 닫는 클릭은 뒤쪽 동작으로 전달되지 않는다. 연결이 끊기면 행을 남기고 확인·이동을 비활성화한다.
- 한글·중문 bracketed paste, 검색 지우기, 영어·한국어 전환, 작은 창으로 변경, 일반 종료와 SIGTERM 후 termios·alternate screen·mouse/paste 모드 복원을 검사했다.
- SGR 마우스 입력을 실제 masil 클라이언트에 보내 사이드패널의 Mark seen까지 도달시켰다.
- 사이드패널 재사용, 확대·원래 pane으로 이동, 종료, 상속된 remain-on-exit, 다른 코어 거절, 공유·읽기 전용 클라이언트 거절, respawn된 과거 패널 보존을 검사했다.
- 두 클라이언트가 같은 session을 볼 때 보이지 않는 창으로 이동하려는 요청도 거절했다. 대상 window뿐 아니라 session의 공유 상태도 명령 실행 시 검사한다.
- 기존 40열 pane만 줄이지 않고 창 전체에서 사이드패널을 분할한다. 80열 창은 거절하며 실행 경계의 최소 너비 식도 숫자로 비교한다.

Rust 회귀 검사는 요청 revision·선택 identity·빈 목록에서 새 행이 나타나는 순간의 버튼 release, modifier가 있는 키, 스크롤바 이동 범위, resize/focus loss/새 press의 capture 취소, UTF-8 바이트와 grapheme 상한을 포함한다. 상세 영역의 휠 입력도 별도로 검사했다.

## 화면과 검토

Unslop과 Impeccable을 적용했다. 사용자가 고른 code-first 방식으로 구현하고 넓은 화면·80열·34열·최소 크기·한국어·밝은 테마·메뉴·도움말·빈 검색·연결 손실을 함께 검토했다. 실제 PTY ANSI를 pyte 셀로 해석한 PNG이며 이미지 시안이 아니다. 원본 ANSI, 셀 JSON, 텍스트와 [캡처 메타데이터](../../.impeccable/review/capture.json)를 보존했다. 제품에는 raster asset이 없다.

- [넓은 화면](../../.impeccable/review/wide.png)
- [사이드패널](../../.impeccable/review/sidebar.png)
- [한국어](../../.impeccable/review/korean.png)
- [연결 손실](../../.impeccable/review/disconnected.png)

독립 correctness 검토의 6개 지적을 수정하고 재검토에서 모두 resolved를 받았다. 버튼 release 대상 변경, 공유 session 이동, 분할 너비 검사, 상세 영역 휠, 검색 byte 상한, 복사 단축키가 해당한다. Impeccable finish 검토의 상태 필터 명칭·요청 수 설명·한국어 결과 문구를 수정했다. DESIGN.md와 sidecar 작성 후 받은 최종 판정은 `ship`이며 4개 항목 모두 resolved다. 미해결 항목은 없고 판정 범위는 제공한 terminal UI와 캡처다.

전용 documenter agent 역할이 이 런타임에 없어 Codex worker에게 동일 문서 규격과 실제 코드를 제공해 DESIGN.md와 sidecar 작성을 맡겼다. 웹 CSS detector는 터미널 UI에 적용하지 않았다.

## 짧은 idle 측정

120열·32행 UI 하나, provider source 하나, native pane 두 개에 관찰 항목 1개 또는 50개를 연결했다. 각 5초 측정이며 실제 모델·provider TUI 비용을 포함하지 않는다. [원시 결과와 실행 파일 해시](data/agent-desk-idle.json)를 보존했다.

| 관찰 항목 | UI RSS | agentd RSS | UI thread | idle 터미널 출력 | 추가 HTTP |
| --- | --- | --- | --- | --- | --- |
| 1 | 3.38 MiB | 3.45 MiB | 2 | 0 byte | 0 |
| 50 | 3.97 MiB | 4.94 MiB | 2 | 0 byte | 0 |

두 경우 SSE 연결은 하나였다. `ps` 누적 CPU 시간의 차이는 UI와 agentd 모두 0.00초였다. 짧은 구간과 계측 해상도의 결과이며 CPU 사용이 절대 0이라는 뜻은 아니다. UI thread 두 개에는 Crossterm 입력 처리가 포함된다. 기본 코어만 실행하면 이 UI 프로세스는 생기지 않는다.

이 결과는 [기존 코어·Herdr 비교](../benchmarks/2026-09-28.md)와 측정 대상이 다르다. 전체 에이전트 관리 성능 gate 통과나 장시간 지연 보장으로 분류하지 않는다.

재현 명령:

```sh
make test
env -u LIBRARY_PATH cargo clippy --locked --manifest-path agent/Cargo.toml --all-targets -- -D warnings
.build/bench-venv/bin/python tests/performance/ui_probe.py \
  --seconds 5 --output docs/validation/data/agent-desk-idle.json

# 화면 기록에만 Pillow가 추가로 필요하다. 제품 의존성이 아니다.
.build/bench-venv/bin/python -m pip install 'Pillow>=11,<13'
.build/bench-venv/bin/python tests/ui_capture.py
```

## 남은 검증 범위

이번 결과를 모든 호스트 터미널의 완전한 마우스·IME 지원으로 일반화하지 않는다. 후속 [Debian·실제 SSH 검증](2026-09-28-debian-ssh.md)에서 Linux 빌드와 SSH PTY 조작을 확인했다. BSD, 실제 OS IME 조합 확정, SSH 끊김·재접속·중첩 tmux, 터미널별 clipboard/마우스 override, 장시간 고출력, 접근성 도구와의 조합은 별도 행렬이 필요하다. UI의 한글 문자열 편집·붙여넣기 검증과 OS IME 조합 중 입력 검증은 다르다.

provider approval·prompt·완료 판정·대화 복구·foreground session identity 검증은 이번 UI에서 제공하지 않는다. 상세 설계의 장기 목표와 현재 구현 범위를 구분한다.
