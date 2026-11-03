# Rust 중앙 업무 중계 (-93)

2026-10-30 현재 계약. 생성 URL·REJECTED 호환 검증과 교차검토 이력은 아래에 통합했다. 과거 중앙 커밋의 정책 관찰을 현재 서버 전체의 수용 판정으로 확대하지 않는다.

후속 -94는 최신 고정 중앙 `a5c7987`의 생성 URL `/api/v1/tasks/{teamId}`를 반영했다. 아래 `02de434` 기록은 당시 검증 증거다. 새 4개 중계·DTO·검증·남은 조건은 [task-operations.md](task-operations.md)를 참고한다. 과거 중앙 정책 blocker가 해결됐다는 의미는 아니다.

## 범위와 상태

검증된 로컬 세션·중앙 Agent Bearer로 기존 Agent 확인, 팀 Agent 목록, 작업 생성을 중계한다. 공통 요청·응답 envelope, 중앙 응답 크기/시간 제한, canonical Long ID, private requestKey 기록을 제공한다. 중앙 인증 필터·권한·DB·ACK·중복 생성 정책은 변경하지 않는다.

**구현 및 실제 중계 검증과 스토리 전체 수용을 구분한다.** 고정 중앙 `02de43468e9dd208a4a3cbc9bf142858e315e156`에서는 오프라인 수신자 작업을 DB에 먼저 저장한 뒤 `DELIVERY_NOT_SENT / RECEIVER_OFFLINE`으로 응답했다. -67/-71의 저장 전 거부 조건이 충족되지 않으므로 -93 전체 완료를 선언하지 않는다. Rust에서 사전 목록 조회로 중앙 정책을 흉내 내거나 이미 저장된 작업을 실패로 변경하지 않는다.

## 실제 공급 경로

| 로컬 POST | 중앙 Agent Bearer provider | 동작 |
| --- | --- | --- |
| `/v1/agents/register` | GET `/api/v1/agents/me` | 현재 자격의 기존 Agent만 확인. 생성·수정·토큰 발급 호출 없음 |
| `/v1/agents/list` | GET `/api/v1/agents/me`, GET `/api/v1/agents/teams/{teamId}` | 현재 user/team/Agent를 대조한 뒤 같은 팀 목록 제공 |
| `/v1/tasks/create` | GET `/api/v1/agents/me`, POST `/api/v1/tasks/{teamId}` | 중앙 context와 대조한 로컬 actor의 팀을 경로에 사용하고 요청자는 Bearer로 결정 |

중앙 `a5c7987`의 공식 생성 경로 `/api/v1/tasks/{teamId}`는 Agent Bearer API다. 이전 `/api/v1/agent/tasks`로의 호출·자동 fallback과 임시 `agentId` 파라미터는 사용하지 않는다. teamId는 검증된 세션에서 가져오며 MCP arguments/body에 actor ID를 추가하지 않는다. 사용자 cookie·CSRF를 업무 provider에 전달하지 않는다. 모든 로컬 경로는 MCP localToken만 사용하며 CLI controlToken이나 중앙 Agent 토큰으로 인증할 수 없다.

`register_agent`만 미바인딩 세션을 허용한다. 중앙 Agent가 현재 READY/신원 공급부와 일치하는지도 재확인한다. 성공 data의 Agent를 기존 MCP 소비자가 별도 `/v1/sessions/:id/bind`로 바인딩한다. 이 확인 응답 자체가 bind 성공이나 ONLINE을 뜻하지 않는다. 기존 MCP 인자 name/toolType/role은 주어졌다면 기존 메타데이터와 일치해야 하고, 다르면 새 Agent 생성/갱신 대신 거부한다. `{}`로 현재 Agent를 확인할 수도 있다. 나머지 업무는 bound 세션이 필수다.

## 로컬 요청과 ID

공통 요청은 `requestId`, `operation`, `arguments`, `context`와 선택적 `mcpRequestId`다. context는 sessionId/userId/teamId 및 선택 agentId/toolType이며 로컬 세션과 정확히 대조한다. actor를 인자나 사용자 파일 값으로 대체하지 않는다. 대기 중 해제·교체·만료·중앙 신원 변경은 응답 반환/전송 전 다시 검사한다.

