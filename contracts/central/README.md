# 중앙 연동 결정표 (S15P31D108-88)

작성일: 2026-10-29 (Asia/Seoul). **설계 산출물이며 실행 가능한 중앙 adapter나 실제 연동 완료 기록이 아니다.**

| 확인 대상 | develop 기준 | 확인 방법 |
| --- | --- | --- |
| client | `b8bd142f148b87693e4639bac507b7f5f6b914f4` | Rust router/protocol, MCP consumer/schema |
| 중앙 server | `5891ef2a6adb4a3d3a7c7e89a5c064386a9c1bb9` | production controller/security/service/DTO |

[기존 로컬 신원 schema](../local-broker/v1/README.md), [4개 세션·7개 업무 경로](../../packages/mcp-server/docs/broker-contract-v0.1.md), [오류 소비자](../../packages/mcp-server/docs/error-contract.md)는 재사용한다. 이 문서는 실제 중앙과의 차이만 정리한다. [adapter-decisions.json](adapter-decisions.json)은 이 결정표의 기계 판독용 목록이며 런타임 설정이 아니다. `null` 경로·자격은 미제공/미확정이라는 뜻으로, 추측한 URL로 요청하지 않는다.

## 1. 자격과 신원의 경계

| 자격 | 보유자·용도 | 현재 확인 | 후속 결정/구현 |
| --- | --- | --- | --- |
| 중앙 사용자 세션·CSRF 자격 | CLI 사용자 관리 명령의 로그인·팀·Agent 관리 REST | TaskController는 `@SessionAttribute("userId")`를 요구한다. 로그인 구현/세션 발행·조회·종료·CSRF 교환은 없다. SecurityConfig의 `/api/**`는 사용자 인증을 요구한다. | -80/-86이 실제 cookie/CSRF·만료·로그아웃 계약 제공 → -89 소비 |
| 중앙 Agent Bearer | Rust → `/ws/broker`, 현재 DEVLINK_TOKEN 환경변수에서 읽음 | WS 전용 AgentTokenAuthenticationFilter와 AgentTokenVerifier 인터페이스. production verifier 구현은 없고 fail closed. | -49/-84 제공 → -91 검증된 프로필 자격으로 연결. WS 토큰을 사용자 REST 세션으로 취급하지 않음 |
| **Rust 업무 REST 자격** | Rust → 중앙 Agent 조회·업무 REST | **결정 필요.** WS의 ROLE_AGENT 필터는 REST에 적용되지 않는다. 현재 Task 생성은 사용자 세션을 요구한다. | -80/-84/-85/-86, -67/-69/-75/-76에서 각 endpoint의 지원 principal/권한 확인 → -92/-93/-94. 지원되지 않은 Bearer나 임의 userId 헤더 금지 |
| MCP `localToken` | CLI가 프로필 전용 `broker.json`에 발행, MCP → Rust | Rust 발행·권한 검증·정상 재시작 토큰 재사용, `/health` 조회는 구현. 업무 API는 아직 없음 | -91에서 검증한 Agent ID를 스냅샷에 포함; -92/-93/-94에서 인증·권한 소비 |
| CLI `controlToken` | 실행별 private `runtime.json`, CLI → Rust `/shutdown`·`/health` | 구현됨. MCP localToken으로 shutdown 불가 | 기존 -46/-50 구현 재사용. MCP로 전달하지 않음 |

중앙 세션 cookie·CSRF·Agent 토큰·비밀번호를 MCP 환경/응답/로그에 넣지 않는다. MCP에는 private 자격 파일 **경로**만 전달한다. 사용자 세션 저장은 -89에서 사용자 전용 권한·원자 교체·로그아웃 후 제거를 구현한다. 사용자 로그아웃이 기존 Agent 토큰이나 WS 연결까지 폐기한다는 보장은 현재 없으며, 서버가 실제로 지원하는 폐기 범위만 표시한다.

