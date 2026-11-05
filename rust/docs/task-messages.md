# 질문·답변 도착 동기화 — task-messages

2026-11-05 (Asia/Seoul), S15P31D108-167. 같은 Task의 상대 Agent가 남긴 새 질문·답변을 브로커가 받아 로컬 업데이트로 노출한다. 보내는 쪽(`ask_question`·`answer_question`)은 [작업 상태·완료·결과](task-operations.md)와 S15P31D108-166을 따른다.

중앙 계약은 DevLink_server develop의 `docs/task-message-sync.md`(pull API)와 `docs/task-message-notification.md`(WebSocket 알림)다. 이 문서는 그 계약을 브로커가 어떻게 쓰는지만 적는다.

## 흐름

1. 중앙은 질문·답변을 저장하고 Outbox로 상대 Agent에게 `TASK_QUESTION_CREATED`·`TASK_ANSWER_CREATED`를 보낸다.
2. 브로커는 알림을 "조회하라"는 신호로만 쓴다. 본문이 없고 ACK도 보내지 않는다.
3. 브로커가 `GET /api/v1/agents/me/task-messages?cursor=…&limit=100`을 호출해 도착 정보와 `nextCursor`를 함께 저장한다.
4. `get_updates`와 `devlink updates-hook`이 저장된 도착 정보를 ID·종류만으로 보여준다. 본문은 `get_task_result`로 읽는다.

알림만으로 cursor를 바꾸거나, 작업을 수락·재개하거나, 답하지 않는다.

## WebSocket 알림

```json
{"event_id":"<Outbox UUID>","type":"TASK_QUESTION_CREATED","task_id":100,"receiver_agent_id":2,
 "payload":{"task_event_id":"502","sequence":"13","event_type":"QUESTION"}}
```

- READY 이후에만 받는다. `protocol::TaskMessageNotice`로 엄격하게 해석한다(알 수 없는 필드 거부).
- 검증: `event_id`는 UUID, `task_id`·`receiver_agent_id`는 양의 Java Long, `task_event_id`·`sequence`는 양의 정수 문자열, `type`과 `event_type`의 짝(QUESTION↔TASK_QUESTION_CREATED, ANSWER↔TASK_ANSWER_CREATED). `receiver_agent_id`는 현재 READY의 Agent와 같아야 한다.
- 유효하면 pull을 요청한다. 형식이 틀리거나 다른 Agent 앞으로 온 알림은 `rejectedEvents`로 세고 pull하지 않는다. 연결은 끊지 않는다. 안전망 pull이 같은 내용을 복구한다.
- 알림의 도착 순서·개수로 이벤트를 추정하지 않는다. 같은 알림이 여러 번 와도 pull 요청이 합쳐질 뿐이다.

## pull 시점

| 시점 | 내용 |
| --- | --- |
| READY(접속·재접속) | 연결이 끊긴 동안 놓친 질문·답변을 따라잡는다. 브로커 재시작 뒤 첫 READY도 같다. |
| 유효한 알림 | 실행 중에 온 신호는 `Notify`로 모아 한 번 더 조회한다. |
| 안전망 주기 | `taskMessageSyncIntervalMs`(기본 60000, 1000~3600000)의 ±1/6 무작위 간격. 기본값은 50~70초다. Connected 상태일 때만 조회한다. |

한 번의 조회에서 `hasNext=false`가 될 때까지 이어 읽는다. 한 번에 최대 32페이지(3,200건)이며, 넘으면 다음 차례로 이어간다.

## 저장 — `.devlink-runtime/task-messages.json`