작업 생성의 직접 HTTP 입력:

```json
{
  "requestId": "caller-correlation-id",
  "operation": "create_task",
  "arguments": { "receiverAgentId": "13", "prompt": "원문 그대로 전달할 프롬프트" },
  "context": { "sessionId": "current-session", "userId": "3", "teamId": "1", "agentId": "12" }
}
```

prompt의 공백·개행을 보존하고 중앙 WebSocket 프레임 한도(256 KiB, envelope 여유 1 KiB) 안에 들어가는지도 검증한다. 이 한도는 로컬 요청 128 KiB 한도보다 크므로 브로커가 보낸 prompt·resultText는 TASK_CREATED·TASK_COMPLETED로 항상 되돌아올 수 있다. 중앙 POST 본문은 `requestKey`, 숫자 `receiverAgentId`, `prompt` 세 필드만 포함한다. 본문에는 requesterAgentId/teamId를 넣지 않으며, teamId는 검증된 세션 값으로 경로에만 사용한다.

현재 MCP의 `recipientAgentId/title/description/taskContext?` 입력도 호환한다. 이 기존 입력은 이전 구현과 같은 JSON 문자열 prompt로 직렬화한다. 두 입력을 섞거나 actor·requestKey를 인자에 추가하면 거부한다. MCP schema/문구의 최종 수렴은 -97이며 이 스토리에서 MCP 소스를 변경하지 않는다. 같은 키로 기존 작업을 확인할 때 중앙의 `REJECTED`도 결과 상태로 보존한다. 이는 새 작업 생성·자동 수락·상태 변경이 아니다.

중앙 숫자 ID를 Rust 정수로 검증하고 로컬에는 정규 10진 문자열로 반환한다. 범위는 1..9223372036854775807이다. 부호·공백·leading zero·범위 초과·opaque ID를 거부하며 JS Number 변환을 거치지 않는다. Agent 공개 DTO의 ID·이름·역할·도구·프로젝트·상태·시각을 매핑하고 알려지지 않은 필드나 서버 메시지/비밀 원문은 복사하지 않는다. 목록은 `data.agents`다.

## requestKey와 결과 불명

`.devlink-runtime/task-requests.json`의 기존 private schema 1을 유지한다. origin/user/requestId를 기준으로 팀·requester·receiver·prompt를 대조한다. **새 논리 요청에만 UUID를 한 번 발급**하고 중앙 호출 전에 비공개 권한·sync·원자 교체로 저장한다. 같은 입력의 같은 requestId는 재시작 뒤에도 같은 키를 사용한다. 다른 내용은 `REQUEST_KEY_CONFLICT`이며 동일 actor가 이미 알고 있는 키도 응답에 보존한다. 다른 팀/Agent의 키는 노출하지 않는다. 보관 상한은 기존 1,024건/64 MiB이며 자동 삭제하지 않는다.

HTTP 라이브러리/업무 adapter는 업무 요청을 자동 재시도하지 않는다. 호출자가 **명시적으로 같은 requestId·같은 입력을 다시 제출한 경우에만** 같은 키의 중앙 POST가 발생할 수 있다. 이때 중앙의 replayed와 기존 taskId를 그대로 반환한다. 새 requestId는 새 논리 요청이므로 응답 유실 후 새 requestId로 재호출하면 중복될 수 있다. MCP가 새 requestId로 자동 복구할 수 있다는 보장은 없으며 최종 UX/DTO는 -97 범위다. requestKey 조회 API를 추측해서 호출하지 않는다.

