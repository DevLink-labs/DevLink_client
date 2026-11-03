# READY 신원과 로컬 세션

2026-10-30 현재 코드 기준. 기본 CLI는 중앙 WebSocket의 ReadyIdentityProvider와 Agent Bearer `GET /api/v1/agents/me`를 연결한다. 이전 `/api/v1/agent/context`와 READY 신원 누락 설명은 [과거 세션 검증 이력](archive/session-verification-2026-10-29-30.md)에 남긴다.

## 신원 공급과 재검증

- CLI는 검증된 중앙 origin에 고정된 ContextClient를 CentralState에 주입한다. 중앙 토큰은 MCP에 전달하지 않는다.
- claim/bind/heartbeat는 현재 중앙 context를 조회해 ownerUserId/teamId/agentId를 대조한다. heartbeat는 로컬 만료·소유권·instance 검사를 먼저 통과해야 하며 조회 후에도 재검사한다. 캐시만으로 lease를 갱신하지 않는다.
- 업무 authorize는 로컬 세션·principal을 먼저 검사한다. 재접속 직후 legacy READY 신원이 미확정일 때 잠금 밖에서 context를 조회한 뒤 최종 재검증한다. 이미 확인한 신원의 반복 인가는 추가 REST 조회를 만들지 않는다.
- 현재 WS가 Connected이고 REST agentId와 READY agent_id가 같아야 한다. READY에 user/team이 있으면 함께 일치해야 한다. 응답 적용 시 generation·connection_id·agent_id를 다시 비교한다.
- 동시 요청이 user/team을 먼저 보완한 것은 새 연결로 취급하지 않는다. 이미 확인한 신원과 모순되는 성공 응답 및 같은 연결의 401/403은 신원을 무효화한다. 인증 거부 후 늦은 200이 권한을 복원하지 못하고, 재접속 전 요청의 응답은 새 연결에 적용하지 않는다.
- REST는 redirect·proxy·자동 retry·cookie를 사용하지 않는다. 본문 포함 3초·256KiB 상한이며 오류 원문·자격을 복사하지 않는다.

## READY 계약

```json
{
  "type": "BROKER_READY",
  "payload": {
    "connection_id": "central-session-id",
    "agent_id": 12,
    "user_id": 3,
    "team_id": 1
  }
}
```

ID는 양의 Java Long 범위 숫자다. 로컬 Principal에는 정규 십진 문자열로 반환한다. null·문자열·0·음수·범위 초과 또는 user/team 중 하나만 있는 READY는 거부한다. toolType은 신원 응답에 없으므로 config나 요청 인자에서 추측하지 않는다.

user_id/team_id가 모두 없는 legacy READY는 WS 연결·heartbeat를 허용한다. 기본 CLI는 같은 Agent의 실시간 context로 사용자·팀을 확인해야 세션을 발급한다. ContextClient가 없는 테스트 공급부에서는 READY에 전체 신원이 있어야 한다. 요청 ID·설정 파일로 부족한 신원을 만들어내지 않는다.

## 소유권·인증 무효화

- agentId 없는 claim은 unbound Principal이다. 검증된 같은 Agent로 bind하거나 해당 ID를 명시한 claim만 bound 세션을 얻는다. 불일치는 422 FORBIDDEN(domain)이다.
- 같은 신원의 새 WS는 lease 안의 소유권을 유지한다. Agent·사용자·팀이 바뀌면 subject epoch를 바꿔 이전 세션을 무효화한다. A→B→A로 돌아와도 과거 소유권을 복원하지 않는다.
- legacy 재접속 직후에는 신원 확인까지 OFFLINE을 유지한다. 성공한 claim/bind/heartbeat가 현재 신원을 확인한 뒤 실제 bound 슬롯에 따라 presence를 게시한다.
- 중앙 인증 거부·신원 모순은 식별자를 제거하고 AuthFailed/OFFLINE으로 전환하며 해당 WS를 종료한다. 자동 재접속을 중단하고 이전 세션은 epoch 변화로 만료된다.
- TASK_CREATED와 대기 ACK는 현재 generation·Connected·Agent 검사를 통과해야 한다. 진행 중인 디스크 기록은 보존될 수 있지만 무효화 이후 ACK를 보내지 않는다.
- claim/bind/heartbeat의 최종 검증·저장·presence 게시를 중앙 신원 read lock 안에서 처리한다. 단절 중 heartbeat는 lease를 연장하지 않는다.
- DELETE는 로컬 sessionId/instanceId로 해제하며 중앙 재인증을 요구하지 않는다. 중앙 불가 상태에서도 로컬 해제·OFFLINE 정리가 가능하다.

## 관련 기능과 검증

로그인·팀·Agent 준비는 [Agent bootstrap](agent-bootstrap.md), 세션 HTTP·lease는 [로컬 세션](local-sessions.md), 업무 REST는 [중계](business-relay.md)·[조회/변경](task-operations.md), 저장 후 ACK는 [수신함](task-inbox.md)을 따른다.

중앙 `02de434`의 확장 READY와 실제 Rust·SDK·PostgreSQL 세션/presence/90초 lease/재시작/토큰 교체 검증은 [세션 이력](archive/session-verification-2026-10-29-30.md)에 보존한다. 최신 -95의 중앙 `0e8c2b7` 시험은 [검증 요약](verification.md)을 참고한다. 실제 Host 업무 전체 왕복·누락 복구·Ubuntu 실행까지 완료했다는 의미는 아니다.
