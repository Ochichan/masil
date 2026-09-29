# 마우스 UI와 설정 화면

`rmux`를 `-f` 없이 시작하면 tmux 기본값 위에 rmux UI 레이어가 적용된다. 처음 실행부터 창·pane·세션을 마우스로 만들고 조작할 수 있다. tmux 옵션과 키 테이블의 기본값 자체는 바꾸지 않는다.

## 마우스로 할 수 있는 일

| 위치 | 동작 |
| --- | --- |
| 상태줄 `rmux` | rmux 메뉴: 설정, 새 창, 새 세션, 세션과 창 목록, 오른쪽·아래로 분할, 키 목록, tmux 설정 다시 읽기, 분리 |
| 창 목록 뒤 `+` | 새 창 |
| 창 탭 클릭·휠 | 창 선택·이전/다음 창 (tmux 기본) |
| 현재 창 탭 오른쪽 클릭 | 창 메뉴: 이름 변경, 새 창, 종료 등 (tmux 기본) |
| `Settings` | 설정 화면 |
| pane 제목 클릭 | pane 이름 변경 |
| pane 제목의 `float`·`zoom`·`x` | floating 전환, 확대, 닫기 (tmux 기본 버튼) |
| pane 오른쪽 클릭 | pane 메뉴: 분할, 확대, 교환, 종료 등 (tmux 기본) |
| pane 경계 드래그 | 크기 조절 (tmux 기본) |
| 스크롤바 | 스크롤할 때 나타나며 끌어서 이동한다 |
| 사이드바 `<`·`>` | 사이드바 폭 접기·펴기 |

메뉴는 클릭한 뒤 버튼을 떼도 열려 있다. 포인터를 항목 위로 옮겨 클릭하면 실행하고 바깥을 누르면 닫힌다. 레이어는 `@rmux-menu-stay-open`으로 이 동작을 켜며 tmux 기본 메뉴 바인딩은 그대로 쓴다.

## 상태줄 위치

설정 화면의 상태줄 위치는 위, 아래, 왼쪽 사이드바, 오른쪽 사이드바 중 하나다. `set -g status-position left|right`로도 바꿀 수 있다.

사이드바는 모든 창의 같은 쪽에 그려진다. 위에서부터 `status-left`, 창마다 한 줄, 맨 아래 `status-right`를 표시한다. 창 줄은 `window-status-format`과 창 스타일을 그대로 쓰고 줄 어디를 눌러도 그 창을 고른다. 폭은 `@rmux-status-width`(기본 24, 8~80)다. 터미널 폭이 사이드바 폭보다 10열 이상 넓지 않으면 사이드바를 그리지 않는다. 메시지와 command prompt는 맨 아래 줄 전체에 겹쳐 표시한다.

사이드바가 있으면 pane이 터미널 전체 폭을 차지하지 않는다. 좌우 여백(DECSLRM)을 지원하지 않는 터미널에서는 스크롤할 때 pane을 다시 그리므로 출력이 많은 작업에서 느릴 수 있다.

## 설정 화면

`Settings` 버튼이나 rmux 메뉴의 설정은 현재 창 위에 modal pane으로 `rmux-agent settings`를 연다. 마우스와 키보드(Tab, 방향키, Enter, q)로 같은 일을 할 수 있다.

| 항목 | 설정 |
| --- | --- |
| 상태줄 | 위치, 표시, 사이드바 너비, 시계 |
| 창 분할 | pane 제목줄, 스크롤바, 경계선 |
| 마우스와 입력 | 마우스, 번호 시작, 창 번호 다시 매기기, 스크롤 기록 |
| 모양 | 테마(어둡게, 밝게, 터미널 색, tmux 기본), 언어(English, 한국어) |
| rmux UI | 레이어 켜기·끄기, 모든 설정 초기화 |

값을 고르면 서버가 받아들인 뒤 저장한다. 고른 옵션은 바로 적용하고, 그 선택에 딸린 다른 옵션(예: 테마의 색)은 아직 레이어 값 그대로일 때만 다시 적용해 사용자 설정을 보존한다. 적용 결과는 서버의 실제 값을 다시 읽어서 표시한다. 이 세션이나 창에 따로 정한 값이 가리면 가려진다고 알린다. 저장하지 못하면 rmux를 끌 때까지만 적용된다고 표시한다. `~/.tmux.conf` 등이 같은 옵션을 정하면 그 값이 표시되며, 여기서 고른 값은 rmux를 다시 시작하면 사용자 설정에 밀린다.

스크립트에서는 화면 없이 쓸 수 있다.

```sh
rmux-agent settings --set @rmux-status-position left --set @rmux-theme light
rmux-agent settings --layer off     # 순수 tmux 기본값으로
rmux-agent settings --reset         # 저장한 선택 지우기
rmux-agent settings --get
```

rmux 밖에서는 `--socket PATH`로 서버를 지정한다.

## 적용 순서와 파일

`-f` 없이 시작하면 다음 순서로 읽는다.

1. `$XDG_CONFIG_HOME/rmux/settings.conf`(절대 경로일 때) 또는 `~/.config/rmux/settings.conf`: 설정 화면이 저장한 `@rmux-*` 선택
2. 코어에 내장한 UI 레이어(`core/rmux-ui-layer.conf`)
3. `/etc/tmux.conf`, `~/.tmux.conf`, `$XDG_CONFIG_HOME/tmux/tmux.conf`, `~/.config/tmux/tmux.conf`

사용자 tmux 설정이 가장 나중이라 레이어보다 우선한다. `rmux -f FILE`로 시작하면 1과 2를 읽지 않으며 stock tmux와 같은 기본값이 된다.

`settings.conf`는 `set -g @rmux-KEY VALUE` 줄만 관리하고 다른 줄은 보존한다. 파일은 0600으로 원자적으로 교체한다. 심볼릭 링크이거나 일반 파일이 아니면 덮어쓰지 않고 현재 세션에만 적용한다.

레이어는 tmux 옵션과 기본 바인딩이 없는 `MouseDown1Control0`~`6`만 쓴다. 각 바인딩의 설명은 `rmux-ui:`로 시작한다. 레이어를 끄면 레이어가 정한 옵션과 색·메뉴용 `@rmux-*` 옵션을 되돌리고 `rmux-ui:` 바인딩만 푼 뒤 사용자 tmux 설정 파일을 다시 읽는다. 저장한 선택(`@rmux-*`)과 `@rmux-agent`는 남는다. 레이어를 다시 켜거나 초기화한 뒤에도 사용자 tmux 설정 파일을 다시 읽는다. 두 설정 화면이 동시에 저장하지 않도록 설정 디렉터리에 잠금 파일을 쓴다.

## 알려진 제한

- tmux next-3.9는 현재가 아닌 창의 탭을 오른쪽 클릭하면 메뉴를 그 창에 붙여 화면에 보이지 않는다. stock tmux도 같다.
- `rmux` 메뉴의 tmux 설정 다시 읽기는 `/etc/tmux.conf`, `~/.tmux.conf`, `~/.config/tmux/tmux.conf`를 읽는다. `$XDG_CONFIG_HOME/tmux/tmux.conf`는 설정 화면의 레이어 켜기·끄기·초기화에서만 다시 읽는다.
- 메뉴 항목과 pane 메뉴의 문구는 tmux 기본값이라 영어로 표시된다. rmux 메뉴, 버튼, 설정 화면은 언어 설정을 따른다.
- rmux 밖에서 `--set`을 쓰면 현재 pane을 알 수 없어 세션·창별 덮어쓰기를 확인하지 않는다.