검증된 신원은 중앙 응답의 `userId`, `teamId`, `agentId`, 필요 시 `toolType`으로 구성한다. CLI의 `Config.agent`는 문자열 실행 설정이며 중앙 Agent ID가 아니다. 현재 `BROKER_READY`는 `connection_id`와 숫자 `agent_id`만 반환한다. **READY만으로 user/team principal을 만들 수 없다.** 도구 인자·파일의 Agent claim·환경변수로 user/team을 보충하지 않는다. 후속 -91/-92는 중앙의 인증된 사용자/멤버십/Agent 소유 확인과 현재 WS Agent를 대조한 뒤에만 세션을 발행한다. 미확인 상태는 거부하고 ONLINE을 보고하지 않는다.

**-92의 권위 출처·신선도는 결정 필요다.** 선택지는 중앙 READY에 검증한 owner/team을 포함하는 계약(-49/-51), 또는 Rust가 지원 자격으로 현재 신원/멤버십을 재확인하는 계약(-85/-86)이다. 어느 쪽도 현재 제공되지 않았다. -91이 저장한 bootstrap 시점 정보는 Agent 선택의 근거 기록일 뿐, 현재 멤버십·권한을 증명하는 인증 principal로 그대로 복원하지 않는다. 탈퇴·삭제·폐기 반영과 재확인 시점을 정하기 전 -92 실제 principal 발행은 blocked다.

## 2. 단일 Agent 생성과 bootstrap 순서

Agent 생성의 서버 측 소유자는 기존 **-84**다. CLI와 MCP가 독립적으로 Agent를 생성하지 않는다. 아래는 **client 구현 방향**이며 아직 없는 중앙 API의 승인된 URL/멱등성 보장을 뜻하지 않는다.

1. -89: CLI 사용자 로그인 및 실제 세션 확인. 로그인되지 않았으면 사용자 세션이 필요한 CLI 명령을 중단한다.
2. -90: 팀 생성/초대 참여/목록에서 검증된 멤버십으로 팀을 선택한다. 선택 파일만 바꿔 현재 Agent의 팀 권한을 바꾸지 않는다.
3. -91: **프로필에 명확한 Agent ID와 유효한 Agent 자격을 모두 보유하고, 중앙이 현재 사용자 소유·팀·도구를 확인하며, 해당 자격의 READY agent_id도 일치하는 경우에만** 같은 Agent를 재사용한다. 이름·owner/team/toolType 조합이나 목록 순서로 자동 채택하지 않는다. 최초 등록은 **사용자의 명시적 등록 의도에 따라 -84만 호출**한다. 생성 후 중앙 확인 ID·소유/팀/도구 정보와 Agent 자격을 프로필에 바인딩한다. 이 기록만으로 이후 현재 권한이 보장되지는 않는다.
4. Agent 생성 응답 유실 시 자동 재생성하지 않고 **결과 불명/blocked**로 둔다. -85 목록은 유실된 생성 요청의 동일성이나 토큰 소유를 증명하지 못한다. -84의 등록 요청 멱등성 또는 -84/-86의 안전한 동일성 확인·자격 재발급 계약이 해제 조건이며, 현재 없는 Agent 갱신/토큰 회전 API를 가정하지 않는다.
5. Rust가 해당 Agent 자격으로 연결하고, 검증된 프로필과 READY의 Agent가 일치하는지 확인한다. -91은 private 자격 발행·`mcp path`·수동 Host 설정 예시와 MCP 자격 읽기까지 담당한다. 실제 세션 활성화는 -92다.
6. -92: 유효한 업무 호출에서 로컬 세션을 지연 생성한다. MCP `register_agent`는 **bootstrap으로 정해진 같은 Agent를 확인하고 bind하는 경로**로 연결하며 **-84 생성 API를 호출하지 않는다.** Agent ID/자격이 없는 프로필은 CLI bootstrap을 먼저 안내한다. 현재 Rust broker.json에는 Agent ID가 아직 없으므로 -91의 발행 변경이 필요하다. MCP 인자(name/toolType/role)를 새 Agent 생성 권한이나 갱신 API로 해석하지 않는다. 다른 Agent/팀/도구를 요구하면 프로필 전환을 안내한다.

