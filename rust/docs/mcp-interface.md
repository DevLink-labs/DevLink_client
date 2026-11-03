# MCP 실행 구조·로컬 통신 인터페이스

2026-10-30 현재 구현 기준. CLI는 독립 Rust 브로커를 제어하고 Host는 stdio MCP 프로세스를 제어한다. MCP는 인증된 loopback HTTP로 브로커를 호출하며 중앙 토큰과 WebSocket은 브로커가 관리한다. MCP 종료는 공유 브로커 종료로 이어지지 않는다.

CLI/설정·자격 파일·세션 4개·업무 HTTP 7개·중앙 신원·수신함/ACK provider는 구현되어 있다. MCP 도구의 최종 DTO 수렴(-97)과 실제 Host 전체 왕복(-66)은 별도다. [진행 현황](../PROJECT_STATUS.md)과 [검증 요약](verification.md)을 참고한다.

## 세션·전송 규칙

기존 [MCP→로컬 브로커 계약](../../packages/mcp-server/docs/broker-contract-v0.1.md)을 기준으로 아래 규칙을 적용한다. 중앙 신원은 [READY 정책](ready-identity.md), 세션 세부 HTTP와 소유권은 [로컬 세션](local-sessions.md)을 따른다.

### 채택한 MCP 규칙

- 탐색·메타데이터·MCP 도구 목록(`tools/list`)·`devlink_ping`은 세션을 활성화하지 않습니다. 업무 도구 `list_agents`는 별도이며 현재 MCP에서는 세션 활성화 후 Agent 등록 여부를 검사합니다. 미바인딩 세션에서는 `register_agent`만 허용합니다.
- MCP 프로세스의 고정 UUID `instanceId`로 세션을 식별합니다. 첫 세션 생성 시도는 `fresh`, 이후에는 응답 유실 여부와 관계없이 `recover`를 사용하며 같은 인스턴스의 세션 생성은 멱등적이어야 합니다. 브로커 자신의 `instanceId`와 구분합니다.
- 인증된 `fresh`는 해당 Agent의 기존 소유자를 교체할 수 있습니다. `recover`는 자기 세션 또는 빈 슬롯만 확보하며 다른 소유자가 있으면 `SESSION_CONFLICT`입니다. 교체된 세션은 `SESSION_REPLACED`로 처리하고 오래된 갱신·해제가 새 세션에 영향을 주지 않게 합니다.
- 브로커 재시작 후 같은 Agent의 빈 슬롯은 먼저 유효하게 복구한 인스턴스가 확보합니다. 이전 소유권은 보존하지 않으며 다른 Agent들은 각자 별도 슬롯을 사용합니다.
- MCP 갱신 기본 30초, 브로커 lease 기본 90초를 채택합니다. 브로커 수신 기준 시간으로 판단하고 클라이언트 시각은 신뢰하지 않습니다. 중앙 서버의 브로커 Heartbeat와 별도입니다.
- 세션 자동 복구는 기본 최대 5회, 1초부터 최대 30초 지수 백오프입니다. 소유권 충돌·교체·인증 거절·잘못된 자격 파일은 자동 복구를 중단합니다. 업무 요청은 자동 재전송하지 않습니다.
- 자격 파일은 `schemaVersion: 1`, `brokerUrl`, `localToken`, 선택적 `agentId`의 엄격한 스키마를 사용하며 `DEVLINK_BROKER_FILE`에 절대 경로를 전달합니다. 토큰은 16~4096자의 영문·숫자·`.`·`_`·`~`·`-`를 허용합니다. 중앙 토큰은 전달하지 않습니다.
- 자격은 일반 파일·16KiB 이하·비공개 권한이어야 하고 파일 및 직계 부모의 심볼릭 링크를 거부합니다. POSIX 소유권·파일 권한·부모 쓰기 권한, Windows 소유권·DACL은 현재 MCP 검사에 맞춥니다. CLI가 완결된 스냅샷을 원자적으로 발행합니다.
- MCP는 주소·토큰을 실행 중 고정합니다. 일반 브로커 재시작에는 토큰을 유지하고 명시적 회전에는 MCP를 재시작합니다. 주소를 바꿀 때도 현재 클라이언트를 다시 시작해야 합니다.
- 로컬 전송은 포트가 명시된 `http://127.0.0.1:<port>`와 Bearer 토큰·JSON이며 리다이렉트를 허용하지 않습니다. 세션 4개 경로와 업무 7개 경로·JSON은 MCP 계약을 따릅니다.
- 응답은 같은 `requestId`와 `data` 또는 `error`를 사용합니다. HTTP 401·403은 현재 MCP의 인증·권한 오류 해석을 따릅니다. 세션 오류 코드도 그대로 사용하되 테스트 대역이 쓰는 409를 모든 실패의 공동 상태 코드로 확정하지 않습니다.
- 요청 기본 제한 10초(응답 본문 수신 포함), 요청 128KiB·응답 256KiB를 채택합니다. timeout·단절·취소·프로토콜 오류는 업무 결과 불명이며 중앙 작업을 임의 실패·취소·재할당하지 않습니다. 요청 추적 ID는 중앙 멱등성 보장의 대체물이 아닙니다.
- MCP 종료 시 진행 요청·타이머를 정리하고 최대 1초 동안 세션 해제를 시도합니다. 해제 실패는 MCP 종료를 막지 않으며 공유 브로커를 종료하지 않습니다.

