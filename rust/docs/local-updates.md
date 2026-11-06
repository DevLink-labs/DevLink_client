# 로컬 업데이트 조회 — `/v1/updates`

2026-11-03 (Asia/Seoul), 2026-11-05 갱신(S15P31D108-167), 2026-11-06 갱신(S15P31D108-177, 보낸 작업 지연 알림). Host가 새 작업, 보낸 작업의 상태 변화, 같은 Task의 새 질문·답변을 놓치지 않도록, 브로커가 이미 저장한 수신함·요청자 알림·질문·답변 도착 정보를 하나의 읽기 전용 피드로 제공한다. Host 훅용 요약은 [update-hook.md](update-hook.md)에 있고, 유휴 세션 기동은 별도 범위다.

## 계약

MCP `get_updates`가 기존 업무 요청 envelope로 호출한다. localToken·Origin 거부는 [업무 adapter](task-operations.md)와 같다. 세션 확인은 로컬 읽기 전용 `SessionManager::authorize_local_read`를 읽기 전후 두 번 사용하며 중앙은 호출하지 않는다.

- 유지하는 검증: 세션 만료·대체·소유권, 요청 context와 세션 principal 일치, 주체 변경·인증 거부 시 올라가는 subject epoch에 따른 세션 무효화.
- 거부: 중앙 `AuthFailed`(503 `CENTRAL_UNAUTHENTICATED`), `Connected` 상태에서 중앙이 밝힌 Agent·user·team이 세션과 다른 경우.
- 허용: 연결 중·재연결 중·단절 같은 일시 상태, 같은 Agent의 legacy READY(user/team 없음). 일반 업무 `authorize`와 달리 live READY를 요구하거나 `GET /api/v1/agents/me`로 신원을 해석하지 않는다. presence는 기존 규칙대로 확인되지 않은 legacy 재접속에서 ONLINE을 다시 알리지 않는다.

```json
{"requestId":"UUID","operation":"get_updates","arguments":{"cursor":"v3.1.2.0.0.<32 hex>","limit":20},"context":{"sessionId":"...","userId":"3","teamId":"1","agentId":"12"}}
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
    {"eventId":"...","kind":"TASK_COMPLETED","direction":"sent","taskId":"201","taskVersion":"2","status":"COMPLETED"},
    {"eventId":"502","kind":"TASK_QUESTION_CREATED","direction":"received","taskId":"300","sequence":"13"},
    {"eventId":"503","kind":"TASK_ANSWER_CREATED","direction":"received","taskId":"300","sequence":"14"},
    {"eventId":"overdue:204","kind":"TASK_OVERDUE","direction":"sent","taskId":"204","minutes":30}
  ],
  "nextCursor": "v3.1.2.2.1.<32 hex>",
  "hasMore": false,
  "cursorReset": false,
  "coverage": "local"
}
```

JSON Schema는 [updates.schema.json](../../contracts/local-broker/v1/updates.schema.json)이다. prompt·resultText·질문·답변 본문은 넣지 않는다. 본문·최신 상태는 `get_task_result`로 확인한다. 질문·답변 항목은 상대 참여자가 남긴 메시지가 도착했다는 사실일 뿐이며, 답하라는 사용자 지시가 아니다.

## 범위와 순서

- 수신: `inbox.json`의 TASK_CREATED 중 현재 origin·수신 Agent·팀에 해당하는 작업. 작업당 한 번이다.
- 송신: `notifications.json`의 TASK_STATUS_CHANGED·TASK_COMPLETED 중 현재 origin·요청 Agent에 해당하는 이벤트. 같은 작업의 여러 version을 모두 보존 순서대로 준다.
- 질문·답변: `task-messages.json`에서 현재 origin·Agent의 도착 정보. 중앙 task-messages pull로 저장한 순서이며 eventId·taskId·sequence·종류(`TASK_QUESTION_CREATED`·`TASK_ANSWER_CREATED`)만 준다. 저장 방식은 [질문·답변 도착 동기화](task-messages.md)다.
- 지연 알림: `sent-tasks.json`의 TASK_OVERDUE 중 현재 origin·요청 Agent·팀에 해당하는 것. 아래 "보낸 작업 지연 알림"을 본다.
- 한 페이지는 남은 수신 항목을 먼저 채우고, 이어서 송신 항목, 질문·답변 항목, 마지막으로 지연 알림을 채운다. 각 출처 안에서는 저장 순서이고, 출처 사이의 시각 순서는 보장하지 않는다.

## cursor

`v3.<수신 위치>.<송신 위치>.<질문·답변 위치>.<지연 알림 위치>.<tag>` 형식이다. tag는 actor 범위(origin·user·team·Agent), 네 위치, 각 출처에서 이미 소비한 모든 eventId를 길이 접두어와 함께 넣은 SHA-256의 앞 16바이트다. 마지막 eventId만 보면 저장소를 정리한 뒤 같은 위치에 같은 이벤트가 다시 저장될 때(`[A, B]` 소비 → 정리 → `[C, B]`) 이전 cursor가 통과해 C를 건너뛰므로, 소비한 앞부분 전체를 대조한다.

