# Agent 정보 갱신 소비 계약 — S15P31D108-146

2026-11-08. 관련 PR: [#85](https://github.com/SSAFY-URAYAHA/DevLink_client/pull/85),
수정 Issue: [#86](https://github.com/SSAFY-URAYAHA/DevLink_client/issues/86).

## 제공 상태와 담당 경계

중앙 API는 server develop `aecdcc3db69254d1262eb7ec82809487717b7ec7`에 제공된다.
`AgentController`, `AgentService`, `UpdateAgentRequest`, `AgentResponse`,
`SecurityConfig`를 기준으로 아래 계약을 확인했다. API 자체를 client에서 구현하거나
fixture를 실제 중앙으로 간주하지 않는다.

CLI `devlink agent update`와 MCP `update_agent`는 같은 기존 Agent를 수정한다.
MCP에서 이 PC의 CLI 사용자 로그인 세션을 사용하는 방안은 PR #85의 채택안이다.
**백엔드 담당자가 이 소비 계약을 확인한 기록은 아직 없다.** 이 문서와 실제 연동 증거는
확인 가능한 자료이며, 담당자의 합의 자체를 대신하지 않는다. Jira 수용 기준 1은
확인 기록이 생기기 전까지 완료로 표시하지 않는다.

중앙의 [auth-team-agent.md](https://github.com/SSAFY-URAYAHA/DevLink_server/blob/aecdcc3db69254d1262eb7ec82809487717b7ec7/docs/auth-team-agent.md)는
아직 Agent 프로필 수정을 MVP 제외로 설명한다. 실제 PATCH 구현과 문서가 상충하므로,
담당자 확인 시 이 범위 문구와 client의 소비 계약을 함께 정리해야 한다.

## 중앙 계약

| 항목 | 제공 동작 |
| --- | --- |
| 경로 | `PATCH /api/v1/agents/{agentId}` |
| 인증 | 사용자 `JSESSIONID`와 CSRF. Agent Bearer 인증 경로가 아니다. |
| 권한 | 선택 Agent의 소유자이며 해당 팀의 현재 멤버여야 한다. |
| 본문 | `{name?, projectName?, roleDescription?}`. 각각 UTF-16 100, 120, 255 단위 이하. |
| 생략·null | 기존 값을 유지한다. |
| 공백 | name·projectName 공백만이면 400, roleDescription 공백만이면 null로 저장한다. |
| 성공 | HTTP 200, `{success:true,data:AgentResponse,...}` |
| 반환 DTO | agentId, teamId, ownerUserId, name, roleDescription, toolType, projectName, status, lastSeenAt, createdAt |
| 주요 거부 | 400 VALIDATION_FAILED, 401 인증 필요·만료, 403 ACCESS_DENIED(권한·CSRF), 404 RESOURCE_NOT_FOUND |

중앙 구현 근거:
[AgentController](https://github.com/SSAFY-URAYAHA/DevLink_server/blob/aecdcc3db69254d1262eb7ec82809487717b7ec7/src/main/java/com/devlink/server/controller/AgentController.java),
[AgentService](https://github.com/SSAFY-URAYAHA/DevLink_server/blob/aecdcc3db69254d1262eb7ec82809487717b7ec7/src/main/java/com/devlink/server/service/AgentService.java),
[UpdateAgentRequest](https://github.com/SSAFY-URAYAHA/DevLink_server/blob/aecdcc3db69254d1262eb7ec82809487717b7ec7/src/main/java/com/devlink/server/dto/UpdateAgentRequest.java),
[SecurityConfig](https://github.com/SSAFY-URAYAHA/DevLink_server/blob/aecdcc3db69254d1262eb7ec82809487717b7ec7/src/main/java/com/devlink/server/config/SecurityConfig.java).

## client 허용 범위

`projectName`·`roleDescription`·`part`·`roles` 중 하나 이상만 받는다(part·roles는 아래
S15P31D108-183 절). 공백만인 값·제어 문자·역할 설명 비우기는 거부한다. name·toolType·소유자·팀·ID·로컬 projectRoot는 변경하지 않으며,
Agent 신규 생성이나 토큰 발급·회전을 하지 않는다.

1. 사용자 세션, 선택 팀, 프로필의 origin/user/team/Agent를 검증한다.
2. 중앙 GET 현재 값과 다른 필드만 준비하고, 변경이 있으면 CSRF를 조회한다.
3. MCP는 모든 사전 IO 후 프로필, 중앙 Agent, 현재 MCP 세션을 재검증한다.
   세션 대체·해제, 중앙 문맥 변경, 로컬 바인딩 변경에서는 전송하지 않는다.
4. PATCH 응답의 ID/owner/team/name/tool과 요청 값을 대조한다. 전송 후 확정된 결과는
   이후 세션 대체만을 이유로 결과 불명으로 바꾸지 않는다.
5. 중앙 성공 후 로컬 캐시의 표시 필드(projectName·roleDescription, -183부터 part·roles 포함
   네 개)만 동기화한다. 다른 작업이 프로필을 바꿨으면
   원본을 보존하고 `localProfileSynced:false`를 반환한다. 캐시 미동기화를 이유로 PATCH를
   자동 재전송하지 않는다. `get_my_context`는 중앙의 최신 값을 반환한다.

## part와 roles (S15P31D108-183)

2026-11-10. 3차 MVP SET-3-02의 client 쪽이다. 중앙 수정 API는
[DevLink_server#76](https://github.com/SSAFY-URAYAHA/DevLink_server/pull/76)(server#73 종료)으로
제공됐고, server develop `d8be28dbc493708542515b1cfa2da477ace048b3`의
[UpdateAgentRequest](https://github.com/SSAFY-URAYAHA/DevLink_server/blob/d8be28dbc493708542515b1cfa2da477ace048b3/src/main/java/com/devlink/server/dto/UpdateAgentRequest.java),
[AgentService](https://github.com/SSAFY-URAYAHA/DevLink_server/blob/d8be28dbc493708542515b1cfa2da477ace048b3/src/main/java/com/devlink/server/service/AgentService.java),
[Agent](https://github.com/SSAFY-URAYAHA/DevLink_server/blob/d8be28dbc493708542515b1cfa2da477ace048b3/src/main/java/com/devlink/server/domain/agent/Agent.java)
코드로 확인했다. 위 표의 본문·반환 DTO에 아래가 더해진다.

| 항목 | 제공 동작 (코드 기준) |
| --- | --- |
| 본문 | `part?`(FRONTEND·BACKEND·INFRA·OTHER), `roles?`(최대 10개, 태그마다 64 UTF-16 단위 이하, 공백만이면 400) |
| 생략·null | part·roles 각각 기존 값을 유지한다. |
| `roles: []` | 태그를 모두 지운다. |
| 정규화 | 태그 양끝 공백 제거 후 같은 문자열은 하나만 남긴다. 목록 전체를 바꾼다. |
| 반환 DTO | `AgentResponse`에 `part`, `roles`가 추가됐다. |

client 동작:

- CLI `devlink agent update`에 `--part`, `--roles TAG[,TAG...]`, `--clear-roles`를 추가했다.
  MCP `update_agent`는 `part`, `roles`를 받는다. `roles: []`는 태그 삭제다.
- 태그는 생성 때와 같은 `normalize_roles`로 다듬은 뒤 중앙 현재 값과 비교한다. 다른 필드만
  본문에 넣고 null은 보내지 않는다. MCP는 입력의 null을 생략과 같게 보고 브로커에 넘기기 전에
  그 키를 뺀다. null만 있는 호출은 MCP 입력 검사에서 거부되고, 브로커 HTTP 경로에 직접 보내면
  VALIDATION_FAILED다.
- 수정 경로는 태그를 한 번 더 검사한다. 다듬은 뒤 빈 문자열이 되는 태그(예: U+0001만 있는
  태그)와 제어 문자가 남는 태그는 중앙에 보내기 전에 거부한다. 앞의 것은 중앙 코드상
  (`@Valid`와 `@NotBlank`) 400으로 거부될 입력이다. 뒤의 것(예: 태그 안의 NUL)은 중앙 저장소에서 실패해 결과 불명이 될 수
  있다고 보았는데, 중앙에서 실행해 확인한 것은 아니다. 생성·setup 경로의 `normalize_roles`는
  [setup 계약](../../contracts/setup/v1/README.md)이 고정하므로 바꾸지 않았다.
- 응답의 part·roles가 요청과 다르면 결과 불명이다.
- 중앙이 실제로 보낸 값과 로컬 기본값을 구분한다(PR #118 리뷰 반영). 프로필 캐시의 `Agent`는
  없는 필드를 OTHER·태그 없음으로 읽는데, 이 기본값은 초기화 요청(`--part OTHER`,
  `--clear-roles`)이 요구하는 값과 같다. 그래서 응답에 필드가 있었는지를 따로 기록한다.
  - 사전 조회에 필드가 없으면 현재 값을 모르는 것으로 보고, 요청한 part·roles를 항상 보낸다.
    "이미 같다"고 판단하지 않는다.
  - PATCH 응답에 필드가 없으면 요청 값과 같아 보여도 확인된 것이 아니므로 결과 불명이다.
  - 그 결과, part·roles가 없는 이전 server나 응답에서 필드를 빼는 server에 part·roles 변경을
    보내면 어떤 값이든 결과 불명으로 끝난다. 성공이나 no-op으로 보고하지 않는다.
  - 중앙이 보내지 않은 part·roles는 `update_agent` 결과의 `agent`에 넣지 않고, 로컬 캐시의
    기존 값도 덮어쓰지 않는다. projectName·roleDescription만 바꾸는 요청은 그대로 성공한다.
- 사전 조회 뒤 세션·바인딩을 다시 확인하고 전송하는 순서(위 3번)는 그대로다. 로컬 캐시는
  네 필드를 동기화하되, part·roles는 중앙이 보낸 경우에만 바꾼다.

**응답 노출 결정.** `update_agent` 결과의 `agent`, 그리고 `register_agent`·`list_agents` 결과에
part·roles를 넣었다(중앙이 보낸 경우에만). 세 결과는 MCP에서 열린 객체로 받으므로 이전 MCP 프로세스와 새 브로커가
섞여도 깨지지 않는다. `register_agent`·`list_agents`는 중앙이 준 값을 그대로 전달한다. 필드가
없거나 null이거나 문자열·문자열 배열이 아니면 그 필드만 뺀다. 이 값 때문에 Agent 확인이나
목록 전체가 실패하지 않게 하기 위해서다(이 확인은 작업 생성·탐색 경로도 거친다). 반대로 새
MCP가 이전 브로커에 part·roles 값을 보내면 브로커가 모르는 필드라 VALIDATION_FAILED로 거부한다.
null은 MCP가 빼고 보내므로 이전 브로커에서도 나머지 필드 수정은 된다.
`get_my_context`의 `agent`에는 이 스토리에서 넣지 않았다. 이 응답은
[context schema](../../contracts/local-broker/v1/context.schema.json)가 필드를 고정하고 MCP가
알 수 없는 필드를 거부하기 때문이다. 이 스토리에서 `contracts/local-broker/v1`은 바뀌지 않았다.

**2026-11-12 갱신: `get_my_context`에 part·roles 추가 (S15P31D108-240).** 3차 API 명세의 MCP Tool 명세
(`get_my_context`: 내 Agent 문맥에 part·roles 추가)에 따라 `agent.part`·`agent.roles`를 넣었다.
규칙은 위와 같다. 중앙 `/agents/me`가 문자열 part, 문자열 배열 roles를 보낸 경우에만 넣고,
없거나 null이거나 형식이 다르면 그 필드만 뺀다. 기본값(OTHER, 빈 목록)으로 채우지 않으며
`roles: []`는 중앙이 태그 없음을 알린 값이다. 이 값 때문에 문맥 조회가 실패하지 않는다.
context schema에는 두 필드를 선택 필드로 추가했다.

- 새 MCP와 이전 브로커: 두 필드가 선택이라 그대로 동작한다(필드가 없을 뿐이다).
- 이전 MCP와 새 브로커: 중앙이 part나 roles를 보내 브로커가 필드를 넣은 경우, 실행 중인 이전
  MCP 프로세스는 모르는 필드를 거부하므로 `get_my_context`가 BROKER_CONTEXT_INVALID가 된다.
  중앙이 두 값을 모두 보내지 않으면 필드가 없어 이전 MCP도 동작한다. 이 실패는 MCP 프로세스의
  차단 상태로 남지 않고 업무 도구는 계속 동작하지만, MCP를 갱신할 때까지 `get_my_context`는
  호출마다 실패한다. 이전 MCP의 오류 문구와 `inspect_profile_then_rebind` 안내는 인증·프로필
  확인을 가리키므로 로그인 문제로 오해하기 쉽다. 원인은 버전 차이다.
- 계약 버전은 올리지 않았다. 대신 브로커를 갱신하면 MCP 런타임도 같은 배포본으로 갱신하고
  Host(MCP 프로세스)를 다시 열어야 한다.

**검증 (2026-11-10, 로컬 Windows).**

- fixture: `cli_agents`의 `update_changes_part_and_role_tags_keeps_omitted_fields_and_clears_tags_explicitly`
  (CLI)와 `mcp_update_agent_changes_part_and_roles_and_keeps_null_or_omitted_values`(로컬 HTTP).
  합성 중앙은 본문에 null이 오면 실패한다. PATCH 응답에서만 part·roles를 빼는 모드와 사전
  조회에서도 빼는 이전 server 모드에서, 초기화 요청(`--part OTHER --clear-roles`)을 포함한
  part·roles 변경이 모두 결과 불명으로 끝나고 중앙 값과 로컬 캐시가 그대로인지 확인한다.
  같은 모드에서 projectName만 바꾸면 성공하고 결과와 캐시에 기본값이 들어가지 않는지도 본다.
  단위 테스트는 필드 없는 응답이 "보고 없음"으로 읽히고 캐시의 part·roles를 지키는지 확인한다. `task_creation`은 중앙 응답에
  part·roles가 없거나 null·비문자열일 때 `register_agent`·`list_agents`가 그 필드만 빼고
  성공하는지 확인한다.
- 실제 중앙: E2E `e2e/specs/p1-s17-agent-metadata-update.spec.ts`. server `d945f8b`(E2E lock,
  server#76 병합 커밋 `dd8cffe`보다 11커밋 뒤라 PATCH part·roles를 포함한다),
  PostgreSQL 16, 실제 CLI·브로커와 MCP SDK 클라이언트로 실행했다. 저장 값은 도구 결과가 아니라
  Agent 토큰의 `GET /api/v1/agents/me`로 읽었다. 생략·null은 유지, `[]`는 삭제, 다듬기·중복 제거,
  CLI `--part`·`--roles`·`--clear-roles`, `register_agent`·`list_agents` 노출, `find_agent`의
  `matchedRoles` 반영, 상대 Agent 불변을 확인했다.
- 실제 LLM Host(Codex·Claude Code)로는 실행하지 않았다. server develop `d8be28d`에서는 코드만
  대조했고 실행은 lock 커밋으로 했다.

## 오류 소비

| 상황 | MCP 결과 |
| --- | --- |
| 미로그인·세션 만료 | USER_LOGIN_REQUIRED / rejected |
| account.lock 경합 | LOCAL_ACCOUNT_BUSY / rejected |
| MCP 세션 대체·해제 | 해당 세션 오류 / rejected |
| 바인딩·권한 불일치 | FORBIDDEN / rejected |
| 중앙 404 | RESOURCE_NOT_FOUND / rejected |
| PATCH 후 전송 실패·5xx·응답 불일치 | CENTRAL_UNAVAILABLE 또는 CENTRAL_PROTOCOL_ERROR / unknown |

미확인 결과를 성공으로 처리하지 않는다. 비밀 Cookie·CSRF·Agent 토큰은 결과·오류·공유
보고서에 출력하지 않는다. 인증 장애 시 자동 로그인, Agent 전환·생성, 작업 재전송을 하지 않는다.

## 재실행

fixture 회귀: `cargo test --manifest-path rust/Cargo.toml -p devlink-cli --test cli_agents --locked`.
실제 Rust broker와 합성 중앙을 사용해 사전 IO 중 대체·해제·문맥·바인딩 변경,
전송 후 확정 결과 유지, `localProfileSynced:false`에서 원본 보존을 검사한다.

실제 Spring/PostgreSQL과 Codex·Claude Code 검증은 격리한 loopback 서버를 시작하고
아래 환경 변수를 설정해 실행한다. 사용자의 운영 프로필은 사용하지 않는다.

```text
DEVLINK_TEST_CENTRAL_URL=http://127.0.0.1:<port>/
DEVLINK_TEST_CENTRAL_COMMIT=<실제 중앙 커밋 40자리>
DEVLINK_TEST_CLI=<빌드한 devlink.exe 절대 경로>
DEVLINK_TEST_MCP_ENTRY=<빌드한 dist/index.js 절대 경로>
DEVLINK_TEST_HOST_ARTIFACTS=<현재 사용자 전용 ACL의 증거 디렉터리>
node packages/mcp-server/scripts/verify-agent-update.mjs
```

실행기는 임시 사용자 2명·Agent 2개를 준비하고, 실제 각 Host에 register → context → update →
context → 같은 값 update를 요청한다. 독립 중앙 GET, Agent·토큰 유지, 상대 Agent 불변과
no-op을 확인하고 상관 ID·transcript hash·`report.json`을 남긴다. 실행기가 시작한 broker와
마커가 붙은 MCP 프로세스의 종료를 확인한다. Spring/PostgreSQL은 호출자가 중지한다.

## 실제 실행 증거 (2026-11-08)

[공유 가능한 결과 JSON](evidence/agent-update-146.json)은 합성 서버 fixture가 아니라
위 커밋의 Spring 서버와 PostgreSQL 17에서 실제 두 Host를 실행한 결과다.

- `passed`, `actualLlmHostsExecuted`, `ownedBrokersStopped`, `ownedMcpProcessesStopped` 모두 true.
- 각 Host는 MCP를 5회 순차 호출했다. 변경 성공 → 중앙 최신 문맥 확인 → 같은 값 no-op과
  부모 실행기의 독립 중앙 GET 확인이 모두 통과했다.
- Agent 수는 두 개로 유지됐고 두 프로필의 Agent 토큰과 ID가 유지됐다. 상대 정보도 보존됐다.
- Codex는 CLI 기본 모델을 사용했다. transcript에 모델 ID가 없어 임의로 특정하지 않는다.
  Claude는 `claude-opus-5-5`, effort high로 실행했고 transcript 모델도 확인했다.
- CLI SHA256: `1a4b6781b354ad2e6737072adc3f99735274203e6e39dabb6cd278c2667b5b78`.
- 공유 JSON에는 각 실제 MCP 요청의 requestId/mcpRequestId와 transcript SHA256이 있다.
  원본 transcript와 비밀 프로필은 사용자 전용 ACL의 로컬 증거 폴더에만 보관한다.
- 실행 후 격리 Spring/PostgreSQL을 중지하고 두 포트가 닫힌 것을 확인했다.

fixture 회귀는 별도다. `cli_agents` 21개(9개 사전 조회 경합 조건 포함), 캐시 동기화
단위 테스트 2개, MCP 전체 181개가 통과했다. 이 결과를 백엔드 담당자의 계약 동의로
해석하지 않는다. 위의 문서 범위 상충과 담당자 확인은 여전히 남아 있다.