## 프로세스와 자격 파일

CLI와 브로커는 동일 실행 파일의 별도 프로세스다. CLI의 숨겨진 broker 명령이 라이브러리를 실행하며, CLI 종료 뒤에도 브로커는 유지된다. Host 자동 실행·MCP 자동 등록·부팅 자동 실행은 제공하지 않는다.

Agent ID의 영속 저장·기존 ID 재사용·토큰 관리는 [Agent bootstrap](agent-bootstrap.md)에 구현되어 있다. 프로필이 있으면 발행 MCP 자격에도 해당 Agent ID를 포함한다. 하나의 브로커에서 Host별 프로필/토큰을 관리하는 제품 정책, 설치·기존 Host 설정 병합·백업, Node 배포 방식은 후속 항목이다.

## 로컬 연결과 제어 API

- 전송: HTTP/1.1 JSON, IPv4 `127.0.0.1`에만 바인딩. 기본 포트 43127, `localPort: 0`이면 자동 할당. MCP URL 검증과 맞추기 위해 포트 80은 거부합니다.
- 발견: Rust CLI는 설정 디렉터리 아래의 `.devlink-runtime/runtime.json`을 읽고 MCP는 같은 디렉터리의 `broker.json`을 읽습니다. `devlink [--config <FILE>] mcp path`는 자격 파일 경로만 출력하며 파일을 생성하지 않습니다. 브로커 기동 시 스냅샷을 발행합니다. `broker.lock`과 `control.lock`도 `.devlink-runtime/`에 저장하며, 설정 디렉터리당 한 인스턴스입니다. 구버전 브로커는 업그레이드 전에 구버전 CLI로 종료해야 합니다.
- 런타임 필드: `apiVersion`, `pid`, `port`, `instanceId`, `controlToken`. 토큰과 전체 런타임 JSON은 로그·Tool 응답에 포함하지 않습니다.
- 인증: `Authorization: Bearer <토큰>`을 사용합니다. CLI의 `controlToken`과 MCP의 `localToken` 모두 health를 조회할 수 있지만 `/shutdown`은 CLI 제어 토큰만 허용합니다. `Origin`이 있는 브라우저 요청은 거절하며 CORS 허용 헤더를 제공하지 않습니다.
- 클라이언트는 프록시를 끄고 loopback으로만 요청해야 합니다. `GET /health`의 서비스·API 버전·PID·instanceId를 확인합니다.
- 새 브로커마다 instanceId와 CLI 제어 토큰은 바뀌지만 MCP localToken은 정상·강제 종료 후 재시작에 유지합니다. `devlink mcp rotate`는 브로커 중지 중에만 새 MCP 토큰을 발행하며 start/stop과 잠금으로 조정합니다. MCP는 자격 파일을 실행 중 다시 읽지 않으므로 회전 후 재시작해야 합니다.
- 브로커는 현재 바인딩된 포트로 brokerUrl을 발행합니다. `localPort: 0`이나 설정 변경으로 주소가 달라지면 MCP도 다시 시작해야 합니다.
- 자격 파일은 16KiB 이하이며 원자적으로 발행하고 파일·직계 부모의 링크 및 Windows reparse point를 거부합니다. Windows 소유권·DACL 설정·검증은 실행 확인했고 Unix 소유권·권한 검사 코드는 실행 미검증입니다. 손상되거나 권한이 안전하지 않은 자격은 기동 시 오류로 처리합니다.
- 중앙 `DEVLINK_TOKEN`은 로컬 제어 토큰과 별개이며 MCP 클라이언트가 읽거나 반환할 필요가 없습니다.