MCP 단독 최초 생성이 추후 필요하면 -84의 동일 생성 경로·멱등성·자격 발행을 먼저 합의해야 한다. 현재 구현 경로로 간주하지 않는다. 현재 MCP DTO의 `register_agent` 설명은 -97에서 이 의미에 맞게 수렴시키며 기존 7개 업무 도구를 추가하지 않는다.

정책은 **사용자당 활성 broker 1개**다. 사용자당 Agent를 1개로 제한하는 정책은 추가하지 않는다. 현재 프로필별 파일 잠금과 중앙의 Agent별 연결 교체만으로 사용자 단위 정책이 완성됐다고 볼 수 없다. 사용자 단위 동시 활성화의 강제 지점은 -49/-84/-86과 결정해야 한다. 정책이 확인되기 전 여러 프로필/Host 병렬 실행을 MVP 지원으로 안내하지 않는다.

현재 client는 **broker 1개 = Agent 자격 1개 = Agent WS 1개**다. 사용자당 broker 1개 정책을 강제하면 동시에 온라인인 Agent도 사용자당 1개가 되는 구조이며, 등록할 수 있는 Agent 수를 제한한다는 뜻은 아니다. broker 하나가 여러 Agent를 운반할지 여부는 -49/-51/-84/-86과 별도 결정 필요다. 현재 다중 Agent 동시 온라인을 지원한다고 안내하지 않는다.

Host/프로필/팀/Agent 전환은 현재 MCP를 종료하고 선택한 프로필의 자격으로 다시 시작한다. 중앙 자격 교체도 Rust/MCP 재시작 경계를 따른다. 실행 중 MCP principal의 user/team을 바꾸지 않는다.

### 세션 소유권과 presence

기존 사용자 결정과 로컬 계약을 유지한다.

- `fresh`: 인증된 새 시작은 기존 소유자를 교체할 수 있다. 같은 instance의 중복 claim은 멱등적으로 처리한다.
- `recover`: 같은 instance의 소유 세션 또는 빈 슬롯만 얻는다. 타 instance가 소유 중이면 `SESSION_CONFLICT`; 복구가 소유권을 빼앗지 않는다.
- Rust 재시작으로 슬롯이 사라지면 **먼저 유효하게 복구한 쪽**을 인정한다. 과거 마지막 소유자 복원을 보장하지 않는다.
- heartbeat/delete/bind와 업무 권한 검사는 sessionId+instance/현재 세대를 확인한다. 오래된 요청의 cleanup이 새 owner의 lease/presence를 지우면 안 된다.
- lease는 Rust 수신 시계 기준 90초, MCP heartbeat 기본 30초다. 만료·명시 해제·교체는 현재 세션에 대해서만 처리한다.
- -92는 이미 있는 `CentralState::set_presence` → `AGENT_PRESENCE` 경로를 연결한다. 확인된 bound 로컬 세션만 ONLINE 근거다. lease/해제 후 OFFLINE은 확인된 슬롯 상태에 따른다. `/health`·도구 목록·로컬 ping만으로 ONLINE이 되지 않는다.

## 3. 중앙 매핑 골격

| MCP operation | 현재 중앙 production | 후속 client | 아직 필요한 중앙 작업 |
| --- | --- | --- | --- |
| register_agent | 없음. CLI bootstrap Agent 확인/bind로 수렴. 생성 호출 금지 | -93/-97 (생성/자격은 -91) | 직접: -49/-85/-86의 신원·소유 확인. -84 생성은 -91의 간접 선행이며 이 route에서 호출하지 않음 |
| list_agents | 없음 | -93 | -85: 팀별 목록/신원·권한·상태 DTO |
| create_task | **POST `/api/v1/tasks/{teamId}`**, 사용자 세션 필요 | -93 | -67: actor/팀 권한·저장 전 offline 거부; -68: 동일키 동시성; -71: 연계 검증 |
| update_task_status | 없음 | -94 | -69: 수락/실패 권한·전이·사유·부분수정 |
| complete_task | 없음. 엔티티 메서드만 존재 | -94/-97 | -75/-77: 완료·실패/파일/검증·트랜잭션 |
| get_tasks | 없음 | -94/-97 | -76: 방향/상태/커서/제한·접근 권한, 최신 delivery/status·reasonCode·deliveredAt 노출 여부 |
| get_task_result | 없음 | -94/-97 | -76: 결과/실패/부분수정/변경 파일·권한, 최신 delivery/status·reasonCode·deliveredAt 노출 여부 |

