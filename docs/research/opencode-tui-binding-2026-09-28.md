# OpenCode TUI binding 후속 구현 근거

2026-09-28. 로컬 clone `b471c2b4495747353af768fbf2e0790c9d820ce2`, 설치 CLI 1.18.32 기준 source 조사다. 이번 attention 구현은 frontend binding을 변경하지 않는다.

## 확인한 Interface

- `packages/plugin/src/tui.ts:581-628`의 `api.route.current`는 현재 route를 읽는 getter다. `api.lifecycle.signal`과 `onDispose`가 있다. 직접적인 route subscribe 메서드나 환경변수 메서드는 없다.
- `packages/tui/src/plugin/adapters.tsx:41-73,173-206`은 home/session/plugin route를 반환한다. session의 `params.sessionID`는 실제 선택한 session이다.
- `packages/tui/src/context/route.tsx:25-53`은 Solid reactive store를 사용한다. app slot에 보이지 않는 component를 등록하고 `createEffect` 안에서 getter를 읽으면 변경을 관찰할 수 있을 것으로 **추론**한다. plugin을 실제 TUI에 로드한 검증은 아직 없다.
- `packages/tui/src/app.tsx:1081-1130`은 전역 app slot을 렌더링한다. `api.slots.register({slots:{app() { ... }}})`가 그 진입점이다.
- `tui.session.select` event만 구독하면 TUI 내부의 모든 route 이동을 보장하지 못한다. child/parent/previous/next 이동은 `route.navigate`를 직접 호출한다. `packages/tui/src/routes/session/index.tsx:432-455,1046-1082`.
- `--session`, `--continue`, `--fork`는 서로 다른 session 선택 동작이다. fork 이후에는 새 ID로 이동한다. `packages/tui/src/app.tsx:494-539`.
- `packages/opencode/src/plugin/tui/runtime.ts:388-461,516-555`는 plugin 종료와 cleanup을 처리한다. native backend가 살아 있어도 TUI frontend 종료 시 binding을 해제해야 한다.

## 로드와 연결

`OPENCODE_TUI_CONFIG`, `OPENCODE_CONFIG_DIR`, tuple plugin option을 통해 특정 실행에 socket/options를 전달할 수 있다. 관련 source는 `packages/opencode/src/config/tui.ts:183-245`와 `packages/opencode/specs/tui-plugins.md:46-53`이다. plugin 의존성 설치는 선택한 config directory를 수정할 수 있다.

이 설정은 기존 global config를 제거하는 보장을 뜻하지 않는다. 검증 시에는 HOME과 XDG config/data/state/cache도 임시 directory로 분리하고 project config·기본 plugin을 끈다. 사용자 설정을 자동으로 덮어쓰거나 설치하지 않는다.

plugin은 같은 TUI process에서 `process.env`를 읽거나 `node:net`의 UDS로 연결할 수 있다. 실제 rmux wire format은 4-byte big-endian 길이 + JSON이다. 그러나 private socket과 same-UID 검사는 **어느 pane/PTY에 속한 frontend인지 증명하지 않는다**.

## 다음 binding 구현의 통과 조건

1. core boot·pane·PTY generation과 실제 frontend 수명을 연결하는 권위 있는 시작/종료 handshake를 정의한다. 환경변수의 pane ID를 받았다는 이유만으로 verified로 표시하지 않는다.
2. 이전 frontend의 늦은 보고, 같은 shell의 다음 실행, session 전환·home·child 이동, plugin dispose·프로세스 중지·connection loss를 구분한다.
3. 원래 TUI를 실제로 로드한 격리 smoke에서 route 관찰과 종료 해제를 검증한다. 이번 조사에서 실행한 것은 CLI version/help 확인까지다.
4. binding이 생겨도 prompt 수락·승인 capability는 별도로 검증한다. read-only native 상태 관찰이 제어 권한의 근거가 되지 않는다.