OS 잠금은 실행 여부의 기준이며 런타임 파일의 PID만 보고 프로세스를 종료하면 안 됩니다. 잠금은 살아 있는데 파일/API가 손상된 경우 실행 여부를 `unknown/unhealthy`로 처리합니다.

## 구현된 API

| 호출             | 권한/책임                                              | 성공 응답                                   |
| ---------------- | ------------------------------------------------------ | ------------------------------------------- |
| `GET /health`    | CLI controlToken 및 MCP localToken으로 상태 조회 | 200, 아래 Health JSON                       |
| `POST /shutdown` | CLI controlToken만 허용. MCP localToken은 401 | 202, `{"apiVersion":1,"status":"stopping"}` |
| `POST /v1/sessions` | MCP localToken·검증된 신원으로 생성/복구 | 200, requestId와 data의 sessionId/principal |
| `POST /v1/sessions/{id}/bind` | 같은 instanceId·검증된 사용자/팀/Agent | 200, requestId와 data의 같은 세션·bound principal |
| `POST /v1/sessions/{id}/heartbeat` | MCP localToken·같은 세션/instanceId | 200, requestId와 빈 data |
| `DELETE /v1/sessions/{id}` | MCP localToken·같은 세션/instanceId | 200, requestId와 빈 data |

```json
{
  "service": "devlink-broker",
  "apiVersion": 1,
  "pid": 12345,
  "instanceId": "uuid",
  "centralServer": "Connected"
}
```

health/shutdown은 요청 본문을 사용하지 않습니다. Health의 상태 enum은 README에 정의되어 있습니다. 두 제어 경로의 인증 실패 401과 기본 404는 아래 legacy JSON입니다. 세션 경로는 이를 사용하지 않고 MCP 계약의 requestId+data/error envelope를 반환합니다. 지원하지 않는 HTTP method는 Axum 기본 405 응답이므로 본문 형식을 가정하지 않습니다.

```json
{
  "apiVersion": 1,
  "error": { "code": 401, "message": "Unauthorized" }
}
```

클라이언트 타임아웃 기준은 조회/종료 요청 1초, 종료 확인 8초입니다. 시작 확인은 기본 준비·health 여유 8초에 Windows 자격 검사의 최대 3회 × 5초를 더한 23초이며, Unix는 기존 8초입니다. ACL 검사를 생략하지 않고 자격 발행 전 준비 시간을 포함합니다. 네트워크 오류·잘못된 JSON·서비스/버전/인스턴스 불일치는 로컬 클라이언트 오류로 다룹니다. 서버 오류 원문, 토큰, 임의 중앙 payload를 Tool 오류에 노출하지 않습니다. 중앙 연결 실패는 HTTP 실패가 아닌 Health의 연결 상태로 표현합니다.

## 업무 HTTP와 MCP DTO

| MCP operation | 로컬 POST | 기준 문서 |
| --- | --- | --- |
| register_agent | /v1/agents/register | [기존 Agent 확인](business-relay.md) |
| list_agents | /v1/agents/list | [팀 목록](business-relay.md) |
| create_task | /v1/tasks/create | [작업 생성·requestKey](business-relay.md) |
| update_task_status | /v1/tasks/status | [상태 변경](task-operations.md) |
| complete_task | /v1/tasks/complete | [완료](task-operations.md) |
| get_tasks | /v1/tasks/list | [목록](task-operations.md) |
| get_task_result | /v1/tasks/result | [상세·결과](task-operations.md) |

register_agent는 최초 Agent 생성이 아니라 bootstrap 자격의 기존 Agent 확인이다. 목록·생성·상태 변경·완료는 bound 세션을 요구한다. 업무 권한·상태 전이·원자 저장은 중앙이 판단한다.

Rust provider가 구현되었다는 사실만으로 MCP의 모든 입력이 호환되는 것은 아니다. 현재 MCP의 version 없는 status/complete와 result:{summary,changedFiles,verification?}는 중앙의 version/resultText/taskFiles와 차이가 있다. Rust가 version을 추측하거나 verification을 조용히 버리지 않으며 최종 소비자 변경은 -97이다.

수신함은 [CLI inbox 조회](task-inbox.md)로 확인한다. Host는 기존 get_tasks(received)를 사용하며 새 push/tool이나 ACK 기반 자동 수락·편집을 추가하지 않는다. 실제 두 사용자 Host 업무 왕복은 -66으로 검증한다.