중앙 URL이 없는 행은 기능별 provider가 머지된 후 대조한다. 사용자 관리 REST의 경로도 SecurityConfig의 permitAll 문자열만으로 존재한다고 보지 않는다. 실제 production controller/서비스가 근거다. Rust는 중앙 도메인 정책·DB·권한을 복제하지 않는다.

### ID·생성 DTO

중앙의 user/team/Agent/task ID는 양수 Java `Long`, MCP 신원/도구 ID는 문자열이다. client adapter 방향은 **양수 signed 64-bit 범위의 정규 10진수 문자열**이다. 공백·부호·leading zero·opaque 예제 ID를 실제 중앙 ID로 변환하지 않는다. Rust에서 정수로 검증/직렬화하고 MCP 응답에는 정확한 십진 문자열로 내보낸다. JavaScript `Number` 왕복으로 2^53 초과 ID를 반올림하면 안 된다. -93/-94/-97의 경계 테스트에 최대 Long·오버플로·잘못된 ID를 포함한다. 이 규칙은 기존 로컬 fixture의 opaque 식별자를 전면 금지하거나 schema를 지금 변경하는 작업이 아니다.

현재 MCP 도구 ID schema는 trim을 적용하므로 원래 입력 공백을 Rust가 복원할 수 없다. -97에서 실제 중앙 ID 규칙과 trim 정책을 함께 수렴시킨다. 중앙 wire는 JSON 숫자이고, MCP 문자열 encoding은 client 방향이라는 점을 구분한다.

| 입력/출력 | adapter 방향 | 상태 |
| --- | --- | --- |
| path `teamId` | 검증한 session principal.teamId | actor를 도구 인자로 받지 않음 |
| requesterAgentId | 검증한 bound principal.agentId | 중앙도 소유/팀 검증 필요(-67) |
| receiverAgentId | MCP recipientAgentId → 정수 | 중앙 수신자·팀·online 검증 필요 |
| prompt | `title + "\n\n" + description` 및 선택 `"\n\n[Context]\n" + taskContext` | client 제안: 필드를 버리지 않는 결정적 매핑. 중앙 길이/저장 정책은 -67/-86 확인 필요 |
| requestKey | 논리 생성 요청당 UUID 1회. 중앙 상한64 이내 | 생성/복구 정책은 아래 참조 |
| taskId/receiverAgentId/status/version/replayed/delivery | 중앙 ApiResponse.data를 검증해 MCP data에 보존. ID만 문자열로 변환 | 실패/전달 결과를 업무 상태로 합치지 않음 |

원문 길이/중앙 저장 한도를 넘으면 전송 전에 거부하고 내용을 자르지 않는다. 현재 MCP 요청128KiB·응답256KiB 제약도 함께 지킨다. 중앙 prompt/결과의 확정 크기와 버전 필드/시각 의미는 후속 provider에서 확인한다.

### 2026-10-29 중앙 ACK 변경 반영

`f72446d` 감사 이후 중앙 PR #13이 develop `5891ef2`에 반영됐다. 현재 CreateTaskResponse.delivery는 **nullable**이고, 다음 중앙 TaskEventType을 반환한다.

| 중앙 delivery.status | 해석 | 업무 상태에 미치는 영향 |
| --- | --- | --- |
| null | 전달 결과 미기록. 이미 전송됐을 수도 있으며 미송신을 뜻하지 않음 | PENDING 보존 |
| ACK_PENDING | 소켓 쓰기 완료, ACK 미확인 | 전달 완료/수락 아님 |
| ACK_RECEIVED | 현재 연결의 유효한 event_id/task_id ACK 저장 | deliveredAt 저장, 업무 status 변경 없음 |
| DELIVERY_NOT_SENT | OFFLINE/연결 없음/교체/작업 상태 등의 미송신 사유 | 생성된 taskId/status 보존 |
| DELIVERY_UNKNOWN | 전송·ACK 만료·연결 종료·이력 저장/조회 실패 등 | 생성된 taskId/status 보존, 자동 재전송 없음 |