타임아웃·취소·프로토콜 오류·5xx·동시 insert의 `409 DUPLICATE_REQUEST`는 `outcome=unknown`이다. 실제 중앙이 같은 키·같은 내용 동시 생성에도 `409 REQUEST_KEY_CONFLICT`를 반환하므로 중앙에서 받은 해당 conflict도 code/409를 보존하면서 outcome은 unknown으로 둔다. 로컬 로그에서 확정한 내용 불일치만 rejected다. 자동 재조회·새 키 생성·업무 FAILED 전이·재할당은 하지 않는다. 유효하게 읽은 알려진 taskId와 requestKey는 오류에도 보존한다. 응답을 읽기 전에 끊기면 taskId를 알 수 없으므로 키만 보존한다. 거부된 응답의 HTTP/code 조합이 provider와 일치하지 않으면 거부로 추측하지 않고 프로토콜 오류/unknown으로 반환한다.

## envelope·오류·예산

공통 성공은 `{requestId, requestKey?, data}`, 실패는 `{requestId, requestKey?, error:{code,scope,outcome,taskId?}}`다. 중앙 `ApiResponse.success/data/code`를 해석하되 message와 비밀 원문을 전달하지 않는다. 중앙 401은 로컬 503 `CENTRAL_UNAUTHENTICATED`이며 local_auth로 바꾸지 않는다. 로컬 401은 localToken 실패 전용이다. 중앙 403은 domain `FORBIDDEN`이다. `RESOURCE_NOT_FOUND`는 리소스 종류를 추측해 다른 코드로 바꾸지 않는다.

로컬 전체 예산은 인증/본문 읽기부터 8초다. 중앙 호출 및 응답 본문 수신은 5초 이내이며 Agent 목록의 context+목록 전체도 5초다. 요청 128 KiB·중앙 응답 256 KiB 제한, 반환 envelope에도 크기 여유를 둔다. redirect/proxy/HTTP 자동 retry는 끈다. 브로커 종료 시 요청을 취소한다. `ACK_PENDING`, `ACK_RECEIVED`, `DELIVERY_NOT_SENT`, `DELIVERY_UNKNOWN`을 별도 전달 결과로 보존하며 ACK 이전 상태를 DELIVERED로 만들지 않는다.

## 검증 증거와 재현

`tests/task_creation.rs`는 실제 Rust HTTP와 **합성 중앙 provider**를 사용한다. 기존 생성 회귀에 Agent 확인/목록, Long 최대값, 위조 actor, metadata mismatch, 중앙 인증 거부, redirect, 본문 수신 타임아웃, 409 동시충돌, 해제 중 확인 응답, 원문 prompt와 키 보존 검증을 추가했다. `cli_broker`의 기존 실제 CLI 생성 fixture도 Agent 전용 provider 계약으로 갱신했다. fixture 통과는 중앙 정책 충족 증거가 아니다.

실제 중앙·PostgreSQL 재현은 `scripts/verify-business-relay.mjs`다. 격리된 loopback 중앙과 전용 DB/role `devlink_relay_test`만 사용한다.

```powershell
$env:DEVLINK_TEST_CENTRAL_URL = 'http://127.0.0.1:<isolated-port>/'
$env:DEVLINK_TEST_CLI = '<absolute-path-to-current-devlink.exe>'
$env:DEVLINK_TEST_PSQL = '<absolute-path-to-psql.exe>'
$env:DEVLINK_TEST_DB_PORT = '<isolated-db-port>'
node rust/scripts/verify-business-relay.mjs
```

두 사용자·다른 팀 사용자와 실제 브로커 두 개를 만들고 기존 Agent 확인 중 중복 생성 없음, 팀 목록, prompt/DB 저장, 수신함·중앙 ACK, 명시 같은 키 재요청, 권한 거부를 확인한다. 사용자 설정이나 전역 Host 설정은 수정하지 않는다. 생성된 합성 계정/데이터는 전용 임시 DB와 함께 정리한다. `passed`는 검증 절차가 관찰 결과와 일치했다는 뜻이며, provider 결함이 있으면 `storyComplete=false`와 blockers를 별도로 반환한다.

