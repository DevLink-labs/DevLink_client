# 간편설정 호출·단계·오류 규격 v1

작성일: 2026-11-02 (Asia/Seoul). 기준 client: `9f6b85f`.

작업 01의 산출물이다. 사용자 위임에 따라 아래 내용을 간편설정 구현의 기준으로 결정한다. 담당자별 연동 경계는 이 문서에서 배분하며, 별도 합의 대기를 구현의 선행 조건으로 두지 않는다. 담당자와 실제 연락하거나 API 상호 검증을 마쳤다는 의미는 아니다.

## 1. 범위와 기존 구현

간편설정은 로그인·팀 선택·프로젝트/도구 지정·Agent 준비·브로커 기동·MCP 설정을 순서대로 연결한다. 사용자 회원가입, 팀 생성/초대 참여, Agent 삭제, 작업 생성/수락, 부팅 자동 실행은 포함하지 않는다. 팀이 없으면 기존 `team create` 또는 `team join`을 안내한다.

| 구분 | 현재 제공 | 이번에 정의한 호출 |
| --- | --- | --- |
| 사용자/팀 | `auth login/status`, `team list/select/current` | 대화형 선택, 비대화형 입력 검증 |
| Agent | `agent create/list/use/token/status/reset-pending` | 기존 bootstrap을 호출하는 등록·복구 모듈 |
| 브로커 | `start/status/stop`, 인증된 `GET /health` | 기동 대기와 연결 상태 집계 |
| MCP | `mcp path/rotate`, Host 수동 설정 | 설정 계획·적용·검사 모듈 |
| 통합 실행 | 아직 없음 | `setup`, `setup status`, JSON/JSONL 응답과 종료 코드 |

아래 `setup` 명령과 모듈 DTO는 **구현할 규격**이다. 기존 CLI가 이미 지원하는 명령/종료 코드로 사용하지 않는다. 기존 `init`의 설정 파일 생성 의미도 변경하지 않는다.

근거: [CLI 명령](../../../rust/crates/devlink-cli/src/cli.rs), [인증](../../../rust/docs/user-auth.md), [팀](../../../rust/docs/team-setup.md), [Agent bootstrap](../../../rust/docs/agent-bootstrap.md), [MCP·health](../../../rust/docs/mcp-interface.md), [READY 신원](../../../rust/docs/ready-identity.md). 과거 Host 검증 문서의 미구현 설명보다 현재 provider 코드·기능 문서를 우선한다.

## 2. 호출 방법

```text
devlink [--config <FILE>] setup
devlink [--config <FILE>] setup --non-interactive --input <FILE> [--password-stdin] --output json
devlink [--config <FILE>] setup --non-interactive --input <FILE> [--password-stdin] --output jsonl
devlink [--config <FILE>] setup status --output json
```

- `--config`는 기존 프로필 선택자다. 생략하면 기존 기본 경로를 사용한다. 입력 파일에는 별도 config 경로를 받지 않는다.
- `setup` 기본은 대화형·`--output human`이다. 입력과 숨김 비밀번호 입력에 TTY가 필요하다. TTY가 없으면 자동 대기하지 않고 입력 필요 오류로 종료한다.
- `--non-interactive`는 `--input <FILE>`이 필수이며 질문·선택·숨김 입력을 열지 않는다. 누락 항목을 모두 모아 변경 전에 반환한다. 이름으로 사용자/팀/Agent를 자동 선택하지 않는다.
- `--input -`는 지원하지 않는다. stdin은 `--password-stdin`의 비밀번호 한 줄 전용이다. 비밀번호와 초대코드를 순서대로 읽는 다중 비밀 입력은 제공하지 않는다.
- `--output`은 `human | json | jsonl`이다. JSON은 stdout에 최종 결과 한 개, JSONL은 stdout에 진행 이벤트와 최종 결과를 줄마다 한 개씩 출력한다. 안내/프롬프트는 stderr, 비밀은 어느 채널에도 출력하지 않는다. machine 모드에는 ANSI·배너·하위 CLI stdout을 섞지 않는다.
- `setup status`는 읽기 전용 조회이며 로그인, 토큰 발급, 시작/종료, Host 실행, MCP 세션 claim/bind를 수행하지 않는다. 조회 성공은 종료 0이고 준비 여부는 응답에서 판단한다. 조회 자체의 실패에는 아래 오류 종료 코드를 사용한다.
- status 응답도 result 형식이다. 조회용 새 runId와 sequence=1, status=succeeded, exitCode=0, stages=[] 및 data.connection을 반환한다. 사용자/팀/Agent ID는 현재 검증할 수 있는 경우에만 data에 추가한다. 상태 조회의 succeeded는 setupReady=true를 뜻하지 않는다.
- 도움말/버전은 기존 CLI 동작을 따른다. `setup` 파서는 잘못된 호출도 안전한 코드 2로 처리하며, 민감한 잘못된 인자를 그대로 echo하지 않는다.

### 공통 입력

