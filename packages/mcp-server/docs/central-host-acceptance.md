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

부분 수정 뒤 FAILED 경로(AC2)는 별도 실행기 `scripts/verify-central-failure.mjs`가 같은 환경 변수로 검증한다. 아래 "2026-11-06 AC2 실행과 AC3 판정 보강"을 따른다.

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

## 2026-11-06 재검증 (develop 병합 후)

2026-11-02 통과는 미병합 client 빌드를 합성한 결과였다. 그래서 관련 client PR이 모두 develop에 병합된 뒤, 병합된 develop 그대로 다시 실행했다.

- **실행 조합**
  - client develop `dfc28c72b7523284ec83c3c7d816dcf18a44fd8a`
    - -96·-97, 질문·답변, 거절, requestKey, 훅 설치(#76), 지연 알림(#78), 프로브 상한(#73)이 모두 들어 있다.
    - native CLI는 이 소스의 debug 빌드다.
  - 중앙 서버 `34b2c15c116041e0bb70fd91a2f9ef74b28c79b9`(실행 시점 DevLink_server `origin/develop`)
  - 격리 PostgreSQL 17.11: 포터블, loopback 임의 포트, 전용 계정
  - Windows 11, Codex CLI 0.166.0, Claude Code 2.1.310. 실행기가 Claude를 `--model claude-opus-5-5 --effort high`로 실행한다.
  - 서버는 `local` 프로필로 실행했다. 기본 프로필만으로는 `devlink.websocket.allowed-origins` placeholder가 채워지지 않아 시작에 실패했다.
- **결과: 양방향 통과**
  - report의 `passed`, `actualLlmHostsExecuted`, `ownedProcessesStopped`가 모두 true다.

| 요청 | 실제 결과 |
| --- | --- |
| Codex → Claude, task 1 | PENDING → 명시적 IN_PROGRESS → invoice 수정 → Host 테스트 통과 → COMPLETED → Codex가 결과·diff 조회 |
| Claude → Codex, task 2 | PENDING → 명시적 IN_PROGRESS → slug 수정 → Host 테스트 통과 → COMPLETED → Claude가 결과·diff 조회 |

- **판정기가 확인한 항목** (2026-11-02와 같음)
  - 부모의 독립 테스트가 통과했다.
  - 고정 테스트와 Git index가 바뀌지 않았다.
  - 지정한 소스 파일만 수정됐다.
  - 실제 diff, 중앙 저장 결과, 요청자가 조회한 결과가 서로 일치한다.
- **모델**
  - Claude는 `claude-opus-5-5`로 보고됐다.
  - Codex는 CLI 기본 모델을 썼다. transcript에 모델 ID가 없어 적지 않는다.
- **훅 설치 실제 확인 (S15P31D108-176)**
  - 같은 격리 중앙에서 별도 사용자 A·B를 만들고, A가 B에게 실제 작업 하나(task 4)를 보냈다.
  - B 프로필로 `devlink hooks install --host claude-code --yes`를 실행했다. 대상은 `CLAUDE_CONFIG_DIR`로 지정한 임시 사용자 계층이다.
  - 결과는 `applied`이고 status는 `installed`였다.
  - B의 프로젝트에서 `claude -p --setting-sources project --settings <설치된 settings.json>`을 실행했다. Claude Code가 훅 문맥에서 "Task #4 (from agent 9)"를 읽었고, 이것이 사용자 승인이 아니라는 점도 인식했다.
  - 이 확인은 설치된 파일을 `--settings`로 직접 넘긴 것이다. 그래서 설치된 내용이 동작한다는 확인이며, Claude Code가 사용자 계층(`CLAUDE_CONFIG_DIR`)의 파일을 스스로 찾아 읽는지는 확인하지 않았다.
  - 세션이 끝난 뒤 같은 작업 폴더에서 `updates-hook --host claude-code --event user-prompt-submit`을 다시 실행하면 출력이 없었다. 훅이 그 알림을 소비했다는 뜻이다. Claude 세션 안에서 SessionStart와 UserPromptSubmit 중 어느 쪽이 문맥을 넣었는지는 따로 관측하지 않았다.
  - Codex는 같은 설치기가 만든 `hooks.json`의 이벤트 키(SessionStart·UserPromptSubmit)와 `trust_codex_hooks` 안내만 확인했다. `CODEX_HOME`을 격리하면 로그인 정보도 함께 빠지기 때문에 실제 Codex 세션은 실행하지 않았다. 사용자의 실제 `~/.claude`·`~/.codex`는 건드리지 않았다.
- **실제 사용자 계층 설치 (같은 날)**
  - 개발 PC의 `~/.claude/settings.json`·`~/.codex/hooks.json`에 `devlink hooks install --yes`로 설치했다. 설치된 `devlink.exe`는 같은 develop의 release 빌드로 교체했다.
  - 두 Host 모두 사용자 설정만으로 DevLink 훅이 실행돼 Host별 cursor가 v1에서 v3로 다시 저장됐다.
    - Claude Code는 SessionStart·UserPromptSubmit 훅 이벤트가 모두 exit 0이었다.
    - Codex는 사용자가 `/hooks`에서 신뢰 승인한 뒤 실행했다.
  - 실제 운영 중앙에는 새 작업을 만들지 않았다. 그래서 이 단계에서 주입된 내용은 없다.
- **증거 보관**
  - report 전문(빌드 출처 SHA256 포함)과 `hook-check.log` 요약은 Notion 2026-11-06 작업 정리에 옮겼다.
  - 프롬프트·JSONL·stderr 원본(`run-0710922e-c4ea-416d-8635-15f3fdfc6bfc`)은 실행 세션의 임시 작업 폴더에만 있어 영구 보관되지 않는다.
  - 자격 파일과 토큰은 기록하지 않았다.
- **아직 하지 않은 것**
  - AC2(부분 수정 뒤 FAILED)와 AC7(제3자 재실행)은 이 실행에서 하지 않았다. AC2는 아래 "AC2 실행과 AC3 판정 보강"에서 실행했다.
  - 교차 팀·offline·전달 실패·중복 요청은 이번에도 Host로 직접 재현하지 않았다. 근거는 위에 적은 중앙 `d06d239` 기준 통합 시험이다.
  - 실행기 report는 taskId만 남기고 requestId·requestKey는 기록하지 않는다.

## 2026-11-06 AC2 실행과 AC3 판정 보강

AC2(부분 수정 → 테스트 실패 → FAILED → 요청자 조회)를 실제 두 Host로 처음 실행했다. 같은 날 AC3 판정기도 보강했다. 이전 판정기는 호출 순서만 봤고, 수락 전에 파일이 바뀌지 않았는지는 확인하지 않았다.

### 실패 경로 실행기 `scripts/verify-central-failure.mjs`

환경 변수와 선행 환경은 위 성공 실행기와 같다. Codex가 요청하고 Claude Code가 받는다.

- **실패를 일으키는 fixture**
  - `src/invoice.mjs`의 합계가 수량을 곱하지 않는다.
  - 고정 테스트 2개 중 하나는 수량 곱셈을 요구한다. 다른 하나는 `src/format.mjs`를 import한다.
  - 수신 프롬프트는 새 파일 생성을 금지한다. 그래서 수신 Host는 첫 테스트만 고칠 수 있고, 남은 테스트는 실패한다.
  - 즉 실패는 실행기가 의도해서 유도한 것이다. Host가 스스로 실패 상황을 판단했다는 증거로 확대하지 않는다.
- **판정**
  - 수신자는 PENDING 조회 → 명시적 IN_PROGRESS → 편집 → 실패한 `node --test` → FAILED 순서여야 한다.
  - FAILED는 정확히 한 번이어야 하고, `complete_task`는 없어야 한다. failureReason은 비어 있으면 안 되고, `partiallyModified=true`여야 한다.
  - 테스트 실패는 node test runner 요약 줄(`ℹ pass 1`, `ℹ fail 1`)로 판단한다. 종료 코드는 쓰지 않는다. Host가 `node --test 2>&1; echo exit=$?`처럼 명령을 이어 붙이면 도구 결과가 성공으로 보이기 때문이다.
  - 실패한 테스트 실행은 마지막 편집 뒤에 있어야 하고, FAILED 호출은 그 실패 실행 뒤에 있어야 한다.
  - 고정 테스트와 Git index는 바뀌면 안 된다. 수정 범위는 `src/invoice.mjs`뿐이고, 추적되지 않는 새 파일도 없어야 한다.
  - 부모가 같은 테스트를 따로 실행해 종료 코드 1과 pass 1/fail 1을 확인한다. 남은 diff와 테스트 출력은 `receiver-remaining.diff`, `parent-node-test.txt`로 보관한다.
  - 중앙 `GET /api/v1/tasks/{id}`가 FAILED, `partiallyModified=true`, 같은 failureReason을 돌려줘야 한다.
  - 요청자 Codex는 `create_task`(PENDING) 뒤 `get_task_result`로 FAILED, failureReason, `partiallyModified=true`를 실제로 읽어야 한다.

### AC3 판정 보강 (성공·실패 실행기 공통)

- **편집 위치**
  - 받는 Host의 성공한 프로젝트 편집은 **도구 호출이 나간 위치**로 판정한다.
  - 수락은 `update_task_status(IN_PROGRESS)` **결과가 돌아온 위치**로 판정한다. 같은 턴에 병렬로 낸 편집이 수락보다 먼저 실행될 수 있기 때문이다.
  - 첫 편집이 수락 결과보다 앞서면 실패다.
- **수락 전 셸 명령**
  - 수락 결과 이전의 셸 명령은 읽기 전용 허용 목록에 있어야 한다. 허용하는 것은 `sleep`, `cat`, `ls`, `git diff/status/log/show`, `node --test` 등이다.
  - pwsh·cmd 래퍼는 벗겨서 본다. `>` 리다이렉트, `tee`, `sed -i`, `rm`, `Set-Content`, `git checkout` 같은 쓰기 표지가 있으면 실패다.
  - 목록에 없는 명령(예: `node fix.mjs`, `python ...`)도 실패로 본다.
- **파일 감시**
  - 실행기가 nonce를 정한 뒤부터 수신 프로젝트의 snapshot(경로·SHA256)을 약 250ms 간격으로 다시 계산한다. 간격은 표본 사이의 대기 시간이라 실제 주기는 조금 더 길다. AC2 실행에서는 평균 약 280ms였다.
  - 같은 시점에 수신자 Agent 토큰으로 중앙의 받은 작업 상태를 조회한다.
  - 받은 작업이 모두 PENDING인 동안 snapshot이 처음과 달라지면 위반이다.
  - 다음 경우도 실패다.
    - 조회 오류가 하나라도 있다.
    - 작업이 있는 PENDING 표본이 0이다.
    - 수락 상태를 한 번도 보지 못했다.
  - 실행기가 관리하는 `config.json`·`mcp.json`·`.devlink-runtime/`·`.git`은 비교에서 뺀다.

### 실행 결과 (2026-11-06)

- **실행 조합**
  - client develop `77f07c3560fa0c8d20f63a471b5d913e3b4eda8e`의 native CLI(debug)와 MCP dist
  - 판정기·실행기는 이 문서와 같은 PR의 것이다. develop `77f07c3`에는 없다.
  - 실행 뒤 교차검토 지적을 반영해 판정기를 더 엄격하게 고쳤다. 래퍼로 쓴 `timeout`, 명령 치환, `find -delete/-exec`, `git diff --output`도 수락 전에 금지했다. 또 감시 표본이 ENOENT를 만나면 한 번 다시 시도하게 했다. 이번 실행의 transcript 3개는 고친 판정기로 다시 판정해도 통과한다(수락 전 명령은 모두 `sleep`·`Start-Sleep`).
  - 중앙 `34b2c15`, PostgreSQL 17.11(격리, loopback)
  - Codex CLI 0.166.0, Claude Code 2.1.310(`claude-opus-5-5`/high)
- **결과:** 두 실행기 모두 `passed`, `actualLlmHostsExecuted`, `ownedProcessesStopped`가 true였다.

| 시나리오 | 결과 |
| --- | --- |
| AC2 Codex → Claude, task 11 | PENDING → 명시적 IN_PROGRESS → `src/invoice.mjs` 수량 곱셈만 수정 → Host 테스트 pass 1/fail 1 → FAILED(`partiallyModified=true`, 사유 538자) → Codex가 같은 사유 조회 |
| AC1·AC3 Codex → Claude, task 12 | 성공 경로. 수락 전 읽기 전용 명령 3개 |
| AC1·AC3 Claude → Codex, task 13 | 성공 경로. 수락 전 읽기 전용 명령 2개 |

- **AC2 세부**
  - 남은 diff는 `src/invoice.mjs` 한 줄이다(`item.unitCents` → `item.unitCents * item.quantity`).
  - 부모 테스트는 종료 코드 1, tests 2, pass 1, fail 1이었다.
  - failureReason에는 통과한 테스트, 실패한 테스트와 원인(`src/format.mjs` 없음, `ERR_MODULE_NOT_FOUND`), 새 파일 생성 금지 때문에 추가하지 않았다는 점, 부분 수정을 남겼다는 점이 들어 있다.
- **파일 감시**

| task | 표본 | PENDING 표본(작업 있음) | 위반 | 조회 오류 |
| --- | --- | --- | --- | --- |
| 11 | 228 | 29 | 0 | 0 |
| 12 | 372 | 11 | 0 | 0 |
| 13 | 372 | 39 | 0 | 0 |

- **증거 보관**
  - report와 빌드 출처 SHA256은 Notion 2026-11-06 작업 정리에 옮긴다.
  - JSONL·프롬프트·stderr·`receiver-remaining.diff`·`parent-node-test.txt` 원본(`run-22b65d95-842d-4cc2-a8cc-6f693c47262d`, `run-f0e91dbd-a814-4de1-8174-f5701ee5f350`)은 실행 세션의 임시 폴더에만 있다.
  - 자격 파일과 토큰은 기록하지 않았다.

### 한계

- **감시 간격**
  - 파일 감시는 약 250ms 간격 표본이다. 그 사이에 바뀌었다가 원래대로 돌아간 변경은 놓칠 수 있다.
  - 마지막 PENDING 표본과 수락 사이의 한 간격 안에 생긴 변경도 감시가 보지 못한다.
  - 이 빈틈은 transcript의 편집 위치와 셸 명령 판정이 보완한다.
- **Codex 편집 이벤트**
  - Codex `file_change`가 `item.completed`만 남기면 편집 위치는 완료 위치로 잡힌다.
  - 그래도 수락 결과보다 앞서면 실패로 판정하는 것은 같다.
- **셸 판정의 범위**
  - 허용 목록 판정은 명령 문자열을 기준으로 한다.
  - 허용된 명령 안에서 파일을 바꾸는 우회까지 의미적으로 막지는 못한다. 이 경우는 파일 감시가 보완한다.
- **failureReason 형식**
  - 프롬프트는 상대 경로만 쓰라고 요구하지만, 판정기는 failureReason에 절대 경로가 없는지 검사하지 않는다.
- **실패 유도**
  - AC2의 실패는 fixture와 프롬프트가 만든 것이다.
  - 실제 업무에서 Host가 실패를 정직하게 보고하는지는 이 실행으로 일반화하지 않는다.

## 제3자 재실행 절차 (AC7)

AC7은 작성자가 아닌 사람이 같은 절차를 따라 재실행하고 결과를 기록해야 충족된다. 아래 절차는 2026-11-06 실행을 그대로 옮긴 것이다.

1. **도구 준비**
   - Windows 11 기준 Node.js 24+, Git, Java 21, Rust stable(MSVC)이 필요하다.
   - Codex CLI와 Claude Code CLI에 각각 로그인한다. Claude는 `claude-opus-5-5`를 쓸 수 있어야 한다.
   - Codex 권한 조건은 위 "선행 환경"과 같다.
2. **중앙 서버 준비**
   - DevLink_server를 clone하고 실행할 커밋을 checkout한 뒤 `gradlew bootJar`로 빌드한다.
   - PostgreSQL 17을 loopback 임의 포트에 따로 띄운다. 전용 계정과 DB를 쓴다. Docker 대신 포터블 배포본도 된다.
   - 다음 환경 변수를 지정하고 실행한다. `--spring.profiles.active=local`이 없으면 WebSocket origin placeholder 때문에 시작에 실패한다.
     - `DB_URL`(`jdbc:postgresql://127.0.0.1:<db-port>/<db>`)
     - `DB_USERNAME`, `DB_PASSWORD`
     - `TASK_MESSAGE_CURSOR_SECRET`(임의의 긴 문자열)

     ```powershell
     java -jar build/libs/app.jar --spring.profiles.active=local --server.port=<port> --server.address=127.0.0.1
     ```

   - `GET http://127.0.0.1:<port>/api/v1/health`가 200인지 확인한다.
3. **client 준비**
   - 이 저장소의 기준 커밋을 checkout한다.
   - 저장소 루트에서 `cargo +stable-x86_64-pc-windows-msvc build --manifest-path rust/Cargo.toml --locked -p devlink-cli`를 실행한다.
   - 빌드 결과 `rust/target/debug/devlink.exe`의 절대 경로를 `DEVLINK_TEST_CLI`에 쓴다.
   - `packages/mcp-server`에서 `npm ci`와 `npm run build`를 실행한다.
4. **실행**
   - 위 "선행 환경"의 환경 변수를 지정한다.
   - 두 COMMIT 변수(`DEVLINK_TEST_CONTRACT_COMMIT`, `DEVLINK_TEST_NOTIFICATIONS_COMMIT`)에는 client 기준 커밋 SHA를 넣는다.
   - `DEVLINK_TEST_HOST_ARTIFACTS`는 실행기마다 다른 빈 폴더로 지정하고, 아래 두 실행기를 차례로 실행한다.

   ```powershell
   node packages/mcp-server/scripts/verify-central-failure.mjs
   node packages/mcp-server/scripts/verify-central-hosts.mjs
   ```

   - 실제 모델을 호출하므로 비용이 들고, 각각 수 분이 걸린다.
5. **판정과 기록**
   - stdout report의 `passed`, `actualLlmHostsExecuted`, `ownedProcessesStopped`가 모두 true면 통과다.
   - 실패하면 report에 남은 첫 assertion 메시지와 run 폴더의 JSONL을 근거로 기록한다.
   - [MVP 통합 문서](mvp-integration.md)의 실행 기록 양식으로 Notion 작성일 문서에 남긴다.
     - 재실행자
     - client·중앙 커밋, Host 버전
     - taskId, 감시 표본·위반 수
   - 자격 파일과 토큰은 붙여 넣지 않는다.
6. **정리:** 중앙 서버 프로세스와 PostgreSQL(`pg_ctl stop`)을 중지한다.

## 2026-11-06 AC7 독립 재실행 (Codex 검토)

사용자 요청에 따라 작성자 Claude Code와 분리된 Codex 리뷰 세션에서 위 절차를 직접 실행했다. 작성자의 기존 report 재판정이 아니라, 새 클러스터·계정·DB·중앙 서버·사용자·팀·Agent·fixture를 만든 독립 재실행이다.

- **실행 조합:** client와 판정기 `e126e939d7bb9d3715aa4bfc236b26a53ee98d88`, 중앙 `34b2c15c116041e0bb70fd91a2f9ef74b28c79b9`.
  - client를 직접 빌드하고 CLI·MCP dist·의존성을 검증 전용 폴더에 동결했다. 두 실행에서 같은 CLI·MCP dist 해시를 사용했다.
  - 중앙은 해당 커밋을 별도 clone해 Java 21.0.11로 bootJar를 빌드했다. PostgreSQL 17.11, 새 전용 계정·DB, loopback 임의 포트를 사용했다.
  - Windows 11, Node 24.18.0, Codex CLI 0.166.0, Claude Code 2.1.310. Claude는 `claude-opus-5-5`/high, Codex는 CLI 기본 모델(모델 ID 미보고)이다.
- **실행 시간(KST):** 실패 실행 15:25:53–15:27:07, 성공 실행 15:27:08–15:29:11, 중앙·DB 정리 완료 15:31:01.
- **회귀 검사:** `npm ci`와 `npm test`를 실행해 180 통과·0 실패(판정기 16건 포함)를 확인했다.
- **판정:** 두 실행기 모두 exit 0이며 `passed`, `actualLlmHostsExecuted`, `ownedProcessesStopped`가 모두 true다. AC7을 PASS로 기록한다.

| 실행 | taskId | 실제 관측 |
| --- | --- | --- |
| AC2 Codex → Claude | 1 | 명시적 수락 → invoice 부분 수정 → Host 테스트 pass 1/fail 1 → FAILED, `partiallyModified=true`, 사유 457자 → Codex가 같은 실패 결과 조회. 부모 테스트 exit 1 |
| AC1·AC3 Codex → Claude | 2 | invoice 수정·Host 테스트·부모 독립 테스트 통과 → COMPLETED → Codex 결과 조회. 실제 diff와 중앙 저장 결과 일치 |
| AC1·AC3 Claude → Codex | 3 | slug 수정·Host 테스트·부모 독립 테스트 통과 → COMPLETED → Claude 결과 조회. 실제 diff와 중앙 저장 결과 일치 |

세 경우 모두 고정 테스트와 Git index가 보존됐다. 성공 경로는 지정 소스만 수정됐고, 실패 경로의 남은 diff는 `receiver-remaining.diff`에 보관했다.

| taskId | 감시 표본 | 작업이 있는 PENDING 표본 | 변경 위반 | 조회 오류 | 수락 전 읽기 전용 명령 |
| --- | --- | --- | --- | --- | --- |
| 1 | 250 | 68 | 0 | 0 | 2 |
| 2 | 427 | 24 | 0 | 0 | 3 |
| 3 | 428 | 43 | 0 | 0 | 3 |

실행기 report에 없는 requestId·requestKey는 MCP transcript의 create_task 응답에서 따로 추출했다. requestKey는 전용 DB의 tasks 테이블과도 대조했다.

| taskId | requestId | requestKey |
| --- | --- | --- |
| 1 | `946ba00e-de9b-40e3-9485-aa7be1cdb6ff` | `6533a3bd-bcf6-44fd-95f9-1cdbbf3e3b0e` |
| 2 | `59add388-6b7a-4883-adb5-88fc188f84e8` | `36535921-5fc0-4798-b300-7640a8132888` |
| 3 | `e257d010-dee0-4c97-86bc-feeb5cb09959` | `mvp66-out-baaf748d-43e1-4f80-b3eb-c8f09f1e67ed` |

### AC7 증거와 정리

- 실패 run: `run-e25fdc34-63f1-4d00-82d6-46e018f64007`, report SHA256 `f2d17bdb5cb2e1272b4f2bf5b792a5ac42182d235741a8749387d1ec2d005db8`.
- 성공 run: `run-4fd5b0cc-4b06-44bb-b988-33032a3aebc5`, report SHA256 `36f078d806fad192da4decd08ef38f45e82506dcac5bbfc01c70b45fd0b2c261`.
- 원본 report·JSONL·프롬프트·stderr·diff·부모 테스트 출력·task-index.csv·cleanup.json은 검증 PC 저장소의 ignored `.planning/ac7-codex-20261106-62cca5cf/`에 보관했다. 세션 임시 폴더에만 두지 않았다. 공유 요약과 빌드 해시는 [Notion 작업 정리의 AC7 독립 재실행 절](https://app.notion.com/p/41035a4f3ca6810d9b20e51a98960504)에 기록했다.
- 실행기가 소유한 Host·MCP·브로커 정리 외에, 직접 띄운 중앙 JVM과 PostgreSQL도 중지했다. 두 서비스 포트가 닫혔고 검증 폴더를 참조하는 소유 런타임 프로세스는 0개다.
- AC2의 유도된 실패, AC3 표본 간격 등 위 한계는 유지된다. AC4·AC5·AC6의 예외 시나리오는 이번에 재실행하지 않았으며 기존 중앙 시험 근거와 구분한다.
