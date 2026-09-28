# Herdr 사용자 불편 조사 반영 기록

## 출처와 확인 수준

사용자가 2026-09-28 대화에서 제공한 다른 AI의 조사 보고서를 요약하고 rmux 요구사항에 연결했다. 조사 대상은 `herdrdev/herdr`다. 이번 문서 보강에서는 이슈 본문·댓글·release·외부 후기를 다시 열거나 재현하지 않았다. 아래 링크는 제공된 번호로 만든 원문 확인 위치이며, 링크 내용을 검증했다는 뜻이 아니다.

보고서는 최신 열린 이슈 100건 중심 조사, 개별 이슈 103건의 별도 색인, 전체 2,245건·열린 348건, 당시 최신 안정판 v0.9.1을 제시했다. 이 수치와 release 시각은 보고서의 주장으로 보관하며 독립 검증된 현재 통계로 사용하지 않는다. 이슈 수는 불만 사용자 수나 확인된 버그 수가 아니다.

103건 색인 원본과 Hacker News·Reddit·개인 후기의 URL은 제공되지 않았다. 제공 본문에 번호가 등장한 고유 이슈는 43건이다. 이 문서는 그 43건과 후기 요약을 반영한 기록이며, 제공되지 않은 나머지 이슈를 추정하거나 103건 전부를 확인했다고 표시하지 않는다.

## 사례와 rmux 요구의 연결

모든 행의 증거 수준은 `사용자 제공 2차 요약, 원문·재현 미확인`이다. 상태에 관한 설명은 보고서 기준이며 현재 이슈 상태를 뜻하지 않는다. 요구 ID는 [신뢰성 계약](../reliability.md)의 설계 요구다.