입력 JSON은 UTF-8, 최대 64 KiB, `schemaVersion: 1`이다. 알 수 없는 필드·중복 JSON 키·명시하지 않은 null은 거부한다. 부분 우선순위 병합은 하지 않는다. 대화형에서는 명시한 입력 → 같은 origin/사용자에 묶인 기존 값의 확인 → 질문 순서로 채운다. 비대화형에서는 아래에 명시한 기본값만 보충한다.

| 필드 | 형식·필수 조건 | 결정 규칙 |
| --- | --- | --- |
| `schemaVersion` | 정수 `1`, 필수 | 다른 버전은 변경 전에 거부 |
| `serverUrl` | 선택, 기존 ws/wss URL 규칙 | 기존 설정 또는 기존 제품 기본값. 운영은 wss, ws는 loopback만 허용. 사용자정보·query·fragment 금지. REST는 같은 origin의 https/http로 기존 인증 모듈이 변환 |
| `user.mode` | `session | login`, 필수 | session은 기존 사용자 세션을 중앙 `/users/me`로 재검증. login은 새 로그인 |
| `user.email` | login일 때 필수 문자열 | 로그인 계정. session 모드에서는 금지. `userId`는 받지 않음 |
| `team.id` | 필수 ID | 현재 사용자의 중앙 멤버십 검증 후 선택. 기존 팀을 쓰려면 그 ID를 명시 |
| `project.path` | 필수 절대 경로 | 기존 디렉터리를 canonicalize. 존재하지 않는 경로를 자동 생성하지 않음. 중앙 전송 금지 |
| `project.name` | create일 때 필수, use일 때 금지 | 중앙 프로젝트 표시명. 로컬 경로와 구분 |
| `tool.type` | `codex | claude-code`, 필수 | 설치 도구를 검사하되 탐지 결과만으로 임의 선택하지 않음 |
| `agent.action` | `create | use`, 필수 | create는 생성 1회와 최초 토큰 발급까지 명시적으로 요청. use는 중앙의 본인 Agent 선택 |
| `agent.id` | use일 때 필수, create일 때 금지 | 이름 일치로 ID를 추정하지 않음 |
| `agent.name` | create일 때 필수, use일 때 금지 | 중앙 표시명 |
| `agent.roleDescription` | create일 때 선택 문자열 | 생략은 없음. use는 중앙 메타데이터를 변경하지 않음 |
| `agent.issueToken` | use일 때 선택 boolean, 기본 false | 토큰이 없거나 무효이면 true일 때만 발급/회전. create에서는 금지. true인 호출을 네트워크 오류 후 자동 반복하지 않음 |
| `mcp.mode` | `apply | manual`, 필수 | apply는 지정한 Host 설정 대상만 반영. manual은 설정 안내만 반환하고 종료 3 |
| `mcp.targetPath` | apply일 때 필수 절대 경로 | 선택 도구와 일치하는 프로젝트 범위 설정 파일. 전역 파일·프로젝트 밖 경로는 거부. manual에서는 금지 |
| `timeoutMs` | 선택 정수, 기본 60000, 범위 30000–300000 | 비대화형 실행 전체 예산. 대화형의 사용자 입력 대기는 제외하며 네트워크/하위 단계 예산은 포함 |

ID는 `1..9223372036854775807` 범위의 정규 십진 **문자열**이다. 선행 0·부호·공백은 금지하며 JS Number로 왕복하지 않는다. 중앙 숫자 ID와의 변환은 Rust가 검증해 수행한다. Agent 이름은 UTF-16 code unit 100 이하, 프로젝트 표시명 120 이하, 역할 설명 255 이하로 기존 bootstrap과 맞춘다. 이름/표시명은 공백만으로 구성할 수 없다.

`user.mode=login`이면 대화형 숨김 입력 또는 `--password-stdin`이 필요하다. 비밀번호는 기존 제약인 UTF-8 1–72 bytes를 적용하고 LF/CRLF 줄 끝만 제거한다. JSON·CLI 인자·환경변수에 비밀번호를 받지 않는다. session 모드에서 `--password-stdin`은 입력 충돌이다. 초대 참여는 별도 `team join`의 숨김 입력/`--code-stdin`을 사용한다.

같은 팀의 다른 사용자가 소유한 Agent는 선택할 수 없다. use의 tool은 중앙 Agent 도구와 일치해야 하고 표시명/역할/프로젝트명은 중앙 응답을 유지한다. 기존 사용자·서버·팀·프로젝트/Agent 바인딩 변경에 브로커 중지가 필요하면 명시적 `stop`을 안내한다. setup이 실행 중인 브로커를 자동 중지하거나 다른 프로필로 우회하지 않는다.

### 비대화형 입력 예시

기존 로그인과 Agent를 재사용한다. 아래 경로·ID는 합성 예시이며 파일에 비밀이 없다.

```json
{
  "schemaVersion": 1,
  "serverUrl": "wss://central.example.test",
  "user": { "mode": "session" },
  "team": { "id": "12" },
  "project": { "path": "C:\\work\\DevLink" },
  "tool": { "type": "codex" },
  "agent": { "action": "use", "id": "34", "issueToken": false },
  "mcp": { "mode": "apply", "targetPath": "C:\\work\\DevLink\\.codex\\config.toml" },
  "timeoutMs": 60000
}
```

