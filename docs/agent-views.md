# 저장한 에이전트 목록 보기

에이전트 목록과 관리 화면은 같은 native rmux 서버에 저장한 view를 사용한다. view는 tmux global option `@rmux-agent-view`에 versioned JSON을 hex로 인코딩해 저장한다. shell 문자열로 평가하지 않으며 별도 daemon이나 설정 파일이 필요하지 않다.

```sh
rmux-agent agent view get
rmux-agent agent view set --provider codex --state blocked --workspace work --sort priority
rmux-agent agent view clear
```

`view set` 옵션은 반복할 수 있다.

| 옵션 | 값 |
| --- | --- |
| `--provider` | rmux가 지원하는 provider ID 또는 alias. 저장할 때 canonical ID로 바꾼다. |
| `--state` | `idle`, `working`, `blocked`, `unknown`, `exited` |
| `--workspace` | 최대 128바이트의 정확한 workspace 이름. 정규식이나 glob이 아니다. |
| `--sort` | `priority`, `name`, `provider`, `workspace` 중 하나 |

같은 filter를 반복하면 하나로 합치며 저장 배열은 정렬한다. filter가 비어 있으면 모든 값을 표시한다. 알 수 없는 provider·state·sort, control character, 제한을 넘는 workspace, 알 수 없는 JSON field는 거부한다. 손상된 view가 있으면 목록을 임의의 기본값으로 열지 않고 `view clear`로 지우도록 오류를 낸다.

`priority` 순서는 blocked, working, idle, unknown, exited다. 같은 우선순위에서는 workspace, 이름, pane 숫자 순으로 정렬한다. 다른 sort도 workspace·이름·pane 숫자를 tie-breaker로 사용하므로 결과가 매번 같다.

view는 표시만 바꾼다. `get`, `start`, 이름 중복 확인, 입력 전달, 저장과 복원은 항상 filter를 적용하지 않은 전체 native 목록을 사용한다. 따라서 화면에서 숨긴 에이전트도 직접 지정할 수 있고, 같은 이름으로 중복 process를 시작할 수 없다.
