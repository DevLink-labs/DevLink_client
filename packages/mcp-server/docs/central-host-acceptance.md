# 실제 두 Host 수용 검증 (S15P31D108-66)

`scripts/verify-central-hosts.mjs`는 실제 Codex와 Claude Code CLI를 두 사용자·서로 다른 합성 프로젝트에 연결한다. Codex가 Claude에 invoice 수정을 요청하고 결과를 조회한 뒤, Claude가 Codex에 slug 수정을 요청한다. 두 Host가 직접 도구를 호출하고 파일을 수정·테스트·완료 등록해야 성공한다. 부모 실행기는 초기 결함과 고정 테스트만 준비한다.

## 선행 환경

- 실제 중앙 서버와 PostgreSQL을 **격리 loopback 환경**에서 실행한다. 운영 URL은 실행기가 거부한다.
- -97 중앙 계약과 -96 알림 구현을 포함한 native CLI/MCP를 빌드한다. 해당 PR이 미머지인 동안에는 합성 소스의 커밋·파일 해시를 별도 기록한다. 기존 develop 자체를 통과했다고 표기하지 않는다.
- Node.js 24+, Git, Claude Code CLI의 정확한 `claude-opus-5-5`/high, 로컬 파일 읽기·편집과 `node --test`/`git diff`가 허용된 Codex CLI가 필요하다. Claude 지정 모델은 임의 대체하지 않는다. Codex는 설치된 CLI 기본 모델을 사용하고 실제 모델 ID가 보고되지 않으면 그 사실을 기록한다.
- Codex는 사용자 설정(Windows sandbox 포함)을 읽고 `default_permissions=":workspace"`와 `approval_policy=on-request`를 명시한다. `sandbox_mode`/`sandbox_workspace_write`를 함께 설정하지 않은 허용 환경이 필요하다. 사용자 전역 설정은 변경하지 않으며 별도 명령 실행 정책이 차단하면 중단한다. 전체 접근·정책 우회 옵션은 사용하지 않는다.

PowerShell에서 필요한 절대 경로와 고정 중앙 커밋을 지정한다.

```powershell
$env:DEVLINK_TEST_CENTRAL_URL = 'http://127.0.0.1:<isolated-port>'
$env:DEVLINK_TEST_CENTRAL_COMMIT = '<40-character-server-commit>'
$env:DEVLINK_TEST_CLI = 'C:\absolute\devlink.exe'
$env:DEVLINK_TEST_MCP_ENTRY = 'C:\absolute\mcp-server\dist\index.js'
$env:DEVLINK_TEST_CONTRACT_COMMIT = '<40-character-client-97-commit>'
$env:DEVLINK_TEST_NOTIFICATIONS_COMMIT = '<40-character-client-96-commit>'
$env:DEVLINK_TEST_HOST_ARTIFACTS = 'C:\absolute\ignored-proof-directory'
node packages/mcp-server/scripts/verify-central-hosts.mjs
```

필요하면 `DEVLINK_TEST_CODEX`/`DEVLINK_TEST_CLAUDE`로 설치된 CLI 실행 파일을 지정한다. 실행기는 독립 테스트 사용자/팀/Agent를 만들고 임시 프로필의 브로커를 실행한다. 시작한 프로세스만 정리하며 자격 원문이 Host 출력에 있으면 증거를 공개하지 않는다.

## 성공 판정

각 요청은 실행별 nonce에 한정해 운영자가 명시적으로 수락한다. 일반 수신 작업에 대한 자동 수락을 허용하지 않는다.

- 실제 MCP 호출 기록에서 대상 Agent와 요청 문맥, 수신자의 PENDING 조회 → 명시적 IN_PROGRESS → COMPLETED 순서를 검증한다.
- 요청자 자신의 `get_task_result` 완료 조회에서 검증 요약·변경 파일과 diff를 실제로 받았는지 확인한다. 모델의 최종 문장이나 CLI exit 0만으로 성공하지 않는다.
- Host의 `node --test` 호출과 성공 결과를 대응시킨다. 부모도 같은 테스트를 독립 실행한다. 고정 테스트와 Git index를 변경했거나 수정 범위가 지정 소스 밖이면 실패다.
- Host가 제출한 diff와 실제 파일의 `git diff`, 중앙 저장 결과가 일치해야 한다.
- 받는 Host 자신의 성공한 편집 이벤트와 두 방향 task ID를 교차 확인한다. 요청자가 읽은 diff도 수신자의 실제 diff와 대조한다. 비교에서 CRLF/LF와 쉘 출력 끝의 개행 하나만 정규화하고 소스 공백은 보존한다.
- Git diff에 나오지 않는 새 파일도 별도 조회한다. 실행기가 관리하는 config/MCP 설정·브로커 runtime만 제외한다. Codex 계획 목록 이벤트는 업무 성공 증거로 사용하지 않는다.
- 합성 fixture의 로컬 Git 설정만 core.autocrlf=false/core.safecrlf=false로 지정해 전역 줄바꿈 설정의 경고가 Host diff에 섞이지 않도록 한다. 사용자 전역 Git 설정은 변경하지 않는다.
- 명시적으로 제공한 MCP entry와 native CLI를 사용하고, 두 client 커밋·중앙 커밋·CLI/entry/전체 MCP dist의 SHA256을 실행별 출처 기록에 보관한다.
- 양쪽이 모두 통과하고 소유 프로세스 정리까지 끝난 경우에만 최종 report의 passed가 true가 된다. 실행별 하위 폴더에 JSONL·프롬프트·stderr·report를 보관한다.

