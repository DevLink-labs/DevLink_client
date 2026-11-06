# 공통 오류·시간 제한 계약 (S15P31D108-63)

작성일: 2026-10-28, 갱신일: 2026-10-30 (S15P31D108-97, Asia/Seoul). 실제 Rust·중앙의 오류 코드·알려진 requestKey 보존과 SDK 입력 계약을 반영한다. [실제 중앙 왕복 검증](central-roundtrip.md)과 합성 오류 주입 시험의 범위를 구분한다.

## 오류 경계와 원본 요청

SDK 2.4.0이 처리하는 프로토콜 오류와 Tool 실행 오류를 구분한다. 존재하지 않는 도구는 JSON-RPC 오류(-32602)이며 SDK가 원본 wire id를 유지한다. 도구 인자 스키마 검증은 SDK가 콜백 전에 수행하고, 현재 SDK는 이유가 있는 `isError` Tool 결과를 반환한다. 이 경로를 내부 broker 요청이 생긴 것처럼 꾸미거나 SDK 메시지를 새 HTTP 요청으로 변환하지 않는다. 표준 SDK의 입력 검증 결과에는 우리 실행 오류의 structuredContent가 없다.

콜백 실행 이후에는 공통 오류를 text와 structuredContent에 동일하게 반환한다. 서버 원문 message·stack·details는 복사하지 않고 허용 코드별 고정 문구를 쓴다.

```json
{
  "error": {
    "code": "TASK_STATE_CONFLICT",
    "category": "domain",
    "message": "The task does not allow this state transition.",
    "mcpRequestId": 27,
    "requestId": "mcp-call-uuid",
    "requestKey": "broker-issued-key",
    "taskId": "known-task",
    "outcome": "rejected"
  }
}
```

| 식별자 | 의미 |
| --- | --- |
| mcpRequestId | SDK가 받은 원본 JSON-RPC id. 숫자 0과 문자열도 그대로 유지하며 로그에는 기록하지 않음 |
| requestId | MCP 호출의 로컬 추적 UUID. 업무 HTTP를 전송하면 그 요청의 requestId와 같음. 사전 실패에서는 전송을 했다는 뜻이 아님 |
| requestKey | 현재 호출의 envelope/error에 있는 브로커 키. 두 메타데이터 위치의 키는 일치해야 함. create_task/register_agent에서만 메타데이터가 없을 때 data의 키를 사용. create_task에 호출자가 넘긴 requestKey는 응답 전 timeout·취소·연결 종료에도 오류에 남는다 |
| taskId | 서버에서 받은 작업 ID 또는 조회·변경 입력에 이미 있던 대상 ID. 후자는 새 작업이 저장됐다는 증거가 아님 |

세션·bind의 내부 requestId와 requestKey를 업무 호출의 키로 바꾸지 않는다. 중앙 등록 성공 이후 bind만 실패하면 확인된 등록 requestKey를 유지한다. 응답 자체가 오지 않아 모르는 키/작업 ID는 만들지 않는다. 성공 data와 브로커가 돌려준 기존 중앙 결과도 그대로 보존한다.

기존 task data의 requestKey는 생성 당시 키일 수 있다. 조회·상태 변경·완료 결과의 data.requestKey를 현재 호출 키로 승격하거나 envelope 키와 비교하지 않는다. data 자체는 수정하지 않는다. 출력 스키마 검증만 실패해도 이미 완료된 호출의 안전한 requestId/requestKey/taskId는 오류에서 보존한다.

서버가 비성공 HTTP 상태에 data를 붙이는 불일치 응답을 보내도 도착한 taskId/requestKey는 안전하게 보존하고 outcome은 unknown으로 둔다. 이때 임의의 data/debug 본문을 오류 결과에 복사하거나 성공으로 단정하지 않는다.

## Wire envelope와 허용 코드

최상위는 requestId, 선택 requestKey, 그리고 data/error 중 정확히 하나다. error의 message·추가 디버그 필드는 검증 결과에서 제거한다. 본문은 최대 256 KiB, 요청은 128 KiB다. 일치하지 않는 requestId, 서로 충돌하는 envelope/error 메타데이터 키, 잘못된 JSON/식별자·동시에 data/error가 있는 응답은 BROKER_PROTOCOL_ERROR이며 결과는 unknown이다.

```json
{
  "requestId": "same-http-request-id",
  "requestKey": "broker-issued-key",
  "error": {
    "scope": "domain",
    "code": "TASK_SAVE_FAILED",
    "outcome": "rejected"
  }
}
```