신규 Agent 생성 시 project에 `name`을 추가하고 agent를 아래처럼 교체한다. 로그인도 필요하면 user를 `{"mode":"login","email":"user@example.test"}`로 교체하고 비밀 저장소에서 stdin으로 한 줄을 전달한다. 실제 비밀번호를 명령 문자열에 넣는 예시는 제공하지 않는다.

```json
{
  "action": "create",
  "name": "DevLink 작업 Agent",
  "roleDescription": "프로젝트 작업과 검증"
}
```

## 3. 단계와 상태

모든 실행은 순서가 고정된 다음 단계를 사용한다. `validate`는 **모든 입력·로컬 경로·지원 도구·Host 설정 충돌의 사전 검사**를 포함한다. 비대화형 필수 입력 누락 또는 로컬에서 이미 알 수 있는 충돌 때문에 중앙 Agent를 먼저 생성하면 안 된다. 중앙 권한·최종 파일 버전은 실제 변경 직전 다시 확인한다.

| 순서 / step | 처리 | succeeded 조건 |
| --- | --- | --- |
| 1 `validate` | 입력·경로·권한·기존 바인딩·설정 계획 검사 | 입력이 완결되고 변경 범위/충돌 확인 |
| 2 `authenticate` | 세션 재사용 또는 명시 로그인 | 중앙에서 현재 사용자 확인 |
| 3 `select_team` | 목록·멤버십 검증 후 선택 | 같은 origin/사용자에 묶인 팀 선택 저장 |
| 4 `bind_project` | 로컬 경로/표시명 검증 | canonical 디렉터리 확정. Agent 프로필 저장은 register_agent에서 수행 |
| 5 `select_tool` | 도구·MCP 실행 파일/런타임 검사 | 선택 도구 및 MCP 실행 경로 사용 가능 |
| 6 `register_agent` | 기존 Agent 재사용 또는 단일 생성·토큰 검증 | 중앙 ID/owner/team/tool 일치 및 ready 프로필 내구 저장 |
| 7 `start_broker` | 기존 일치 프로세스 재사용 또는 기동 | OS lock과 검증된 health로 로컬 기동 확인 |
| 8 `configure_mcp` | 계획 재검증·백업·적용 또는 수동 안내 | apply 후 해당 DevLink 항목 검사 성공 |
| 9 `verify_connection` | local health·중앙 WS·현재 신원·자격·MCP 파일 검사 | 아래 setupReady 조건 모두 충족 |

단계 상태는 `pending | running | waiting_input | succeeded | skipped | failed | cancelled`다. `skipped`는 선행 단계 실패/취소로 미실행한 단계에만 사용한다. 기존 값을 검증해 재사용한 단계는 `succeeded`와 `reused:true`다.

```text
pending → running → succeeded | failed | cancelled
running → waiting_input → running           (대화형만)
pending → skipped                          (선행 실패/취소)
```

run 상태는 `running | waiting_input | succeeded | action_required | failed | cancelled`다. 입력 대기 중 Ctrl+C는 cancelled다. 대화형 선택 거절/입력 EOF도 cancelled로 종료한다. 실행 중 오류는 현재 단계를 failed, 이후 단계를 skipped로 기록한다. manual 모드는 configure_mcp를 waiting_input으로 남기지 않고 failed(SETUP_MANUAL_ACTION_REQUIRED)로 마무리하며 run은 action_required다.

`outcome`은 **해당 단계의 변경 결과**로 `not_applied | applied | unknown`이다. failed와 unknown은 함께 가능하다. 예를 들어 생성 POST의 응답 유실은 `failed/unknown`이며 Agent 미생성을 뜻하지 않는다. 이전 단계의 applied 상태는 결과의 stages에 보존한다.

재실행은 새 runId를 발급하고 현재 중앙/로컬 상태를 다시 검사한다. 이전 succeeded 표시만으로 인가를 생략하지 않는다. 기존 bootstrap의 `create_pending → needs_token → ready`를 권위 상태로 사용하며 별도 setup 체크포인트로 덮어쓰지 않는다. runId는 추적용이며 중앙 멱등키가 아니다.

## 4. 출력 계약과 예시

| 필드 | 의미 |
| --- | --- |
| `schemaVersion` | 출력 계약 버전 1 |
| `type` | JSONL의 `progress` 또는 마지막 `result`. json 출력에는 result 한 개 |
| `runId` / `sequence` | 실행 UUID / 1부터 증가하는 이벤트 순서. result도 같은 순서 사용 |
| `status` | run 상태 |
| `step` / `stepStatus` / `outcome` | progress의 단계·상태·변경 결과 |
| `stages` | result의 모든 단계별 step/status/outcome/reused. 미실행도 포함 |
| `data` | 성공 result 전용. 공개 식별자·프로젝트 경로·설정 위치·연결 상태 |
| `error` | 실패/action_required/cancelled result 전용. 아래 공통 오류 |
| `exitCode` | result에만 포함하는 실제 프로세스 종료 코드 |

