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

`projectName`·`roleDescription` 중 하나 이상만 받는다. 공백만인 값·제어 문자·역할 비우기는
거부한다. name·toolType·소유자·팀·ID·로컬 projectRoot는 변경하지 않으며,
Agent 신규 생성이나 토큰 발급·회전을 하지 않는다.

1. 사용자 세션, 선택 팀, 프로필의 origin/user/team/Agent를 검증한다.
2. 중앙 GET 현재 값과 다른 필드만 준비하고, 변경이 있으면 CSRF를 조회한다.
3. MCP는 모든 사전 IO 후 프로필, 중앙 Agent, 현재 MCP 세션을 재검증한다.
   세션 대체·해제, 중앙 문맥 변경, 로컬 바인딩 변경에서는 전송하지 않는다.
4. PATCH 응답의 ID/owner/team/name/tool과 요청 값을 대조한다. 전송 후 확정된 결과는
   이후 세션 대체만을 이유로 결과 불명으로 바꾸지 않는다.
5. 중앙 성공 후 로컬 캐시의 두 표시 필드만 동기화한다. 다른 작업이 프로필을 바꿨으면
   원본을 보존하고 `localProfileSynced:false`를 반환한다. 캐시 미동기화를 이유로 PATCH를
   자동 재전송하지 않는다. `get_my_context`는 중앙의 최신 값을 반환한다.

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