ACK_PENDING/ACK_RECEIVED의 reasonCode는 null이며 실패 상태의 reasonCode는 필수다. `HISTORY_RECORD_FAILED`, `HISTORY_READ_FAILED`, `ROUTING_ERROR`도 생성 성공 응답의 전달 불명 사유일 수 있다. 이때 `TASK_SAVE_FAILED`나 새 생성 실패로 바꾸지 않는다. ACK는 요청 HTTP 안에서 기다리지 않는다. **생성 응답의 delivery는 해당 시점 snapshot이며, -76이 최신 전달 상태/사유/deliveredAt 조회를 제공하기 전 최신 ACK는 미확인이다.** 현재 같은 키 create replay는 최신 이력을 반환하지만 이를 조회 수단으로 사용할지는 -68 계약으로 남기며 자동 호출하지 않는다. `version`과 delivery는 별도 조회 시점일 수 있어 하나의 원자 snapshot으로 단정하지 않는다.

**갱신된 판단:** 이전 감사의 “SENT를 즉시 DELIVERED로 처리, ACK DB 반영 없음”은 최신 소스에 해당하지 않는다. 소스 반영과 실제 DB·네트워크 통과는 구분한다. 신규 저장 전 offline 거부/요청자 소유 검증, 동일키 동시 경합 재사용은 여전히 미구현이며 Rust에서 우회하지 않는다. 중앙이 생성 후 DELIVERY_NOT_SENT를 반환했다고 이미 저장한 작업을 삭제하거나 저장 전 거부였다고 표시하지 않는다.

### 오류·상관관계

중앙 ApiResponse(`success/data/code/message`)를 로컬 requestId/data-or-error envelope로 변환한다. HTTP/JSON/필드 검증 실패는 결과 불명이다. 중앙 원문 message·stack·토큰·cookie를 Host로 복사하지 않는다.

| 중앙 상황 | 로컬 방향 | 제약 |
| --- | --- | --- |
| AUTHENTICATION_REQUIRED / 401 | CENTRAL_UNAUTHENTICATED, domain scope, **로컬 HTTP401 금지** | 중앙 재로그인 필요. localToken 실패/세션 교체와 구분 |
| ACCESS_DENIED / 403 | BROKER_FORBIDDEN, domain scope | local_auth로 바꾸지 않음 |
| VALIDATION_FAILED / 400 | VALIDATION_FAILED, domain/rejected | 전송 전/중앙 검증 거부만 |
| RESOURCE_NOT_FOUND / 404 | operation별 receiver/task 부재로 합의 후 매핑 | endpoint 미제공404와 구분; 하나의 도메인 코드로 무조건 변환 금지 |
| REQUEST_KEY_CONFLICT 또는 DUPLICATE_REQUEST / 409 | -68 이전은 **BROKER_REJECTED + 명시적 outcome=unknown**, domain scope, 알려진 키 보존. taskId는 실제 알려진 경우만 | 다른 경합 요청이 저장했을 수 있음. 성공 재해석·재조회로 숨김·새 키/자동 재전송 금지 |
| RECEIVER_OFFLINE | 저장 전 거부가 보장될 때만 rejected | 생성 후 전달 이력 실패와 구분 |
| 5xx·응답 유실·타임아웃·취소 | 안전한 CENTRAL_UNAVAILABLE/CENTRAL_TIMEOUT 또는 프로토콜 오류, unknown | HTTP 코드만으로 저장 안 됐다고 단정 금지 |