result는 data/error 중 정확히 하나를 갖는다. progress에는 data/error를 넣지 않고 필요한 입력은 `requiredInputs`에 기록한다. 문자열은 제어/bidi 문자를 이스케이프하며, 오류 문구는 허용 코드별 고정 문구다. 공개 사용자 DTO도 allowlist로 재구성하고 session/profile/runtime 원문을 직렬화하지 않는다.

진행 이벤트:

```json
{
  "schemaVersion": 1,
  "type": "progress",
  "runId": "9d978406-2adb-4a98-9ddc-5f6386a70618",
  "sequence": 12,
  "status": "running",
  "step": "register_agent",
  "stepStatus": "running",
  "outcome": "not_applied"
}
```

성공 결과(앞선 progress와 같은 run):

```json
{
  "schemaVersion": 1,
  "type": "result",
  "runId": "9d978406-2adb-4a98-9ddc-5f6386a70618",
  "sequence": 19,
  "status": "succeeded",
  "exitCode": 0,
  "stages": [
    { "step": "validate", "status": "succeeded", "outcome": "not_applied", "reused": false },
    { "step": "authenticate", "status": "succeeded", "outcome": "not_applied", "reused": true },
    { "step": "select_team", "status": "succeeded", "outcome": "not_applied", "reused": true },
    { "step": "bind_project", "status": "succeeded", "outcome": "not_applied", "reused": true },
    { "step": "select_tool", "status": "succeeded", "outcome": "not_applied", "reused": true },
    { "step": "register_agent", "status": "succeeded", "outcome": "not_applied", "reused": true },
    { "step": "start_broker", "status": "succeeded", "outcome": "applied", "reused": false },
    { "step": "configure_mcp", "status": "succeeded", "outcome": "applied", "reused": false },
    { "step": "verify_connection", "status": "succeeded", "outcome": "not_applied", "reused": false }
  ],
  "data": {
    "userId": "3",
    "teamId": "12",
    "agentId": "34",
    "toolType": "codex",
    "projectPath": "C:\\work\\DevLink",
    "mcpConfigPath": "C:\\work\\DevLink\\.codex\\config.toml",
    "brokerFile": "C:\\work\\profile\\.devlink-runtime\\broker.json",
    "connection": {
      "observedAt": "2026-11-02T10:00:00+09:00",
      "broker": "running",
      "central": "Connected",
      "identity": "verified",
      "credentials": "valid",
      "mcpConfig": "applied",
      "hostSession": "not_checked",
      "setupReady": true,
      "collaborationReady": null
    },
    "nextActions": ["open_or_restart_host"]
  }
}
```

등록 결과가 불명확할 때의 오류 결과:

```json
{
  "schemaVersion": 1,
  "type": "result",
  "runId": "f91f92b4-578e-461d-82e8-fd3d87ca760b",
  "sequence": 13,
  "status": "action_required",
  "exitCode": 3,
  "stages": [
    { "step": "validate", "status": "succeeded", "outcome": "not_applied", "reused": false },
    { "step": "authenticate", "status": "succeeded", "outcome": "not_applied", "reused": true },
    { "step": "select_team", "status": "succeeded", "outcome": "not_applied", "reused": true },
    { "step": "bind_project", "status": "succeeded", "outcome": "not_applied", "reused": false },
    { "step": "select_tool", "status": "succeeded", "outcome": "not_applied", "reused": false },
    { "step": "register_agent", "status": "failed", "outcome": "unknown", "reused": false },
    { "step": "start_broker", "status": "skipped", "outcome": "not_applied", "reused": false },
    { "step": "configure_mcp", "status": "skipped", "outcome": "not_applied", "reused": false },
    { "step": "verify_connection", "status": "skipped", "outcome": "not_applied", "reused": false }
  ],
  "error": {
    "code": "SETUP_REGISTRATION_UNKNOWN",
    "step": "register_agent",
    "message": "Agent 등록 결과를 확인할 수 없습니다. 기존 Agent 목록을 확인하세요.",
    "outcome": "unknown",
    "retryable": false,
    "requiredInputs": [
      { "field": "agent.id", "kind": "selection", "secret": false, "source": "owned_agent_list" }
    ],
    "recovery": { "action": "inspect_owned_agents_then_use", "automatic": false },
    "known": { "userId": "3", "teamId": "12" }
  }
}
```

## 5. 오류·추가 입력·복구·종료 코드

오류는 `code/step/message/outcome/retryable/requiredInputs/recovery`가 필수다. 추가 입력이 없으면 빈 배열을 사용한다. `known`은 확인한 userId/teamId/agentId만 포함하며, 알 수 없는 ID는 만들지 않는다. `requiredInputs[]`는 `field/kind/secret/source`이며 kind는 `value | selection | secret`, source는 `input | hidden_prompt_or_stdin | verified_team_list | owned_agent_list` 중 하나다. 비밀은 필드 설명만 반환한다.

MANUAL_ACTION_REQUIRED에서만 error.manualConfig에 `toolType`, `suggestedTargetPath`, `executable`, `brokerFile`, `snippet`, `restartRequired`를 추가할 수 있다. 검증된 DevLink 항목만 포함하며 일반 Host 설정 전체를 복사하지 않는다. `requiredInputs`는 비워두고 적용 후 setup 재실행을 안내한다.

