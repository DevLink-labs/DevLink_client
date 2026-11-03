# -94 작업 상태·완료·목록·결과 중계

2026-10-30 구현·검증 기록. Rust 기준 `8199ba7` 대비 -94 구현(`1264566`, PR #25)의 계약을 설명한다. 당시 중앙은 `SSAFY-URAYAHA/DevLink_server`의 `a5c7987e1253a8a17912d4ea272ff105f4f4a658` 소스와 실행 JAR로 고정했다. 로컬 형제 서버 폴더의 과거 소스와 구분한다.

## 구현 범위

| 로컬 POST 경로 | operation | 실제 중앙 요청 |
| --- | --- | --- |
| `/v1/tasks/status` | `update_task_status` | `PATCH /api/v1/tasks/{taskId}/status` |
| `/v1/tasks/complete` | `complete_task` | `POST /api/v1/tasks/{taskId}/complete` |
| `/v1/tasks/list` | `get_tasks` | `GET /api/v1/teams/{teamId}/tasks` |
| `/v1/tasks/result` | `get_task_result` | `GET /api/v1/tasks/{taskId}` |

`task_operations.rs`는 provider DTO/변환, `task_routes.rs`는 세션·예산·HTTP 연결을 담당한다. -93의 TaskClient·Agent Bearer·세션 principal·공통 envelope를 재사용한다. 소유권·상태 전이·상태/결과/파일/이력/outbox 저장은 중앙이 결정한다. Rust에 업무 상태 저장소나 로컬 파일 접근을 추가하지 않는다.

최신 중앙의 생성 경로는 과거 `/api/v1/agent/tasks`에서 **`POST /api/v1/tasks/{teamId}`**로 변경되었다. 기존 생성 클라이언트·fixture·-93/-95 검증 스크립트 URL을 함께 갱신했다. actor는 query/body가 아닌 Bearer에서 추출한다. 수신함 저장·ACK 구현은 수정하지 않았다.

## 공통 계약

요청은 -93과 같은 `{requestId, operation, arguments, context, mcpRequestId?}`이다. context의 sessionId/userId/teamId/agentId/toolType?를 바인딩된 유효 세션과 대조하고 중앙 context로 자격을 확인한 뒤 세션을 재검사한다. arguments의 actor 필드는 거부한다. 조회는 중앙 응답 후에도 세션을 검사해 해제·교체된 소유자에게 데이터를 반환하지 않는다. 이미 전송한 변경 요청의 확정 응답은 그 사이 세션이 해제되어도 보존한다.

로컬 인증은 MCP용 localToken, 중앙 인증은 별도 Agent Bearer다. Origin 요청을 거부한다. 기존 요청 128 KiB·중앙 응답 256 KiB·로컬 전체 8초·중앙 호출별 본문 포함 5초 예산을 사용한다. 새 경로의 context 확인과 업무 호출은 합쳐 로컬 8초 안에 끝난다. redirect/proxy/자동 HTTP retry를 끄고 중앙 message·추가 필드를 무작정 복사하지 않는다.

성공은 `{requestId, requestKey?, data}`, 실패는 `{requestId, requestKey?, error:{code,scope,outcome,taskId?}}`이다. 유효하게 파싱한 알려진 taskId를 오류에도 남긴다. status/complete/result의 선택적 requestKey는 **이미 알려진 상관 정보**로만 에코한다(비어 있지 않은 최대 64 UTF-16 코드 단위). 중앙에 보내거나 새로 발급·저장하지 않으며 변경 업무의 멱등 키가 아니다. 구문 자체가 잘못되어 공통 파싱에 실패한 요청은 기존 validation envelope를 따른다.

ID는 정규 양의 Java Long 범위 문자열이다. version은 필수이며 0..9223372036854775807의 정수 또는 정규 십진 문자열을 받는다. 중앙에는 정수로 보내고 로컬 ID·version은 문자열로 반환해 JS 정밀도 손실을 피한다. 현재 결과 조회의 version을 명시적으로 읽고 다음 변경에 제출한다. Rust에서 version을 추측하거나 충돌 뒤 자동 재조회·재전송하지 않는다.

## arguments와 응답

예제는 공통 envelope의 arguments 부분이다. 알 수 없는 입력 필드는 거부한다.

수락:

```json
{"taskId":"101","version":"0","status":"IN_PROGRESS","requestKey":"known-create-key"}
```

실패:

```json
{"taskId":"101","version":"1","status":"FAILED","failureReason":"검증 실패 원문","partiallyModified":false}
```

reason은 failureReason의 호환 별칭이다. FAILED에는 비어 있지 않은 사유와 명시적 boolean이 필요하다. 수락에는 사유·partiallyModified를 넣지 않는다. 이 adapter는 수락/실패만 변경하며 중앙의 REJECTED는 목록·상세에서 보존한다. 응답은 taskId/status/version/rejectionReason/failureReason/partiallyModified/startedAt/completedAt이다.

완료:

```json
{
  "taskId":"101",
  "version":"1",
  "resultText":"요약\n검증: cargo test 통과",
  "taskFiles":[
    {"relativePath":"src/main.rs","changeType":"MODIFIED","diffText":"- old\n+ new"}
  ]
}
```

resultText는 비어 있지 않아야 하며 공백·개행을 보존한다. changeType은 ADDED/MODIFIED/DELETED/RENAMED, relativePath는 비어 있지 않은 최대 500 UTF-16 코드 단위다. diffText의 문자열/null을 보존한다. taskFiles 생략/null/빈 배열은 중앙 계약대로 빈 배열이다. 실제 로컬 파일 존재 여부나 diff 내용은 검사·수정하지 않는다. 응답은 taskId/status/version/completedAt이다.

목록:

```json
{"direction":"received","status":"FAILED","limit":20}
```

direction은 sent/received(기본 received), status는 다섯 상태 중 선택값이다. 중앙 scope는 mine으로 고정한다. page(기본 0, 최대 Java Integer), size(기본 20, 1..100) 또는 limit/cursor를 지원한다. size와 limit을 함께 쓰거나 cursor와 page/size를 함께 쓰면 거부한다. cursor 조회에는 같은 direction/status/limit을 다시 보낸다. cursor는 origin·user·team·agent·필터·크기에 묶인 페이지 표식이며 인증 자격이나 일관된 스냅샷을 보장하지 않는다.

items는 taskId/teamId/requesterAgent/receiverAgent/status/createdAt/startedAt/completedAt을 보존한다. page/size/totalElements/totalPages/hasNext를 유지하고 nextCursor를 추가한다(마지막은 null). actor·필터와 맞지 않는 provider 목록은 성공으로 내보내지 않는다.

상세·결과:

```json
{"taskId":"101","requestKey":"known-create-key"}
```

응답은 taskId/teamId/requesterAgent/receiverAgent/prompt/status/version/resultText/failureReason/rejectionReason/partiallyModified/taskFiles/events/createdAt/startedAt/completedAt이다. 파일의 fileId·relativePath·changeType·diffText, 이벤트의 eventType·message·createdAt을 보존한다. 아직 미완료인 작업도 중앙 상세 그대로 조회한다. 팀 접근 허용 여부는 중앙 조회 정책을 따른다.

## 오류·결과 불명

중앙 409 TASK_VERSION_CONFLICT/TASK_STATE_CONFLICT는 code/409를 보존한 rejected다. 중앙 403은 domain FORBIDDEN, 중앙 401은 로컬 503 CENTRAL_UNAUTHENTICATED이며 로컬 자격 만료로 바꾸지 않는다. timeout/취소/전송 실패/5xx/잘못된 성공 DTO는 unknown이고 알려진 taskId/키를 보존한다. 자동 FAILED 전이·완료 재전송·409 보정·새 키 발급을 하지 않는다. 결과 불명 후 호출자가 기존 result 경로로 명시적으로 확인한다.

## 검증 증거

- `tests/task_operations.rs`: 실제 Rust HTTP + 합성 중앙 provider. 정상 매핑, Long 정밀도, actor 위조·미바인딩·기존 DTO의 호출 전 거부, 필터/cursor, 파일 기본값·길이, 중앙 인증/권한/버전/상태/404/5xx/redirect/거대 본문/잘못된 DTO, 헤더·본문 timeout, 세션 해제, 전송 중 취소와 자동 재전송 부재를 검증한다.
- `scripts/verify-task-operations.mjs`: 실제 Rust CLI/브로커 HTTP → 고정 Spring → PostgreSQL 17.11. 합성 사용자 3명·Agent 3개·두 팀으로 수신자 소유권, 수락, stale version·상태충돌, 완료·반복 완료 거부, 양측 결과, FAILED 사유/partiallyModified, sent/received/필터/페이지, 타 팀 거부, 위조 context, 원문·키 보존을 확인했다. DB에서 파일 2건·상태 이력 2건·상태/완료 outbox 2건과 resultText 원문을 대조했다.
- 실제 결과: `target/story94/actual-central.log` passed=true. 고정 소스 bootJar, 전용 DB Hibernate ddl-auto=update/Flyway 비활성화다. 운영 설정·migration 및 rollback 장애 주입은 포함하지 않는다. 중앙 transaction 보장은 -69/-75/-76/-77/-70에서 검증한다.
- 전체 Rust 순차 회귀는 164 passed/0 failed/2 ignored이고, 이후 보강한 최신 중계 회귀 7개도 통과했다. fmt·clippy·diff-check를 통과했다. Windows 실행 증거이며 Ubuntu 실행은 이번 범위에서 검증하지 않았다.
- 로그·파일 해시는 `target/story94`에 보관한다. Windows 샌드박스 권한 제한 실패는 정상 권한 실행과 구분한다. 기존 WebSocket 타이밍 시험은 병렬 실행 중 1회 실패했고 전체 순차 재실행에서 통과했다.

재현은 기동된 **격리 loopback 중앙·전용 DB/role devlink_relay_test**를 사용한다.

```powershell
$env:DEVLINK_TEST_CENTRAL_URL = 'http://127.0.0.1:<isolated-port>/'
$env:DEVLINK_TEST_CENTRAL_COMMIT = 'a5c7987e1253a8a17912d4ea272ff105f4f4a658'
$env:DEVLINK_TEST_CLI = '<absolute-path>/devlink.exe'
$env:DEVLINK_TEST_PSQL = '<absolute-path>/psql.exe'
$env:DEVLINK_TEST_DB_PORT = '<isolated-db-port>'
node rust/scripts/verify-task-operations.mjs
```

## 남은 조건·범위 구분

네 경로는 실제 provider까지 검증했지만 **전체 스토리 최종 완료 선언은 보류**한다.

1. 중앙은 구조화된 verification 필드 없이 resultText와 taskFiles만 저장한다. 검증 문자열을 resultText에 넣으면 원문이 유지되지만 별도 검증 DTO 왕복은 증명하지 않았다. 현재 MCP의 result:{summary,changedFiles,verification?} 및 version 없는 status/complete 입력은 조용히 축약하거나 version을 추측하지 않고 거부한다. 구조화 DTO가 필요하면 중앙 담당자와 계약을 확정해야 한다.
2. 최종 MCP DTO/소비자 변경은 -97, 실제 Codex/Claude Host 왕복은 -66, 전체 통합 스위트는 -52다. 이번 시험은 MCP/Host를 사용하지 않는다.
3. 사용자 요청으로 Claude 대신 실행한 **Gemini 교차검토는 신규 결함 0건으로 완료**했다. 결과 수신 시 검토 manifest의 15개 파일 해시가 모두 일치함을 확인했다. 해당 판정은 사용자 제공 보고이며 기록 시 Gemini나 시험을 재실행한 것은 아니다. Windows·임시 DB 결과와 실제 Host·운영 migration 미검증 범위를 유지한다.
4. 과거 고정 중앙 02de434의 -67/-71 오프라인 저장 전 거부 및 -68 동일 키 동시 생성 정책은 -93 의존성으로 유지한다. 이번 시험에서 두 정책을 재검증·수정하지 않았다. 최신 중앙에서 해결됐다고 추정하지 않는다.

현재 구현·남은 검증은 [진행 현황](../PROJECT_STATUS.md)에서 확인한다. 이 문서 정리로 일정·포인트·스프린트·외부 이슈 상태를 변경하지 않는다.
