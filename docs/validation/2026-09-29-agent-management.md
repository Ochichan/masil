# Native Agent 관리와 원격 통합 검증

macOS arm64에서 로컬 rmux 기준 commit `b579a15e22f991f2385dc6c42de347ffe839367e`에 대한 변경을 검증했다. Herdr 비교 기준은 `0d5d6f1f317e238c8297076bc6ab5c3a0cd56283`이다. 기존 Herdr checkout과 사용자 provider 설정은 수정하지 않았다.

## 구현과 비교 범위

24종 provider catalog, 22개 감지 manifest, native 실행·조회·상태·이름·이동·키·대기, session ref 재개, snapshot 복구, 목록 필터·정렬, 선택적 integration export, 관리 화면과 사이드패널을 추가했다. 22개 manifest는 Herdr의 `src/detect/manifests`와 byte 단위로 같다. Apache-2.0 원문과 출처를 보존했다.

사용자가 명시적으로 선택한 Herdr 방식에 따라 idle·전경 실행을 검증한 prompt+Enter 제출을 구현했다. run별 operation receipt를 기록하고, 같은 번호 재시도는 입력을 반복하지 않는다. 전달은 provider 수락이나 작업 성공을 뜻하지 않는다.

사용자가 추가 요청한 원격 통합은 로컬 서버와 최대 8개 endpoint를 대상으로 한다. 같은 이름과 pane ID를 구분하고, 서버별 독립 조회·오래된 상태 표시·조작 거절·native TUI 연결을 제공한다. 기존 OpenSSH 인증과 호스트 키 검증을 유지한다.

## 주요 회귀 검사

- prompt는 바뀐 run, 화면·제목·progress, working/blocked/unknown, copy mode, pane 동기화, 입력 차단에 제출하지 않는다. paste 실패 뒤 Enter를 보내거나 delivered로 기록하지 않는다.
- pending·만료된 receipt를 재전송하지 않는다. 명시적 operation과 receipt 조회는 원래 run에 묶는다.
- provider callback의 자식 session·과거 sequence·다른 run을 거절한다. session 검증에 사용한 snapshot 그대로 최종 native CAS를 실행한다.
- JS/TS hook의 ENOENT/EPIPE가 provider를 죽이지 않는다. 설정 없는 callback과 큰 payload는 프로세스를 만들지 않는다.
- restore는 native 서버 lock과 canonical snapshot lock을 함께 사용한다. 두 서버와 경로 alias가 같은 파일을 동시에 복구하거나 save로 덮지 못한다.
- 같은 이름·pane ID를 가진 두 실제 rmux 서버 사이에서 prompt가 올바른 endpoint로만 간다. 끊긴 서버를 조회해도 다시 시작하지 않는다.
- 로컬 서버를 SIGSTOP해도 원격 행은 계속 갱신된다. CLI SIGTERM·비동기 취소·timeout은 transport 프로세스 그룹과 하위 프로세스를 정리한다.
- 원격 연결은 실제 native PTY로 열고 detach 뒤 provider가 살아 있는지 확인한다. 기존 client의 session/window 선택을 바꾸는 attach는 거절한다.
- 연결 pane을 respawn하거나 원격 client가 다른 pane으로 이동하면 과거 연결을 재사용하지 않는다. 새 연결은 실제 remote client lease를 확인한 뒤 성공으로 표시한다.
- UI가 열린 뒤 endpoint 추가, Start dialog 중 같은 ID의 다른 서버로 교체, server 재시작·run 교체, stale 행의 조작 비활성화를 검사한다.

## 우클릭 메뉴

메뉴의 표시 창과 명령 대상을 분리했다. 상태줄에서 비활성 창을 우클릭해도 현재 보이는 창 위에 메뉴가 표시되며, 명령은 원래 대상에 적용된다. tiled/floating pane 출력은 메뉴를 덮지 않는다. 메뉴 대상이 삭제되면 crash나 다른 창으로의 대체 실행 없이 거절한다.

상태줄 네 위치, 계속 출력하는 pane, floating pane, 비활성 대상, 대상 삭제를 실제 mouse/PTY 회귀 검사에 포함했다. upstream `menu-mouse.sh`와 `mode-tree-menu-position.sh`도 rmux와 baseline에서 통과했다. 별도 upstream `screen-redraw-menus` snapshot은 rmux의 현재 표시 창 정책 때문에 `menu-target-window-noborder`에서 다르고, baseline도 `menu-over-split`에서 기존 snapshot과 다르다. 이 snapshot suite를 통과했다고 분류하지 않았고 기대 파일을 덮어쓰지 않았다.

## 검증 도구와 근거

- 최종 `make test` 전체 통과. native 호환성·bridge/watch·agentd·attention·CLI·복구·endpoint·fleet·마우스·UI·실패 주입을 포함한다. 로컬 전체 로그는 `.build/final-parity-test.log`다.
- Rust unit test 99개 통과.
- Clippy `--locked --all-targets -- -D warnings`, rustfmt, diff whitespace 검사 통과.
- Agent endpoint 검사 4개, fleet 검사 7개, managed UI runner 45개 통과. UI runner에는 공유 backend fixture의 재실행도 포함되므로 고유 UI 시나리오 수를 뜻하지 않는다.
- release build와 smoke 검사 통과. core build는 warnings를 오류로 처리한다.
- 독립 코드 검토에서 발견한 입력·snapshot·callback·endpoint identity·연결 재사용·process lifetime 문제를 수정하고 최종 검토에서 blocking finding 없음 판정을 받았다.

실제 PTY ANSI를 셀로 변환한 화면을 확인했다. 넓은 화면, 일반 크기, 사이드패널, 영어·한국어 메뉴와 dialog, fleet online/offline 상태를 검토했다. 로컬 캡처는 `.build/managed-ui-review/`에 있으며 제품 raster asset은 없다. 저장한 목록 순서와 idle redraw 억제도 테스트했다.

## 한계

실제 Claude/Codex 등 provider 계정이나 SSH 목적지는 구성하지 않았다. 새 provider integration은 source 기반 export 및 합성 callback 검증이며 모든 최신 설치본의 live 호환성을 뜻하지 않는다. 원격 transport는 실제 로컬 두 서버와 strict SSH argv 검사로 검증했고, 실제 SSH 인증·장시간 네트워크 단절·새 기능의 Linux/BSD 조합은 별도 검증 대상이다. macOS에서는 Linux 전용 non-root PTY permission 검사를 skip한다.