| 사례 | 제공된 신고와 조건 | 해석에 남길 제한·정정 | 반영 요구 |
| --- | --- | --- | --- |
| H-01 | [#4476](https://github.com/herdrdev/herdr/issues/4476): v0.9.1·iTerm2 키 보고 설정에서 Ctrl 입력이 문자로 전달 | terminal 설정과 관련된 환경별 신고. 모든 Ctrl 입력의 결함으로 일반화하지 않음 | R-06, R-12 |
| H-02 | [#4543](https://github.com/herdrdev/herdr/issues/4543), [#4356](https://github.com/herdrdev/herdr/issues/4356): AltGr·러시아어/영어 배열 전환, Windows Terminal Preview/WSL 또는 Linux/Ghostty | 서로 다른 입력 경로와 terminal 조합 | R-06, R-12 |
| H-03 | [#4327](https://github.com/herdrdev/herdr/issues/4327): fcitx5 중국어 popup 입력. [#3499](https://github.com/herdrdev/herdr/issues/3499): Windows 한글 IME→SSH→Linux의 Gemini/Cline에서 마지막 음절 손실 | #4327은 pane/paste 대조에서 정상. #3499는 Claude 대조 정상이며 추가 버전 정보 미응답으로 종료됐다는 설명. 수정 완료로 분류하지 않음 | R-06, R-12 |
| H-04 | [#4341](https://github.com/herdrdev/herdr/issues/4341): WSL2·Alacritty·byobu/tmux에서 복사 성공 알림과 실제 clipboard 불일치 | 최초 신고자는 중첩 밖에서 정상, macOS byobu에서도 정상이라고 정정. `WSL에서 복사 불가`로 일반화하지 않음 | R-01, R-07 |
| H-05 | [#4551](https://github.com/herdrdev/herdr/issues/4551), [#4545](https://github.com/herdrdev/herdr/issues/4545): Codex animation·OMP 출력 중 더블클릭/드래그 선택 취소 | provider와 선택 방식의 조건을 나누어 재현 | R-06 |
| H-06 | [#4315](https://github.com/herdrdev/herdr/issues/4315), [#4515](https://github.com/herdrdev/herdr/issues/4515), [#4500](https://github.com/herdrdev/herdr/issues/4500): PNG drop·file URI/이미지 clipboard 차이·원격에 로컬 경로 전달 | bytes, 파일 이름, URI와 실행 위치는 별개. 같은 원인으로 묶지 않음 | R-07 |
| H-07 | [#4436](https://github.com/herdrdev/herdr/issues/4436), [#4447](https://github.com/herdrdev/herdr/issues/4447): browser 중복 열기 또는 원격 링크 클릭 불가 | 링크 강조와 실제 열기 지원을 구분 | R-07, R-11 |
| H-08 | [#4557](https://github.com/herdrdev/herdr/issues/4557), [#4454](https://github.com/herdrdev/herdr/issues/4454), [#4511](https://github.com/herdrdev/herdr/issues/4511): OpenCode 승인 대기를 working으로 표시, background·부모/자식 상태 혼선 | 한 신고자의 빈도는 전체 사용자의 발생률이 아님 | R-03 |
| H-09 | [#4418](https://github.com/herdrdev/herdr/issues/4418): 노트북에서 확인한 완료가 다른 client에서 새 Done으로 보이고 이동 명령과 badge가 불일치 | 동일 사용자 다기기 확인 상태와 client별 화면 상태의 경계 문제라는 제품적 해석 | R-05, R-11 |
| H-10 | [#4537](https://github.com/herdrdev/herdr/issues/4537): background Claude pane prompt 명령은 성공하지만 turn 시작이 불명확 | 일부 판단은 수신보다 늦은 처리 로그를 유실로 오인한 것으로 철회됐고 재시도로 중복 지시가 생겼다는 설명. 실제 유실 여부와 분리 | R-01, R-02 |
| H-11 | [#4690](https://github.com/herdrdev/herdr/issues/4690): tab 삭제 후 wait가 120초 timeout까지 유지, pane 직접 종료는 빠르게 반환 | 삭제 경로별 실제 대상 수명과 waiter 해제를 검증할 사례 | R-04 |
| H-12 | [#4641](https://github.com/herdrdev/herdr/issues/4641): unknown 상태의 자동 Enter가 신뢰 선택에 전달될 가능성 | 실제 화면을 사용한 모형 Codex 재현이라는 설명. 실사용 무단 승인 사고로 확인된 자료가 아님 | R-02 |
| H-13 | [#4414](https://github.com/herdrdev/herdr/issues/4414), [#4574](https://github.com/herdrdev/herdr/issues/4574), [#4472](https://github.com/herdrdev/herdr/issues/4472): 배치 12개/대화 3개 복구, 느린 shell 초기화, Windows cwd 변경 | 대화 파일은 남았다는 사례 포함. 영구 삭제와 자동 resume 실패를 구분 | R-08 |
| H-14 | [#4540](https://github.com/herdrdev/herdr/issues/4540): macOS/Ghostty client UI 정지. [#4578](https://github.com/herdrdev/herdr/issues/4578): 큰 worktree 삭제 후 endpoint_busy로 다른 작업 차단 | #4540은 server·agent가 살아 있었다는 설명. client 장애를 전체 작업 소실로 바꾸어 적지 않음 | R-10, R-12 |
| H-15 | [#4506](https://github.com/herdrdev/herdr/issues/4506): 256 logical CPU Linux에서 약 20개 agent 동시 출력 시 높은 CPU·API 지연 | 후속 조건은 idle 저부하/동시 출력 고부하로 좁혀짐. pending-release 후 재개됐다는 설명. 일반 노트북 수치나 수정 완료로 일반화하지 않음 | R-10, R-12 |
| H-16 | [#4496](https://github.com/herdrdev/herdr/issues/4496), [#4258](https://github.com/herdrdev/herdr/issues/4258), [#4291](https://github.com/herdrdev/herdr/issues/4291): 재인증 경로 누락, tcsh 원격 연결, 명시 중지한 server 자동 재시작 | 인증·shell 호환·사용자 의도를 구분 | R-09 |
| H-17 | [#4639](https://github.com/herdrdev/herdr/issues/4639), [#4614](https://github.com/herdrdev/herdr/issues/4614): 원시 PowerShell 출력/깨진 한글, 원격 하단 줄 잘림 | 설정의 항상 실패를 의미하지 않음. 오류 안내와 화면 전달을 따로 검증 | R-06, R-09, R-11, R-12 |
| H-18 | [#4692](https://github.com/herdrdev/herdr/issues/4692), [#4375](https://github.com/herdrdev/herdr/issues/4375), [#4364](https://github.com/herdrdev/herdr/issues/4364), [#4399](https://github.com/herdrdev/herdr/issues/4399): scrollbar와 접기 조작 충돌, 선택 tab·sidebar·분할 상태 초기화 | 화면 크기와 endpoint 전환 조건을 포함 | R-11 |
| H-19 | [#4283](https://github.com/herdrdev/herdr/issues/4283), [#4460](https://github.com/herdrdev/herdr/issues/4460): focus 명령 성공과 실제 client 화면 불일치 | server 처리와 대상 client 적용은 별도 단계 | R-01, R-11 |
| H-20 | [#1803](https://github.com/herdrdev/herdr/issues/1803), [#3187](https://github.com/herdrdev/herdr/issues/3187), [#3651](https://github.com/herdrdev/herdr/issues/3651), [#3701](https://github.com/herdrdev/herdr/issues/3701), [#1797](https://github.com/herdrdev/herdr/issues/1797): 이름 입력 편집, Windows 긴 prompt/원격, submodule worktree 관련 개선 | 보고서는 v0.9.1 release 포함으로 분류. 같은 증상의 모든 변형이 해결됐다는 뜻이 아니며 이번 세션에서 release를 재검증하지 않음 | R-06, R-08, R-09, R-10, R-12 |
| H-21 | [#1340](https://github.com/herdrdev/herdr/issues/1340): 과거 AGPL 때문에 회사 도입이 막혔다는 개발자의 전달 | 간접 요구이며 보고서는 현재 Apache-2.0 전환을 명시. 현재 라이선스 장벽이나 법률 판단으로 재사용하지 않음 | 도입 판단 참고 |

## 번호 없는 후기와 정책 요약

보고서는 키 재학습, VS Code/가상 데스크톱 배치와의 충돌, 2~3개 이상 작업에서의 맥락 전환 부담, worktree 생성 후 파일 준비·설치용 plugin 필요, browser plugin의 화질 한계, 기존 설정·layout 이전 비용을 소개했다. 후기 URL·원문·작성일은 이번 자료에 없다. 전체적으로 긍정적인 전환 후기도 있었다는 설명을 함께 보존한다.

rmux에는 다음 목표로 반영한다.

- tmux 기본 키와 설정 호환을 유지한다. 새로운 키 체계의 학습을 필수 조건으로 만들지 않는다.
- 1~3개 작업만 운영해도 attention과 원래 pane 복귀가 유용해야 한다. 동시 agent 수를 늘리는 것만 성공 지표로 삼지 않는다.
- 기존 editor·외부 terminal·가상 데스크톱 흐름을 유지할 수 있게 하고, 원하는 창이나 파일을 사용자가 선택해 연다.
- worktree 초기화 단계의 상태·실패·재시도·취소를 다룬다. 숨김 파일·비밀 설정 복사나 설치 명령은 사용자가 명시한 규칙으로만 실행한다.
- terminal browser가 일반 browser를 대체해야 한다는 요구를 추가하지 않는다. 이번 지적은 plugin에 관한 후기다.

기여 제한 정책에 관한 설명은 사용자 패치 경로의 제약으로만 기록한다. 광범위한 반발이 있었다는 근거는 제공되지 않았다. rmux의 라이선스와 기여 정책은 별도 결정이며, 이 조사만으로 정하지 않는다.

## 후속 검증에서 유지할 구분

이슈별로 원문 확인 여부, 신고 version·OS·terminal·중첩·provider, 직접 재현 여부, 설명의 정정, upstream 수정 commit과 배포 version, 같은 조건의 재검증 결과를 분리해 기록한다. 코드 소스에서 가능한 원인을 읽었다는 사실은 사용자 신고의 재현을 대신하지 않는다.

upstream 분류는 적어도 `열린 신고`, `수정 배포로 보고됨`, `해결 미확인 종료`, `수정 시도 후 재개`를 구분한다. 이는 제공 보고서의 분류이며, GitHub 상태를 확인하면 확인 시각과 근거를 추가한다. `closed` 또는 `pending-release`만으로 해결을 판정하지 않는다.

rmux 요구사항의 검증 상태는 upstream 이슈 상태와 별도다. Herdr가 해결했더라도 rmux의 회귀 시나리오는 남고, Herdr에서 미해결이어도 rmux에 같은 버그가 있다고 주장하지 않는다. 검증할 동작은 [완료 판정의 신뢰성 시나리오](../acceptance.md)에 연결한다.