`recovery.action`은 아래 표의 고정 식별자, `automatic`은 boolean이다. `retryable:true`는 **수정/복구 후 명시적으로 재호출할 수 있음**이며 자동 재전송 허용을 뜻하지 않는다. 이번 setup의 모든 recovery는 `automatic:false`다. 읽기 전용 상태 폴링과 기존 브로커 WS 재접속은 별도이며 전체 시간 예산 안에서만 대기한다.

| code | 조건 / 필요한 추가 입력 | recovery.action | retryable / 종료 |
| --- | --- | --- | --- |
| `SETUP_INPUT_REQUIRED` | 필수 누락·TTY 없음. 누락 필드별 descriptor | `provide_inputs` | true / 2 |
| `SETUP_INPUT_INVALID` | 스키마·버전·URL·ID·필드 조합 오류. 잘못된 field | `correct_inputs` | true / 2 |
| `SETUP_AUTH_REQUIRED` | 세션 없음/만료. user.mode/email/password | `login` | true / 4 |
| `SETUP_AUTH_REJECTED` | 로그인 실패. password(secret) | `login` | true / 4 |
| `SETUP_TEAM_FORBIDDEN` | 현재 팀 멤버가 아님. team.id | `select_verified_team` | true / 4 |
| `SETUP_AGENT_FORBIDDEN` | 다른 사용자/팀 소유. agent.id | `select_owned_agent` | true / 4 |
| `SETUP_PROJECT_INVALID` | 경로 없음·디렉터리 아님. project.path | `choose_existing_project` | true / 2 |
| `SETUP_TOOL_UNAVAILABLE` | 지원하지 않거나 실행 도구/MCP 런타임 없음. tool.type | `install_or_select_tool` | true / 5 |
| `SETUP_PROFILE_CONFLICT` | origin/사용자/팀/도구/프로젝트 바인딩 충돌 | `inspect_profile_then_rebind` | false / 6 |
| `SETUP_REGISTRATION_UNKNOWN` | 생성 응답 유실·5xx·기존 create_pending. 확인 후 agent.id | `inspect_owned_agents_then_use` | false / 3 |
| `SETUP_TOKEN_REQUIRED` | ID는 알려졌지만 토큰 없거나 발급/검증 결과 불명. agent.issueToken | `explicit_issue_token` | false / 3 |
| `SETUP_BROKER_BUSY` | 브로커 실행 중 변경·다른 owner lock | `stop_owned_broker_then_retry` | true / 6 |
| `SETUP_BROKER_START_FAILED` | 포트·기동·health 확인 실패 | `inspect_local_broker` | true / 7 |
| `SETUP_CENTRAL_UNAVAILABLE` | 중앙 단절·조회 실패 | `check_network_then_retry` | true / 7 |
| `SETUP_CENTRAL_AUTH_FAILED` | WS AuthFailed 또는 현재 신원 인증 거부 | `stop_then_reauthenticate_agent` | false / 4 |
| `SETUP_CONNECTION_REPLACED` | WS Replaced 또는 확인한 다른 소유 세션 | `inspect_connection_owner` | false / 6 |
| `SETUP_PROTOCOL_ERROR` | 잘못된 provider/READY 응답·신원 모순 | `check_provider_contract` | false / 7 |
| `SETUP_INBOX_FAILED` | WS InboxFailed, 로컬 저장 실패 | `repair_inbox_then_restart` | false / 5 |
| `SETUP_MCP_CONFIG_CONFLICT` | DevLink 키 충돌·변경 전 digest 불일치 | `review_mcp_diff` | false / 6 |
| `SETUP_MCP_CONFIG_WRITE_FAILED` | Host 설정 반영/검사/복원 실패 | `inspect_mcp_backup` | false / 5 |
| `SETUP_MANUAL_ACTION_REQUIRED` | manual 설정 안내만 생성 | `apply_manual_mcp_config` | true / 3 |
| `SETUP_UNSAFE_FILE` | 자격/설정 경로 링크·소유권/권한 부적합·손상 | `repair_private_file` | false / 5 |
| `SETUP_TIMEOUT` | 전체 예산 만료. 진행 중 변경 결과는 별도 보존 | `inspect_status_then_retry` | false / 7 |
| `SETUP_CANCELLED` | Ctrl+C·대화형 취소/EOF | `inspect_status_before_resume` | false / 130 |
| `SETUP_INTERNAL_ERROR` | 분류되지 않은 내부 실패 | `inspect_safe_diagnostics` | false / 1 |

종료 0은 setupReady 충족, 1은 내부 실패, 2는 입력/사용법, 3은 수동 복구·추가 동작, 4는 인증/권한, 5는 로컬 환경/파일, 6은 충돌/점유, 7은 연결/제한시간, 130은 사용자 취소다. 파이프 단절·강제 종료처럼 최종 JSON을 전달할 수 없는 상황은 예외다. 부분 적용 내역은 재조회로 확인한다.

