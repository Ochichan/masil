# 참조 소스와 기준 목록

2026-09-28에 로컬 클론을 정적으로 조사했다. upstream 최신 release 여부를 확인한 기록이 아니며, 소스에 있는 테스트를 실행한 결과도 아니다.

## 고정한 소스

| 프로젝트 | 로컬 경로 | 조사한 HEAD | 역할 |
| --- | --- | --- | --- |
| tmux | `~/Documents/git_clones/tmux` | `94796f6b1182507efac8a272fc309a79e22e58a5` | 전체 기능과 기본 키의 필수 호환 기준. `next-3.9` 개발판 |
| Herdr | `~/Documents/git_clones/herdr` | `0d5d6f1f317e238c8297076bc6ab5c3a0cd56283` | server가 소유하는 PTY, 상태/runtime 분리, agent 관찰 |
| OpenCode | `~/Documents/git_clones/opencode` | `b471c2b4495747353af768fbf2e0790c9d820ce2` | 원래 TUI와 같은 backend session의 plugin/API 연동 |
| T3 Code | `~/Documents/git_clones/t3code` | `94f92a7a386a26c98892b24fabdd0ea9fa804ce3` | provider adapter, capability, 명령 수락과 실행 완료의 구분 |

네 저장소 모두 조사 당시 tracked/untracked 변경사항이 없는 상태였다. 참조 저장소는 수정하지 않았다.

## 사용자 제공 조사

[Herdr 사용자 불편 조사 반영 기록](../research/herdr-feedback-2026-09-28.md)은 사용자가 다른 AI의 조사 결과를 제공한 자료다. 이 소스 탐색과 증거 수준을 구분한다. 번호가 제공된 43건의 이슈와 후기 요약을 요구사항에 연결했으며, 언급된 103건 색인 원본은 전달받지 않았다. 이슈 상태·release·후기 원문은 이번 보강에서 독립 검증하지 않았다.

## tmux 비교 자료

- [tmux-baseline.json](tmux-baseline.json): 명령·alias·옵션·hook·정적 format 변수·copy command·control notification·기본 키의 기계 판독 목록과 출처.
- [tmux-default-bindings.conf](tmux-default-bindings.conf): `defaults[]`의 308개 문자열을 C escape 해석과 메뉴 매크로 확장 후 보존한 비교용 명령 목록.
- [tmux-NOTICE.txt](tmux-NOTICE.txt): 복사·가공한 기본 바인딩과 참조 데이터의 원본 고지. rmux 자체의 라이선스를 결정하는 파일은 아니다.

| 추출 항목 | 수 | 정확한 범위 |
| --- | ---: | --- |
| 명령 | 92 | `cmd.c`의 registry에 등록한 command entry |
| 내장 command alias | 78 | 위 entry의 `.alias`. 사용자 `command-alias[]`와 별개 |
| 일반 옵션 | 180 | `options-table.c`의 이름 있는 명시적 option row |
| Hook 옵션 | 89 | after hook 38, 일반 hook 24, pane hook 19, window hook 8 |
| Format 변수 | 214 | `format.c` 정적 callback table만 집계 |
| Copy-mode 명령 | 99 | `window_copy_cmd_table[]`의 명령 |
| Control notification | 25 | manpage CONTROL MODE의 notification 목록. `%begin`/`%end`/`%error` 응답 guard와 별개 |
| 기본 바인딩 | 308 | prefix 93, move 19, root 30, copy-mode 78, copy-mode-vi 88 |
| Repeat 바인딩 | 17 | 기본 문자열 중 `-r`가 있는 항목 |
| 회귀 shell script | 164 | `regress/` 바로 아래의 `*.sh`. 테스트 case 수가 아님 |

이 목록은 전체 기능의 하한을 추적하는 자료다. 명령별 flag와 실패 조건, 사용자 정의 alias, 옵션 값과 초기화 효과, dynamic format, format 연산자, 메뉴·chooser·prompt 내부 키, 조건부 빌드 동작까지 전부 열거한 schema는 아니다. 그 범위도 [호환 계약](../tmux-compatibility.md)에 포함한다.

