# 중앙 연동 현재 상태

확인일: 2026-11-09 (Asia/Seoul). 실제 중앙 server와 client가 지금 어떤 경로·자격·DTO로 연결되는지 정리한다. 2026-10-29 설계 결정과 그 근거는 [중앙 연동 결정표](README.md)에 그대로 남겨 두었다. 그 문서는 역사 기록이므로 현재 상태는 이 문서를 본다.

## 기준과 확인 방법

| 확인 대상 | 기준 SHA | 확인한 것 |
| --- | --- | --- |
| 중앙 server develop | `aecdcc3db69254d1262eb7ec82809487717b7ec7` (2026-11-06 커밋) | `config/SecurityConfig`, `controller/*`, `dto/**`, `handler/TaskRequestAuthorizationAdvice`, `repository/TaskRepository`, `service/TaskAuthorizationService`·`TeamAuthorizationService`·`TaskCreationService`·`TaskRoutingService`·`AgentPresenceService`·`TaskStatusService`·`TaskCompleteService`·`TaskQueryService`·`DatabaseAgentTokenVerifier`·`AgentOnlineTaskListener`, `handler/DevLinkWebSocketHandler`, `domain/task/Task`, `common/ErrorCode` |
| client develop | `c4c751d0fce65df28aa11042f9b2c486acf1787a` | `rust/crates/devlink-broker/src/`의 `task_client.rs`·`task_operations.rs`·`user_auth.rs`·`agent_context.rs`·`agent_api.rs`·`teams.rs`·`task_messages.rs`·`agent_profile.rs`·`agent_profile/` |

```bash
gh repo clone SSAFY-URAYAHA/DevLink_server server -- --depth 50 --branch develop
```

```bash
git -C server rev-parse HEAD
```

```bash
grep -rnE '@(Get|Post|Put|Patch|Delete|Request)Mapping|@AuthenticationPrincipal' server/src/main/java/com/devlink/server/controller
```

```bash
grep -rnoE '"/api/v1[^"]*"|format!\(\s*"/api/v1[^"]*"' rust/crates --include=*.rs
```

