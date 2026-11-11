# 담당 Agent 추천 중계 (S15P31D108-188, FIND-3-01)

MCP `find_agent({question, repoKey?})` → 로컬 `POST /v1/agents/find` → 중앙 `POST /api/v1/teams/{teamId}/agents/recommendations`.

점수 계산(역할 60·Git 40, 후보 선정)은 중앙이 한다. 브로커는 입력 한도를 미리 확인하고, 중앙 응답을 검증해 로컬 형식으로 바꾼다. 추천 전에 바인딩된 프로젝트의 작업 파일 목록을 중앙에 반영할 수 있다. 수신자 선택과 작업 생성은 별도 절차를 따른다. 추천 과정에서 LLM을 호출하지 않는다.

중앙 구현 기준: server `docs/agent-recommendation.md`. client가 확인한 커밋은 develop `d945f8b`(2026-11-09)와 E2E 고정 커밋 `aecdcc3`이며, 두 커밋의 요청·응답 DTO는 같다.

## 요청

| 필드 | 규칙 |
| --- | --- |
| question | 필수. MCP는 먼저 전체 길이 16,000 UTF-16 코드 단위 이하와 짝 없는 서로게이트 금지를 확인한다. 중앙 `String.strip`과 같이 Java 공백(`Character.isWhitespace`)을 양끝에서 뺀 뒤 1–1,000 코드포인트. 브로커는 뺀 값을 보낸다. NBSP(U+00A0)는 Java 공백이 아니므로 남고 길이에 들어간다 |
| repoKey | 선택. 소문자 16진수 64자리. 중앙은 요청자 브로커의 저장소 힌트로만 받고 검색 범위를 좁히지 않는다. `null`은 받지 않는다 |

- 사용자·팀·Agent ID 인자는 받지 않는다(`deny_unknown_fields`). bind된 세션의 Agent만 쓴다. 미바인딩 세션은 AGENT_UNREGISTERED다.
- 중앙 호출 전 `GET /api/v1/agents/me`로 연결 토큰이 세션의 사용자·팀·Agent와 같은지 확인하고, 확인된 팀 ID로 경로를 만든다. 다르면 중앙 추천을 호출하지 않고 FORBIDDEN이다.
- 입력이 한도를 넘으면 중앙을 호출하지 않고 400 VALIDATION_FAILED다. MCP 입력 스키마도 같은 규칙으로 먼저 거부한다.

## 응답

```json
{
  "status": "MATCHED",
  "evaluatedAt": "2026-11-09T02:00:00Z",
  "insufficientReason": null,
  "gitCoverage": {"state": "NOT_COLLECTED", "reason": "NO_TEAM_REPOSITORIES", "workingState": "NOT_COLLECTED"},
  "candidates": [{
    "agentId": "13", "name": "로그인 담당", "part": "BACKEND", "connectionStatus": "ONLINE",
    "score": 20.0, "scores": {"role": 20.0, "git": 0.0, "working": 0.0, "frequency": 0.0, "recency": 0.0},
    "matchedRoles": ["로그인"], "relatedFiles": [], "latestCommitAt": null, "workingObservedAt": null,
    "insufficientReason": null
  }]
}
```

- `agentId`·`repoId`는 문자열로 바꾼다(Java Long 보존). 그 밖의 값은 중앙 값을 그대로 둔다. 점수는 중앙이 소수 둘째 자리로 반올림한 표시 값이다.
- `connectionStatus`는 중앙 평가 시점의 상태다. 점수·순위에 반영되지 않으며, 작업 전달 결과는 `create_task`의 delivery로 따로 확인한다.
- `relatedFiles[].sources`는 `WORKING`·`COMMIT`만 남긴다. 중앙이 4차에서 값을 추가할 수 있으므로 모르는 값은 오류로 보지 않고 뺀다.
- 중앙 응답의 그 밖의 필드는 버린다.
- MCP는 이 결과를 `contracts/local-broker/v1/recommendation.schema.json`과 상태 일관성 규칙으로 다시 검증한다. 점수 상한(총점 100, 역할 60, Git 40, 작업 중 15, 횟수 15, 최근성 10)과 양의 점수 규칙(선정 후보는 0보다 크고 참고 후보는 0)은 MCP에서만 확인한다. 상태별 예제는 `recommendation.examples.json`이다(S15P31D108-189).