## 추출 규칙

1. tmux `git HEAD`와 clean 상태를 확인한다. `configure.ac`의 버전 문자열을 읽는다.
2. `cmd_table[]`의 entry symbol을 `cmd-*.c` 정의와 대응해 name, alias, 인자 template, 위치를 기록한다. registry에 없는 선언은 명령 수에 넣지 않는다.
3. `options_table[]`의 명시적 row와 hook 매크로 호출을 분리한다. `AFTER_HOOK`는 `after-` 접두어를 붙여 기록한다. 옵션의 scope와 type은 C 식으로 보존한다.
4. `format_table[]`, `window_copy_cmd_table[]`, manpage의 CONTROL MODE notification 목록을 각각 읽는다. 정적 format table을 전체 format 언어로 취급하지 않는다.
5. `key_bindings_init()`의 문자열 초기화 항목을 순서대로 읽는다. 인접한 C 문자열과 `DEFAULT_*` 메뉴 매크로를 재귀 확장한다. bind의 `-T`, `-n`, `-r`, note, key를 함께 기록한다.
6. 원본 source line과 주요 파일 SHA-256을 기록한다. `.conf`에는 같은 순서의 명령을 한 줄씩 쓴다.
7. 항목 수·중복 이름·registry 대응·source 위치·JSON과 `.conf`의 일치를 검사한다. 이는 runtime `list-keys` 비교를 대신하지 않는다.

refresh 시 같은 범위로 다시 추출하고 원본 소스의 구문 변경에 따라 추출 방법도 검토한다. 수를 맞추려고 누락된 항목을 지우지 않는다. mode handler와 새 source table 등 추출 범위 밖의 변경은 별도로 확인한다.

## tmux에서 먼저 읽을 위치

모든 링크는 위의 고정 commit을 가리킨다.