| 분류 | Wire 코드/조건 | MCP 의미 |
| --- | --- | --- |
| local_auth | HTTP 401 | BROKER_UNAUTHORIZED, 본문이 JSON이 아니어도 자격 거부로 세션 중단 |
| local_auth | 명시적 local_auth 거부 | LOCAL_AUTH_FORBIDDEN, 세션 중단 |
| session | SESSION_REPLACED / SESSION_CONFLICT | 다른 소유자가 있으므로 자동 복구 중단 |
| session | SESSION_EXPIRED / AGENT_UNREGISTERED | 만료 또는 등록 필요를 구분. 만료만 제한적으로 세션 복구 |
| domain | FORBIDDEN / BROKER_FORBIDDEN | 업무 경로의 권한 거부만으로 다른 업무 호출·heartbeat를 중단하지 않음 |
| domain | CENTRAL_UNAUTHENTICATED | 중앙 인증 필요. 로컬 토큰 거부와 구분하고 Rust의 인증 복원을 기다릴 수 있음 |
| domain | VALIDATION_FAILED / TASK_NOT_FOUND / TASK_STATE_CONFLICT / TASK_VERSION_CONFLICT | 업무 검증·대상·상태·버전 오류를 구분. 충돌 뒤 자동 재전송하지 않음 |
| domain | RESOURCE_NOT_FOUND / REQUEST_KEY_CONFLICT / BROKER_STORAGE_FAILED | Rust의 원본 코드와 알려진 taskId/requestKey 및 outcome을 보존. 중앙 키 충돌의 unknown과 로컬 사전 저장 실패의 rejected를 구분 |
| domain | RECEIVER_NOT_FOUND / RECEIVER_OFFLINE | 저장 전 수신자 거부. 새 작업이 저장된 것으로 표시하지 않으며 오류의 taskId는 제거 |
| domain | TASK_SAVE_FAILED | 저장 실패 보고. 5xx에서는 명시적 rejected가 없으면 unknown |
| domain | TASK_CONVERSATION_UNAVAILABLE / INVALID_QUESTION_REFERENCE | 질문·답변 규칙 위반(S15P31D108-166). 요청자와 수신자가 같은 Agent인 Task, 또는 같은 Task의 상대 질문이 아닌 questionEventId. rejected이며 다른 ID를 추측해 재시도하지 않음 |
| transport | CENTRAL_UNAVAILABLE / CENTRAL_TIMEOUT | 중앙 연결 오류. unknown이며 로컬 브로커 세션 자체의 장애로 치환하지 않음 |
| transport | CENTRAL_PROTOCOL_ERROR / CENTRAL_RESULT_UNKNOWN | 중앙 응답 계약 위반·확정되지 않은 결과. unknown과 알려진 taskId/requestKey 보존 |
| local transport | BROKER_UNAVAILABLE / BROKER_CONNECTION_CLOSED / BROKER_TIMEOUT | 미기동·연결 중단·시간 초과 구분. 결과 unknown, 세션 복구만 가능 |

scope 없는 기존 세션 코드도 지원한다. 업무 경로의 분류되지 않은 403은 안전한 BROKER_FORBIDDEN이며 특정 업무 거부만으로 세션을 영구 중단하지 않는다. /v1/sessions 및 그 하위 claim/heartbeat/bind/delete의 미분류 403은 scope가 없어도 LOCAL_AUTH_FORBIDDEN으로 중단한다. 다만 requestId·본문 검증을 통과한 알려진 session 코드는 HTTP 403에서도 원래 사유를 보존한다. SESSION_EXPIRED는 복구하고 CONFLICT/REPLACED는 소유권 사유로 중단한다. 원문 status 매핑은 Rust와 대조할 항목이다. 업무 경로에서의 정확한 로컬 자격 거부에는 local_auth scope를 제공해야 한다. 401은 로컬 HTTP 자격 거부 전용으로 쓰고 중앙 인증 실패는 domain 오류로 전달하는 초안이다.

알 수 없는 5xx/2xx 오류 코드는 BROKER_SERVER_ERROR와 unknown으로 보수적으로 매핑한다. 4xx 일반 거부는 BROKER_REJECTED다. 서버의 `outcome: unknown`을 rejected로 바꾸지 않는다. 5xx를 확정 미저장으로 다루려면 허용된 domain 코드와 명시적 `outcome: rejected`가 함께 있어야 한다. 중앙 transport timeout은 서버가 rejected를 주장해도 unknown이다.

