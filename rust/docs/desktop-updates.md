# 데스크톱 알림용 로컬 업데이트 조회 — `devlink updates`

2026-11-09 (Asia/Seoul). 데스크톱 앱(Electron)이 새 작업·질문·답변·보낸 작업 상태를 PC 알림으로 보여 주도록, 브로커가 저장한 로컬 피드를 MCP 세션 없이 JSON으로 읽는다. 피드 내용과 cursor 규칙은 [로컬 업데이트 조회](local-updates.md)와 같고, 범위 결정은 [Host 훅](update-hook.md)과 같다.

## 사용법

```text
devlink --config <PROFILE> updates [--cursor <nextCursor>] [--limit 1-50] [--latest]
```

- `--cursor`: 이전 응답의 `nextCursor`. 생략하면 보관된 가장 오래된 항목부터 준다.
- `--limit`: 한 번에 받을 항목 수. 기본 20, 최대 50.
- `--latest`: 보관된 이력을 건너뛰고 끝 위치의 cursor만 받는다. 처음 조회할 때 기존 이력으로 알림이 쏟아지지 않게 할 때 쓴다. `--cursor`와 함께 쓸 수 없다.

## 범위와 보장

- 조회 범위는 프로필에 bind된 Agent와 선택된 팀으로만 정한다. 호출자가 사용자·팀·Agent를 지정할 수 없다.
- 로컬 저장소만 읽는다. 중앙 요청, 작업 수락, ACK, 읽음 처리, 파일 쓰기를 하지 않는다. 브로커가 꺼져 있어도 저장된 항목은 읽힌다.
- cursor는 호출자가 보관한다. Host 훅(`updates-hook`)과 MCP `get_updates`의 cursor와 독립이라 서로의 조회 위치를 소비하지 않는다.
- 읽는 동안 프로필이 다른 Agent로 다시 bind되면 결과를 버리고 `UPDATES_BINDING_MISMATCH`로 끝낸다.

## 응답

성공하면 종료 코드 0과 JSON 한 줄을 출력한다. `updates`·`nextCursor`·`hasMore`·`cursorReset`·`coverage`는 [local-updates.md](local-updates.md)의 `get_updates` 응답과 같다.

```json
{
  "schemaVersion": 1,
  "scope": {"userId":"3","teamId":"1","agentId":"12","toolType":"codex","projectPath":"C:\\work\\project"},
  "updates": [
    {"eventId":"...","kind":"TASK_CREATED","direction":"received","taskId":"101","requesterAgentId":"11"},
    {"eventId":"...","kind":"TASK_COMPLETED","direction":"sent","taskId":"201","taskVersion":"2","status":"COMPLETED"},
    {"eventId":"502","kind":"TASK_QUESTION_CREATED","direction":"received","taskId":"300","sequence":"13"}
  ],
  "nextCursor": "v3.1.2.1.0.<32 hex>",
  "hasMore": false,
  "cursorReset": false,
  "coverage": "local"
}
```

- `toolType`은 `codex` 또는 `claude_code`다.
- `projectPath`는 bind에 저장된 정규화 경로다. Windows에서는 `\\?\C:\...` 형식이므로 비교 전에 같은 방식으로 정규화한다(`setup mcp-guide`의 `projectPath`와 같음).
- prompt·resultText·질문·답변 본문은 넣지 않는다. 본문은 기존 상세 조회로 확인한다.
- 질문·답변 항목에는 보낸 Agent ID가 없다. 작업 ID로 상세를 조회한다.

실패하면 JSON 한 줄과 함께 0이 아닌 종료 코드로 끝난다. 메시지에는 경로나 저장 내용을 넣지 않는다.

```json
{"schemaVersion":1,"error":{"code":"UPDATES_AGENT_UNBOUND","message":"No Agent is bound to this profile."}}
```

| 코드 | 종료 코드 | 의미 |
| --- | --- | --- |
| `UPDATES_INPUT_INVALID` | 2 | cursor 형식이 틀림. 저장한 cursor를 버리고 `--latest`로 다시 시작한다. |
| `UPDATES_AGENT_UNBOUND` | 1 | 프로필이 없거나 Agent가 bind되지 않음 |
| `UPDATES_BINDING_MISMATCH` | 1 | 선택된 팀·프로젝트 폴더가 bind와 다르거나, 읽는 동안 bind가 바뀜 |
| `LOCAL_UPDATES_UNAVAILABLE` | 1 | 프로필·저장소를 읽지 못함 |

`--limit` 범위 밖 값, `--latest`와 `--cursor` 동시 사용 같은 인자 오류는 종료 코드 2이고 stdout에 JSON을 출력하지 않는다.

## 호출자 권장 사항

- 프로필(프로젝트 폴더)마다 따로 조회하고 cursor도 프로필별로 보관한다. 권장 주기는 5–10초다. 받은 작업·상태는 수신 즉시, 질문·답변은 동기화 뒤, 지연 알림은 1분 주기 판정 뒤 저장된다.
- 처음 조회하거나 `UPDATES_INPUT_INVALID`를 받으면 `--latest`로 시작 위치만 받는다.
- `hasMore`가 true면 바로 다음 페이지를 읽는다.
- 항목은 출처 순서(받은 작업 → 보낸 작업 상태 → 질문·답변 → 지연 알림)이며 시간순이 아니다.
- `cursorReset`이 true면 보관된 항목이 처음부터 다시 나온다. 저장소 정리나 보관 한도(출처별 1024–4096건) 초과로 생긴다. 이미 알린 `kind`+`eventId`를 기억해 다시 알리지 않는다.
- 보낸 작업의 상태 이벤트는 늦게 도착한 낮은 `taskVersion`이 섞일 수 있다. 작업별로 가장 높은 version만 표시한다.
- `TASK_OVERDUE`는 로컬 판정이다. 중앙 상태는 바뀌지 않았으므로 상세 조회로 확인한다.

## 검증

- `cargo test -p devlink-broker --test local_updates`: 다른 팀·Agent 제외, scope, cursor 이어 읽기, limit·hasMore, `--latest`, 파일 미변경과 훅 cursor 독립, 정리 후 `cursorReset`, 입력 오류·미바인딩·bind 불일치 코드.
- `cargo test -p devlink-cli --test cli_updates`: JSON 오류 형식과 종료 코드, 인자 오류, bind된 프로필의 scope와 cursor 재사용.
