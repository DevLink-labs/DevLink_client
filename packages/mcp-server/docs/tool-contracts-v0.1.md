# 도구 계약·확장 예제 v0.1 (S15P31D108-61)

2026-10-30 S15P31D108-97 갱신. Rust 중계 및 중앙 server develop `0e8c2b7` 계약을 따른다. Notion의 빈 요청·응답 템플릿과 구분하며, 중앙 업무 규칙과 저장 처리는 중앙이 결정한다. 실제 연결 검증은 [중앙 왕복 검증](central-roundtrip.md)에 기록한다.

## 공개 도구와 입력

`devlink_ping` 진단과 아래 11개 업무 도구를 등록한다. 별도 activate 도구는 없다. 모든 입력 객체는 strict다. 호출자의 userId/teamId/agentId/sessionId/context 필드를 허용하지 않는다. `recipientAgentId`는 작업 대상이며 발신 actor가 아니다. 브로커가 인증한 세션에서 context를 주입한다.

아래 객체의 각 값은 해당 도구의 `arguments` 예제다.

```json
{
  "register_agent": {"name":"MCP worker","toolType":"codex"},
  "list_agents": {},
  "update_agent": {"projectName":"DevLink client","roleDescription":"MCP 담당"},
  "create_task": {"recipientAgentId":"202","title":"인터페이스 검토","description":"계약 초안을 확인해주세요","taskContext":"관련 Jira: S15P31D108-60"},
  "update_task_status": {"taskId":"101","version":"1","status":"FAILED","failureReason":"의존 API 미제공","partiallyModified":false},
  "complete_task": {"taskId":"101","version":"1","resultText":"구현 완료\n검증: npm test 통과","taskFiles":[{"relativePath":"src/example.ts","changeType":"MODIFIED","diffText":"- old\n+ new"}]},
  "get_tasks": {"direction":"received","status":"PENDING","limit":20},
  "get_task_result": {"taskId":"101"},
  "get_updates": {"limit":20},
  "ask_question": {"taskId":"101","message":"로그인 API는 v2 기준인가요?"},
  "answer_question": {"taskId":"101","questionEventId":"501","message":"네, v2 기준입니다."}
}
```

