# MCP → 로컬 브로커 계약 v0.1 (S15P31D108-60)

2026-10-27, MCP 측 선구현 초안. Notion의 7개 도구 API 페이지는 현재 요청·응답 템플릿이 비어 있다. 아래 경로·필드는 Rust 브로커 담당자와 상호 검증하기 전까지 공동 확정 계약이 아니다. 세션 재시작 정책은 사용자 결정에 따라 **먼저 복구한 인스턴스 인정**으로 MCP 측 확정했다.

## 연결과 인증

CLI가 사용자 전용 디렉터리에 다음 JSON 스냅샷을 원자적으로 발행한다. MCP에는 파일의 절대 경로만 전달한다. 중앙 서버 토큰은 전달하지 않는다.

```json
{
  "schemaVersion": 1,
  "brokerUrl": "http://127.0.0.1:43127",
  "localToken": "CLI-generated-private-local-token",
  "agentId": "optional-previously-registered-agent-id"
}
```

`agentId`를 생략하면 인증된 사용자·팀의 미바인딩 세션으로 등록부터 수행한다. 브로커는 등록 결과의 Agent 소유권을 검증한 뒤 bind한다. 파일은 일반 파일이어야 하며 심볼릭 링크, 16KiB 초과, 비공개가 아닌 권한을 거부한다. POSIX는 파일 소유 UID·0600 수준 및 직계 부모 쓰기 권한을, Windows는 현재 사용자 소유와 DACL(현재 사용자·SYSTEM·Administrators 외 파일 Allow, 직계 부모 쓰기·삭제·권한 변경 Allow 금지)을 검사한다. Windows ACL 검사 실패 시 허용하지 않는다. 상위 디렉터리와 CLI 발행 경로의 신뢰성은 CLI 설치 범위다.

HTTP는 포트가 명시된 리터럴 `127.0.0.1`, 평문 로컬 HTTP, 루트 URL만 허용한다. 요청마다 `Authorization: Bearer <localToken>`과 JSON content-type을 사용한다. 리다이렉트는 따라가지 않는다. 토큰과 설정은 프로세스 수명 동안 고정한다. 정상 브로커 재시작은 토큰을 유지하고, 명시적 토큰 교체 시 MCP를 재시작한다. MCP는 브로커를 실행하지 않는다.

## 세션과 소유권

| 요청 | 입력(body에는 requestId 필수) | 성공 data |
| --- | --- | --- |
| POST `/v1/sessions` | `instanceId`, `mode: fresh / recover`, 선택 `agentId`, 선택 `processId` | `sessionId`, `principal: {userId, teamId, agentId?}` |
| POST `/v1/sessions/:sessionId/bind` | `instanceId`, `agentId` | 동일 sessionId와 바인딩된 principal |
| POST `/v1/sessions/:sessionId/heartbeat` | `instanceId` | 객체 |
| DELETE `/v1/sessions/:sessionId` | `instanceId` | 객체 |

- 유효한 도구 요청에서 지연 활성화한다. 초기화·목록·진단만으로 세션이나 중앙 presence를 만들지 않는다.
- 프로세스마다 고정 UUID instanceId를 쓰며 동일 인스턴스의 중복 생성은 멱등적이어야 한다. 처음 시도한 뒤에는 응답 유실 여부와 관계없이 recover만 사용한다.
  - -142: 모든 자동 MCP claim은 첫 시도부터 recover를 사용해 병렬 업무/등록에서도 활성 소유자를 보호한다. 강제 교체 fresh는 Rust의 명시적 API에만 남긴다.
