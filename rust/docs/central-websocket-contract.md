# 중앙 WebSocket 연동 규칙과 Rust 적용 기준

2026-10-30 현재 구현 기준. Bearer Upgrade·READY·presence·JSON heartbeat·TASK_CREATED와 저장 후 ACK를 연결한다. 신원 공급은 [READY 정책](ready-identity.md), 실제 실행 범위는 [검증 요약](verification.md)을 따른다. 과거 준비 단계의 상태 설명은 검증 이력으로 분리했다.

## 1. 근거와 적용 범위

중앙의 고정 소스 `0e8c2b74e14ff056cdede24711e804647cd044df`와 현재 Rust 구현을 기준으로 읽는다.

- [중앙 WebSocket 핸들러](https://github.com/SSAFY-URAYAHA/DevLink_server/blob/0e8c2b74e14ff056cdede24711e804647cd044df/src/main/java/com/devlink/server/handler/DevLinkWebSocketHandler.java)
- [중앙 연결 Registry](https://github.com/SSAFY-URAYAHA/DevLink_server/blob/0e8c2b74e14ff056cdede24711e804647cd044df/src/main/java/com/devlink/server/realtime/ConnectionRegistry.java)
- [중앙 ACK 처리](https://github.com/SSAFY-URAYAHA/DevLink_server/blob/0e8c2b74e14ff056cdede24711e804647cd044df/src/main/java/com/devlink/server/service/TaskAckService.java)
- [Rust 프로토콜 DTO](../crates/devlink-broker/src/protocol.rs)·[연결 루프](../crates/devlink-broker/src/central.rs)

중앙의 Agent별 WebSocket 교체와 MCP 로컬 세션 fresh/recover 소유권은 별개다. 배포 환경의 설정값과 전체 Host 수용은 고정 소스·합성 소켓 시험만으로 확정하지 않는다.

## 2. 연결과 인증

- 중앙 연결 경로는 raw WebSocket `/ws/broker`다.
- WebSocket Upgrade 요청의 `Authorization: Bearer <Agent 토큰>`으로 인증한다.
- 사용자 로그인은 세션 방식이고, Agent 인증은 별도다. 사용자 세션으로 WebSocket Agent 인증을 대체하지 않으며 Agent 인증을 사용자 REST API 권한으로 사용하지 않는다.
- 중앙의 전용 인증 필터가 `AgentTokenVerifier` 결과를 인증 Principal로 전달한다. 검증 구현 Bean이 없거나 인증이 실패하면 중앙은 401로 연결을 거부한다.
- 중앙은 설정된 Origin 정책을 적용한다. Origin 검증만으로 인증을 대신하지 않는다. 현재 Rust는 Upgrade 요청에 Origin·Cookie를 추가하지 않는다.
- Agent 토큰의 DB 저장 계약은 원문이 아닌 해시다. 발급·만료·폐기·연결 종료 정책의 최종 판단은 중앙 인증 담당 범위다.
- Rust는 중앙 Agent 토큰과 MCP의 로컬 토큰을 분리한다. 중앙 연결에는 [bootstrap](agent-bootstrap.md)으로 저장한 Agent 자격 또는 legacy `DEVLINK_TOKEN`을 사용한다.

현재 Rust는 Upgrade 헤더로 인증하며, 연결 후 별도의 `auth`·`hello` JSON을 보내지 않는다. 고정 중앙·합성 계정 토큰의 실제 검증과 운영 배포 환경의 수용은 구분한다.

## 3. 연결 등록과 BROKER_READY

중앙은 인증된 `AgentPrincipal`의 Agent ID로 `ConnectionRegistry`에 연결을 등록하고, 현재 등록된 연결에 `BROKER_READY`를 보낸다. 클라이언트가 보낸 Agent ID를 인증 근거로 사용하지 않는다.

```json
{
  "type": "BROKER_READY",
  "payload": {
    "connection_id": "current-websocket-session-id",
    "agent_id": 11,
    "user_id": 3,
    "team_id": 1
  }
}
```

- `BROKER_READY`의 의미는 중앙 서버와 브로커 사이의 인증·연결 등록 완료다.
- WebSocket 연결 등록은 사용자나 Agent를 DB에 새로 등록하는 작업과 별개다.
- 토큰 검증 성공이나 READY 전송만으로 로컬 Agent를 ONLINE 처리하지 않는다.
- Rust는 유효한 READY 수신까지 `Authenticating`을 유지한다. 현재 DTO는 비어 있지 않은 `connection_id`와 양의 정수 `agent_id`를 검사한다.
- 최신 READY는 검증된 `user_id/team_id`를 포함한다. 둘 다 없는 legacy READY는 연결·heartbeat를 허용하되 세션 신원은 같은 Agent의 실시간 context로 보완해야 한다. 한쪽만 있거나 null·잘못된 ID인 READY는 거부한다.
- 연결이 종료·교체되면 Rust는 해당 연결의 식별자를 무효화한다. 이전 연결의 지연 처리가 새 연결의 상태·식별자를 덮어쓰지 못하게 한다.

## 4. AGENT_PRESENCE와 온라인 판정

로컬 Agent와 브로커 사이의 접속 상태는 다음 형식으로 보고한다.

```json
{
  "type": "AGENT_PRESENCE",
  "payload": {
    "status": "ONLINE"
  }
}
```

| 항목 | 중앙 문서의 규칙 |
| --- | --- |
| 허용 상태 | `ONLINE`, `OFFLINE` |
| Agent 식별 | 메시지 본문 대신 현재 인증된 연결의 Agent를 사용 |
| 전송 시점 | 최초 READY 수신 직후, 로컬 Agent 상태 변경 시, 재접속 후 READY 수신 직후 |
| 중앙 검증 | 인증 정보·현재 연결 여부, DB Agent의 소유자·팀과 인증 정보의 일치 여부 확인 |
| ONLINE 저장 | `agents.status`와 서버 현재 시각의 `last_seen_at` 갱신 |
| OFFLINE 저장 | 상태만 갱신하고 마지막 온라인 확인 시각 유지 |
| 동일 상태 재보고 | 오류 없이 처리 |
| 잘못된 메시지 | 잘못된 JSON·필드·타입·상태와 JSON 뒤 추가 값은 1008로 종료 |
| ACK | `AGENT_PRESENCE`에는 별도 ACK 없음 |

Rust는 상태 보고에 `agent_id`, 사용자·팀 식별자, 클라이언트 시각을 덧붙이지 않는다. 중앙이 인증된 Agent와 서버 시각으로 검증·저장한다.

현재 Rust는 `watch`로 최신 로컬 상태를 보존하며, 확인된 상태가 있으면 최초·재접속 READY 직후 보고하고 연결 유지 중 변경도 같은 WebSocket으로 보고한다. 로컬 상태가 미확인인 경우에는 ONLINE/OFFLINE을 추측해서 보내지 않는다. 같은 상태의 중복 갱신은 생략하며 연결 전·재접속 중 지나간 상태를 큐에 쌓아 재생하지 않는다. PRESENCE ACK를 기다리지 않고 전송 실패 시 연결 재시도 정책을 따른다.

로컬 HTTP 핸들러와 중앙 연결 태스크는 같은 `CentralState`를 공유한다. 세션 관리가 확인한 상태는 `CentralState::set_presence(PresenceStatus::Online/Offline)`으로 게시한다. `SharedPresence`는 `watch::Sender<Option<PresenceStatus>>`이며 기존 `Arc<RwLock<...>>` 직접 쓰기 방식 대신 이 게시 메서드를 사용한다. 기본 상태는 미확인(`None`)이다.

로컬 세션 관리자는 생성·bind·해제·만료와 같은 잠금 안에서 위 게시 메서드를 호출하도록 연결했다. 첫 성공 세션 전에는 미확인 상태를 유지하고, 이후 활성 bound 소유자가 있으면 ONLINE, 없으면 OFFLINE이다. 거부된 요청·health는 상태를 활성화하지 않는다. 중앙 신원 공급부는 [READY+실시간 context](ready-identity.md)로 연결했다. 고정 중앙·DB 세션 검증과 실제 Host 전체 업무 수용은 구분한다. 로컬 세션 만료를 중앙 작업의 실패·취소로 변환하지 않는다. 상세 내용은 [로컬 세션 API](local-sessions.md)를 따른다.

## 5. 연결 교체·종료와 오래된 이벤트 보호

- 중앙 Registry는 `Agent ID → 현재 열린 WebSocket 연결`을 관리한다.
- 같은 Agent가 새로 연결하면 새 연결을 등록하고 기존 연결을 4002로 종료한다.
- 이전 연결 종료에 실패해도 이전 연결을 활성 연결로 복원하지 않는다.
- 해제는 현재 등록된 실제 세션과 일치할 때만 수행한다. 이전 연결의 지연 종료가 새 연결을 제거하면 안 된다.
- 중앙의 상태 보고·종료 처리는 Agent별 DB 행 잠금으로 직렬화하고, 잠금 획득 후 현재 연결을 다시 확인한다.
- 현재 연결이 실제로 해제되고 새 활성 연결이 없을 때 중앙은 Agent를 OFFLINE 처리한다.
- 실제 전달 가능 여부는 DB 상태와 Registry의 현재 열린 연결을 함께 확인한다.
- 중앙 서버 재시작으로 메모리 연결 정보가 사라지면 브로커 재접속으로 다시 구성한다. 자동 재접속은 Rust 담당 범위다.

현재 Rust의 종료 분류는 다음과 같다. 서버 `BrokerHeartbeatService`가 4001을 `Heartbeat timeout`에 사용하는 것을 확인했으므로 기존의 4001 인증 실패 분류를 수정했다. HTTP 401/403 인증 실패와 WebSocket Close 4001은 구분한다.

| 응답·종료 | 현재 Rust 처리 |
| --- | --- |
| Upgrade HTTP 401·403 | `AuthFailed`, 자동 재접속 중단 |
| Close 4001 (Heartbeat timeout) 또는 JSON PONG 응답 제한 초과 | `autoReconnect=true`이면 백오프 재접속, false이면 `Disconnected` |
| Close 4002 | `Replaced`, 자동 재접속 중단 |
| Close 1008 또는 잘못된 초기 READY | `ProtocolViolation`, 자동 재접속 중단 |
| 일시적 단절·READY 시간 초과 | `autoReconnect` 설정에 따라 지수 백오프로 재접속 |
| 브로커 종료 요청 | WebSocket·재접속 대기 취소 후 종료 |

## 6. JSON Heartbeat와 만료 처리

2026-10-27 문서는 하트비트를 초기안으로 남겼지만 현재 로컬 서버에는 JSON Heartbeat와 만료 정리가 구현돼 있다. Rust도 이 JSON 규칙을 따른다. 아래 서버 값은 로컬 소스의 기본값이며 배포 환경의 실제 설정을 확인한 것은 아니다.

```json
{"type":"HEARTBEAT","payload":{"kind":"PING","ping_id":"unique-ping-id"}}
```

서버는 같은 `ping_id`를 사용해 다음과 같이 응답한다.

```json
{"type":"HEARTBEAT","payload":{"kind":"PONG","ping_id":"unique-ping-id"}}
```

| 구분 | 기준 |
| --- | --- |
| 현재 Rust | READY 후 첫 PING까지 20초, 이후 20초 간격 JSON PING. 각 PING의 동일 ping_id PONG을 전송 시점부터 최대 20초 대기 |
| 현재 로컬 서버 | 등록 시점부터 감시. 유효한 JSON PING만 수신 시각 갱신, 기본 40초 이상 무수신이면 다음 검사에서 4001로 종료 |
| 개발용 Rust Mock | 같은 JSON PING/PONG과 40초 무수신 만료·4001 적용. presence·제어 프레임으로 감시 시간 갱신하지 않음 |
| MCP 로컬 세션 | MCP 갱신 기본 30초, 브로커 lease 90초. 중앙 Heartbeat와 별도 |

각 PING은 UUID ping_id를 사용하고 응답 대기 중에는 새 PING으로 기존 요청·제한 시간을 덮어쓰지 않는다. 잘못된 형식·다른 ID·이전 PONG·일반 JSON·WebSocket 제어 Ping/Pong은 PONG 대기를 완료하거나 연장하지 않는다. 제어 Ping에는 전송 계층의 Pong을 응답하지만 이를 JSON Heartbeat로 인정하지 않는다. PONG이 만료되면 연결을 정리하고 재접속 설정을 따른다.

재접속할 때는 이전 연결의 식별자·PONG 대기를 폐기하고 새 READY 후 최신 확인된 presence를 다시 보고한다. 4002 교체·1008 정책 위반·Upgrade 401/403은 계속 자동 재접속을 중단한다. 연결 유지·PONG 불일치/재사용·만료·4001 재접속과 autoReconnect=false의 로컬 검증은 [검증 기록](verification.md)을 참고한다.

실제 주기의 연결 유지는 CLI 통합 테스트 `json_heartbeat_keeps_cli_connected_for_45_seconds_without_reconnect`로 자동 검증한다. JSON PING 무수신 시 40초 만료·4001로 종료하는 Mock을 사용하고, 시간 단축 없이 45초간 Connected·연결 1회·만료 0회와 서로 다른 ping_id의 반복 수신을 확인한다. 재접속 후 Connected로 돌아온 경우는 연결 횟수 검사에서 실패한다. 기본 테스트 실행에 포함하며 실제 중앙 배포 환경 검증과는 구분한다.

## 7. 신원·업무·ACK의 경계

중앙은 토큰의 Agent Principal과 DB의 소유자·팀을 대조한다. Rust는 READY와 /api/v1/agents/me를 현재 연결과 대조해 로컬 principal을 공급한다. [신원·인증 무효화](ready-identity.md)와 [세션 계약](local-sessions.md)을 따른다.

최초 Agent 생성·토큰 발급·ID 재사용은 [CLI bootstrap](agent-bootstrap.md), MCP register_agent의 기존 Agent 확인·팀 목록·생성은 [업무 중계](business-relay.md), 작업 상태·완료·조회는 [업무 adapter](task-operations.md)에 구현되어 있다. MCP의 최종 소비자 DTO는 -97로 구분한다.

TASK_CREATED는 인증된 수신자·팀·필드를 검사하고 디스크 저장을 완료한 뒤 정확한 event_id/task_id로 ACK한다. 파싱할 수 없는 TASK_CREATED는 ProtocolViolation, 기록 실패는 InboxFailed로 끝나며 성공 ACK를 보내지 않는다. 중복·재시작·오래된 generation·CLI 조회/정리·실제 중앙 시험은 [수신함](task-inbox.md)에 둔다. AGENT_PRESENCE에 ACK가 없다는 규칙을 업무 ACK에 적용하지 않는다.

## 8. 후속 검증

현재 구현·검증과 남은 작업은 [진행 현황](../PROJECT_STATUS.md)과 [검증 요약](verification.md)을 따른다. 상태·결과 알림(-96), 누락 복구 정책, MCP 최종 DTO(-97), 전체 통합(-52), 실제 Host 양방향(-66), 운영 설정·Ubuntu 실행을 개별 범위로 확인한다.