| 도구 | 입력 규칙 |
| --- | --- |
| register_agent | `{}`로 CLI가 준비한 기존 Agent 확인·bind. name(1–100자), toolType(`codex / claude_code`), role(1–500자)은 각각 선택이며 전달 시 중앙 정보와 정확히 일치해야 함. 중앙 생성·수정·토큰 회전 없음 |
| list_agents | 빈 객체 |
| update_agent | 선택 projectName(1–120 UTF-16 코드 단위)·roleDescription(1–255) 중 하나 이상. 공백만인 값은 거부하며 역할 비우기는 지원하지 않는다. 현재 세션의 검증된 Agent만 바꾸고 name·toolType·소유자·팀은 바꾸지 않는다. 제어 문자는 받지 않는다(중앙 `isBlank`가 일부를 공백으로 보아 역할이 지워질 수 있음). 중앙 값과 다른 필드만 PATCH하며, 모두 같으면 PATCH를 보내지 않고 `updated:false`를 돌려준다. Agent 생성·토큰 발급·회전 없음(`credentialsRotated:false`). 중앙 PATCH가 사용자 로그인 세션만 받으므로 이 PC의 `devlink auth login` 세션을 쓴다. 없거나 만료되면 USER_LOGIN_REQUIRED(S15P31D108-146) |
| create_task | recipientAgentId 1–128자, title 1–200자, description 1–16000자, 선택 taskContext 1–16000자, 선택 requestKey(공백 없는 식별자, 최대 64 UTF-16 코드 단위). requestKey가 없으면 호출마다 새 요청이다. 이전 결과·오류의 requestKey나 미리 정한 키를 같은 인자와 함께 넘기면 같은 작업을 돌려받고, 같은 키에 다른 인자를 넣으면 REQUEST_KEY_CONFLICT로 거부된다(S15P31D108-165) |
| update_task_status | taskId, version, status `IN_PROGRESS / FAILED / REJECTED` 필수. FAILED에는 failureReason·partiallyModified 필수. REJECTED에는 rejectionReason 필수이며 실패 필드를 넣지 않음(중앙은 PENDING 작업만 거절 허용). IN_PROGRESS에는 실패 필드·rejectionReason을 넣지 않음. 거절은 사용자가 명시적으로 거절을 결정했을 때만 사용. 선택 requestKey는 알려진 상관 키이며 멱등 키가 아님 |
| complete_task | taskId·version·resultText 필수. 선택 taskFiles 배열/null: relativePath(최대 500 UTF-16 코드 단위)·changeType(ADDED/MODIFIED/DELETED/RENAMED), 선택 diffText(문자열/null). 선택 requestKey |
| get_tasks | direction `sent / received`(기본 received), 선택 status `PENDING / IN_PROGRESS / COMPLETED / FAILED / REJECTED`, limit 1–100(기본 20), 선택 cursor 1–512자. 응답 items·nextCursor 사용 |
| get_task_result | taskId 1–128자, 선택 requestKey. 현재 version·원문 prompt·resultText·taskFiles·events 보존. events의 eventType·message·createdAt에 더해 중앙이 보낸 eventId·sequence·authorAgent·questionEventId도 보존한다. 질문에 답할 때는 이 eventId를 쓴다 |
| ask_question | taskId, message(공백만 불가, 최대 8,000 코드포인트, NUL 불가) 필수. 선택 requestKey(공백 없는 식별자, 최대 64 UTF-16 코드 단위). 키가 없으면 MCP가 UUID를 발급해 브로커에 보내고, 응답·오류의 requestKey로 돌려준다. 같은 키와 같은 본문으로 다시 호출하면 같은 eventId를 받는다(`replayed: true`). 같은 키에 다른 본문은 REQUEST_KEY_CONFLICT. 중앙은 IN_PROGRESS Task의 참여자만 받으며, 요청자와 수신자가 같은 Agent면 TASK_CONVERSATION_UNAVAILABLE이다(S15P31D108-166) |
| answer_question | taskId, questionEventId(정규 양의 Long 십진 문자열), message 필수. 선택 requestKey는 ask_question과 같다. questionEventId는 같은 Task에서 상대가 남긴 QUESTION의 eventId여야 하며, 아니면 INVALID_QUESTION_REFERENCE다. 답변은 Task 상태를 바꾸지 않는다 |
| get_updates | 선택 cursor 1–512자(이전 `nextCursor`), limit 1–50(기본 20). 브로커가 저장한 새 수신 작업, 보낸 작업의 상태·완료 알림, 질문·답변 도착, 보낸 작업 지연 알림(`TASK_OVERDUE`, 로컬 전용·작업 취소 아님, S15P31D108-177)의 ID·상태만 반환. 읽기 전용이며 수락·ACK·읽음 처리 없음. `coverage: local`, `cursorReset`은 로컬 이력 재시작. 응답은 [updates schema](../../../contracts/local-broker/v1/updates.schema.json)로 검증하고 위반 시 BROKER_PROTOCOL_ERROR. 저장소 오류는 LOCAL_UPDATES_UNAVAILABLE([Rust 계약](../../../rust/docs/local-updates.md)) |

업무 텍스트는 공백·개행을 보존하고 공백만 있는 입력을 거부한다. ID는 중앙에서 받은 양의 십진 문자열을 사용한다. version은 0..9223372036854775807의 정규 십진 문자열 또는 JS 안전 정수이며 출력은 문자열로 유지한다. 변경 전 상세에서 현재 version을 읽고 추측·자동 재전송하지 않는다. JSON Schema는 `tools/list`로 제공하고 조건부 FAILED 입력도 실행 전 검증한다. 전체 요청은 [HTTP 계약](broker-contract-v0.1.md)의 128KiB 제한도 적용한다.

