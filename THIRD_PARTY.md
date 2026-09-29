# 포함한 소스와 고지

- `core/`는 tmux commit `94796f6b1182507efac8a272fc309a79e22e58a5`를 기반으로 한다. 원본 [COPYING](core/COPYING)과 source별 copyright를 유지한다. 가져온 파일 hash는 [UPSTREAM.json](core/UPSTREAM.json)에 있다. GitHub workflow는 실행 소스에 포함하지 않았다.
- `core/compat/yyjson/`은 yyjson 0.13.0의 고정 source와 원본 LICENSE를 포함한다.
- 선택적 Rust 관찰 client의 직접·간접 dependency version은 `agent/Cargo.lock`에 고정한다.
- 성능 측정용 Python 환경은 `tests/performance/requirements.txt`의 pyte 0.8.2와 wcwidth 0.2.13을 사용한다. 이 환경은 `.build/bench-venv`에만 설치하며 masil 실행 파일에는 포함하지 않는다.
- `agent/src/manifests/`의 agent 화면 감지 manifest는 Herdr commit `0d5d6f1f317e238c8297076bc6ab5c3a0cd56283`에서 가져왔다. Herdr는 Apache License 2.0으로 배포되며 원문은 [docs/reference/herdr-agent-LICENSE.txt](docs/reference/herdr-agent-LICENSE.txt)에 있다.

빌드 도구 m4·autoconf·automake는 필요할 때 `.build/tools`에 준비한다. 해당 도구의 원본 source·고지는 `.build/tool-src`에 남으며 masil 실행 파일에 포함하지 않는다. 기존 reference clone과 시스템 tmux/Herdr 설치는 변경하지 않는다.