## 저장·전달·재시도

`taskId`, `status: PENDING`, `recipientAgentId`, `requestKey`, `delivery.status: FAILED/UNKNOWN`을 담은 성공 data는 성공으로 유지한다. 전달 실패를 저장 실패나 업무 FAILED로 바꾸지 않는다. HTTP timeout·취소·연결 종료·세션 만료 때문에 update_task_status/complete_task를 호출하지 않는다.

업무·등록·전달 자동 재전송은 없다. 같은 requestKey에 대한 중앙의 기존 결과는 수정하지 않지만, MCP가 키를 발급하거나 중앙 중복 판정/저장을 구현하지 않는다. 서로 다른 LLM 호출을 같은 업무로 자동 추정하지 않는다. requestKey 없이 다시 호출하면 새 요청이므로 중복 생성 가능성이 있다. create_task의 명시적 requestKey 재호출만 브로커 로컬 기록과 중앙의 요청 Agent별 키로 같은 작업을 돌려받는다.

taskId 없는 결과 불명을 requestId/requestKey로 조회하는 get_tasks 필터는 공동 계약 미확정이므로 추가하지 않았다. 키가 반환됐다고 그 필터를 사용할 수 있다는 뜻은 아니다. 키 재접수는 create_task에 같은 requestKey와 같은 인자를 넘기는 방식이다(S15P31D108-165). 이 재호출은 중앙에 다시 생성 요청을 보내 기존 작업을 replay하며, 처음 저장에 실패했던 요청이면 이때 새로 저장될 수 있다.

## 시간 예산과 취소

| 설정 | CLI | 환경변수 | 미지정 시 소비자 기본값 |
| --- | --- | --- | --- |
| HTTP 요청별 | --request-timeout-ms | DEVLINK_MCP_REQUEST_TIMEOUT_MS | 10000 ms |
| 업무 호출 전체 | --call-timeout-ms | DEVLINK_MCP_CALL_TIMEOUT_MS | 30000 ms |

정수 1–300000 ms만 허용하고 CLI가 env보다 우선한다. 기본값은 현재 소비자의 제한이며 운영 합의/SLA가 아니다. 중앙 ACK·업무 처리 시간과 대조해 운영값을 확정해야 한다. 테스트에는 짧은 값을 주입한다. MCP 프로세스 종료 시 세션 해제는 기존 최대 1초다.

전체 예산은 등록 대기·자격 로딩·공유 세션 생성·중앙 호출·bind를 모두 포함한다. 각 HTTP가 따로 제한 시간 안에 응답하더라도 전체 예산을 넘으면 BROKER_TIMEOUT이다. monotonic clock으로 다음 단계 전에 남은 예산을 확인한다. 취소·timeout은 로컬 대기/전송을 중단하며 중앙 작업 취소를 뜻하지 않는다.

대기자 하나가 취소·timeout되어도 다른 호출이 공유하는 세션 claim을 중단하지 않는다. 만료된 호출은 claim 완료 후 업무를 뒤늦게 보내지 않는다. 이미 시작한 bind의 늦은 결과도 만료된 호출에서 게시하지 않는다. 프로세스 종료는 모든 호출 예산을 중단하고 HTTP pending을 정리한다.

전체 호출 예산 만료는 정상 공유 세션의 장애 증거가 아니다. 해당 호출만 중단하고 다른 등록·업무 호출의 snapshot을 폐기하지 않는다. 실제 HTTP 제한 시간 초과·연결 단절은 기존 세션 복구 규칙을 따르며, 이미 시도한 bind의 결과 불명은 별도로 snapshot을 폐기하고 복구한다. 완료된 등록 결과를 다음 microtask의 시간 경과만으로 timeout으로 바꾸지 않는다.

## 검증과 한계

공유 error-fixtures.json, HTTP 대역, 실제 SDK/stdio 자식 프로세스로 오류 분류·네 식별자·403 이후 세션 생존·저장/전달 결과 보존·전체 시간 예산·공유 claim 취소 격리·종료·비밀 비노출을 검증한다. 원본 요청 ID는 SDK wire id와 HTTP body를 대조한다.

실제 Rust /v1 및 중앙 오류·등록 멱등성·복구 조회는 미검증이다. 실제 Codex/Claude Code 표시 차이와 종단 간 오류 동작은 -64/-66에서 확인한다. 이 소비자 테스트나 스토리 커밋만으로 Jira Done 판정하지 않는다.