로컬 HTTP401은 local_auth 실패 전용이다. MCP는401이면 scope와 무관하게 로컬 자격 오류로 자동 복구를 중단하므로 중앙401을 그대로 전달하지 않는다. 중앙 인증/권한 실패는 유효한 로컬 envelope와 domain scope로 전달하며, 특히 세션 control 경로에는 중앙 거부를 로컬403으로 반환하지 않는다. 구체적 비401/403 HTTP status는 -92/-93에서 기존 [오류 계약](../../packages/mcp-server/docs/error-contract.md)과 소비자 테스트로 확정한다. 표에 없는 중앙 code/status 조합은 안전한 BROKER_SERVER_ERROR 또는 프로토콜 오류 + **outcome unknown**, 알려진 식별자 보존, 원문 비복사가 기본이다. 명시적 unknown은 코드의 rejected 기본값보다 우선해야 한다. CSRF 거부와 업무 권한 거부를 분리할지는 -86/-93에서 확인한다.

MCP requestId는 호출 상관관계 UUID, mcpRequestId는 Host 진단값, requestKey는 중앙 멱등키다. 서로 대체하지 않는다. 알려진 taskId/requestKey는 실패에도 보존한다. 현 MCP는 응답 envelope의 알려진 requestKey를 보존하지만 **Host 생성 입력에 requestKey가 없고 broker가 응답하기 전 단절 시 새 키를 알 수 없다.** -93/-97에서 키 생성/내구 기록·노출·재조회 계약을 수렴시켜야 하며 이미 end-to-end 복구가 된다고 표현하지 않는다.

중앙 현재 멱등 범위는 `(requesterUserId, requestKey)`. 같은 키의 team/requesterAgent/receiverAgent/prompt가 같으면 기존 작업을 반환하고, 내용이 다르면 충돌이다. 같은 내용의 동시 unique 경합을 기존 작업으로 수렴시키는 보장은 아직 없다(-68).

권장 client 방향은 -93의 논리 생성 진입점에서 키를 1회 발급하고 private 요청 기록에 키와 식별 가능한 입력을 보존하는 것이다. 이미 알려진 taskId는 -94 상세 조회로 확인한다. taskId 없는 응답 유실은 키로 찾는 실제 조회 계약이 필요하다(-68/-76/-86). 현재 endpoint가 없으므로 수동 확인/미검증으로 남기며 무조건 create 재호출하지 않는다. 세션만 복구할 수 있으며 업무 자동재전송·자동 재할당·timeout으로 FAILED 전이는 금지한다.

### 예산

| 구간 | 기존 값/동작 | 남은 합의 |
| --- | --- | --- |
| 중앙 ACK | 기본5000ms (`devlink.broker.ack-timeout-ms`), 소켓 전송 직전 대기 등록 | 생성 HTTP는 ACK를 기다리지 않음. 별도의 전달 결과 |
| Rust → 중앙 업무 REST | 아직 client 없음 | -93: 응답 본문까지의 deadline·크기·취소, 자동 retry 없음 |
| MCP → Rust 한 HTTP | 기본10초, 응답 본문 수신 포함 | Rust가 이 안에 안전한 응답을 반환할 예산과 여유 합의 |
| MCP 전체 도구 호출 | 기본30초, queue/세션/등록/bind/업무 포함 | 개별 timeout의 단순 합산으로 무한 연장하지 않음 |
| MCP 종료 세션 해제 | 최대1초 best effort | 업무 취소나 중앙 task 취소를 뜻하지 않음 |

ACK5초 < MCP HTTP10초라는 숫자만으로 운영 SLA를 보장하지 않는다. 생성 HTTP의 socket write 상한·REST 예산과 deadline 전파는 -54/-67/-93에서 확인한다. deadline 이후의 결과는 unknown이며 업무 요청을 재생하지 않는다.

## 4. 수신·결과와 담당별 결정 필요 목록

현재 중앙 TASK_CREATED에는 top-level event_id, payload의 숫자 task_id/team_id/requester_agent_id/receiver_agent_id/prompt/created_at이 있다. Rust는 아직 이를 처리하지 않는다. -95에서 사용자 전용 내구 수신함 저장·중복 event 방지 후에만 `ACK`의 payload.event_id/task_id를 송신한다. 저장 실패에는 성공 ACK를 보내지 않는다. event 보관/정리/용량과 재시작 중복 정책도 같은 스토리에서 검증한다. **ACK ≠ 목록 읽기 ≠ 명시적 수락 ≠ 업무 완료**다. CLI inbox가 사용자 확인 표면이며 Host는 기존 get_tasks(received)를 사용한다. 새 MCP push나 자동 수락/실행을 추가하지 않는다.

