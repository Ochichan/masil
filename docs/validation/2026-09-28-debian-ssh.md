# Debian과 실제 SSH 연결 검증

2026-09-28, SSH 별칭 `debian-ts`로 연결된 Debian 13.7 x86_64에서 코어·stock baseline·Rust agent를 빌드하고 테스트했다. macOS 클라이언트에서 실제 `ssh -tt` 연결을 열어 관리 화면과 native 사이드패널도 조작했다. 두 경로 모두 통과했다.

테스트는 private socket과 모델 호출이 없는 HTTP/SSE fixture를 사용했다. 실제 코어·agentd·UI·SSH를 실행했지만, 원격 OpenCode TUI나 모델 작업을 검증한 것은 아니다.

## 환경과 빌드

| 항목 | 검증 환경 |
| --- | --- |
| OS | Debian 13.7, Linux 6.12.107+deb13-amd64, x86_64 |
| C compiler | GCC 14.2.0 |
| Rust | rustc / Cargo 1.98.1 |
| Python | 3.13.5 |
| 테스트 locale | `C.UTF-8` |
| tmux 기준 | `94796f6b1182507efac8a272fc309a79e22e58a5` |
| 소스 기준 | `c03a2b4`에 이 검증에서 발견한 수정 적용. 변경 파일과 실행 파일 해시는 아래 환경 기록에 보존 |

[환경·소스·실행 파일 해시](data/debian-ssh/environment.json), [코어 빌드](data/debian-ssh/core-build.json), [stock 빌드](data/debian-ssh/baseline-build.json)를 보존했다.

원격 작업 폴더는 `/home/ochi/.cache/masil-validation.K2fYQL1P`다. 저장소 archive, 고정 upstream archive와 해시를 확인한 GNU 빌드 도구 archive를 복사했다. 필요한 Debian 개발 패키지는 `apt-get download`로 받고 `dpkg-deb -x`로 작업 폴더의 `.build/debian/sysroot`에 풀었다. 시스템 패키지·SSH 설정·기존 tmux 설치는 바꾸지 않았다.

개인 prefix의 aclocal이 시스템 `pkg.m4`를 찾도록 `ACLOCAL_PATH=/usr/share/aclocal`을 지정했다. pkg-config에는 private sysroot와 그 안의 `.pc` 경로를 지정했다. 실행 시에는 기기에 이미 설치된 공유 라이브러리를 사용한다. 설정은 원격 `.build/debian/env.sh`에 남아 있다.

이 기기의 Python에는 `ensurepip`가 없었다. 따라서 `make test` 자체의 venv 생성 단계 대신 `venv --without-pip`와 내려받은 Debian pip wheel로 private venv를 만들고, Makefile의 Linux 대상 검사들을 직접 실행했다. 전역 pip 설치는 하지 않았다.

## 검사 결과

| 검사 | Debian 결과 |
| --- | --- |
| 코어·동일 기준 stock·Rust release 빌드 | 통과 |
| tmux 공개 동작·기본 명령·키·옵션 비교 | 10개 통과 |
| bridge 통합 / watch CLI | 각각 통과 |
| core watch | 8개 통과 |
| agentd | 9개 통과 |
| attention | 6개 통과 |
| agent stream CLI | 3개 통과 |
| benchmark 도구 | 6개 통과 |
| UI 실제 PTY | 10개 통과 |
| Rust release 단위 테스트 | 64개 통과 |
| Clippy `--all-targets -- -D warnings` / rustfmt | 통과 |
| 실제 SSH 관리 화면 / native 사이드패널 | 각각 통과 |

[원격 검사 출력](data/debian-ssh/gates.txt)과 [SSH 결과](data/debian-ssh/ssh-results.json)를 보존했다. 같은 수정으로 macOS에서도 전체 `make test`, Clippy, rustfmt를 통과했다. macOS UI 검사는 9개 통과, Linux 전용 PTY 권한 검사 1개 skip이다. macOS 전용 forkpty 실패 주입도 해당 기기에서 통과했다.

## 실제 SSH에서 확인한 조작

`tests/ssh_ui_smoke.py`는 로컬 PTY → 실제 SSH → 원격 PTY → UI 경로로 입력을 보낸다. native 모드에는 masil 클라이언트와 sidebar pane 경로도 포함된다. 테스트가 끝나면 backend 결과를 받아 화면에 나타난 성공과 비교한다.

- 관리 화면과 사이드패널에서 SGR 마우스로 Mark seen을 클릭하고 agentd의 공유 확인 상태가 바뀌었는지 검사했다. provider 요청은 모두 GET이었다.
- 한글·중문 bracketed paste, 검색 지우기, 한국어 전환을 확인했다. 24×8 최소 크기 안내와 80×24 복귀를 검사했다.
- 사이드패널 확대, 마우스로 지정 pane 이동, 기본 `C-b o`로 sidebar 복귀, `q`로 sidebar 종료, `C-b d`로 detach했다.
- 로컬·원격 termios가 원래 값으로 돌아왔고, sidebar 종료 후 작업 pane `%0`이 남았다. 두 SSH 클라이언트와 UI 모두 exit 0이었다.
- 테스트가 끝난 뒤 원격 `/proc`에서 이 검증 폴더의 실행 파일을 사용하는 프로세스가 없는 것을 확인했다.