이번 실제 검증은 PostgreSQL 17.11 새 클러스터, 고정 중앙 02de434, 현재 Rust CLI를 사용했다. 중앙 main 소스 128개를 고정 snapshot과 로컬 중앙 소스에서 SHA-256 대조해 불일치 0을 확인했다. jar SHA-256은 `C9AFB406931BD624231C29D02350616F12C97C4BD21D7DD9AA76A591A6E5DA2D`다. 테스트 중앙에만 `ddl-auto=update`, Flyway 비활성화를 적용했으므로 운영 migration 검증은 아니다. ignored `rust/target/story93/`에 로그·소스 대조·재현 환경을 보관한다.

## 2026-10-30 최초 구현 검사 (Gemini 지적 전)

| 검사                                             | 결과                                                 |
| ------------------------------------------------ | ---------------------------------------------------- |
| 전체 Rust workspace/all-targets                  | 종료 0, 145 통과·실패 0·기존 MCP opt-in 2 ignored    |
| 최신 -93 HTTP 회귀                               | 14 통과·실패 0·위 opt-in 2 ignored                   |
| 포맷·전체 targets Clippy `-D warnings`           | 종료 0                                               |
| `git diff --check`, Node 재현 스크립트 문법 검사 | 종료 0                                               |
| 실제 중앙·PostgreSQL·Rust 직접 HTTP              | 절차 통과, storyComplete=false, provider blocker 2개 |
| 검증용 환경 정리                                 | 중앙/DB 종료·임시 DB 삭제, 종료 전 ONLINE 0          |

처음 샌드박스 HTTP 실행은 Windows 10013으로 실패해 승인된 외부 loopback/ACL 환경에서 재실행했다. 첫 전체 회귀에는 기존 CLI fixture가 이전 생성 경로/최소 context를 기대해 1건 실패했으며 fixture 수정 후 해당 테스트와 전체 회귀를 재검증했다. 마지막 중앙 REQUEST_KEY_CONFLICT의 unknown 매핑은 전체 회귀 빌드가 시작된 뒤 보완했으므로 최신 HTTP 14개에서 별도로 확인했다. 실제 중앙 검증도 이 보수적인 마지막 409 분기 보완 이전에 수행했으며, 실제 Rust가 중앙 409를 받은 재현 증거로 부풀리지 않는다. 로그는 ignored rust/target/story93* 아래에 보관한다. MCP opt-in/Host는 -93 직접 HTTP 검증의 선행으로 강제하지 않았다.

## Gemini 지적 후속 검사

사용자가 전달한 P2 두 건을 수정 전 실제 Rust HTTP 회귀에서 재현했다. 동시 내용 충돌은 저장소 잠금 안에서 동일 actor의 requestKey를 오류와 함께 반환하고, register_agent는 로컬 toolType 형식 검사 후 중앙 신원·세션 재검증을 메타데이터 비교보다 먼저 수행한다. `known_key` 선조회는 제거했다. 동일 actor의 키 보존과 등록 신원 검증 순서를 회귀로 확인했다.

수정 후 라이브러리 42개·세션 검증 6개·업무 HTTP 18개, 합계 66개가 통과했다. 기존 MCP opt-in 2개는 미실행이며 이전 전체 145개 결과와 합산하지 않는다. 같은 actor의 충돌/저장 오류 키 보존, 타 팀·Agent의 키 비노출, 다른 READY Agent, 응답 대기 중 해제·교체, 잘못된 toolType의 중앙 호출 0회를 검증했다. 실제 CLI 생성·재시작 키 재사용 회귀 1개도 별도로 통과했다. 전체 targets Clippy `-D warnings`·포맷·diff 검사도 통과했다. Clippy 초기 불필요한 closure 차용 지적은 제거 후 재검증했다.

이번 후속에서 전체 workspace 및 실제 중앙/DB/Host를 다시 실행했다고 주장하지 않는다. 기존 실제 중앙 provider blocker는 유지한다. 사용자가 제공한 후속 Gemini 재검토는 신규 결함 0건이었다. 이 기록을 실제 중앙/DB 재실행으로 계산하지 않는다.

## 남은 수용 조건