- 비공개 파일이다. 기존 `private_file`의 권한 설정·검증(Windows DACL, Unix 0600)과 원자 교체, `task-messages.lock` 파일 잠금을 쓴다.
- 범위는 (중앙 origin, READY Agent ID)이고 범위마다 `cursor`와 도착 정보 목록을 둔다. 도착 정보는 `eventId`·`taskId`·`sequence`·`eventType`·`createdAt`뿐이다. 응답에 본문 같은 다른 필드가 와도 저장하지 않는다.
- 한 페이지의 항목과 `nextCursor`를 한 번의 교체로 저장한다. 저장에 실패하면 cursor가 그대로라서 다음 조회가 같은 위치에서 다시 시작한다.
- `eventId`로 중복을 제거한다. 이미 있는 eventId가 다른 내용으로 다시 오면 기존 기록을 유지하고 `rejectedEvents`로 센다.
- 상한: 전체 4,096건, 파일 8MiB. 범위 개수에는 상한이 없다. Agent 재등록이나 테스트 서버 변경으로 범위가 계속 늘어도 현재 Agent의 동기화는 멈추지 않고, 옛 범위의 cursor도 그대로 남는다.
- 범위는 조회한 순서로 저장하고, 조회한 범위를 맨 뒤로 옮긴다. 파일이 8MiB를 넘게 될 때만 가장 오래전에 조회한 범위를 통째로 지운다. 현재 범위는 지우지 않는다. 지운 범위의 Agent가 돌아오면 그 범위만 처음부터 다시 조회한다. cursor 하나가 최대 64KiB이므로, 이 정리는 범위가 백 개 이상 쌓인 뒤에야 일어난다.
- 이벤트 상한(4,096건): 페이지를 저장하다 이 상한을 넘게 되면, 저장 전에 2,048건을 한 번에 지운다. 현재가 아닌 범위(Agent 재등록이나 서버 주소 변경으로 남은 옛 범위)를 큰 것부터 먼저 지우고, 모자라면 현재 범위의 가장 오래된 기록을 지운다. 새 페이지는 정리 뒤에 넣으므로 아무도 보기 전에 지워지지 않는다. 오래 쓴 Agent도 새 질문·답변을 계속 받는다. 현재 범위를 지울 때만 로컬 업데이트 cursor가 처음부터 다시 시작(`cursorReset`)하며, 이는 2,048건마다 한 번이다.
- 지운 이력은 `get_task_result`로 본다. cursor 없이 처음부터 재동기화하면 지운 항목이 새 항목으로 맨 뒤에 다시 들어올 수 있다. 재동기화는 cursor 거부 때만 일어나므로 드물다.
- `devlink notifications --clear`(브로커 중지 상태)로 도착 정보를 비울 수 있으며, 이때 중앙 cursor는 남겨 전체 재동기화를 하지 않는다.
- 손상된 파일은 덮어쓰지 않는다. 브로커는 계속 실행되며 `lastError: TASK_MESSAGE_STORE_INVALID`로 보고하고, `get_updates`는 503 `LOCAL_UPDATES_UNAVAILABLE`이 된다.

## 오류와 재동기화

| 상황 | 동작 |
| --- | --- |
| 저장된 cursor에 400 `VALIDATION_FAILED`(변조·손상·구버전·서명키 교체) 또는 403(다른 Agent·팀의 cursor) | cursor 없이 한 번 처음부터 다시 조회하고 eventId로 중복을 제거한다. |
| cursor 없이도 403, 401, 5xx, timeout | 실패로 세고 cursor를 유지한다. 다음 신호나 안전망 주기에 다시 조회한다. |
| `hasNext=true`인데 항목이 없거나 요청한 cursor를 그대로 돌려줌, 잘못된 항목·cursor, 100건 초과 | `CENTRAL_PROTOCOL_ERROR`. 그 페이지는 저장하지 않고 cursor를 유지한다. |
| 조회 중 연결이 끊김 | 실패로 세지 않고 저장하지 않는다. 다음 READY가 같은 cursor로 다시 조회한다. |
| 조회 중 READY Agent가 바뀜 | `CENTRAL_IDENTITY_CHANGED`. 저장하지 않는다. |

상태는 `devlink status`(문제가 있을 때만 표시)와 `/health`의 `taskMessages`(`rejectedEvents`, `failedPulls`, `lastError`)로 확인한다.

## 보장하지 않는 것

`coverage: "local"`이다. 브로커가 꺼져 있거나 중앙에 연결되지 않은 동안에는 조회하지 않는다. 다음 READY에서 따라잡는다. 상한을 넘어 버린 항목은 로컬에 없다. 질문·답변 본문과 최신 상태는 언제나 `get_task_result`가 기준이다.

## 검증

- `cargo test -p devlink-broker --test task_messages`: mock 중앙 WebSocket·HTTP로 READY 직후 pull과 hasNext 이어 읽기, Bearer·limit 전달, 다른 Agent 앞 알림 거부(pull·응답 없음), 알림 신호의 ACK 없음, 중복 eventId 제거, 여러 알림 합치기, 재접속 READY로 놓친 항목 복구, 저장된 cursor 400·403 시 처음부터 재동기화, 같은 cursor 반복 중단, 5xx 실패 후 cursor 유지와 안전망 주기 재시도, 잘못된 페이지에서 cursor 유지, 저장 파일에 본문·토큰 없음과 비공개 권한, 빈 조회 범위 16개가 있어도 17번째 Agent의 조회 결과 저장과 기존 cursor 보존.
- `--test task_updates`·`--test update_hook`: 빈 조회 범위 16개 뒤에도 현재 Agent의 질문·답변이 `get_updates`와 훅 요약에 나타남.
- `task_messages::tests`: 중복·충돌·범위 분리, clear 후 cursor 유지, 16개를 넘는 새 범위 저장과 기존 cursor 보존, 조회 순서 유지, 크기 상한에서 가장 오래전 조회 범위만 정리(현재 범위는 유지), 상한 도달 시 옛 범위 우선·큰 범위 우선 정리, 상한에서 들어온 100건 페이지 전체 보존, 손상 파일 비덮어쓰기, cursor 문자·길이와 jitter 범위.
- `devlink-cli --test cli_broker`: 실제 CLI 브로커가 READY 뒤 같은 origin으로 task-messages를 조회해도 기존 작업 생성·수신·heartbeat 흐름이 그대로인지 mock 중앙에서 확인한다.
- `protocol::tests::task_message_notice_is_a_strict_body_less_signal`: 알림 형식·짝·본문 거부.
