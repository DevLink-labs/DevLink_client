# CLI Agent bootstrap (S15P31D108-91)

사용자 로그인과 검증된 선택 팀을 기존 중앙 Agent API, Rust 브로커, MCP 자격 파일에 연결한다. 중앙은 Agent ID·소유자·팀의 권한 근거다. 프로젝트 경로는 사용자가 명시한 실제 로컬 디렉터리를 canonicalize해 저장하며 중앙으로 전송하지 않는다.

```powershell
devlink --config C:\work\profile\config.json auth login --email me@example.test
devlink --config C:\work\profile\config.json team select --id 12
devlink --config C:\work\profile\config.json agent create --name "My Agent" --tool codex --project-name "DevLink" --project C:\work\DevLink
devlink --config C:\work\profile\config.json agent status
devlink --config C:\work\profile\config.json start
devlink --config C:\work\profile\config.json mcp path
devlink --config C:\work\profile\config.json stop
```

`--tool`은 `codex` 또는 `claude-code`다. `--role-description`은 선택 입력이다. `agent list`는 검증된 선택 팀의 목록을 표시한다. 생성/바인딩/발급은 로그인 사용자, 현재 서버 멤버십, 선택 팀을 매번 확인하며 다른 팀이나 같은 팀의 다른 소유자 Agent를 바인딩하지 않는다. 팀 selector만 바꾸면 기존 Agent 토큰으로 다른 팀을 가장할 수 없으며 기동 시 불일치를 거부한다.

## 단일 생성과 복구

중앙 POST 생성에는 idempotency key가 없다. CLI는 같은 프로필의 생성 전에 `create_pending`을 먼저 원자적으로 저장한다. 성공한 ID를 즉시 `needs_token`으로 보관한 후 토큰을 발급한다. 생성 응답 유실·5xx·중단은 자동 재생성하지 않는다. 같은 바인딩·메타데이터의 명령을 다시 실행하면 기존 ID를 확인해 재사용하며 토큰도 자동 회전하지 않는다. 명시적으로 거절된4xx만 `vacant`로 기록해 수정 후 재시도를 허용한다.

불확실한 생성은 목록을 확인한 뒤 **실제 생성된 본인 ID**로 복구한다. 새 프로필을 만들어 같은 생성 요청을 반복해 우회하지 않는다. 기존 ID를 선택하는 명령은 생성/토큰 발급을 암묵적으로 수행하지 않는다.

```powershell
devlink agent list
devlink agent use --id 34 --project C:\work\DevLink
devlink agent token
```

CSRF 선행 조회가 실패하면 생성 POST 전이므로 pending을 남기지 않는다. 실제 POST의 결과가 불확실하고 목록에도 생성된 Agent를 찾을 수 없으면, 서버 기록·지연 처리를 확인한 사용자가 `devlink agent reset-pending --accept-duplicate-risk`로 해당 pending만 명시적으로 비울 수 있다. 이 명령은 생성 요청을 보내지 않는다. 중앙에 지연 생성이 남았을 가능성은 사라지지 않으므로 기존 ID 복구를 우선하며, 이후 새 생성은 사용자의 별도 명령이다. 이미 ID가 알려진 프로필은 reset할 수 없다.

`agent token`은 중앙에서 지원하는 명시적 발급/회전이다. 먼저 `needs_token`을 내구 저장하므로 응답 유실 시 옛 토큰을 계속 사용하지 않는다. 발급 후 Agent Bearer `GET /api/v1/agents/me`의 ID/ownerUserId/teamId가 일치해야 `ready`를 게시한다. 발급 성공 후 검증이나 저장이 실패해도 새 Agent를 생성하지 않고 같은 ID에서 명시적으로 다시 발급한다. 메타데이터 수정은 아래 `agent update`만 쓰며, 중앙에 없는 삭제·폐기 API를 가정하지 않는다.

## Agent 정보 수정 (S15P31D108-146)

```powershell
devlink agent update --project-name "DevLink client" --role-description "MCP 담당"
```

프로필에 묶인 Agent의 `projectName`·`roleDescription`만 바꾼다. 둘 중 하나 이상이 필요하고, 공백만인 값·제어 문자·역할 비우기는 받지 않는다. 중앙 `isBlank`가 U+001C–U+001F를 공백으로 보아 역할을 null로 저장하므로 제어 문자를 막는다. 같은 동작을 MCP `update_agent`(로컬 `/v1/agents/update`)로도 제공한다.