| 소스 | 읽을 내용 |
| --- | --- |
| [tmux.1](https://github.com/tmux/tmux/blob/94796f6b1182507efac8a272fc309a79e22e58a5/tmux.1) | 공개 동작의 전체 범위 |
| [cmd.c](https://github.com/tmux/tmux/blob/94796f6b1182507efac8a272fc309a79e22e58a5/cmd.c#L123), [cmd-parse.y](https://github.com/tmux/tmux/blob/94796f6b1182507efac8a272fc309a79e22e58a5/cmd-parse.y), [cmd-find.c](https://github.com/tmux/tmux/blob/94796f6b1182507efac8a272fc309a79e22e58a5/cmd-find.c) | 명령 목록·파싱·target |
| [key-bindings.c](https://github.com/tmux/tmux/blob/94796f6b1182507efac8a272fc309a79e22e58a5/key-bindings.c#L383), [server-client.c](https://github.com/tmux/tmux/blob/94796f6b1182507efac8a272fc309a79e22e58a5/server-client.c#L1261) | 기본 키와 table/mode/prefix 입력 처리 |
| [options-table.c](https://github.com/tmux/tmux/blob/94796f6b1182507efac8a272fc309a79e22e58a5/options-table.c#L290), [tmux.c](https://github.com/tmux/tmux/blob/94796f6b1182507efac8a272fc309a79e22e58a5/tmux.c#L585) | 옵션과 환경별 초기화 |
| [spawn.c](https://github.com/tmux/tmux/blob/94796f6b1182507efac8a272fc309a79e22e58a5/spawn.c#L243), [session.c](https://github.com/tmux/tmux/blob/94796f6b1182507efac8a272fc309a79e22e58a5/session.c) | PTY·환경·session group과 window 연결 |
| [layout-custom.c](https://github.com/tmux/tmux/blob/94796f6b1182507efac8a272fc309a79e22e58a5/layout-custom.c), [window-panes.c](https://github.com/tmux/tmux/blob/94796f6b1182507efac8a272fc309a79e22e58a5/window-panes.c) | layout 표현과 floating/tiled pane |
| [window-copy.c](https://github.com/tmux/tmux/blob/94796f6b1182507efac8a272fc309a79e22e58a5/window-copy.c#L3298), [mode-tree.c](https://github.com/tmux/tmux/blob/94796f6b1182507efac8a272fc309a79e22e58a5/mode-tree.c#L1644), [prompt.c](https://github.com/tmux/tmux/blob/94796f6b1182507efac8a272fc309a79e22e58a5/prompt.c#L1212) | 기본 바인딩만으로 설명되지 않는 mode 입력 |
| [format.c](https://github.com/tmux/tmux/blob/94796f6b1182507efac8a272fc309a79e22e58a5/format.c#L3584), [hooks.c](https://github.com/tmux/tmux/blob/94796f6b1182507efac8a272fc309a79e22e58a5/hooks.c) | format 언어와 hook |
| [control.c](https://github.com/tmux/tmux/blob/94796f6b1182507efac8a272fc309a79e22e58a5/control.c), [tmux-protocol.h](https://github.com/tmux/tmux/blob/94796f6b1182507efac8a272fc309a79e22e58a5/tmux-protocol.h) | 공개 control mode와 별도 내부 binary protocol |
| [input.c](https://github.com/tmux/tmux/blob/94796f6b1182507efac8a272fc309a79e22e58a5/input.c), [tty-keys.c](https://github.com/tmux/tmux/blob/94796f6b1182507efac8a272fc309a79e22e58a5/tty-keys.c), [tty-term.c](https://github.com/tmux/tmux/blob/94796f6b1182507efac8a272fc309a79e22e58a5/tty-term.c) | terminal 출력 해석·입력·capability |
| [regress](https://github.com/tmux/tmux/tree/94796f6b1182507efac8a272fc309a79e22e58a5/regress) | differential 검증에 사용할 회귀 사례 |

## 에이전트 관련 근거

- Herdr [runtime registry](https://github.com/herdrdev/herdr/blob/0d5d6f1f317e238c8297076bc6ab5c3a0cd56283/src/terminal/runtime_registry.rs), [hook authority](https://github.com/herdrdev/herdr/blob/0d5d6f1f317e238c8297076bc6ab5c3a0cd56283/src/detect/mod.rs#L323), [OpenCode TUI integration](https://github.com/herdrdev/herdr/blob/0d5d6f1f317e238c8297076bc6ab5c3a0cd56283/src/integration/assets/opencode/herdr-tui-session.js#L73).
- OpenCode [TUI의 내부/외부 server 선택](https://github.com/anomalyco/opencode/blob/b471c2b4495747353af768fbf2e0790c9d820ce2/packages/opencode/src/cli/cmd/tui.ts#L210), [기존 backend attach](https://github.com/anomalyco/opencode/blob/b471c2b4495747353af768fbf2e0790c9d820ce2/packages/opencode/src/cli/cmd/attach.ts#L114), [V2 입력 admission](https://github.com/anomalyco/opencode/blob/b471c2b4495747353af768fbf2e0790c9d820ce2/packages/core/src/session/input.ts#L245).
- T3 Code [ProviderAdapter](https://github.com/pingdotgg/t3code/blob/94f92a7a386a26c98892b24fabdd0ea9fa804ce3/apps/server/src/provider/Services/ProviderAdapter.ts#L45), [명령 transaction](https://github.com/pingdotgg/t3code/blob/94f92a7a386a26c98892b24fabdd0ea9fa804ce3/apps/server/src/orchestration/Layers/OrchestrationEngine.ts#L245), [provider 제약](https://github.com/pingdotgg/t3code/blob/94f92a7a386a26c98892b24fabdd0ea9fa804ce3/docs/internals/providers.md).

소스에서 확인한 메커니즘과 rmux의 제품 결정은 구분한다. 특정 프로젝트가 하는 모든 일을 rmux의 필수 기능으로 자동 채택하지 않는다. tmux 전체 기능과 기본 키 지원은 사용자가 명시한 별도의 필수 요구다.