다음 경우는 CENTRAL_PROTOCOL_ERROR(unknown)다.

- `status`·`gitCoverage.state`·`reason`·`workingState`·`connectionStatus`가 알려진 값이 아니다. `AVAILABLE`에 reason이 있거나 `NOT_COLLECTED`에 reason이 없다.
- 후보가 5명을 넘거나, Agent ID가 중복되거나, 요청 Agent 자신이거나, Long 범위를 벗어난다.
- 상태와 후보 수·사유가 맞지 않는다. MATCHED는 1명, MULTIPLE_CANDIDATES는 2–5명이며 둘 다 사유가 없다. INSUFFICIENT_EVIDENCE는 `NO_ELIGIBLE_AGENTS`(후보 0명) 또는 `NO_MATCHING_EVIDENCE`(참고 후보 1명 이상, 각 후보 사유도 같음)다.
- 점수가 음수·NaN·무한대이거나, 이름·경로·역할 태그가 빈 문자열이거나, 관련 파일이 5개를 넘는다. 공백만인 이름(예: NBSP)은 중앙이 받아들이므로 거부하지 않는다. 팀원 한 명의 이름 때문에 팀 전체의 `find_agent`가 실패하면 안 되기 때문이다.

## 오류

| 중앙 | 로컬 | outcome |
| --- | --- | --- |
| 400 VALIDATION_FAILED | 400 VALIDATION_FAILED | rejected |
| 401 | 503 CENTRAL_UNAUTHENTICATED | rejected |
| 403 ACCESS_DENIED(팀 불일치·탈퇴) | 403 FORBIDDEN → MCP BROKER_FORBIDDEN | rejected |
| 503 GIT_ROUTING_UNAVAILABLE | 503 GIT_ROUTING_UNAVAILABLE | rejected. 추천 계산 실패가 확인됐다. 추천 요청 전 작업 파일 갱신은 이미 반영됐을 수 있다. MCP nextActions `check_network_then_retry`. 중앙은 추천 계산 중 `DataAccessException`만 이 코드로 바꾼다. 트랜잭션 시작 실패(500)나 앞선 `/agents/me` 실패는 아래 행으로 나온다 |
| 그 밖의 5xx | 502 CENTRAL_UNAVAILABLE | unknown |
| 시간 초과 | CENTRAL_TIMEOUT | unknown |

## 범위 밖과 남은 결정

- **IDX-3-02 작업 파일 재확인 트리거**: 2026-11-10 S15P31D108-186에서 추가했다. 이 라우트는 세션 확인 뒤 중앙 호출 전에 작업 파일을 한 번 더 보내고, 3초 안에 끝나지 않거나 실패해도 계속한다. [작업 파일 수집](working-files.md)을 참고한다.
- **repoKey 기본값**: 현재 프로젝트 저장소의 원격 URL 해시를 기본값으로 쓸지는 정하지 않았다. 해시 규칙은 [커밋 이력 동기화](commit-sync.md)(S15P31D108-185)와 [작업 파일 수집](working-files.md)이 쓰는 정규화한 원격 URL의 SHA-256이다. 지금은 호출자가 넘긴 값만 전달한다. 중앙은 이 값을 아직 점수에 쓰지 않는다.
- **결과 표시(FIND-3-02)·작업 전달 연계(REQ-3-01)**: S15P31D108-189·-190에서 다룬다. 이 스토리의 도구 설명과 Host 지침은 "후보를 보여주고 사용자가 고른 ID로만 create_task"까지만 정한다.

## 검증

- 단위: `agent_recommendations` 질문 공백·길이, repoKey, 응답 정합성, 오류 매핑.
- 통합: `tests/agent_recommendations.rs`(모의 중앙). 미바인딩 거부, 확인된 팀 경로·본문, 문자열 ID, 입력·위조 문맥의 중앙 호출 전 거부, 중앙 오류 매핑, 거부 뒤 세션 유지.
- MCP: `test/tools.test.mjs`·`test/errors.test.mjs`. 입력 스키마, 경로, GIT_ROUTING_UNAVAILABLE의 rejected·복구 안내.
- 실제 중앙: `e2e/specs/p1-find-agent.spec.ts`(격리 루프백 Spring·PostgreSQL, 실제 CLI·브로커·MCP SDK). 실제 LLM Host는 쓰지 않는다.