**중앙 계약.** server develop `34b2c15`의 `AgentController`·`AgentService` 코드를 기준으로 했다. 백엔드와 문서로 따로 합의한 계약은 아니다.

2026-11-08 server `aecdcc3`에서도 재확인했다. 제공 상태·인증·권한·DTO·오류와
담당자 확인 상태는 [Agent 갱신 소비 계약](agent-update-contract.md)에 기록한다.

- `PATCH /api/v1/agents/{agentId}`, 본문 `{name?, roleDescription?, projectName?}`(`UpdateAgentRequest`, 각각 최대 100·255·120자).
- 사용자 세션(`JSESSIONID`)과 CSRF만 받는다. Agent Bearer는 받지 않는다.
- 팀원이면서 Agent 소유자여야 한다. 아니면 403 ACCESS_DENIED이고, 없는 Agent는 404 RESOURCE_NOT_FOUND다.
- null은 기존 값을 유지한다. 공백 name·projectName은 400 VALIDATION_FAILED이고, 공백 roleDescription은 null로 저장된다.
- 응답은 `AgentResponse`다. part·roles(3차 MVP)는 이 API로 바꿀 수 없다.

**client 규칙.**

- name은 `register_agent`의 일치 확인과 팀 내 이름·도구 고유 제약에 쓰이므로 바꾸지 않는다. toolType·소유자·팀도 바꾸지 않는다.
- 실행 순서: 사용자 세션 확인 → 선택 팀 확인 → 프로필의 origin·사용자·팀 일치 확인 → `GET /api/v1/agents/{id}`로 현재 값 확인 → 다른 필드만 PATCH. 모두 같으면 PATCH하지 않는다.
- 응답의 ID·소유자·팀·name·toolType과 요청한 값이 모두 맞아야 성공이다. 어긋나면 결과 불명(unknown)으로 보고하고 로컬 프로필을 바꾸지 않는다.
- PATCH가 4xx로 거부되면 rejected다. 전송 실패·5xx·응답 불일치는 중앙에 반영됐을 수 있으므로 결과 불명으로 안내한다. PATCH 전 단계(세션·팀·Agent 조회)의 연결 실패도 지금은 unknown으로 보고하는데, 안전한 쪽으로 틀리는 분류라 그대로 두었다.
- 성공하면 프로필에 캐시한 projectName·roleDescription만 갱신한다. ID·projectRoot·토큰은 그대로이고, 그 사이 다른 작업이 프로필을 바꿨으면 덮어쓰지 않고 `localProfileSynced:false`로 알린다. 새 Agent 생성·토큰 발급은 하지 않는다.
- 브로커는 Agent 정보를 중앙 `/agents/me`에서 실시간으로 읽으므로, 수정 후 `get_my_context`에 새 값이 바로 보인다. 프로필 바인딩(ID·팀·projectRoot·도구)은 바뀌지 않아 실행 중인 브로커를 멈출 필요가 없다.

**MCP 경로의 권한.** 중앙 PATCH가 사용자 세션만 받으므로, 브로커는 이 PC의 `devlink auth login` 세션 파일을 읽어 호출한다. 이 경로를 쓰는 조건은 다음과 같다.