- 조회는 아무것도 삭제·ACK·읽음 처리하지 않는다. 같은 cursor는 같은 결과를 돌려준다.
- 위치가 보관 범위를 넘거나 tag가 현재 이력과 맞지 않으면(정리된 저장소, 다른 actor의 cursor, 변조) 처음부터 다시 주고 `cursorReset: true`를 표시한다. 건너뛰거나 추정하지 않는다.
- 네 출처는 추가 전용이고 정리(`inbox clear`, `notifications --clear`)만 이력을 바꾸므로 저장 형식 변경 없이 위치를 쓴다.
- 이전 형식 `v1.<수신>.<송신>.<tag>`도 받는다. 질문·답변 위치를 0으로 보고 v1 방식(두 출처)으로 tag를 대조하므로, 업그레이드 전에 저장한 MCP·훅 cursor가 재시작 없이 이어지고 그동안 쌓인 질문·답변은 새 항목으로 나온다. v2 cursor도 같은 방식으로 지연 알림 위치를 0으로 보고 v2 방식(세 출처)으로 대조한다. 응답의 `nextCursor`는 항상 v3다.

## 보낸 작업 지연 알림 (RCV-005)

- 브로커는 `create_task`가 성공하면 `sent-tasks.json`(비공개)에 origin·요청 Agent·팀·taskId·로컬 생성 시각을 기록한다.
  - 같은 requestKey로 replay된 같은 작업은 다시 기록하지 않는다. 보관 한도 때문에 기록이 지워진 뒤에 replay돼도, 이미 지연 알림이 있으면 다시 알리지 않는다.
  - 기록은 응답과 분리된 최선 노력이다. 생성 응답은 기록을 기다리지 않고, 기록에 실패해도 생성 결과는 바뀌지 않는다.
  - 원장에는 세대 번호가 있고 `notifications --clear`마다 바뀐다. 브로커는 시작할 때 읽은 세대에만 기록한다. 따라서 브로커가 멈추기 직전에 큐에 남은 기록이 정리 뒤에 실행돼도 원장을 되살리지 않는다(#80). 정리 뒤 새로 시작한 브로커는 새 세대에 정상 기록한다.
  - `taskOverdueMinutes`가 0이면 기록하지 않는다. 그래서 나중에 기능을 켜도 그 전의 작업이 한꺼번에 지연으로 나오지 않는다. 설정 변경은 브로커를 다시 시작해야 적용된다.
- 브로커가 실행 중일 때 1분마다 기록을 확인한다. 생성 후 `taskOverdueMinutes`(기본 30, 0이면 끔, 최대 10080)가 지났는데 `notifications.json`에 그 작업의 COMPLETED·FAILED·REJECTED가 없으면 `{"eventId":"overdue:<taskId>","kind":"TASK_OVERDUE","direction":"sent","taskId":"...","minutes":30}`를 작업당 한 번 추가한다. 이미 최종 상태가 있으면 알림 없이 판정만 끝낸다.
- 로컬 알림일 뿐이다. 중앙 상태는 바꾸지 않고, 작업을 취소하거나 재시도하지도 않는다. 중앙 알림을 놓친 경우에는 이미 끝난 작업이 지연으로 보일 수 있으므로 `get_task_result`로 확인한다. 브로커가 꺼져 있는 동안은 판정하지 않고, 다시 켜진 뒤 판정한다.
- 기록은 1024건, 알림은 1024건까지 보관한다. 기록이 가득 차면 판정이 끝난 오래된 기록부터 지운다. 알림이 가득 차면 새 판정을 미루므로, `notifications --clear`로 정리한다.

## 보장하지 않는 것

`coverage: "local"`은 이 브로커가 받아 저장한 것만 포함한다는 뜻이다. 브로커가 꺼져 있던 동안 중앙이 보내지 못한 이벤트, 저장 한도 초과·저장 실패로 거부된 이벤트, 중앙 Outbox가 재시도를 포기한 알림은 없다. 질문·답변은 다음 READY·안전망 pull에서 중앙 이력으로 따라잡지만, 그 전까지는 피드에 없다. 중앙 replay API 전까지 완전한 이력은 `get_tasks`(sent/received)와 `get_task_result`로 확인한다. 업데이트는 사용자의 수락이 아니다.

## 검증

- `cargo test -p devlink-broker --test task_updates`: 질문·답변 항목의 범위 격리·순서·본문 미포함·페이지 경계·정리 후 cursor 재시작, 범위 격리, 저장 순서, 본문 미포함, 같은 cursor 재조회, 페이지 이어 읽기, 정리·변조·범위 초과 cursor 재시작, 정리 후 같은 마지막 이벤트 재저장 시 재시작, 입력 검증, 미바인딩 세션, 손상된 저장소 503, 중앙 단절 중 조회·AuthFailed 거부, legacy 재접속 시 중앙 요청 0회, 다른 주체 재접속 시 세션 무효화.
- `updates::tests`: v1·v2·v3 cursor 형식, v1 cursor 이어 읽기, v2 cursor 뒤 지연 알림 이어 읽기·응답 형식·지연 위치 불일치 재시작, limit 경계.
- `sent_tasks::tests`: 기준 시간 전후, 작업당 한 번, replay 시 첫 기록 유지, 최종 상태가 있는 작업 미보고, Agent·origin·팀 범위, 0이면 끔, 정리, 보관 한도에서 판정 끝난 기록부터 삭제.
- `cargo test -p devlink-broker --test task_creation`: requestKey replay 뒤에도 원장 기록이 1건.
- MCP `test/updates.test.mjs`: 읽기 전용 annotation, 전달 인자, 질문·답변·지연 알림 항목, v1·v3 cursor 수용, 본문 누출·방향·sequence·상태 불일치·형식 위반 응답의 BROKER_PROTOCOL_ERROR 처리, 저장소 오류 매핑.