- 인증된 fresh 시작은 기존 소유자를 교체할 수 있다. recover는 자기 세션 또는 빈 슬롯만 얻는다. 기존 소유자가 있으면 `SESSION_CONFLICT`로 자동 복구를 중단한다.
- 브로커 재시작으로 휘발 상태가 사라지면 먼저 유효하게 복구한 쪽이 소유한다. 과거 마지막 소유자를 복원한다는 보장은 없다.
- 오래된 heartbeat/delete는 새 세션에 영향을 주면 안 된다. sessionId와 instanceId를 함께 검증한다.
- MCP heartbeat 기본 30초, 브로커 수신 시계 기준 lease 90초. 클라이언트 시각은 만료 판정에 사용하지 않는다.
- MCP는 claim에 자기 `processId`(`process.pid`)를 넣는다. 브로커는 그 프로세스가 끝나면 lease를 기다리지 않고 세션을 끝낸다. Host가 MCP를 정상 종료 없이 끝내는 경우(Codex on Windows, #108) 대비다. `processId`를 모르는 구버전 브로커는 400 `VALIDATION_FAILED`로 거부하며 아무것도 만들지 않으므로, MCP는 그때 한 번만 이 필드 없이 다시 claim한다. 그 재시도가 성공했을 때만 그 프로세스에서 더 보내지 않으며, 재시도도 실패하면 다음 claim에서 다시 보낸다. 본문 시간 초과(408)·크기 초과(413)·Content-Type 오류(415)도 `VALIDATION_FAILED`이므로, 그런 일시 오류 뒤 재시도가 성공하면 그 프로세스에서는 꺼진다(로컬 loopback의 작은 본문이라 실제로는 예상하지 않는다). 자세한 규칙은 [로컬 세션](../../../rust/docs/local-sessions.md#소유권과-수명).
- 연결 실패·타임아웃·세션 만료 시 세션만 최대 5회 지수 backoff(1초부터 최대 30초)로 복구한다. 업무 요청은 자동 재전송하지 않는다. 소유권 충돌·교체·인증 거절·잘못된 자격 파일은 자동 복구를 멈춘다.

## 업무 경로

모두 POST. `register_agent`만 미바인딩 세션에서도 허용한다. 중앙 인증·업무 상태 전이·DB·라우팅 규칙은 브로커와 중앙 서버가 검증한다.

| operation | 경로 |
| --- | --- |
| register_agent | `/v1/agents/register` |
| list_agents | `/v1/agents/list` |
| update_agent | `/v1/agents/update` |
| find_agent | `/v1/agents/find` |
| create_task | `/v1/tasks/create` |
| update_task_status | `/v1/tasks/status` |
| complete_task | `/v1/tasks/complete` |
| get_tasks | `/v1/tasks/list` |
| get_task_result | `/v1/tasks/result` |

```json
{
  "requestId": "MCP-generated-UUID",
  "mcpRequestId": 27,
  "operation": "create_task",
  "arguments": {"recipientAgentId": "agent-b", "title": "작업", "description": "요청 내용"},
  "context": {"sessionId": "local-session", "userId": "broker-user", "teamId": "broker-team", "agentId": "agent-a"}
}
```

context는 브로커가 반환한 principal에서만 주입한다. 도구 입력으로 actor를 덮어쓸 수 없다. mcpRequestId는 선택적인 진단 상관관계 값이며 인증이나 멱등키가 아니다. 서로 다른 업무 요청의 requestId는 독립적이다. 중앙 요청의 멱등성 구현은 이 어댑터에 포함하지 않는다.

```json
{"requestId":"MCP-generated-UUID","data":{"taskId":"task-1","status":"PENDING","delivery":{"status":"FAILED"}}}
```

```json
{"requestId":"MCP-generated-UUID","error":{"code":"SESSION_EXPIRED","taskId":"optional-known-task"}}
```

모든 응답은 해당 요청의 requestId와 일치해야 하며 data 또는 error를 갖는다. 성공 data는 taskId, 업무 상태, 수신자, 전달 결과 및 확장 필드를 변경 없이 보존한다. `register_agent`의 data.agentId는 필수이며 bind 후 성공을 반환한다. 로컬 전달 ACK와 사용자의 읽기·수락·작업 완료는 별개다.

요청 제한은 128KiB, 응답은 256KiB. 기본 요청 제한 10초는 응답 본문 수신까지 포함한다. 타임아웃·단절·취소·프로토콜 오류는 업무 결과 `unknown`이며 중앙 상태를 FAILED로 바꾸거나 재할당하지 않는다. 오류에는 안전한 code·고정 message·requestId·알려진 taskId만 노출한다. 브로커의 원문 오류·토큰은 노출하지 않는다.

종료 시 타이머와 진행 요청을 취소하고 최대 1초의 best-effort 세션 해제를 수행한다. 세션 해제 실패가 MCP 종료를 막지 않는다.

## 검증 경계

`test/helpers/fake-broker.mjs`는 이 계약을 실행하는 테스트 대역이다. 재시작·교체·만료·인증·응답 유실·동시 호출·오류 주입을 검증한다. 실제 Rust 구현과의 경로/JSON/인증/presence/ACK 상호 검증, 중앙 서버와 두 Agent의 실제 왕복은 미확인이다. 코드 커밋만으로 Jira 완료로 판정하지 않는다.