**표시 규칙.** 이 문서의 server·client 동작은 모두 **코드 기준**이다. 이번 갱신에서 server나 client를 실행하지 않았다. 다른 스토리가 실제로 실행해 확인한 내용은 [실행 기록](#실행-기록)에 따로 적었다. 코드 기준 항목은 실행 기록이 생기기 전까지 동작 보장으로 쓰지 않는다.

## 자격과 보안 체인 (코드 기준)

`SecurityConfig`에는 세 개의 필터 체인이 있다.

| 순서 | 대상 경로 | 자격 | 세션·CSRF |
| --- | --- | --- | --- |
| 1 | `GET /ws/broker` | Agent Bearer, `ROLE_AGENT` | STATELESS |
| 2 | `/api/v1/tasks`, `/api/v1/tasks/**`, `/api/v1/agents/me`, `/api/v1/agents/me/task-messages`, `GET /api/v1/teams/*/tasks`, `POST /api/v1/teams/*/agents/recommendations`, `Authorization` 헤더가 있는 `GET /api/v1/agents/teams/{teamId}` | Agent Bearer, `ROLE_AGENT` | STATELESS, CSRF 해제 |
| 3 | 그 밖의 경로. `POST /api/v1/auth/signup·login`, `GET /api/v1/auth/csrf`, `/api/v1/health`, `/actuator/health`는 permitAll이다. `/api/v1/auth/users/me`, `/api/v1/teams/**`, `/api/v1/agents/**`는 `ROLE_USER`가 필요하다. | 사용자 세션 `JSESSIONID` | `HttpSessionCsrfTokenRepository`. 로그아웃은 `/api/v1/auth/logout`이며 세션을 무효화하고 `JSESSIONID`를 지운다 |

- **Agent Bearer 검증.** `DatabaseAgentTokenVerifier`가 원문 토큰의 SHA-256으로 Agent를 찾는다. 그 Agent 소유자가 현재 같은 팀 멤버인지 요청마다 확인한다. 결과는 `AgentPrincipal(agentId, ownerUserId, teamId)`다. 2026-10-29에 fail closed였던 production verifier는 이제 구현돼 있다.
- **Agent 토큰 발급.** `POST /api/v1/agents/{agentId}/token`은 사용자 세션이 필요하다. 201을 반환하고 `Cache-Control: no-store`를 붙인다. 응답은 `AgentTokenResponse{agentId, token}`이다. client는 `agent_profile/tokens.rs`·`agent_profile.rs`에서 호출한다.
- **BROKER_READY.** `connection_id`, `agent_id`, `user_id`, `team_id`를 보낸다. 10-29 기록의 "READY만으로 user/team principal을 만들 수 없다"는 더 이상 맞지 않는다.
- **로그아웃 범위.** 로그아웃은 HTTP 세션만 정리한다. 발급된 Agent 토큰을 폐기하는 코드는 로그아웃 경로에 없다.

## 2026-10-29 기록과 달라진 항목

| 항목 | 10-29 기록 (server `5891ef2`) | 현재 server (`aecdcc3`, 코드 기준) | client 소비 (`c4c751d`) |
| --- | --- | --- | --- |
| 사용자 로그인·CSRF | 로그인 구현과 세션 발행·조회·종료, CSRF 교환이 없다 | `AuthController`: `GET /auth/csrf`, `POST /auth/signup`, `POST /auth/login`, `GET /auth/users/me`. 로그아웃은 체인 3의 logout 설정 | `user_auth.rs`가 csrf → login → users/me 순서로 호출하고 logout도 호출한다. `JSESSIONID` 쿠키를 보관한다 |
| Rust 업무 REST 자격 | 결정 필요. WS 전용 Bearer | 체인 2가 업무 REST에 Agent Bearer를 받는다 | `task_client.rs`, `task_operations.rs`, `agent_context.rs`, `task_messages.rs`가 Bearer로 호출한다 |
| create_task 자격 | 사용자 세션 (`@SessionAttribute("userId")`) | Agent Bearer. 요청 Agent는 토큰의 Agent다. `TaskRequestAuthorizationAdvice`가 body를 읽은 직후 토큰의 팀과 경로 `teamId`를 비교해 다르면 403 `ACCESS_DENIED`를 낸다. 이어서 `TaskAuthorizationService`가 소유자 팀 멤버십(403)과 요청·수신 Agent의 존재·팀(404)을 확인한다 | `POST /api/v1/tasks/{teamId}` |
| 멱등 범위 | `(requesterUserId, requestKey)` | `(requester_agent_id, request_key)`. 유니크 제약 이름은 `uk_tasks_request_key` | 같은 키 재시도는 같은 Agent(agent_id)의 토큰으로 보내야 한다. 토큰을 재발급해도 Agent가 같으면 범위는 같다 |
| 같은 키 동시 경합 | 없음 | `recoverReplay`가 유니크 제약 위반 뒤 승자의 작업을 다시 읽는다. 내용이 같으면 `replayed=true`(200), 다르면 409 `REQUEST_KEY_CONFLICT` | 409 `REQUEST_KEY_CONFLICT`를 `outcome=unknown`으로 보존한다 |
| 요청자 소유 검증 | 없음 | 요청자가 토큰의 Agent로 고정되므로 body로 요청자를 바꿀 수 없다 | 요청자를 body·query로 보내지 않는다 |
| 생성 DTO | `requestKey`, `receiverAgentId`, `prompt` | `taskSummary`가 추가됐다(선택). 앞뒤 공백을 지운 뒤 1~1,000 코드포인트여야 한다. `prompt`는 `@NotBlank`이고 DTO 길이 상한이 없으며 DB 컬럼은 `TEXT`다 | `task_client.rs`는 `{requestKey, receiverAgentId, prompt}`만 보내고 `taskSummary`는 보내지 않는다 |
| 업무 경로 | create만 있음. 나머지 6개는 `null` | 아래 [MCP 업무 도구 매핑](#mcp-업무-도구-매핑) 참조 | 7개 도구 모두 중앙 경로를 호출한다 |
| 완료 파일 | `TaskFile` 엔티티만 있음 | `POST /tasks/{taskId}/complete`가 `taskFiles[{relativePath ≤500, changeType, diffText?}]`를 받는다. 생략하면 빈 목록이다 | `task_operations.rs` |
| 전달 이력 조회 | 없음 (-76 대기) | `GET /tasks/{taskId}`의 `events`, `GET /tasks/{taskId}/events`. 응답 DTO에 `deliveredAt` 필드는 없다 | get_task_result가 `GET /tasks/{taskId}`를 사용한다 |
| 수신 Agent 오프라인 | 저장 전 거부가 없다 | 아래 [오프라인 수신자](#오프라인-수신자-현재-구현만) 참조 | 아래 참조 |

## MCP 업무 도구 매핑

모든 경로는 `/api/v1` 아래다. 자격은 모두 Agent Bearer다. 응답 봉투는 `ApiResponse{success, data, code, message}`다.

| MCP 도구 | 중앙 경로 (코드 기준) | 성공 코드 | 주요 거부 |
| --- | --- | --- | --- |
| register_agent | `GET /agents/me`로 토큰의 Agent를 확인하고 로컬 세션에 묶는다. 생성 API는 호출하지 않는다 | 200 | 401 |
| list_agents | `GET /agents/teams/{teamId}?ownedByMe=` (`Authorization` 헤더가 있을 때 체인 2). list_agents 경로의 client는 `ownedByMe`를 보내지 않는다 (사용자 세션 경로인 `agent_profile/registration.rs`는 `ownedByMe=true`를 보낸다) | 200 | 토큰의 팀과 다르면 403 |
| create_task | `POST /tasks/{teamId}`, body `{requestKey ≤64, receiverAgentId, taskSummary?, prompt}` | 새 작업 201, 같은 키 재생 200 | 400 `VALIDATION_FAILED`, 403 `ACCESS_DENIED`(토큰 팀 불일치·소유자 비멤버), 404 `RESOURCE_NOT_FOUND`(수신자 없음·수신자 팀 불일치), 409 `REQUEST_KEY_CONFLICT` |
| update_task_status | `PATCH /tasks/{taskId}/status`, body `{status, version, rejectionReason?, failureReason?, partiallyModified?}` | 200 | 수신 Agent가 아니면 403 `ACCESS_DENIED`, 409 `TASK_VERSION_CONFLICT`·`TASK_STATE_CONFLICT` |
| complete_task | `POST /tasks/{taskId}/complete`, body `{version, resultText, taskFiles?}` | 200 | 다른 팀이거나 수신 Agent가 아니면 403, 409 `TASK_VERSION_CONFLICT`, IN_PROGRESS가 아니면 409 `TASK_STATE_CONFLICT` |
| get_tasks | `GET /teams/{teamId}/tasks?scope=mine\|team&direction=sent\|received\|all&status=&page=&size=` (size 1~100, 기본 20). client는 `scope=mine`만 보낸다 | 200 | 400, 토큰의 팀과 다르면 403, 팀이 없으면 404 |
| get_task_result | `GET /tasks/{taskId}`. 접근 검사는 작업의 팀과 토큰의 팀 일치, 팀 존재, 소유자 멤버십만 본다. 요청·수신 Agent인지는 보지 않는다 | 200 | 다른 팀 작업이면 403, 작업·팀이 없으면 404 |

상태 전이(`TaskStatusService`, `ChangeTaskStatusRequest`)는 다음과 같다. `PENDING → IN_PROGRESS`와 `PENDING → REJECTED`(`rejectionReason` 필수)가 있다. `IN_PROGRESS → FAILED`에는 `failureReason`과 `partiallyModified`가 모두 필요하다. `IN_PROGRESS → COMPLETED`는 complete 경로로만 바뀐다. 상태 이름은 `PENDING`, `IN_PROGRESS`, `COMPLETED`, `FAILED`, `REJECTED`다.

client의 오류 매핑은 `task_client.rs`에 있다.

- 401은 `CENTRAL_UNAUTHENTICATED`(rejected, 로컬 503), 403은 `FORBIDDEN`(rejected), 3xx는 `CENTRAL_PROTOCOL_ERROR`(unknown)다. 이 셋은 `central_error`보다 먼저 처리한다.
- 생성 응답이 실패인데 `taskId`가 있으면 `CENTRAL_RESULT_UNKNOWN`으로 taskId를 보존한다.
- `central_error`: 5xx는 `CENTRAL_UNAVAILABLE`(unknown)이다. 400 `VALIDATION_FAILED`·`INVALID_QUESTION_REFERENCE`, 404 `RESOURCE_NOT_FOUND`, 409 `RECEIVER_OFFLINE`·`TASK_VERSION_CONFLICT`·`TASK_STATE_CONFLICT`·`TASK_CONVERSATION_UNAVAILABLE`는 rejected다. 409 `REQUEST_KEY_CONFLICT`는 rejected 코드에 `outcome=unknown`을 붙인다. 409 `DUPLICATE_REQUEST`는 `CENTRAL_RESULT_UNKNOWN`이다. server `aecdcc3`는 `DUPLICATE_REQUEST`를 던지지 않으므로 이 매핑은 방어용이다. 그 밖의 조합은 `CENTRAL_PROTOCOL_ERROR`(unknown)다.

## 그 밖의 중앙 경로

| 경로 | 자격 (코드 기준) | client 소비 |
| --- | --- | --- |
| `POST /tasks/{taskId}/questions`, `POST /tasks/{taskId}/answers` | Agent Bearer. 새 메시지 201, 같은 키 재생 200 | `task_operations.rs` (S15P31D108-166) |
| `GET /agents/me/task-messages?cursor=&limit=` (기본 50) | Agent Bearer | `task_messages.rs` |
| `GET /tasks/{taskId}/events` | Agent Bearer | 호출하지 않음 |
| `POST /teams/{teamId}/agents/recommendations` | Agent Bearer | 호출하지 않음 |
| `POST /teams`, `GET /teams`, `GET /teams/{id}`, `GET /teams/{id}/members`, `POST /teams/{id}/invites`, `POST /teams/join` | 사용자 세션·CSRF | `teams.rs` (invites는 호출하지 않음) |
| `POST /agents/teams/{teamId}`, `GET /agents/teams/{teamId}`(헤더 없음), `GET·PATCH·DELETE /agents/{agentId}`, `POST /agents/{agentId}/token` | 사용자 세션·CSRF | `agent_profile.rs`·`agent_profile/` (DELETE는 호출하지 않음) |
| `GET /web/teams/{teamId}/tasks`, `/web/teams/{teamId}/tasks/{taskId}`, `/web/teams/{teamId}/tasks/{taskId}/events` | 사용자 세션. 팀 멤버여야 하고, 상세·이력은 요청 또는 수신 Agent를 소유한 사용자만 본다 | client는 호출하지 않음 (웹 화면용) |

## 오프라인 수신자 (현재 구현만)

오프라인 수신자 정책(409로 거부할지, 201로 저장한 뒤 다시 보낼지)은 팀에서 논의 중이다. 이 절은 결론을 내리지 않고 `aecdcc3`의 코드와 `c4c751d`의 소비 코드만 적는다.

- **server, 코드 기준.** `TaskCreationService.create`는 수신자의 존재와 팀만 확인하고 접속 여부는 보지 않는다. 작업을 `PENDING`으로 저장한 뒤 전송한다. `TaskRoutingService`는 수신 Agent의 DB 상태가 `ONLINE`이 아니면 `RECEIVER_OFFLINE`, `ONLINE`이지만 현재 WS 연결이 없으면 `CONNECTION_UNAVAILABLE`을 돌려준다. 둘 다 `DELIVERY_NOT_SENT` 이력으로 저장된다. 응답은 201이고 `delivery = {status: DELIVERY_NOT_SENT, reasonCode: RECEIVER_OFFLINE 또는 CONNECTION_UNAVAILABLE}`다.
- **server, 코드 기준.** 재전송은 WS 연결만으로 시작하지 않는다. 수신 broker가 `AGENT_PRESENCE {status: ONLINE}`을 보내고 `AgentPresenceService`의 ONLINE 저장이 커밋되면 `AgentOnlineTaskListener`가 `retryUndelivered`를 호출한다. 그 세션이 현재 연결일 때만 `PENDING`이면서 `deliveredAt`이 null인 작업을 id 순으로 다시 보낸다.
- **server, 코드 기준.** `ErrorCode.RECEIVER_OFFLINE`(409)은 정의돼 있다. `aecdcc3`의 생성 경로에서는 이 코드를 던지지 않는다.
- **client, 코드 기준.** `central_error`는 409 `RECEIVER_OFFLINE`을 rejected로 매핑한다. 생성 응답의 `delivery.reasonCode = RECEIVER_OFFLINE`도 유효한 전달 사유로 받는다. 따라서 두 정책 어느 쪽이든 받아들인다.

## 아직 제공되지 않는 것

| 항목 | 근거 | 영향 |
| --- | --- | --- |
| requestKey로 작업 조회 | server `aecdcc3`의 컨트롤러에 requestKey를 받는 조회 경로가 없다. `TaskRepository.findByRequesterAgentIdAndRequestKey`는 생성 경로 안에서만 쓰인다 | taskId를 받기 전에 응답이 유실되면 같은 키로 생성을 다시 보내 replay를 받는 것 말고는 확인할 방법이 없다 |
| 조회 응답의 `deliveredAt` | `TaskDetailResponse`와 `TaskListItemResponse`에 필드가 없다. 전달 이력은 `events`의 `ACK_*`·`DELIVERY_*` 항목으로만 보인다 | 최신 ACK 시각은 이벤트로 추정해야 한다 |
| 로그아웃 시 Agent 토큰 폐기 | 로그아웃 설정은 HTTP 세션만 정리한다 | 로그아웃 뒤에도 발급된 Agent 토큰과 WS는 유지될 수 있다 (코드 기준) |

## 실행 기록

아래 실행은 이 문서를 갱신하면서 다시 하지 않았다. 각 기록의 SHA는 이 문서의 기준 SHA와 다를 수 있다.

- 2026-10-30, server `0e8c2b7`, client `970b5ea`: 격리 loopback Spring Boot와 PostgreSQL 17.11에서 두 Agent의 업무 도구 왕복을 실행했다. [중앙 계약 정합화와 두 Agent 컨텍스트 왕복](../../packages/mcp-server/docs/central-roundtrip.md) 참조.
- server `aecdcc3`: S15P31D108-146에서 실제 Host로 register_agent, get_my_context, update_agent를 실행했다. [증거 JSON](../../rust/docs/evidence/agent-update-146.json)과 [Agent 갱신 소비 계약](../../rust/docs/agent-update-contract.md) 참조.
- Issue #89는 2026-11-02 server `b5ae244` 실제 중앙 검증을 언급한다. 이 기록은 Notion에 있고 저장소에는 없다.

## 갱신 방법

server develop이 바뀌면 위 [기준과 확인 방법](#기준과-확인-방법)의 명령으로 다시 대조한다. 그다음 기준 SHA, 확인일, 바뀐 행을 고친다. 실행으로 확인한 항목은 실행 기록에 SHA와 함께 추가한다. 2026-10-29 결정표는 고치지 않는다.
