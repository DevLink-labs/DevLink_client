# 로컬 업데이트 조회 — `/v1/updates`

2026-11-03 (Asia/Seoul). Host가 새 작업과 보낸 작업의 상태 변화를 놓치지 않도록, 브로커가 이미 저장한 수신함과 요청자 알림을 하나의 읽기 전용 피드로 제공한다. Host 훅 연결과 유휴 세션 기동은 별도 범위다.

## 계약

MCP `get_updates`가 기존 업무 요청 envelope로 호출한다. localToken·Origin 거부·세션 소유자 확인은 [업무 adapter](task-operations.md)와 같다. 중앙은 호출하지 않는다.

```json
{"requestId":"UUID","operation":"get_updates","arguments":{"cursor":"v1.1.2.<32 hex>","limit":20},"context":{"sessionId":"...","userId":"3","teamId":"1","agentId":"12"}}
```

- `limit`은 1–50(기본 20), `cursor`는 이전 응답의 `nextCursor`다. 알 수 없는 인자, 범위 밖 limit, 형식이 틀린 cursor는 400 `VALIDATION_FAILED`다.
- bind되지 않은 세션은 409 `AGENT_UNREGISTERED`다. 요청 context는 대조용이며 조회 범위는 브로커가 인증한 actor로만 정한다.
- 저장 파일을 읽지 못하면 503 `LOCAL_UPDATES_UNAVAILABLE`이며 파일 내용은 반환하지 않는다.

응답 `data`:

```json
{
  "updates": [
    {"eventId":"...","kind":"TASK_CREATED","direction":"received","taskId":"101","requesterAgentId":"11"},
    {"eventId":"...","kind":"TASK_STATUS_CHANGED","direction":"sent","taskId":"201","taskVersion":"1","status":"IN_PROGRESS"},
    {"eventId":"...","kind":"TASK_COMPLETED","direction":"sent","taskId":"201","taskVersion":"2","status":"COMPLETED"}
  ],
  "nextCursor": "v1.1.2.<32 hex>",
  "hasMore": false,
  "cursorReset": false,
  "coverage": "local"
}
```

JSON Schema는 [updates.schema.json](../../contracts/local-broker/v1/updates.schema.json)이다. prompt와 resultText는 넣지 않는다. 본문·최신 상태는 `get_task_result`로 확인한다.

## 범위와 순서

- 수신: `inbox.json`의 TASK_CREATED 중 현재 origin·수신 Agent·팀에 해당하는 작업. 작업당 한 번이다.
- 송신: `notifications.json`의 TASK_STATUS_CHANGED·TASK_COMPLETED 중 현재 origin·요청 Agent에 해당하는 이벤트. 같은 작업의 여러 version을 모두 보존 순서대로 준다.
- 한 페이지는 남은 수신 항목을 먼저, 이어서 송신 항목을 채운다. 각 출처 안에서는 저장 순서다. 두 출처 사이의 시각 순서는 보장하지 않는다.

## cursor

`v1.<수신 위치>.<송신 위치>.<tag>` 형식이다. tag는 actor 범위(origin·user·team·Agent), 두 위치, 각 위치 직전 eventId의 SHA-256 앞 16바이트다.

- 조회는 아무것도 삭제·ACK·읽음 처리하지 않는다. 같은 cursor는 같은 결과를 돌려준다.
- 위치가 보관 범위를 넘거나 tag가 현재 이력과 맞지 않으면(정리된 저장소, 다른 actor의 cursor, 변조) 처음부터 다시 주고 `cursorReset: true`를 표시한다. 건너뛰거나 추정하지 않는다.
- 두 저장소는 추가 전용이고 정리(`inbox clear`, 알림 초기화)만 이력을 바꾸므로 저장 형식 변경 없이 위치를 쓴다.

## 보장하지 않는 것

`coverage: "local"`은 이 브로커가 받아 저장한 것만 포함한다는 뜻이다. 브로커가 꺼져 있던 동안 중앙이 보내지 못한 이벤트, 저장 한도 초과·저장 실패로 거부된 이벤트, 중앙 Outbox가 재시도를 포기한 알림은 없다. 중앙 replay API 전까지 완전한 이력은 `get_tasks`(sent/received)와 `get_task_result`로 확인한다. 업데이트는 사용자의 수락이 아니다.

## 검증

- `cargo test -p devlink-broker --test task_updates`: 범위 격리, 저장 순서, 본문 미포함, 같은 cursor 재조회, 페이지 이어 읽기, 정리·변조·범위 초과 cursor 재시작, 입력 검증, 미바인딩 세션, 손상된 저장소 503.
- `updates::tests`: cursor 형식과 limit 경계.
- MCP `test/updates.test.mjs`: 읽기 전용 annotation, 전달 인자, 본문 누출·상태 불일치·형식 위반 응답의 BROKER_PROTOCOL_ERROR 처리, 저장소 오류 매핑.