- -67/-71: 오프라인 수신자 저장 전 거부. 현재 고정 provider의 실제 미충족을 기록했다.
- -68: 실제 동일 키·동일 입력 동시 생성에서 한 실행은 201/200, 최종 실행은 409 REQUEST_KEY_CONFLICT/201로 갈렸다. DB 작업은 1건이었다. 최종 provider 정책·회귀가 필요하며 Rust는 409를 임의 성공으로 바꾸지 않는다.
- 초기 Gemini 지적 2건은 보완·재검토되었다. 공식 경로 호환의 별도 검토와 실행은 아래 기록을 따른다.
- -94 나머지 네 업무, -95 CLI 수신함 조회, -96 알림, -97 MCP 최종 DTO, -52/-66 교차 기능·Host A→B/B→A는 이 구현의 완료 범위가 아니다.

## 공식 생성 경로·REJECTED 호환 검증

기준 client `8199ba7`→`4188973`, 실제 중앙 `a5c7987e1253a8a17912d4ea272ff105f4f4a658`의 2026-10-30 실행이다. 초기 중앙 `02de434`와 구분한다.

삭제된 /api/v1/agent/tasks 호출은 공식 경로 fixture에서 생성 실패로 재현되었다. URL만 바꾼 중간 상태에서는 거절된 기존 작업의 같은 키 재접수가 502 CENTRAL_PROTOCOL_ERROR로 실패했다. 검증한 teamId를 경로에 사용하고 REJECTED를 정상 상태로 보존하도록 수정했다. actor ID 본문·자동 retry·redirect·구 경로 fallback·ACK 규격은 변경하지 않았다.

| 검증 | 당시 결과 |
| --- | --- |
| 전체 Rust workspace/all-targets | 159 passed / 0 failed / 2 ignored |
| 추가 HTTP 회귀 | 20 passed / 0 failed / 2 ignored. 앞선 전체 집계와 합산하지 않음. 중앙 팀 불일치 시 POST·중앙 저장 0건 포함 |
| MCP | 빌드·계약 export·100건 통과 |
| build·fmt·Clippy | workspace 빌드·전체 포맷·all-targets -D warnings 통과. Windows CLI 파일 잠금 실패 후 순차 빌드로 완료 |
| 실제 중앙·DB | 생성·prompt 원문·수신함/ACK·같은 키·내용 충돌·권한 거부·거절 후 재접수·작업 1건 유지. 같은 키 동시 요청은 이번 실행에서 201/200 |
| 실제 수신함 | 16건 ACK + 재시작 후 1건 ACK, 기록 실패 ACK 0건·DELIVERY_UNKNOWN. 자동 수락 없음 |

Java 21·전용 loopback PostgreSQL 17.11, Flyway V1~V4·Hibernate validate를 유지했다. ONLINE Agent 0건·migration 4건을 확인한 뒤 임시 중앙·DB·브로커를 종료했다. private 로그는 ignored .planning/task-create93에 보관한다. 이 실행의 passed=true는 절차 일치이며 오프라인 저장 전 거부 차이로 storyComplete=false를 유지한다. 한 번의 동시 성공으로 -68 전체 수용을 완료하지 않는다.

당시 기록된 Claude Code CLI 2.1.310, claude-opus-5-5, effort high 교차검토는 팀 불일치 회귀·teamId 설명 2건을 반영한 뒤 13:53 KST 최종 no actionable findings, exit 0이었다. 최종 prompt SHA-256은 906FD5B7205209C9FA6696FCE0E63A66798AED8662D40A58BC9859E454CF5EEA다. 이 문서 통합에서 검토를 재실행한 것은 아니다.

PR #26은 -94가 병합된 develop a3fb511과 통합하면서 request_http·PATCH/조회/완료 확장과 위 REJECTED·팀 검증·추가 회귀를 함께 보존했다. 위 159건·20건·실제 중앙 결과는 원래 4188973의 증거이며 충돌 통합본의 재실행으로 취급하지 않는다.

재현은 위 verify-business-relay.mjs와 [검증 명령](verification.md)을 사용한다. 기존 MCP opt-in·실제 Host·최종 코드 수정 왕복은 해당 결과에 포함하지 않았다.