- 요청 세션이 bind된 Agent가 프로필의 Agent와 같아야 한다.
- 사용자·팀·Agent 조회와 CSRF 준비를 마친 뒤, PATCH 직전에 프로필·중앙 신원·MCP 세션을 다시 검증한다. 조회 중 대체·해제된 세션은 전송하지 않는다(Issue #86). 같은 값 요청도 현재 세션을 검증한다.
- 사용자 세션의 사용자가 프로필 사용자와 같아야 한다.
- 바꿀 수 있는 필드는 위 두 가지뿐이다.

사용자 세션이 없거나 만료되면 `USER_LOGIN_REQUIRED`(412), 다른 CLI 작업이 계정 잠금을 쥐고 있으면 `LOCAL_ACCOUNT_BUSY`(409)로 거부한다. 둘 다 중앙을 변경하기 전이다. 모델에게 비밀번호나 쿠키를 넘기지 않는다.

## 자격과 소유권

| 파일 | 보관 내용 | 소비자 |
| --- | --- | --- |
| user-session.json | 사용자 cookie·검증된 사용자 | 로그인·팀·Agent 관리 CLI |
| selected-team.json | origin/user/team selector, 초대코드 없음 | CLI, 기동 시 바인딩 대조 |
| agent-profile.json | origin/user/team/Agent ID·로컬 프로젝트·상태·중앙 Agent 토큰 | CLI·Rust |
| broker.json | schemaVersion/brokerUrl/localToken | MCP Host의 DEVLINK_BROKER_FILE |
| runtime.json | 이번 프로세스 controlToken·PID·포트 | CLI 제어 |

자격 파일은 기존 사용자 전용 ACL/권한·안전 경로 확인·크기 제한·원자적 교체를 적용한다. 중앙 토큰·사용자 cookie·controlToken은 MCP 파일/Host 설정/인자/출력에 넣지 않는다. Host에는 `mcp path`가 출력한 **파일 경로**만 설정한다. 파일명과 Host 설정 예시는 [기존 MCP reader 문서](../../packages/mcp-server/docs/agent-context.md)와 [호환성 문서](../../packages/mcp-server/docs/host-compatibility.md)를 따른다. 전역 Host 설정은 수정하지 않는다.

프로필이 없을 때만 기존 `DEVLINK_TOKEN` 기동을 지원한다. 프로필이 있으면 미완성/손상/다른 서버/충돌하는 환경 토큰에 대해 fallback하지 않는다. 기동 때 저장 토큰을 중앙에서 다시 확인한다. 로그아웃·사용자 cookie 만료는 별도 Agent 토큰의 서버 폐기가 아니다. 다른 사용자/서버로 로그인할 때 실행 중인 브로커는 먼저 중지해야 하며, 중지 후 전환은 이전 로컬 Agent 토큰을 제거한다. 다른 사람의 Agent에 자동 재바인딩하지 않는다.

프로필 변경과 토큰 발급/회전은 브로커 중지 후만 허용한다. 동일 프로필의 account/control/broker lock과 추가적인 **동일 중앙 origin·사용자의 로컬 머신 owner lock**으로 여러 config 프로필이 같은 사용자의 브로커를 동시에 실행하지 못하게 한다. OS lock은 프로세스 종료 시 해제되며 다른 사용자의 파일이나 세션을 강제로 제거하지 않는다. 이 로컬 잠금은 중앙의 Agent별 registry를 서버 전체·다른 기기의 사용자당 단일 브로커 정책으로 확장한 것이 아니다. 중앙 -49와 실제 Host 업무 세션 소유권/lease -92의 검증은 별도다.

## 검증 범위

추가 owner lock의 범위는 같은 OS 계정·같은 중앙 origin을 사용하는 Agent 프로필 기동이다. 다른 OS 계정/다른 origin alias나 프로필 없는 기존 DEVLINK_TOKEN 기동을 중앙 전체 사용자 정책으로 묶지 않는다. 프로젝트 경로를 바꿔 명시적으로 다시 바인딩하면 로컬 자격을 다시 발행하고 토큰 발급을 요구한다. 사용자 전환 시 Agent 토큰 제거 후 새 사용자 세션 저장이 실패하면 이전 세션이 남더라도 Agent 재인증이 필요할 수 있다.

`cli_agents.rs`는 실제 CLI·파일·로컬 Rust 브로커와 합성 중앙 API/WS를 사용한다. 생성 응답 유실과 중복 방지, 명시적 복구, 잘못된 소유자·팀·토큰 신원, 손상 프로필, 자격 분리, 재시작, 두 프로필 소유권을 회귀로 확인한다.

`rust/scripts/verify-agent-bootstrap.mjs`는 실제 loopback Spring/PostgreSQL과 두 합성 사용자를 사용한다. `DEVLINK_TEST_CENTRAL_URL`·절대 경로 `DEVLINK_TEST_CLI`를 지정한다. `DEVLINK_TEST_HOSTS=both`는 실제 Codex·Claude Code를 임시 설정으로 기동해 같은 발행 파일의 로딩을 검증한다. 비밀 인자나 전역 설정 수정 없이 실제 MCP tool event를 확인하고 소유한 MCP 자식 프로세스를 정리한다.

초기 검증의 과거 중앙 READY에는 user/team이 없어 업무 세션 수립이 `CENTRAL_UNAUTHENTICATED`로 닫혔다. 현재 CLI는 [READY와 실시간 신원 조회](ready-identity.md)로 세션을 공급하므로 이 과거 제한을 현재 차단 사유로 사용하지 않는다. 유효한 MCP 파일로 해당 응답을 받는 것은 reader와 로컬 자격 인증을 통과했다는 범위의 증거다. **Agent 등록/WS 연결·Host 파일 읽기 성공과 업무 세션/전체 MVP 왕복 성공은 구분한다.** -92의 신원 공급, -93/-94/-97 업무 연결 및 기존 -66 최종 왕복 수용을 완료했다고 주장하지 않는다.
