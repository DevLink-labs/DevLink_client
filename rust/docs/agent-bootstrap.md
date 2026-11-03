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

`agent token`은 중앙에서 지원하는 명시적 발급/회전이다. 먼저 `needs_token`을 내구 저장하므로 응답 유실 시 옛 토큰을 계속 사용하지 않는다. 발급 후 Agent Bearer `GET /api/v1/agents/me`의 ID/ownerUserId/teamId가 일치해야 `ready`를 게시한다. 발급 성공 후 검증이나 저장이 실패해도 새 Agent를 생성하지 않고 같은 ID에서 명시적으로 다시 발급한다. 중앙에 없는 메타데이터 수정·삭제·폐기 API를 가정하지 않는다.

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