텍스트 캡처는 [34열 sidebar](data/debian-ssh/ssh-sidebar.txt), [확대 화면](data/debian-ssh/ssh-expanded.txt), [한국어 80열 화면](data/debian-ssh/ssh-korean.txt)에 있다. 행 끝의 공백과 끝의 빈 행만 제거했다. 로컬 원본 ANSI는 `.build/debian-validation/ssh/`에 남아 있다.

## 발견해서 수정한 문제

### SSH에서 상속받은 터미널을 다시 열지 못함

이 연결의 `/dev/pts/0`은 root:tty 소유, mode 0600이었다. 사용자 `ochi`는 SSH가 넘겨준 stdin/stdout으로 입출력할 수 있었지만, UI가 같은 pathname을 다시 열면 `Permission denied`로 종료했다. `/dev/tty`를 통한 독립 open은 가능했다.

UI는 pathname open이 권한 오류일 때 `/dev/tty`로 재시도한다. raw mode나 화면 제어 코드를 보내기 전에 stdin·stdout·새 descriptor가 같은 controlling terminal과 process session인지 확인한다. Linux에서는 `TIOCGDEV`로 실제 장치 identity도 비교한다. 상속받은 descriptor의 nonblocking flag를 바꾸지 않는다. 원격 PTY 권한이나 SSH 설정을 바꾸는 우회는 하지 않았다.

Linux 테스트는 테스트가 소유한 PTY pathname 권한을 000으로 바꿔 같은 실패 조건을 재현한다. 수정 전 UI 시작 실패, 수정 후 입력·확인·정상 종료·termios 복원이 통과했다. stdin과 stdout이 서로 다른 PTY인 경우도 화면 모드를 바꾸기 전에 거절한다.

### deadline 뒤 EOF를 연결 손실로 분류함

watch CLI가 partial frame을 기다리는 동안 peer가 deadline 뒤 연결을 닫으면 timeout 대신 연결 손실을 반환할 수 있었다. blocking read가 끝난 직후 deadline을 먼저 검사하도록 수정했다. deadline 이전 EOF는 기존 연결 손실을 유지한다. 두 경우를 Unix socket 단위 테스트로 구분했다.

### 플랫폼에 의존한 검사 조건

Linux fixture locale을 이 기기에 없는 `en_US.UTF-8` 대신 `C.UTF-8`로 변경했다. native zoom flag 변경과 UI 동작 완료 receipt 사이의 차이를 기다리도록 검사도 수정했다. respawn 검사는 shell의 exec 최적화에 기대지 않도록 `/bin/cat -`를 직접 실행한다. Rust 1.98 Clippy가 지적한 고정 크기 인자 순회도 `as_chunks`로 변경했다.

독립 검토에서 발견한 SSH fixture cleanup 실패 누락과 deadline 회귀 테스트의 시작 순서 문제도 수정하고 재검토를 통과했다.

## 짧은 idle 측정

120×32 UI 하나, source 하나, native pane 두 개, 관찰 항목 1개 또는 50개로 각각 5초 측정했다. 실제 모델·provider TUI 비용은 제외했다. [원시 결과](data/debian-ssh/ui-idle.json)에 실행 파일 해시와 측정값을 보존했다.

| 관찰 항목 | UI RSS | agentd RSS | UI thread | idle 출력 | 추가 HTTP |
| --- | --- | --- | --- | --- | --- |
| 1 | 4.21 MiB | 4.50 MiB | 2 | 0 byte | 0 |
| 50 | 4.78 MiB | 5.50 MiB | 2 | 0 byte | 0 |

각 경우 SSE 연결은 하나였다. `ps` 누적 CPU 차이는 UI와 agentd 모두 0.00초였다. 짧은 구간과 계측 해상도의 결과이며 CPU 비용이 없다는 뜻은 아니다. macOS 결과와 하드웨어가 다르고, 이번에는 Herdr 성능 비교를 다시 실행하지 않았다.

## 다시 실행하기

이미 빌드한 원격 폴더의 검사는 다음과 같이 실행한다.

```sh
ssh debian-ts
cd /home/ochi/.cache/masil-validation.K2fYQL1P
. .build/debian/env.sh

python3 scripts/build.py --agent
.build/bench-venv/bin/python tests/test_ui.py
cargo test --locked --release --manifest-path agent/Cargo.toml
cargo clippy --locked --manifest-path agent/Cargo.toml --all-targets -- -D warnings
```

실제 SSH 조작 검사는 **로컬 masil 저장소에서** 실행한다. 빌드나 파일 복사는 수행하지 않으며, 지정한 원격 checkout의 private fixture 서버만 만든 뒤 종료한다. 알려진 host key를 요구한다.

```sh
.build/bench-venv/bin/python tests/ssh_ui_smoke.py \
  --host debian-ts \
  --remote-root /home/ochi/.cache/masil-validation.K2fYQL1P \
  --output .build/debian-validation/ssh
```

일반 코어를 직접 사용하려면 다음 명령으로 별도 session을 열 수 있다.

```sh
ssh -t debian-ts '/home/ochi/.cache/masil-validation.K2fYQL1P/bin/masil -L try-masil new-session -A -s work'
```

## 아직 검증하지 않은 범위

실제 OS IME의 조합 확정, SSH 끊김·재인증·재접속, 고지연 네트워크, 중첩 tmux, 터미널별 clipboard, 장시간 고출력, BSD는 이번 범위에 없다. 한국어 붙여넣기 성공을 모든 IME 환경의 입력 성공으로 일반화하지 않는다. Debian에서 upstream 전체 회귀 suite를 실행한 것도 아니다. 기존 macOS upstream suite 결과와 이번 프로젝트 검사 결과를 구분한다.