중앙에 별도 verification 필드는 없다. 요약·실제 검증 명령/결과·미실행 항목을 resultText에 명시한다. 구형 result.summary/changedFiles/verification 입력은 거부하며 파일 변경 유형이나 검증 결과를 추측해 변환하지 않는다. TASK_VERSION_CONFLICT는 rejected, CENTRAL_PROTOCOL_ERROR/CENTRAL_RESULT_UNKNOWN은 unknown으로 알려진 taskId/requestKey를 보존한다.

## 공통 호출·응답

SDK 입력 검증 → 공통 등록 콜백 → 지연 세션 준비/신뢰 context 주입 → 명시적 HTTP 경로 → 공통 출력 검증/매핑 순서다. 각 도구 핸들러는 HTTP·토큰·세션을 직접 다루지 않는다. taskId/status/recipient/delivery 등 성공 data의 확장 필드를 그대로 보존한다.

```json
{"requestId":"UUID","data":{"taskId":"task-1","status":"PENDING","delivery":{"status":"FAILED"}}}
```

업무 실행 실패는 `isError: true`와 다음 structuredContent를 반환한다. content[0].text에도 같은 JSON을 넣는다. SDK의 입력 거부는 해당 프로토콜의 표준 validation 오류 경로를 사용하며 브로커에 요청하지 않는다.

```json
{"error":{"code":"BROKER_TIMEOUT","message":"Local broker response timed out; the business outcome is unknown.","requestId":"UUID","outcome":"unknown"}}
```

알려진 taskId는 오류에 보존한다. HTTP 원문 오류와 임의 예외 메시지는 반환하지 않는다. 타임아웃·취소는 중앙 작업 취소/실패를 뜻하지 않으며 업무를 자동 재전송하지 않는다. list/get은 작업을 수락하거나 상태를 변경하지 않는다.

## 모듈 추가 방법

`src/examples/echo.ts`는 공통 등록 구조를 사용하는 실행 가능한 예제다. 기본 공개 목록에는 넣지 않는다. 새 모듈은 다음 형태로 작성한다.

```typescript
import { z } from 'zod';
import { defineTool } from '../tools/registry.js';

export const echoExample = defineTool({
  name: 'example_echo',
  description: 'Return a short message without external I/O.',
  inputSchema: z.strictObject({ message: z.string().min(1).max(200) }),
  outputSchema: z.strictObject({ message: z.string() }),
  annotations: { readOnlyHint: true, destructiveHint: false, idempotentHint: true, openWorldHint: false },
  handler: async ({ message }) => ({ message }),
});
```

구성 지점에서 `createServer(logger, broker, [echoExample])`로 추가한다. 등록 코어를 수정하지 않는다. 모듈은 name·description·inputSchema·outputSchema·annotations·handler만 정의한다. 공통 함수는 중복 이름을 사전 거부하고 정상 결과를 검증하며 안전한 공통 오류로 변환한다. 업무 예제는 `businessTools(broker)`의 명시적 operation과 스키마 정의를 참고한다.

## 검증 및 남은 확인

- 가짜 브로커로 7개 도구의 성공 경로와 신뢰 context·requestId를 확인했다.
- 모든 도구의 잘못된 입력·actor 덮어쓰기가 네트워크 전 거부되는지 확인했다.
- 공통 실행 오류, 예제 추가, 출력 검증 실패 시 내부 정보 비노출을 확인했다.
- legacy·2026-07-28의 stdio 자식 프로세스에서 개인 설정 파일을 거쳐 7개 도구를 호출했다.
- 브로커 없이 진단 사용, 업무 처리 중 EOF로 타이머·요청 정리를 확인했다.

실제 SDK·Rust·중앙의 두 Agent 왕복과 실제 LLM Host의 자율 코드 작성은 구분한다. 재현 방법과 실행 결과는 [중앙 왕복 검증](central-roundtrip.md)을 따른다.