네트워크 오류를 먼저 만났더라도 생성 결과가 불명이면 REGISTRATION_UNKNOWN을, ID가 알려진 토큰 발급 결과 불명이면 TOKEN_REQUIRED를 우선한다. timeout/cancelled를 보고할 때도 해당 단계 outcome=unknown과 안전한 known ID를 보존하고 생성/발급을 자동 반복하지 않는다. 401 사용자 세션, 중앙 Agent 인증 거부, 로컬 localToken 거부를 서로 바꿔 해석하지 않는다.

누락 입력 결과는 validate 단계에서 반환한다. 예를 들어 `requiredInputs`에 `team.id/selection/false/verified_team_list`, `project.path/value/false/input`을 함께 담는다. 비대화형에서는 질문으로 이어가지 않고 종료 2다. 인증을 해야 목록을 얻을 수 있으면 descriptor만 반환하고 권한 없는 목록을 노출하지 않는다.

## 6. 모듈별 연동 경계

아래 이름은 **프로세스 내부 호출 규격**이며 새 중앙/브로커 HTTP endpoint가 아니다. 오류는 위 공통 SetupError로 정규화한다. 세션/자격 핸들은 내부 전용이며 JSON 출력 DTO와 분리한다.

### 6.1 등록·복구: 박성수 연동 창구

```text
Registration.inspect(context) -> PublicRegistrationState
Registration.ensure(context, selection) -> PublicRegistrationState
```

- context: configPath, 검증한 origin/user/team에 대한 내부 인증 핸들, canonical projectPath, toolType, deadline/cancellation.
- selection: create(name, projectName, roleDescription?) 또는 use(agentId, issueToken=false). setup runId를 중앙 멱등키로 전송하지 않는다.
- 응답: `state: vacant | creation_unknown | token_required | ready`, 공개 userId/teamId/알려진 agentId, `created/reused`, outcome. token 값은 반환 DTO에 포함하지 않고 기존 private profile에서만 관리한다.
- 중앙 근거 경로는 기존 사용자 세션 기반 `POST /api/v1/agents/teams/{teamId}`, `GET /api/v1/agents/teams/{teamId}`, `GET /api/v1/agents/{id}`, `POST /api/v1/agents/{id}/token`, Agent Bearer `GET /api/v1/agents/me`다. 실제 호출은 기존 Rust bootstrap이 맡으며 MCP 설정 모듈이 중앙으로 직접 호출하지 않는다.
- create 전 create_pending 내구 저장, 응답의 ID 확인 즉시 needs_token 저장, 발급·실신원 검증·저장 성공 시 ready다. 응답 유실/중단 시 생성 자동 재요청은 없다.
- inspect는 조회만 한다. creation_unknown은 본인 목록 확인 후 use로 복구한다. 이름·메타데이터만으로 동일 생성임을 단정하지 않는다. ID가 알려지면 같은 ID의 명시 발급으로 복구한다.
- `reset-pending --accept-duplicate-risk`는 기존 고급 수동 복구 명령으로만 유지한다. setup 자동 복구나 기본 requiredInputs에 중복 위험 동의를 넣지 않는다.
- 중앙에 없는 등록 멱등성/키 조회 API를 가정하지 않는다. 후속 provider가 지원하면 별도 버전에서 수용 검증 후 추가한다.

### 6.2 MCP 설정: 임효민 연동 창구

2026-11-02 구현: [Rust McpConfig 공급부·지원 버전·파일/오류 계약](mcp-config-provider.md).
모듈은 임효민(-143), CLI 호출·통합은 최준혁(-111) 경계를 유지한다.