중앙 TaskFile에는 relativePath/changeType/diffText와 ADDED/MODIFIED/DELETED/RENAMED가 있지만, 엔티티만으로 업로드/조회 API가 제공된 것은 아니다. 현재 MCP complete_task는 파일 경로 문자열 배열과 summary/verification만 받는다. -75/-76/-77에서 파일 타입·diff/내용·rename·크기/경로 정책·검증 결과·실패/부분수정 DTO를 확인한 후 -97에서 기존7개 도구의 schema/예제/출력과 맞춘다. 파일 내용을 MCP가 임의로 읽어 전송하지 않는다.

| 결정 필요 | 기존 담당 이슈 | 소비하는 신규 스토리 / 해제 근거 |
| --- | --- | --- |
| 로그인/상태/종료·cookie/CSRF·세션 수명 | 박성수 -80/-86 | -89: 실제 API와 사용자 세션 test |
| 팀 생성/참여/멤버십·선택 권한 | 박성수 -81/-82/-83 | -90: 실제 membership 거부/성공 test |
| Agent 단일 생성·재사용·토큰/실신원·사용자당 활성 broker 강제·broker/Agent cardinality | 박성수 -84/-85/-86, 최보경 -49/-51 | -91/-92: 검증된 같은 Agent/profile/READY 및 경쟁 실행 |
| 세션 principal user/team의 권위 출처·멤버십/폐기 반영·신선도 | 최보경 -49/-51(READY), 박성수 -85/-86(실시간 조회 자격) | -92: READY 확장 또는 중앙 재조회 계약 선택 전 실제 principal 발행 blocked. bootstrap 파일만으로 권한 복원 금지 |
| Rust 업무 REST/목록 자격·principal/범위 | 박성수 -80/-84/-85/-86, 업무 담당 -67/-69/-75/-76 | -92/-93/-94: 실제 인증·권한 소비 test; WS Bearer를 REST에 추측 적용 금지 |
| offline 저장 전 거부·요청자 소유·prompt 길이 | 이정민 -67/-71 | -93: 실제 중앙 저장 유무/권한 test |
| 동일키 동시성·키로 결과 찾는 방법 | 이정민 -68, 지상근 -76, 박성수 -86 | -93/-97: 동일키·응답유실 복구 검증, 조회 API 없는 동안 보장하지 않음 |
| ACK 이력·수신 경계·최신 전달 상태/사유/deliveredAt 조회 | 최보경 -53/-54, 이정민 -71, 지상근 -76 | -95/-93/-94: 최신 실제 socket/DB·조회 검증. 소스 ACK 개선은 이미 반영됨 |
| 수락/실패/완료/목록/결과 최종 DTO | 이정민 -69, 지상근 -72/-73/-75/-76/-77 | -94/-97: 실제 controller/service/transaction 검증 |
| 상태·결과 알림 event/payload | 최보경 -55, 이정민 -71, 지상근 -76 | -96: 내구 표시·중복·재접속, -94 조회로 대조 |

이 표는 의사결정 요청의 기록이며 기존 이슈 담당 변경·새 중앙 도메인 구현의 근거가 아니다. 실제 담당자에게 댓글/메시지를 발송한 기록도 아니다.

## 5. 실행 순서와 완료 판정

1. **-88**: 이 결정표/기계 목록·source 대조를 산출물로 판정한다. 미정 항목을 숨기거나 외부 API 머지를 -88 문서 완료의 전제조건으로 만들지 않는다.
2. **-89 → -90 → -91 → -92**: CLI 사용자 자격/팀/Agent bootstrap/세션. 확인된 provider 없이 신원·권한·token을 조립하지 않는다.
3. **-93 + -95**, 이어서 **-94**, 이후 **-96/-97**. adapter는 직접 로컬 HTTP→Rust→중앙으로 기능 시험하고 최종 MCP DTO와 시험 의존 순환을 만들지 않는다.
4. 기존 **-52** 전체 Rust 통합과 **-66** 두 사용자 A→B/B→A 실제 Host 코드 수정/검증/결과 조회로 닫는다. 새 최종 E2E 스토리를 만들지 않는다.