`node --test test/central-host-proof.test.mjs`는 성공 오인 방지 판정기를 검증한다. 실제 Host·중앙 실행을 대신하는 시험은 아니다.

## 2026-11-02 첫 시도와 재개

최초 실제 실행에서 Claude의 A→B 코드 수정·테스트·완료 회신은 도구 기록과 중앙 task 21로 확인했다. Codex는 B→A task 22를 수락한 뒤 파일 읽기·`node --test`·`git diff`가 실행 정책에서 거부돼 FAILED/partiallyModified=false를 실제 등록했고 Claude가 조회했다. 전체 수용 검증은 실패했다.

공식 `:workspace` 설정의 별도 제한된 프로브도 단순 파일 읽기를 실행 정책에서 거부했다. 이후 사용자가 권한 설정을 변경했다. 사용자 설정을 읽는 제한된 프로브에서 파일 읽기·apply_patch 편집·`node --test`·`git diff`가 실제 성공한 것을 확인하고, 실행기의 `--ignore-user-config`를 제거해 허용된 Windows 설정을 사용하도록 했다.

재개 시험은 다음 조합으로 **양방향 통과**했다. client develop `083aa38c86f079e660bd650e466dd5af6937207d`에는 -97 PR #28이 머지됐으며 원본 -97 커밋과 Git tree가 동일하다. native CLI는 이 소스에 아직 미머지인 -96 `07d4c48fba783bbb96d3e9b819d657dad1158928`을 합성한 빌드이고, 중앙 서버는 아직 미머지인 -67 `d06d23969f341eaa96a3af562adad3ae9b53c201`과 PostgreSQL 17.11이다. develop 단독 또는 배포 환경 전체가 완료됐다는 판정은 아니다.

| 요청 | 실제 결과 |
| --- | --- |
| Codex → Claude, task 27 | PENDING → 명시적 IN_PROGRESS → invoice 수정 → Host 테스트 2/2 → COMPLETED → Codex의 결과·diff 조회 |
| Claude → Codex, task 28 | PENDING → 명시적 IN_PROGRESS → slug 수정 → Host 테스트 2/2 → COMPLETED → Claude의 결과·diff 조회 |

부모의 독립 테스트도 양쪽 2/2 통과했고, 고정 테스트·index 불변, 지정 소스만 수정, 실제 diff/중앙 저장 결과/요청자 조회 결과 일치, 시작한 브로커/MCP 종료를 확인했다. report의 `passed`와 `actualLlmHostsExecuted`, `ownedProcessesStopped`는 모두 true다. Claude 실제 모델은 `claude-opus-5-5`/high이며, Codex는 CLI 기본 모델을 사용했고 JSONL에 모델 ID가 없어 임의 기재하지 않는다.

재현 증거는 ignored `.planning/mvp-closeout-2026-11-02/66-resumed-artifacts/run-bbd58f6e-5f1c-4094-bce5-57a12386b3cb/`의 프롬프트·JSONL·report·빌드 출처/SHA256과 `66-hosts-resumed.log`에 보관한다. 자격 파일·토큰은 PR에 포함하지 않는다. 최신 develop 기준 MCP 전체 시험은 115 passed, 0 failed이며, 이 중 판정기 시험 10건은 실제 Host 수용을 대체하지 않는다.

다른 팀 접근 거부, 실패 사유/부분 수정 여부, 미존재·오프라인 수신자의 저장 전 거부, 저장 후 전달 실패의 PENDING 보존, 동일 requestKey 멱등성과 ACK 시간 초과는 선행 -97 실제 SDK 왕복 및 -67 중앙 시험에서 별도로 검증했다. 중앙 d06d239의 PostgreSQL 기반 전체 575건 통과 기록을 사용하며, 모든 예외를 두 LLM Host가 직접 재현했다고 확대하지 않는다. 주요 근거는 `TaskCreationIntegrationTest`의 duplicate/replay/routingFailure/unavailableReceiver/missingReceiver/timeout 사례, `TaskRoutingIntegrationTest`의 readyOnlyAndStaleOnlineReceiversCannotCreateAnyTask, `TaskAckIntegrationTest`의 missingAckTimesOutAndLateAckCannotChangeTimeoutResult, `TaskQueryIntegrationTest`의 rejectsAccessToAnotherTeam이다.