2026-11-04 CLI 통합: [작업 10 사용법·실행 경로·확인·복구](../../../rust/docs/setup.md#작업-10-프로젝트-mcp-설정-통합).
`--apply-mcp`는 이번 실행에서 확인한 계획 적용을 허용한다. 수동 모드/미검증 Host는 예시만 반환한다.
적용 후 `verify_connection`은 12번의 연결 집계로 설정 준비 여부를 판단한다.
오류의 `mcpConfig`에 앞 단계의 적용 결과를, `mcpError`에는 제공자 outcome/rollback/백업 근거를 보존한다.
사전 검사 충돌 시 `mcpError.conflict`에 대상 경로(`targetPath`)와 토큰 값이 없는 제안 항목(`proposed`)을 반환한다.

```text
McpConfig.plan({toolType, projectPath, targetPath, brokerFile, executable}) -> ConfigPlan
McpConfig.apply({planId, expectedDigest}) -> ConfigApplyResult
McpConfig.inspect({toolType, targetPath, brokerFile, executable}) -> ConfigInspection
```

- 입력 executable은 선택 도구용 adapter가 검증한 MCP 프로세스의 절대 실행 경로와 argv다. Node script는 script 절대 경로도 검증한다. 다운로드/설치나 임의 shell 문자열 실행은 하지 않는다.
- Host에 전달하는 자격은 `DEVLINK_BROKER_FILE` 절대 경로 하나뿐이다. 중앙 token/cookie/controlToken/localToken 값을 Host 설정에 복사하지 않는다. 실행 인수에는 자격을 넣지 않는다.
- plan 응답: planId, toolType, targetPath, expectedDigest(미존재면 null), `change: create | merge | unchanged | conflict`, 안전한 DevLink 항목 diff, `restartRequired`. plan은 파일을 쓰지 않는다. diff에 다른 MCP 항목/환경변수의 비밀을 포함하지 않는다.
- apply는 동일 경로/예상 digest를 다시 확인하고 DevLink 항목만 병합한다. 다른 MCP 서버 항목은 보존한다. 기존 DevLink 항목이 다른 실행 경로/자격 파일을 가리키면 conflict이며 묵시적으로 덮어쓰지 않는다. global 설정은 이번 범위에서 제외한다.
- 기존 파일이 있으면 같은 사용자 전용 위치에 비공개 백업을 내구 저장한 후 원자 교체한다. 원본/백업·부모의 링크/권한을 검사한다. 성공 후 해당 항목을 다시 파싱해 확인한다. 검사 실패 시 본인이 쓴 버전인 경우만 백업 복원하며, 동시 변경이 발견되면 덮어쓰지 않고 오류를 반환한다. 백업 내용은 출력하지 않는다.
- apply 결과: `status: applied | unchanged`, targetPath, 선택 backupPath, restartRequired. 백업 저장소 용량 한도 오류는 선택 recoveryDirectory로 수동 점검 위치를 제공한다. 실패도 `outcome`과 `rollback: restored | not_needed | failed | conflict`를 보존한다. inspect 결과는 `applied | missing | conflict | unknown`과 targetPath다.
- 도구별 파일 문법/프로젝트 설정 지원·실행 경로는 adapter 구현에서 고정 지원 버전에 대조한다. 지원하지 않는 Host 버전은 명시적으로 manual 또는 TOOL_UNAVAILABLE이며, 임의 전역 경로에 대신 쓰지 않는다.
- manual은 안전한 DevLink 설정 조각과 적용 위치/재시작 안내만 반환한다. 적용 완료로 표시하지 않는다. 적용 후 새 setup 실행에서 inspect가 일치하면 unchanged로 통과한다.

### 6.3 연결 상태: 최보경 연동 창구

```text
Connection.inspect({configPath, targetPath, toolType, deadline}) -> ConnectionSnapshot
```

setup과 `setup status`가 같은 집계 규칙을 사용한다. CLI는 OS 잠금과 인증된 `GET /health`로 브로커를 확인하고, 동일 인스턴스의 제어 토큰으로 `GET /v1/setup-connection`에서 현재 중앙 신원·자격·Host 세션을 조회한다. 프로젝트 설정은 `McpConfig.inspect`로 비교한다. 조회는 fresh/recover 세션을 만들거나 기존 Host 소유권을 교체하지 않는다.

| 필드 | 값 / 근거 |
| --- | --- |
| `observedAt` | ISO 8601 시각. 이 시점의 스냅샷이며 지속 준비 보장 아님 |
| `broker` | `running | stopped | unhealthy | unknown`. OS lock + service/version/PID/instance 일치 health. PID만으로 판정/강제 종료 금지 |
| `central` | 기존 `Connecting/Authenticating/Connected/Reconnecting/AuthFailed/Replaced/ProtocolViolation/InboxFailed/Disconnected` 또는 관측 불가 `unknown` |
| `identity` | `verified | unauthenticated | mismatch | unknown`. 현재 WS generation에 묶인 중앙 신원과 선택 user/team/agent 일치 |
| `credentials` | `valid | missing | invalid | unknown`. MCP 파일 schema/권한 및 현재 브로커 대상 검증. 값은 비공개 |
| `mcpConfig` | `applied | missing | conflict | unknown`. 현재 대상의 DevLink 항목 검사 |
| `hostSession` | `bound | unbound | conflict | not_checked | unknown`. 읽기 전용 provider로 현재 같은 Agent 소유 세션을 확인한 경우만 bound |
| `setupReady` | boolean. running + Connected + verified + valid + applied 모두 참일 때만 true |
| `collaborationReady` | boolean 또는 null. setupReady=false이면 false, true일 때 Host bound면 true, unbound/conflict면 false, not_checked/unknown이면 null |

`Connected`·`devlink_ping`·tools/list·설정 파일 존재만으로 setupReady/Host 바인딩을 확정하지 않는다. setup 성공은 Host가 로드할 설정과 브로커 준비까지이며 Host 실행/재시작은 nextActions로 안내한다. Host가 아직 실행되지 않아도 setupReady는 true일 수 있다. 협업 준비는 별도 필드로 표시하며 작업 왕복 E2E 완료와 동일시하지 않는다.

현재 신원 진단을 안전하게 얻지 못하면 verify_connection은 성공할 수 없다. Connected이지만 identity=unknown인 경우 제한시간까지 읽기 전용 확인 후 SETUP_PROTOCOL_ERROR와 check_provider_contract로 반환한다. 인증 거부/신원 불일치는 즉시 중단한다. 호스트 세션 관측만 미지원이면 hostSession=not_checked로 표시하고 setupReady 판정에는 사용하지 않는다.

## 7. 비밀 비노출·부분 실패 규칙

1. 비밀번호·초대코드·사용자 cookie·CSRF·중앙 Agent token·localToken·controlToken을 stdout/stderr·JSON/JSONL·로그·오류·trace·명령 인수·Host 설정 diff에 넣지 않는다. 접두/접미 일부나 해시도 출력하지 않는다. 입력 전체를 디버그 직렬화하지 않는다.
2. 비밀 입력은 숨김/전용 stdin, 저장은 기존 private file 모듈의 사용자 전용 ACL 또는 0600/0700·링크 거부·크기 제한·flush/원자 교체를 따른다. 비밀을 재개용 setup 입력 파일에 저장하지 않는다. 자격 파일 **경로**는 공개 결과에 반환할 수 있다.
3. provider 원문 오류·HTTP headers/body·stack·일반 환경변수를 복사하지 않는다. 메시지는 코드별 고정 문구, 공개 값은 allowlist로 재구성한다. 알려지지 않은 오류는 INTERNAL_ERROR로 축약한다.
4. 하위 구현은 구조화된 결과를 반환해야 한다. 기존 CLI stdout/에러 문자열을 무검증 중계하거나 문자열 일치로 API 성공·중복 생성 여부를 판단하지 않는다.
5. 뒤 단계 실패로 앞서 만든 중앙 Agent/팀·발급 token을 자동 삭제/폐기하지 않는다. 알려진 ID와 단계 outcome을 보존하고 같은 ID로 재개한다. 원격 변경과 로컬 저장은 단일 트랜잭션이 아니다.
6. 이번 실행이 시작한 브로커도 후속 실패 시 자동 강제 종료하지 않는다. 실패 결과의 stages에 기동 적용 내역을 남기고 `setup status`/명시 `stop`으로 확인한다. 기존 공유 브로커를 정리 대상으로 삼지 않는다.

## 8. 완료 기준과 후속 구현 검사

작업 01에서 결정한 항목:

- [x] 대화형/비대화형의 사용자·팀·프로젝트·도구·Agent 입력과 조건부 필수값.
- [x] 호출 방식·출력 채널·진행/성공/오류 응답 예시.
- [x] 단계·상태·변경 결과 불명·재개 규칙.
- [x] 오류 코드·추가 입력 descriptor·복구 action·종료 코드.
- [x] 비밀번호·토큰 비출력과 자격 전달/저장 경계.
- [x] 사용자 위임에 따른 박성수(등록/복구)·임효민(MCP 설정)·최보경(인증/연결) 연동 경계 배분. 별도 연락/담당자 승인 완료를 주장하지 않음.

아래는 규격을 구현할 때의 수용 검사이며 이번 문서 작업의 실행 결과가 아니다.

| 상황 | 확인할 결과 |
| --- | --- |
| 대화형 정상 / 기존 Agent 비대화형 | 같은 단계 순서·공개 DTO·ID/도구 변환, 기존 자격 재사용 시 발급/생성 0회 |
| 누락 입력·잘못된 경로·Host 사전 충돌 | validate에서 종료, 중앙 변경/설정 쓰기 0회 |
| 비대화형 stdin·JSON/JSONL | 질문 없음, 비밀 및 하위 출력 없음, result 1개와 실제 종료 코드 일치 |
| Agent 생성 응답 유실/프로세스 중단 | 재실행에도 생성 POST 자동 반복 0회, 목록 확인/use 복구 |
| ID 확인 후 토큰 유실/저장 실패 | 같은 ID 유지·token_required, 명시 발급만 가능 |
| 다른 팀/소유자·실행 중 바인딩 변경 | 권한/점유 코드, 기존 정상 프로필 보존 |
| MCP 동시 변경·부분 쓰기·검사 실패 | digest 충돌, 원자 교체/조건부 복원, 비공개 백업 및 rollback 결과 |
| health 정상이나 WS 인증/신원 실패 | setupReady=false, 기존 Host 소유권을 진단으로 교체하지 않음 |
| 실제 지원 Host 두 종류 | 설정 파싱·경로 전달·재시작 안내·읽기 전용 상태와 실제 바인딩 결과 대조 |
| timeout/취소·비밀 표식 주입 | 중앙 결과 불명 보존, 자동 생성/발급/업무 재전송 0회, stdout/stderr/로그 비밀 0건 |

변경 시 계약 버전·담당 경계·fixture를 함께 검토한다. 단순 필드 추가라도 필수값/기본 동작/종료 의미를 바꾸면 기존 자동화와 호환되는지 먼저 확인한다.

MCP 소비 확장: `get_connection_status`는 로컬 프로세스의 persistent 차단을 선택
`clientIssue`로 별도 제공한다. provider의 위 표·setupReady·issue는 유지하고,
MCP collaborationReady는 clientIssue가 있으면 false로 제한한다. 이는 CLI/Rust의
공통 ConnectionSnapshot 공식을 변경하지 않는다. 자세한 소비 계약은
`contracts/local-broker/v1`과 MCP onboarding 가이드를 따른다.