설계/독립 모듈 개발은 가능하지만, 미제공 API의 mock 통과를 실제 연동 완료로 기록하지 않는다. 각 스토리는 provider SHA·성공/거부/응답유실·테스트 구간·미검증 수용 기준을 남긴다. 현재 -89~97은 실제 중앙 통합이 미검증이며, 이 문서는 그 상태를 바꾸지 않는다.

## 소스 근거

링크는 위 SHA에 고정했다. 기존 감사(f72446d)의 지적을 최신 코드에 그대로 적용하지 않는다.

- [중앙 SecurityConfig](https://github.com/SSAFY-URAYAHA/DevLink_server/blob/5891ef2a6adb4a3d3a7c7e89a5c064386a9c1bb9/src/main/java/com/devlink/server/config/SecurityConfig.java), [AgentTokenVerifier](https://github.com/SSAFY-URAYAHA/DevLink_server/blob/5891ef2a6adb4a3d3a7c7e89a5c064386a9c1bb9/src/main/java/com/devlink/server/service/AgentTokenVerifier.java)
- [중앙 TaskController](https://github.com/SSAFY-URAYAHA/DevLink_server/blob/5891ef2a6adb4a3d3a7c7e89a5c064386a9c1bb9/src/main/java/com/devlink/server/controller/TaskController.java), [TaskCreationService](https://github.com/SSAFY-URAYAHA/DevLink_server/blob/5891ef2a6adb4a3d3a7c7e89a5c064386a9c1bb9/src/main/java/com/devlink/server/service/TaskCreationService.java), [생성/ACK 구현 기록](https://github.com/SSAFY-URAYAHA/DevLink_server/blob/5891ef2a6adb4a3d3a7c7e89a5c064386a9c1bb9/docs/task-creation-implementation.md)
- [중앙 TaskCreatedMessage](https://github.com/SSAFY-URAYAHA/DevLink_server/blob/5891ef2a6adb4a3d3a7c7e89a5c064386a9c1bb9/src/main/java/com/devlink/server/dto/TaskCreatedMessage.java), [WS ACK handler](https://github.com/SSAFY-URAYAHA/DevLink_server/blob/5891ef2a6adb4a3d3a7c7e89a5c064386a9c1bb9/src/main/java/com/devlink/server/handler/DevLinkWebSocketHandler.java), [TaskFile](https://github.com/SSAFY-URAYAHA/DevLink_server/blob/5891ef2a6adb4a3d3a7c7e89a5c064386a9c1bb9/src/main/java/com/devlink/server/domain/taskfile/TaskFile.java)
- [client Rust router](https://github.com/SSAFY-URAYAHA/DevLink_client/blob/b8bd142f148b87693e4639bac507b7f5f6b914f4/rust/crates/devlink-broker/src/broker.rs), [READY/protocol](https://github.com/SSAFY-URAYAHA/DevLink_client/blob/b8bd142f148b87693e4639bac507b7f5f6b914f4/rust/crates/devlink-broker/src/protocol.rs), [central state/presence](https://github.com/SSAFY-URAYAHA/DevLink_client/blob/b8bd142f148b87693e4639bac507b7f5f6b914f4/rust/crates/devlink-broker/src/central.rs)
- [MCP schema/입력](../../packages/mcp-server/src/tools/business.ts), [MCP client/복구](../../packages/mcp-server/src/broker/client.ts), [MCP 예산](../../packages/mcp-server/src/broker/call-budget.ts)
- [Jira -88](https://ssafy.atlassian.net/browse/S15P31D108-88), [MVP 감사](https://www.notion.so/40835a4f3ca68148b14ae4cf73928d6e), [추가 스토리/기존 담당 경계](https://www.notion.so/40835a4f3ca681c0bcb8ce5384958057)
